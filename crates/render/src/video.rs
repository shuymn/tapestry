//! Skin movies on the GPU (design.md "Rendering and Skins", video frames):
//! each movie's newest due frame, I420 as the media worker publishes it, in
//! three `R8Unorm` plane textures of its own, sized to that movie (Y at its
//! size, U and V at half its size rounded up), which the sprite shader
//! samples for a quad with [`FLAG_VIDEO`] and converts to R'G'B' by the
//! movie's matrix and range, as players convert video on the GPU (SDL's
//! and mpv's planar paths; wgpu has no I420 format). A movie quad is a
//! sprite of the batch, so it keeps its destination's place in the draw
//! order, its blend and its clip.
//!
//! Each movie also has an upload buffer of its own, one frame at the
//! texture copy's 256-byte row pitch. The render thread copies a due frame
//! from its ring straight into that buffer (the queue's mapped view of that
//! movie's span alone, through the `upload` module's one write function);
//! when the copy proves whole, the frame's encoder copies it into the
//! movie's planes before the pass that samples them. A copy that fails
//! leaves the planes as they were and records no copy.
//!
//! A movie's planes and buffer are made when it is reserved (its admission,
//! at a preparation boundary, never a frame's upload) and touch no other
//! movie's: a peer admitted later leaves its planes, its frame and its
//! queued copy as they are. They are released when the movie or its screen
//! leaves ([`Video::release`]): a slot with none binds a shared 1×1
//! stand-in, so the GPU holds only the planes of movies still kept. Making
//! the bind group allocates nothing of the app's: its views and entries are
//! fixed-size arrays.

use crate::gpu::Gpu;

/// A video quad: its layer (low 15 bits) is the movie's slot.
pub const FLAG_VIDEO: u32 = 1 << 20;
/// The movie's matrix is BT.601 (else BT.709).
pub const FLAG_VIDEO_BT601: u32 = 1 << 21;
/// The movie's range is full (else limited).
pub const FLAG_VIDEO_FULL: u32 = 1 << 22;

/// The movie slots of a screen (design.md "Rendering and Skins", skin
/// movies): the first two for the skin's movies, the other two for the
/// chart's videos (BGA).
pub const MAX_MOVIES: u32 = skin_model::movie::SLOTS as u32;

/// The planes the video group binds: three a slot.
const PLANES: usize = 3 * skin_model::movie::SLOTS;

/// The uv of a movie's whole frame: its planes are its own size.
pub const VIDEO_UV: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// The group of the planes, after the atlas's (0) and the BGA's (1).
pub(crate) const GROUP: u32 = 2;

/// The video group's layout: per slot, its Y, U and V planes.
pub(crate) fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("video"),
        entries: &entries(),
    })
}

/// The video group's layout entries: slot `i`'s Y, U and V planes at
/// bindings `3i`, `3i + 1` and `3i + 2`.
pub(crate) fn entries() -> [wgpu::BindGroupLayoutEntry; PLANES] {
    let mut binding = 0;
    [(); PLANES].map(|()| {
        let entry = wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        binding += 1;
        entry
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

/// A movie of `width` × `height`'s GPU bytes: its three planes' texels and
/// its upload buffer (each plane's rows at the copy's pitch).
pub(crate) const fn movie_bytes(width: u32, height: u32) -> (u64, u64) {
    let (w, h) = (width as u64, height as u64);
    let (cw, ch) = (half(width) as u64, half(height) as u64);
    let texels = w * h + 2 * cw * ch;
    let upload = aligned(width) as u64 * h + 2 * aligned(half(width)) as u64 * ch;
    (texels, upload)
}

fn plane(gpu: &Gpu, label: &str, width: u32, height: u32) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// One movie's planes and upload buffer, sized to it.
struct Movie {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    width: u32,
    height: u32,
    /// Its frame, each plane's rows at `pitch` (luma, chroma).
    upload: wgpu::Buffer,
    pitch: [u32; 2],
    /// Whether the next frame copies the buffer into the planes.
    copy: bool,
}

impl Movie {
    fn sized(gpu: &Gpu, width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        let (_, bytes) = movie_bytes(width, height);
        Self {
            y: plane(gpu, "video y", width, height),
            u: plane(gpu, "video u", half(width), half(height)),
            v: plane(gpu, "video v", half(width), half(height)),
            width,
            height,
            upload: gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("video upload"),
                size: bytes,
                usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            pitch: [aligned(width), aligned(half(width))],
            copy: false,
        }
    }
}

/// The movies' planes, by slot, their bind group and upload buffers.
pub(crate) struct Video {
    layout: wgpu::BindGroupLayout,
    /// The 1×1 planes a slot with no movie binds.
    blank: Movie,
    movies: [Option<Movie>; MAX_MOVIES as usize],
    pub(crate) bind: wgpu::BindGroup,
}

impl Video {
    /// No movie in any slot over the video group's `layout`: each binds
    /// the 1×1 stand-in.
    pub(crate) fn new(gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> Self {
        let blank = Movie::sized(gpu, 1, 1);
        let movies = std::array::from_fn(|_| None);
        let bind = bind(gpu, layout, &blank, &movies);
        Self {
            layout: layout.clone(),
            blank,
            movies,
            bind,
        }
    }

    /// Replaces slot `slot`'s planes and buffer with ones of a movie of
    /// `width` × `height`: a movie's admission, not a frame's upload. The
    /// other slots keep their planes, their frames and their queued copies.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn reserve(&mut self, gpu: &Gpu, slot: u32, width: u32, height: u32) {
        let Some(m) = self.movies.get_mut(slot as usize) else {
            return;
        };
        *m = Some(Movie::sized(gpu, width, height));
        self.bind = bind(gpu, &self.layout, &self.blank, &self.movies);
    }

    /// Releases slot `slot`'s planes and buffer, its movie or its screen
    /// gone: the slot binds the stand-in again. The other slots keep their
    /// own.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn release(&mut self, gpu: &Gpu, slot: u32) {
        let Some(m) = self.movies.get_mut(slot as usize) else {
            return;
        };
        if m.take().is_some() {
            self.bind = bind(gpu, &self.layout, &self.blank, &self.movies);
        }
    }

    /// `fill` writes a frame of `width` × `height` into slot `slot`'s
    /// mapped upload buffer through its sink (a byte offset in it and the
    /// bytes), each plane's rows at the pitch it is given (luma, chroma: Y,
    /// then U, then V), and says whether the frame is whole; then the next
    /// frame copies it into that slot's planes. `false` when the frame is
    /// not the size the slot was reserved for, the buffer cannot be mapped
    /// or `fill` says no, and nothing is copied.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn write_with(
        &mut self,
        gpu: &Gpu,
        slot: u32,
        width: u32,
        height: u32,
        fill: impl FnOnce(&mut dyn FnMut(usize, &[u8]) -> bool, [usize; 2]) -> bool,
    ) -> bool {
        let Some(Some(m)) = self.movies.get_mut(slot as usize) else {
            return false;
        };
        if m.width != width || m.height != height {
            return false;
        }
        let Some(size) = wgpu::BufferSize::new(m.upload.size()) else {
            return false;
        };
        let pitch = [m.pitch[0] as usize, m.pitch[1] as usize];
        let whole = crate::upload::write_buffer_with_unpaced(gpu, &m.upload, 0, size, |sink| {
            fill(sink, pitch)
        });
        if whole {
            m.copy = true;
        }
        whole
    }

    /// Records the copies in hand into `encoder`, before the pass that
    /// samples the planes, and clears them.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn record(&mut self, encoder: &mut wgpu::CommandEncoder) {
        for m in self.movies.iter_mut().flatten().filter(|m| m.copy) {
            m.copy = false;
            let luma = u64::from(m.pitch[0]) * u64::from(m.height);
            let chroma = u64::from(m.pitch[1]) * u64::from(half(m.height));
            let (cw, ch) = (half(m.width), half(m.height));
            for (texture, offset, pitch, w, h) in [
                (&m.y, 0, m.pitch[0], m.width, m.height),
                (&m.u, luma, m.pitch[1], cw, ch),
                (&m.v, luma + chroma, m.pitch[1], cw, ch),
            ] {
                encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: &m.upload,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset,
                            bytes_per_row: Some(pitch),
                            rows_per_image: None,
                        },
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
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

    /// The GPU bytes the slots' movies hold, as their resources were
    /// made: their planes' texels and their upload buffers (the stand-in
    /// left out).
    #[cfg(any(test, feature = "dev-tools"))]
    pub(crate) fn bytes(&self) -> (u64, u64) {
        let texels = |t: &wgpu::Texture| u64::from(t.width()) * u64::from(t.height());
        self.movies.iter().flatten().fold((0, 0), |(t, u), m| {
            (
                t + texels(&m.y) + texels(&m.u) + texels(&m.v),
                u + m.upload.size(),
            )
        })
    }
}

/// The video group over `layout`: each slot's movie's planes, or
/// `blank`'s for a slot with none. Fixed-size arrays: the app allocates
/// nothing here, wgpu's own objects aside.
fn bind(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    blank: &Movie,
    movies: &[Option<Movie>; MAX_MOVIES as usize],
) -> wgpu::BindGroup {
    let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
    let views: [wgpu::TextureView; PLANES] = std::array::from_fn(|i| {
        let m = movies.get(i / 3).and_then(Option::as_ref).unwrap_or(blank);
        view([&m.y, &m.u, &m.v].get(i % 3).copied().unwrap_or(&m.y))
    });
    let mut binding = 0;
    let entries = views.each_ref().map(|view| {
        let entry = wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(view),
        };
        binding += 1;
        entry
    });
    gpu.device().create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("video"),
        layout,
        entries: &entries,
    })
}
