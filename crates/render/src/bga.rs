//! The BGA array (design.md "Rendering and Skins", BGA): one `Rgba8Unorm`
//! texture array of [`PAGE_SIDE`] pages, no mips, the chart's pictures
//! written into it cell by cell at preparation, bound as the sprite
//! pipelines' second group. Without an array, a one-texel stand-in is
//! bound, which no table describes.
//!
//! **A batch's upload** ([`BgaUpload`]): the render thread takes a batch of
//! cells in hand and writes it at its frame boundaries, outside every
//! frame, at most [`BGA_WRITE_BUDGET`] bytes at a boundary (rows padded as
//! the queue pads them, a cell split between boundaries by its rows), each
//! cell's bytes dropped once written, and submits each boundary's writes,
//! so their staging is released as the GPU completes them; after the last
//! write it registers one flag-only completion, and the batch is done at
//! the first boundary whose non-blocking poll saw it. Nothing waits for
//! the GPU (design.md "Memory Budget").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use skin_model::bga::{BgaKey, CellPlace, PAGE_BUDGET, PAGE_SIDE};

use crate::gpu::Gpu;

/// The most bytes the queue stages for the BGA array's writes at one frame
/// boundary, rows padded as the queue pads them (design.md "Memory
/// Budget"; a provisional limit: a 1 024² cell, so the six pages' 384 MiB
/// go over in about 96 boundaries).
pub const BGA_WRITE_BUDGET: u64 = 4 << 20;

/// What a frame boundary's step of a batch's upload came to
/// ([`Renderer::bga_boundary`](crate::Renderer::bga_boundary)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgaStep {
    /// No batch in hand.
    Idle,
    /// Its writes go on, or its completion is not seen yet: stepped again
    /// at the next boundary.
    Pending,
    /// The batch ended: written and its completion seen (`Ok`), or its
    /// array gone ([`BgaFault::Stale`]) or the device's refusal
    /// ([`BgaFault::Refused`]). Nothing of it is left in hand.
    Done(Result<(), BgaFault>),
}

/// A batch of cells in hand, written a boundary at a time.
pub(crate) struct BgaUpload {
    pub(crate) key: BgaKey,
    /// Each cell's place and texels; a cell's texels are dropped once
    /// written.
    cells: Vec<(CellPlace, Vec<u8>)>,
    /// The next cell to write and its next row.
    cell: usize,
    row: u32,
    /// The batch's completion, registered after its last write.
    pub(crate) done: Option<Arc<AtomicBool>>,
}

impl BgaUpload {
    /// Whether its completion was seen.
    pub(crate) fn completed(&self) -> bool {
        self.done
            .as_ref()
            .is_some_and(|d| d.load(Ordering::Acquire))
    }

    /// Its texels' bytes still in hand.
    #[must_use]
    pub(crate) fn bytes(&self) -> u64 {
        self.cells
            .iter()
            .map(|(_, t)| u64::try_from(t.len()).unwrap_or(u64::MAX))
            .fold(0, u64::saturating_add)
    }
}

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

    /// Takes `cells` (each its place and its texels, premultiplied RGBA8
    /// rows of its padded size) in hand as a batch for the array `key`
    /// names, nothing written yet.
    ///
    /// # Errors
    ///
    /// [`BgaFault::Stale`] for the stand-in; [`BgaFault::Cell`] for a cell
    /// off its page or bytes of another size: nothing is taken.
    pub(crate) fn upload(
        &self,
        key: BgaKey,
        cells: Vec<(CellPlace, Vec<u8>)>,
    ) -> Result<BgaUpload, BgaFault> {
        if self.texture.is_none() {
            return Err(BgaFault::Stale);
        }
        if !cells.iter().all(|(place, texels)| self.fits(place, texels)) {
            return Err(BgaFault::Cell);
        }
        Ok(BgaUpload {
            key,
            cells,
            cell: 0,
            row: 0,
            done: None,
        })
    }

    /// Whether a cell at `place` with `texels` lies on one of its pages,
    /// its bytes its padded size.
    fn fits(&self, place: &CellPlace, texels: &[u8]) -> bool {
        let [w, h] = place.padded();
        let [x, y] = place.origin;
        place.page < self.pages
            && x.checked_add(w).is_some_and(|r| r <= PAGE_SIDE)
            && y.checked_add(h).is_some_and(|b| b <= PAGE_SIDE)
            && u64::try_from(texels.len()).ok() == Some(u64::from(w) * u64::from(h) * 4)
    }

    /// Queues `up`'s next rows, at most `budget` bytes as the queue stages
    /// them (at least one row), each cell's texels dropped once its last
    /// row is queued. Whether every cell is queued.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn write_some(&self, gpu: &Gpu, up: &mut BgaUpload, budget: u64) -> bool {
        let Some(t) = &self.texture else {
            return true;
        };
        let mut bytes = 0_u64;
        while let Some((place, texels)) = up.cells.get_mut(up.cell) {
            let [w, h] = place.padded();
            let [x, y] = place.origin;
            let row = crate::text_upload::padded(w);
            let room = budget.saturating_sub(bytes) / row.max(1);
            if room == 0 && bytes > 0 {
                return false;
            }
            let n = u32::try_from(room.max(1))
                .unwrap_or(u32::MAX)
                .min(h.saturating_sub(up.row));
            let stride = usize::try_from(w).unwrap_or(0) * 4;
            let from = usize::try_from(up.row).unwrap_or(0) * stride;
            let to = from + usize::try_from(n).unwrap_or(0) * stride;
            if let Some(rows) = texels.get(from..to) {
                crate::skin_atlas::write_region(
                    gpu.queue(),
                    t,
                    (place.page, 0),
                    [x, y + up.row],
                    [w, n],
                    rows,
                );
            }
            bytes = bytes.saturating_add(row * u64::from(n));
            up.row += n;
            if up.row >= h {
                *texels = Vec::new();
                up.cell += 1;
                up.row = 0;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use skin_model::bga::CellPlace;

    use crate::{AdapterChoice, BgaStep, Renderer, Target};

    /// Each boundary's writes are submitted at that boundary (design.md
    /// "Memory Budget": the queue's staging is released as the GPU
    /// completes it, not held until the batch's last write): four cells of
    /// one budget each are written one a boundary, and every one of those
    /// boundaries submits once, the last included.
    #[test]
    fn each_boundary_s_writes_are_submitted_at_that_boundary() {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 64,
                height: 64,
            },
            AdapterChoice::Warp,
        )
        .unwrap();
        let key = r.begin_bga(1).unwrap();
        let cells = (0..4_u32)
            .map(|i| {
                let place = CellPlace {
                    page: 0,
                    origin: [i % 2 * 1_024, i / 2 * 1_024],
                    content: [1_022, 1_022],
                };
                (place, vec![255; 1_024 * 1_024 * 4])
            })
            .collect();
        r.stage_bga_cells(key, cells).unwrap();
        for boundary in 1..=3 {
            assert_eq!(r.bga_boundary(), BgaStep::Pending);
            assert_eq!(
                r.bga_submits.get(),
                boundary,
                "submitted at boundary {boundary}"
            );
        }
        let mut step = r.bga_boundary();
        assert_eq!(
            r.bga_submits.get(),
            4,
            "the last write submitted at its boundary"
        );
        let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while step == BgaStep::Pending && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(1));
            step = r.bga_boundary();
        }
        assert_eq!(step, BgaStep::Done(Ok(())));
        assert_eq!(
            r.bga_submits.get(),
            4,
            "a boundary with nothing written submits nothing"
        );
    }
}
