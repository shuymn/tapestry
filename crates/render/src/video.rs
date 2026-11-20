//! Skin movies on the GPU (design.md "Rendering and Skins", video frames):
//! each movie's newest due frame, I420 as the media worker publishes it, in
//! three `R8Unorm` plane arrays (Y at full size, U and V at half size
//! rounded up, one layer a movie), which the sprite shader samples for a
//! quad with [`FLAG_VIDEO`] and converts to R'G'B' by the movie's matrix
//! and range, as players convert video on the GPU (SDL's and mpv's planar
//! paths; wgpu has no I420 format). A movie quad is a sprite of the batch,
//! so it keeps its destination's place in the draw order, its blend and
//! its clip.
//!
//! The render thread copies a due frame from its ring straight into the
//! movie's region of an upload buffer (the queue's mapped view, through
//! the `upload` module's one write function), each plane's rows at the texture copy's 256-byte row pitch; when
//! the copy proves whole, the frame's encoder copies the region into the
//! planes before the pass that samples them. A copy that fails leaves the
//! planes as they were and records no copy.

use crate::gpu::Gpu;

/// A video quad: its layer (low 15 bits) is the movie's.
pub const FLAG_VIDEO: u32 = 1 << 20;
/// The movie's matrix is BT.601 (else BT.709).
pub const FLAG_VIDEO_BT601: u32 = 1 << 21;
/// The movie's range is full (else limited).
pub const FLAG_VIDEO_FULL: u32 = 1 << 22;

/// The movies a screen plays at most (design.md "Rendering and Skins",
/// skin movies): one layer each.
pub const MAX_MOVIES: u32 = 2;

/// The group of the plane arrays, after the atlas's (0) and the BGA's (1).
pub(crate) const GROUP: u32 = 2;

/// The video group's layout: the Y, U and V plane arrays.
pub(crate) fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let plane = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2Array,
            multisampled: false,
        },
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("video"),
        entries: &[plane(0), plane(1), plane(2)],
    })
}

/// The chroma side of a luma side, rounded up.
const fn half(side: u32) -> u32 {
    side.div_ceil(2)
}

/// `n` rounded up to the texture copy's row alignment.
const fn aligned(n: u32) -> u32 {
    n.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

fn plane(gpu: &Gpu, label: &str, width: u32, height: u32, layers: u32) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: layers,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..wgpu::TextureViewDescriptor::default()
    })
}

/// A movie's frame copy the next frame records: its layer and size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Copy {
    layer: u32,
    width: u32,
    height: u32,
}

/// The movies' plane arrays, their bind group and the upload buffer.
pub(crate) struct Video {
    layout: wgpu::BindGroupLayout,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    pub(crate) bind: wgpu::BindGroup,
    /// The arrays' luma size and layers.
    width: u32,
    height: u32,
    layers: u32,
    /// One region a layer, each plane's rows at `pitch` (luma, chroma).
    upload: wgpu::Buffer,
    pitch: [u32; 2],
    region: u64,
    /// The copies the next frame records, at most one a layer.
    copies: [Option<Copy>; MAX_MOVIES as usize],
}

impl Video {
    /// Arrays of one 1×1 layer over the video group's `layout`, the
    /// stand-in bound when no movie is reserved.
    pub(crate) fn new(gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> Self {
        Self::sized(gpu, layout.clone(), 1, 1, 1)
    }

    fn sized(
        gpu: &Gpu,
        layout: wgpu::BindGroupLayout,
        width: u32,
        height: u32,
        layers: u32,
    ) -> Self {
        let (width, height, layers) = (width.max(1), height.max(1), layers.max(1));
        let y = plane(gpu, "video y", width, height, layers);
        let u = plane(gpu, "video u", half(width), half(height), layers);
        let v = plane(gpu, "video v", half(width), half(height), layers);
        let (vy, vu, vv) = (view(&y), view(&u), view(&v));
        let entry = |binding, view| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(view),
        };
        let bind = gpu.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &layout,
            entries: &[entry(0, &vy), entry(1, &vu), entry(2, &vv)],
        });
        let pitch = [aligned(width), aligned(half(width))];
        let region = u64::from(pitch[0]) * u64::from(height)
            + 2 * u64::from(pitch[1]) * u64::from(half(height));
        let upload = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("video upload"),
            size: region * u64::from(layers),
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            layout,
            y,
            u,
            v,
            bind,
            width,
            height,
            layers,
            upload,
            pitch,
            region,
            copies: [None; MAX_MOVIES as usize],
        }
    }

    /// Replaces the arrays with `layers` (at most [`MAX_MOVIES`]) movies up
    /// to `width` × `height`: a screen's preparation, not a frame's work.
    pub(crate) fn reserve(&mut self, gpu: &Gpu, width: u32, height: u32, layers: u32) {
        let layout = self.layout.clone();
        *self = Self::sized(gpu, layout, width, height, layers.min(MAX_MOVIES));
    }

    /// Whether a movie of `width` × `height` fits layer `layer`.
    const fn fits(&self, layer: u32, width: u32, height: u32) -> bool {
        layer < self.layers && width <= self.width && height <= self.height
    }

    /// `fill` writes a frame of `width` × `height` into layer `layer`'s
    /// region of the mapped upload buffer through its sink (a byte offset
    /// in the region and the bytes), each plane's rows at the pitch it is
    /// given (luma, chroma: Y, then U, then V), and says whether the frame
    /// is whole; then the next frame copies it into the planes. `false`
    /// when it does not fit, the buffer cannot be mapped or `fill` says
    /// no, and nothing is copied.
    pub(crate) fn write_with(
        &mut self,
        gpu: &Gpu,
        layer: u32,
        width: u32,
        height: u32,
        fill: impl FnOnce(&mut dyn FnMut(usize, &[u8]) -> bool, [usize; 2]) -> bool,
    ) -> bool {
        let Some(size) = wgpu::BufferSize::new(self.region) else {
            return false;
        };
        if !self.fits(layer, width, height) {
            return false;
        }
        let pitch = [self.pitch[0] as usize, self.pitch[1] as usize];
        let offset = self.region * u64::from(layer);
        let whole =
            crate::upload::write_buffer_with_unpaced(gpu, &self.upload, offset, size, |sink| {
                fill(sink, pitch)
            });
        if whole {
            let copy = Copy {
                layer,
                width,
                height,
            };
            if let Some(c) = self
                .copies
                .iter_mut()
                .find(|c| c.is_none_or(|c| c.layer == layer))
            {
                *c = Some(copy);
            }
        }
        whole
    }

    /// Records the copies in hand into `encoder`, before the pass that
    /// samples the planes, and clears them.
    pub(crate) fn record(&mut self, encoder: &mut wgpu::CommandEncoder) {
        for c in self.copies.iter_mut().filter_map(Option::take) {
            let base = self.region * u64::from(c.layer);
            let luma = u64::from(self.pitch[0]) * u64::from(c.height);
            let chroma = u64::from(self.pitch[1]) * u64::from(half(c.height));
            for (texture, offset, pitch, w, h) in [
                (&self.y, base, self.pitch[0], c.width, c.height),
                (
                    &self.u,
                    base + luma,
                    self.pitch[1],
                    half(c.width),
                    half(c.height),
                ),
                (
                    &self.v,
                    base + luma + chroma,
                    self.pitch[1],
                    half(c.width),
                    half(c.height),
                ),
            ] {
                encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: &self.upload,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset,
                            bytes_per_row: Some(pitch),
                            rows_per_image: None,
                        },
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: c.layer,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }

    /// The uv of a movie of `width` × `height` in these arrays: its frame
    /// at the arrays' top left.
    pub(crate) fn uv(&self, width: u32, height: u32) -> [f32; 4] {
        #[expect(
            clippy::cast_precision_loss,
            reason = "sides are at most 8 192, exact in f32"
        )]
        [
            0.0,
            0.0,
            width as f32 / self.width as f32,
            height as f32 / self.height as f32,
        ]
    }
}
