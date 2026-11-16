//! The BGA array (design.md "Rendering and Skins", BGA): one `Rgba8Unorm`
//! texture array of [`PAGE_SIDE`] pages, no mips, the chart's pictures
//! written into it cell by cell at preparation, bound as the sprite
//! pipelines' second group. Without an array, a one-texel stand-in is
//! bound, which no table describes.

use skin_model::bga::{BgaKey, CellPlace, PAGE_BUDGET, PAGE_SIDE};

use crate::gpu::Gpu;

/// Why the BGA array could not take a step: its pictures stay absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BgaFault {
    /// No page, more than [`PAGE_BUDGET`], or more than the device holds.
    #[error("{0} pages")]
    Pages(u32),
    /// The array named is not the one in use.
    #[error("the array is not the one in use")]
    Stale,
    /// A cell that leaves its page, or whose bytes are not its size.
    #[error("a cell outside its page")]
    Cell,
    /// The device refused the array or a write.
    #[error("refused by the device")]
    Refused,
}

/// The array in use, or the stand-in.
pub(crate) struct BgaArray {
    /// `None` for the stand-in.
    texture: Option<wgpu::Texture>,
    pub(crate) bind: wgpu::BindGroup,
    pages: u32,
    /// Its serial among this renderer's arrays; 0 for the stand-in.
    serial: u64,
}

/// The second group's layout: the BGA array.
pub(crate) fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("bga"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2Array,
                multisampled: false,
            },
            count: None,
        }],
    })
}

fn texture(gpu: &Gpu, side: u32, pages: u32) -> wgpu::Texture {
    gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("bga"),
        size: wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: pages,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    t: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view = t.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..wgpu::TextureViewDescriptor::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bga"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&view),
        }],
    })
}

impl BgaArray {
    /// The stand-in: one transparent texel on one layer.
    pub(crate) fn stand_in(gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> Self {
        let t = texture(gpu, 1, 1);
        Self {
            bind: bind(gpu.device(), layout, &t),
            texture: None,
            pages: 0,
            serial: 0,
        }
    }

    /// A new array of `pages` pages, its serial `serial`, every texel
    /// transparent.
    ///
    /// # Errors
    ///
    /// [`BgaFault::Pages`] for no page, more than [`PAGE_BUDGET`] or more
    /// than the device's limits; [`BgaFault::Refused`] when the device
    /// refuses it.
    pub(crate) fn new(
        gpu: &Gpu,
        layout: &wgpu::BindGroupLayout,
        pages: u32,
        serial: u64,
    ) -> Result<Self, BgaFault> {
        let limits = gpu.device().limits();
        if pages == 0
            || pages > PAGE_BUDGET
            || pages > limits.max_texture_array_layers
            || PAGE_SIDE > limits.max_texture_dimension_2d
        {
            return Err(BgaFault::Pages(pages));
        }
        let (made, refused) = crate::skin_atlas::scoped(gpu.device(), || {
            let t = texture(gpu, PAGE_SIDE, pages);
            let b = bind(gpu.device(), layout, &t);
            (t, b)
        });
        if refused.is_some() {
            return Err(BgaFault::Refused);
        }
        let (t, bind) = made;
        Ok(Self {
            texture: Some(t),
            bind,
            pages,
            serial,
        })
    }

    /// Its key under renderer `renderer`; `None` for the stand-in.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn key(&self, renderer: u64) -> Option<BgaKey> {
        self.texture.as_ref().map(|_| BgaKey {
            renderer,
            array: self.serial,
        })
    }

    /// Its pages' bytes on the GPU.
    pub(crate) fn bytes(&self) -> u64 {
        u64::from(self.pages) * u64::from(PAGE_SIDE) * u64::from(PAGE_SIDE) * 4
    }

    /// Queues the write of a cell's `texels` (premultiplied RGBA8 rows of
    /// its padded size) at `place`.
    ///
    /// # Errors
    ///
    /// [`BgaFault::Cell`] for a cell off its page or bytes of another size;
    /// [`BgaFault::Stale`] for the stand-in.
    pub(crate) fn write(
        &self,
        gpu: &Gpu,
        place: &CellPlace,
        texels: &[u8],
    ) -> Result<(), BgaFault> {
        let Some(t) = &self.texture else {
            return Err(BgaFault::Stale);
        };
        let [w, h] = place.padded();
        let [x, y] = place.origin;
        let fits = place.page < self.pages
            && x.checked_add(w).is_some_and(|r| r <= PAGE_SIDE)
            && y.checked_add(h).is_some_and(|b| b <= PAGE_SIDE)
            && u64::try_from(texels.len()).ok() == Some(u64::from(w) * u64::from(h) * 4);
        if !fits {
            return Err(BgaFault::Cell);
        }
        crate::skin_atlas::write_region(gpu.queue(), t, (place.page, 0), [x, y], [w, h], texels);
        Ok(())
    }
}
