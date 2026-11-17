//! The selection's chart-image pool (design.md "Rendering and Skins",
//! chart images; docs/stage3.md S10-35): an `Rgba8Unorm` texture array of
//! its own, bound beside the selection installation's atlas, whose layers
//! are cut into a grid of slots, each a stage-file cell above a banner
//! cell ([`ChartCells`]).
//!
//! The pool holds the lesser of [`POOL_MAX_SLOTS`] slots and
//! [`POOL_MAX_TEXELS`] level-0 texels, grid waste counted, in whole layers
//! of one side. Its first layer is firm: it holds slot 0, charged with the
//! selection installation, and the other slots of that layer, which cost
//! nothing more. Every further layer is optional, a reclaimable term of the
//! shared texel total that yields before any admission is refused
//! ([`Renderer::admit`](crate::Renderer::admit)).
//!
//! Each creation of the array takes a new generation from a process-wide
//! counter, so a generation names one array whatever renderer made it; a
//! write or a table of another generation is not acted on.
//!
//! A write puts one slot's cell pair into the array: every level of each
//! image at its cell's origin `>> k`, queued inside error scopes, with no
//! submission and no wait for the GPU; the next frame's submission flushes
//! it, ordered after every frame already submitted. A table is a copy of a
//! slot's record, built per frame with nothing allocated.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};

use skin_model::MIP_LEVELS;
use skin_model::chart_image::{
    ChartCells, ChartKey, ChartTable, ChartUv, Fitted, POOL_LAYER, Slot,
};

use crate::gpu::Gpu;
use crate::skin_atlas;

/// The most slots a pool holds (provisional, design.md "Rendering and
/// Skins", chart images).
pub const POOL_MAX_SLOTS: u16 = 48;

/// The most level-0 texels a pool's layers hold, grid waste counted
/// (provisional): a pool whose one layer passes it keeps that layer, its
/// firm slot, as the chart layer it replaces would.
pub const POOL_MAX_TEXELS: u64 = 16 << 20;

/// The next pool generation; 0 names none.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// A live pool's shape, as the renderer publishes it: its generation, its
/// slots and their cells, and its layers of `side` texels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolLayout {
    /// The array's generation, never 0 and never given to another array
    /// of the process.
    pub generation: u64,
    /// Its slots, slot 0 the firm one.
    pub slots: u16,
    /// Each slot's cells: the stage file's above the banner's.
    pub cells: ChartCells,
    /// Its layers' side.
    pub side: u32,
    /// Its layers.
    pub layers: u32,
}

/// Why a pool write was not applied ([`Renderer::write_pool`](crate::Renderer::write_pool)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PoolWriteError {
    /// The generation is not the live one, or the slot is not one of its.
    #[error("the pool generation is not live")]
    Stale,
    /// A fit does not lie in its cell, or the device refused the write:
    /// the slot's record is cleared.
    #[error("the pool write was refused")]
    Refused,
    /// The device was lost: no pool is live.
    #[error("the device was lost")]
    DeviceLost,
}

/// A pool's grid: the cells of one slot and how many slots a layer of
/// `side` holds, with the most layers a pool of them may have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Grid {
    cells: ChartCells,
    side: u32,
    /// A slot's size: the stage cell above the banner cell.
    pair: [u32; 2],
    cols: u32,
    rows: u32,
    max_layers: u32,
}

impl Grid {
    /// The grid of `cells` in layers of `side` texels on a device of
    /// `limits`; `None` without a cell, or for a side the device does not
    /// hold or a slot no layer holds.
    pub(crate) fn of(cells: ChartCells, side: u32, limits: &wgpu::Limits) -> Option<Self> {
        let size = |c: Option<Slot>| c.map_or([0, 0], Slot::size);
        let [sw, sh] = size(cells.stage);
        let [bw, bh] = size(cells.banner);
        let pair = [sw.max(bw), sh.saturating_add(bh)];
        let [pw, ph] = pair;
        if pw == 0 || ph == 0 || side > limits.max_texture_dimension_2d {
            return None;
        }
        let (cols, rows) = (side / pw, side / ph);
        let per_layer = cols.checked_mul(rows).filter(|&n| n > 0)?;
        let layer = u64::from(side) * u64::from(side);
        let by_slots = u32::from(POOL_MAX_SLOTS).div_ceil(per_layer);
        let by_texels = u32::try_from(POOL_MAX_TEXELS / layer)
            .unwrap_or(u32::MAX)
            .max(1);
        let max_layers = by_slots
            .min(by_texels)
            .min(limits.max_texture_array_layers)
            .max(1);
        Some(Self {
            cells,
            side,
            pair,
            cols,
            rows,
            max_layers,
        })
    }

    /// One layer's level-0 texels: what the firm slot is charged.
    pub(crate) fn layer_texels(&self) -> u64 {
        u64::from(self.side) * u64::from(self.side)
    }

    /// The most layers a pool of this grid has.
    pub(crate) const fn max_layers(&self) -> u32 {
        self.max_layers
    }

    /// The slots `layers` layers hold, at most [`POOL_MAX_SLOTS`].
    fn slots(&self, layers: u32) -> u16 {
        let n = self.cols.saturating_mul(self.rows).saturating_mul(layers);
        u16::try_from(n).map_or(POOL_MAX_SLOTS, |n| n.min(POOL_MAX_SLOTS))
    }

    /// Slot `slot`'s layer and the origin of its stage cell; the banner
    /// cell lies below it.
    fn origin(&self, slot: u16) -> Option<(u32, [u32; 2])> {
        let per_layer = self.cols.checked_mul(self.rows).filter(|&n| n > 0)?;
        let slot = u32::from(slot);
        let (layer, i) = (slot / per_layer, slot % per_layer);
        let [pw, ph] = self.pair;
        Some((layer, [(i % self.cols) * pw, (i / self.cols) * ph]))
    }
}

/// What a slot holds once its write was applied: each image's uv, `None`
/// for one absent.
#[derive(Clone, Copy, Debug)]
struct Record {
    stage: Option<ChartUv>,
    banner: Option<ChartUv>,
}

/// Unit tests read the pool back.
const READ_BACK: wgpu::TextureUsages = if cfg!(test) {
    wgpu::TextureUsages::COPY_SRC
} else {
    wgpu::TextureUsages::empty()
};

/// One pool array of a generation and the records of its slots.
pub(crate) struct ChartPool {
    grid: Grid,
    layers: u32,
    generation: u64,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    records: [Option<Record>; POOL_MAX_SLOTS as usize],
}

impl ChartPool {
    /// A pool of `layers` layers (between 1 and the grid's most) of
    /// `grid`, every slot empty, with a new generation. One zero texel is
    /// written to each level of each layer, so the device zeroes the
    /// levels now and not at the first write. The writes are queued, not
    /// submitted; what the device refuses is reported to the caller's
    /// error scopes ([`skin_atlas::scoped`]).
    pub(crate) fn create(gpu: &Gpu, grid: Grid, layers: u32) -> Self {
        let layers = layers.clamp(1, grid.max_layers);
        let levels = u32::try_from(MIP_LEVELS).unwrap_or(1);
        let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("chart pool"),
            size: wgpu::Extent3d {
                width: grid.side,
                height: grid.side,
                depth_or_array_layers: layers,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | READ_BACK,
            view_formats: &[],
        });
        for layer in 0..layers {
            for level in 0..levels {
                skin_atlas::write_region(
                    gpu.queue(),
                    &texture,
                    (layer, level),
                    [0, 0],
                    [1, 1],
                    &[0; 4],
                );
            }
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("chart pool"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..wgpu::TextureViewDescriptor::default()
        });
        Self {
            grid,
            layers,
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
            texture,
            view,
            records: [None; POOL_MAX_SLOTS as usize],
        }
    }

    /// Its shape.
    pub(crate) fn layout(&self) -> PoolLayout {
        PoolLayout {
            generation: self.generation,
            slots: self.grid.slots(self.layers),
            cells: self.grid.cells,
            side: self.grid.side,
            layers: self.layers,
        }
    }

    /// Its generation.
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Its layers.
    pub(crate) const fn layers(&self) -> u32 {
        self.layers
    }

    /// Its grid.
    pub(crate) const fn grid(&self) -> Grid {
        self.grid
    }

    /// The array's view, for the selection's bind group.
    pub(crate) const fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The array, for reading back.
    #[cfg(test)]
    pub(crate) const fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// The firm layer's level-0 texels, charged with the selection
    /// installation.
    pub(crate) fn firm_texels(&self) -> u64 {
        self.grid.layer_texels()
    }

    /// The optional layers' level-0 texels: the reclaimable term.
    pub(crate) fn optional_texels(&self) -> u64 {
        u64::from(self.layers.saturating_sub(1)).saturating_mul(self.grid.layer_texels())
    }

    /// Slot `slot`'s table, keyed `key`, when its write was applied:
    /// the stage file and banner it holds, no back image. Allocates
    /// nothing.
    pub(crate) fn table(&self, slot: u16, key: ChartKey) -> Option<ChartTable> {
        let r = self.records.get(usize::from(slot))?.as_ref()?;
        Some(ChartTable::new(r.stage, None, r.banner).keyed(key))
    }

    /// Writes `stage` and `banner` (each fitted into its cell, `None` for
    /// one absent) into slot `slot`, every level at its cell's origin
    /// `>> k`, inside error scopes; queued, not submitted, and waiting for
    /// no GPU work. The slot's record is cleared first and set only by a
    /// whole write. `fault` runs inside the scopes after the writes (unit
    /// tests make the device refuse something there).
    pub(crate) fn write(
        &mut self,
        gpu: &Gpu,
        slot: u16,
        stage: Option<&Fitted>,
        banner: Option<&Fitted>,
        fault: Option<fn(&Gpu)>,
    ) -> Result<(), PoolWriteError> {
        let in_pool = slot < self.grid.slots(self.layers);
        let (Some((layer, [x, y])), Some(record)) = (
            self.grid.origin(slot).filter(|_| in_pool),
            self.records.get_mut(usize::from(slot)),
        ) else {
            return Err(PoolWriteError::Stale);
        };
        *record = None;
        let stage_h = self.grid.cells.stage.map_or(0, |c| c.size()[1]);
        let side = self.grid.side;
        let tag = u16::try_from(layer)
            .ok()
            .filter(|l| l & POOL_LAYER == 0)
            .ok_or(PoolWriteError::Refused)?
            | POOL_LAYER;
        let place = |fit: Option<&Fitted>, cell: Option<Slot>, origin: [u32; 2]| match fit {
            None => Ok(None),
            Some(f) => cell
                .and_then(|c| ChartUv::at(f, origin, c, side, tag))
                .filter(|_| f.levels().len() == MIP_LEVELS)
                .map(Some)
                .ok_or(PoolWriteError::Refused),
        };
        let stage_at = [x, y];
        let banner_at = [x, y.saturating_add(stage_h)];
        let stage_uv = place(stage, self.grid.cells.stage, stage_at)?;
        let banner_uv = place(banner, self.grid.cells.banner, banner_at)?;
        let ((), refused) = skin_atlas::scoped(gpu.device(), || {
            for (fit, origin) in [(stage, stage_at), (banner, banner_at)] {
                let Some(f) = fit else {
                    continue;
                };
                for ((k, bytes), level) in (0_u32..).zip(f.levels()).zip(0..) {
                    let Some(size) = f.level_size(level) else {
                        continue;
                    };
                    skin_atlas::write_region(
                        gpu.queue(),
                        &self.texture,
                        (layer, k),
                        origin.map(|o| o >> k),
                        size,
                        bytes,
                    );
                }
            }
            if let Some(fault) = fault {
                fault(gpu);
            }
        });
        if refused.is_some() {
            return Err(PoolWriteError::Refused);
        }
        *record = Some(Record {
            stage: stage_uv,
            banner: banner_uv,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use skin_model::chart_image::{ChartCells, Slot};

    use super::{Grid, POOL_MAX_SLOTS};

    fn cells(stage: Option<[u32; 2]>, banner: Option<[u32; 2]>) -> ChartCells {
        ChartCells {
            stage: stage.map(|s| Slot::new(s).unwrap()),
            banner: banner.map(|b| Slot::new(b).unwrap()),
        }
    }

    /// A pool's grid: cell pairs in rows across a layer, the lesser of 48
    /// slots and 16 Mi texels in whole layers, and one layer kept when a
    /// layer alone passes 16 Mi; no grid without a cell.
    #[test]
    fn a_grid_holds_cell_pairs_within_its_caps() {
        let limits = wgpu::Limits::default();
        // 448×(640+256) on 2 048: 4 across, 2 down, 4 layers of 16 Mi.
        let g = Grid::of(cells(Some([448, 640]), Some([448, 256])), 2_048, &limits).unwrap();
        assert_eq!((g.cols, g.rows, g.max_layers()), (4, 2, 4));
        assert_eq!(g.slots(4), 32);
        assert_eq!(g.origin(0), Some((0, [0, 0])));
        assert_eq!(g.origin(5), Some((0, [448, 896])));
        assert_eq!(g.origin(9), Some((1, [448, 0])));
        // Small cells: 48 slots reach the cap before the texels do.
        let g = Grid::of(cells(None, Some([256, 64])), 2_048, &limits).unwrap();
        assert_eq!((g.max_layers(), g.slots(1)), (1, POOL_MAX_SLOTS));
        // A layer past 16 Mi keeps one layer.
        let g = Grid::of(cells(Some([1_024, 1_024]), None), 8_192, &limits).unwrap();
        assert_eq!(g.max_layers(), 1);
        assert_eq!(Grid::of(ChartCells::NONE, 2_048, &limits), None);
        assert_eq!(
            Grid::of(cells(Some([448, 640]), None), 16_384, &limits),
            None
        );
    }
}
