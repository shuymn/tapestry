//! The BGA array (design.md "Rendering and Skins", BGA): one `Rgba8Unorm`
//! texture array of [`PAGE_SIDE`] pages, no mips, the chart's pictures
//! written into it cell by cell at preparation, bound as the sprite
//! pipelines' second group. Without an array, a one-texel stand-in is
//! bound, which no table describes.
//!
//! **A batch's upload** ([`BgaUpload`]): the render thread takes a batch of
//! cells in hand and writes it at its frame boundaries, outside every
//! frame, one step at a time: at most [`BGA_WRITE_BUDGET`] bytes (rows
//! padded as the queue pads them, a cell split between steps by its
//! rows), written and submitted inside one pair of error scopes, each
//! cell's bytes dropped once written, and a flag-only completion
//! registered for the step ([`completion`]). No step is written while the
//! last one's completion is unseen, whichever batch it belonged to, so at
//! most one step's staging is in flight. Within a boundary the steps go on
//! while each completion is seen at once, until the boundary's steps have
//! taken [`UPLOAD_TIME_SLICE`]; the batch is done at the round that sees its
//! last step's completion. Nothing waits for the GPU (design.md "Memory
//! Budget").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use skin_model::bga::{BgaKey, CellPlace, PAGE_BUDGET, PAGE_SIDE};

use crate::gpu::Gpu;

/// The most bytes a step of the BGA array's upload writes, which the queue
/// stages until the GPU completes it, rows padded as the queue pads them
/// (design.md "Memory Budget"; a provisional limit: a 1 024² cell, so the
/// six pages' 384 MiB go over in about 96 steps).
pub const BGA_WRITE_BUDGET: u64 = 4 << 20;

/// The render thread's time for upload steps at one frame boundary,
/// outside every frame: a step after the boundary's first begins only
/// while the boundary's steps so far took less (design.md "Memory
/// Budget"; a provisional limit, a quarter of a 60 Hz frame). It is the
/// frame boundary's upload time slice, for any upload made at frame
/// boundaries; the BGA array's is the first.
pub const UPLOAD_TIME_SLICE: std::time::Duration = std::time::Duration::from_millis(4);

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
}

/// A flag set once the GPU completed every submission made so far,
/// registered without submitting anything.
pub(crate) fn completion(gpu: &Gpu) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    gpu.queue()
        .on_submitted_work_done(move || flag.store(true, Ordering::Release));
    done
}

impl BgaUpload {
    /// Whether every cell is written.
    pub(crate) fn written(&self) -> bool {
        self.cell >= self.cells.len()
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
    /// row is queued.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn write_some(&self, gpu: &Gpu, up: &mut BgaUpload, budget: u64) {
        let Some(t) = &self.texture else {
            return;
        };
        let mut bytes = 0_u64;
        while let Some((place, texels)) = up.cells.get_mut(up.cell) {
            let [w, h] = place.padded();
            let [x, y] = place.origin;
            let row = crate::text_upload::padded(w);
            let room = budget.saturating_sub(bytes) / row.max(1);
            if room == 0 && bytes > 0 {
                return;
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
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use skin_model::bga::{BgaKey, CellPlace};

    use crate::gpu::Gpu;
    use crate::{AdapterChoice, BgaFault, BgaStep, Renderer, Target, UPLOAD_TIME_SLICE};

    /// One budget's cell: a 1 022² picture, padded to 1 024 rows of 4 KiB.
    const QUARTER: usize = 1_024 * 1_024 * 4;

    fn renderer() -> (Renderer, BgaKey) {
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 64,
                height: 64,
            },
            AdapterChoice::Warp,
        )
        .unwrap();
        let key = r.begin_bga(1).unwrap();
        // Each step takes the whole slice on the boundary's clock: one
        // step a boundary, unless a test says otherwise.
        r.bga_step_cost.set(Some(UPLOAD_TIME_SLICE));
        (r, key)
    }

    /// Four cells of one budget each, at a page's four quarters.
    fn cells() -> Vec<(CellPlace, Vec<u8>)> {
        quarters(4)
    }

    /// `n` (at most 16) cells of one budget each, along page 0's rows.
    fn quarters(n: u32) -> Vec<(CellPlace, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let place = CellPlace {
                    page: 0,
                    origin: [i % 4 * 1_024, i / 4 * 1_024],
                    content: [1_022, 1_022],
                };
                (place, vec![255; QUARTER])
            })
            .collect()
    }

    /// Steps `r`'s boundaries until `n` steps were submitted, within 30 s.
    fn until_submitted(r: &mut Renderer, n: usize) {
        let end = Instant::now() + Duration::from_secs(30);
        while r.bga_submits.get() < n {
            assert_eq!(r.bga_boundary(), BgaStep::Pending);
            assert!(Instant::now() < end, "step {n} never went");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Steps `r`'s boundaries until its batch ends, within 30 s.
    fn until_done(r: &mut Renderer) -> Result<(), BgaFault> {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            match r.bga_boundary() {
                BgaStep::Done(done) => return done,
                BgaStep::Pending => {}
                BgaStep::Idle => panic!("no batch in hand"),
            }
            assert!(Instant::now() < end, "the batch never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A validation error on `gpu`'s device: a buffer mappable for reading
    /// and writing at once.
    fn refuse(gpu: &Gpu) {
        let _ = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
            mapped_at_creation: false,
        });
    }

    /// Each step is one submission (design.md "Memory Budget"): four cells
    /// of one budget each go one a step, each dropped once written, and
    /// the batch is answered after the fourth step's completion with no
    /// submission of its own.
    #[test]
    fn each_step_is_submitted_once() {
        let (mut r, key) = renderer();
        r.stage_bga_cells(key, cells()).unwrap();
        for step in 1..=4 {
            until_submitted(&mut r, step);
            let left = u64::try_from((4 - step) * QUARTER).unwrap();
            assert_eq!(r.bga_upload_bytes(), left, "one cell a step");
        }
        assert_eq!(until_done(&mut r), Ok(()));
        assert_eq!(r.bga_submits.get(), 4, "nothing submitted beside the steps");
    }

    /// One step is in flight at a time: while its completion is withheld,
    /// boundaries write and submit nothing more, and the next step goes
    /// once it is seen.
    #[test]
    fn no_step_goes_while_the_last_is_in_flight() {
        let (mut r, key) = renderer();
        r.stage_bga_cells(key, cells()).unwrap();
        until_submitted(&mut r, 1);
        r.bga_seen.set(Some(false));
        for _ in 0..5 {
            assert_eq!(r.bga_boundary(), BgaStep::Pending);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(r.bga_submits.get(), 1, "nothing written while in flight");
        assert_eq!(r.bga_upload_bytes(), u64::try_from(3 * QUARTER).unwrap());
        r.bga_seen.set(None);
        until_submitted(&mut r, 2);
        assert_eq!(r.bga_upload_bytes(), u64::try_from(2 * QUARTER).unwrap());
    }

    /// A step in flight holds back the next one whatever batch is in hand:
    /// a batch that replaces it, or one staged after it was cancelled,
    /// writes nothing until that step's completion is seen.
    #[test]
    fn a_step_in_flight_holds_back_the_batch_that_replaces_it() {
        let (mut r, key) = renderer();
        r.stage_bga_cells(key, cells()).unwrap();
        until_submitted(&mut r, 1);
        r.bga_seen.set(Some(false));
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Pending);
        r.cancel_bga_upload();
        r.stage_bga_cells(key, cells()).unwrap();
        for _ in 0..3 {
            assert_eq!(r.bga_boundary(), BgaStep::Pending);
        }
        assert_eq!(r.bga_submits.get(), 1, "the new batch waited");
        r.bga_seen.set(None);
        until_submitted(&mut r, 2);
    }

    /// A refusal at a step's submission is caught in the step's own scopes
    /// (not the device's handler): the batch ends refused, the step's
    /// writes were submitted with it and stay in flight, and a batch staged
    /// afterwards is written whole.
    #[test]
    fn a_refused_submission_refuses_the_batch() {
        let (mut r, key) = renderer();
        r.bga_submit_fault = Some(refuse);
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Done(Err(BgaFault::Refused)));
        assert_eq!(r.bga_submits.get(), 1, "its writes submitted");
        assert!(r.bga_in_flight.is_some(), "and in flight");
        r.bga_submit_fault = None;
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(until_done(&mut r), Ok(()));
    }

    /// A refusal at a step's writes refuses the batch the same way.
    #[test]
    fn a_refused_write_refuses_the_batch() {
        let (mut r, key) = renderer();
        r.bga_write_fault = Some(refuse);
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Done(Err(BgaFault::Refused)));
        r.bga_write_fault = None;
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(until_done(&mut r), Ok(()));
    }

    /// Within a boundary the steps go on while each completion is seen at
    /// once, until the boundary's steps have taken the time slice
    /// (design.md "Memory Budget"): at a millisecond a step on the
    /// boundary's clock, a 4 ms slice takes four of eight steps, the next
    /// boundary the other four, and that boundary answers the batch once
    /// the last completion is seen, with no boundary of its own.
    #[test]
    fn steps_go_on_within_a_boundary_until_its_time_slice() {
        assert_eq!(UPLOAD_TIME_SLICE, Duration::from_millis(4));
        let (mut r, key) = renderer();
        r.bga_step_cost.set(Some(Duration::from_millis(1)));
        r.bga_seen.set(Some(true));
        r.stage_bga_cells(key, quarters(8)).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Pending);
        assert_eq!(r.bga_submits.get(), 4, "four steps in the slice");
        assert_eq!(r.bga_upload_bytes(), u64::try_from(4 * QUARTER).unwrap());
        assert_eq!(r.bga_boundary(), BgaStep::Done(Ok(())));
        assert_eq!(r.bga_submits.get(), 8);
    }

    /// Takes twice the time slice, inside a step's scopes before its
    /// submission.
    fn slow(_: &Gpu) {
        std::thread::sleep(UPLOAD_TIME_SLICE * 2);
    }

    /// On the real clock, a boundary's first step that takes longer than
    /// the slice ends the boundary's steps, every completion seen at once
    /// (design.md "Memory Budget"): the slice is measured from the
    /// boundary's start, the step's own time included.
    #[test]
    fn a_step_past_the_slice_ends_the_boundary_on_the_real_clock() {
        let (mut r, key) = renderer();
        r.bga_step_cost.set(None);
        r.bga_seen.set(Some(true));
        r.bga_submit_fault = Some(slow);
        r.stage_bga_cells(key, quarters(4)).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Pending);
        assert_eq!(r.bga_submits.get(), 1, "no second step past the slice");
        assert_eq!(r.bga_upload_bytes(), u64::try_from(3 * QUARTER).unwrap());
    }

    /// A step in flight when the device is lost: the batch ends stale at
    /// the next boundary with nothing more written, its key names nothing
    /// to publish or write to, and a renderer built again, as the render
    /// thread builds one after a loss, has a key of its own, refuses the
    /// old one and uploads a batch whole (design.md "Rendering and Skins",
    /// BGA).
    #[test]
    fn a_step_in_flight_through_a_device_loss_ends_stale() {
        let (mut r, key) = renderer();
        r.bga_seen.set(Some(false));
        r.stage_bga_cells(key, cells()).unwrap();
        assert_eq!(r.bga_boundary(), BgaStep::Pending);
        assert!(r.bga_in_flight.is_some(), "a step outstanding");
        r.gpu().device().destroy();
        // The device reports its loss at its next poll.
        let _ = r.gpu().device().poll(wgpu::PollType::wait_indefinitely());
        assert!(r.gpu().lost());
        assert_eq!(r.bga_boundary(), BgaStep::Done(Err(BgaFault::Stale)));
        assert_eq!(r.bga_submits.get(), 1, "nothing written after the loss");
        assert_eq!(r.bga_key(), None);
        assert!(matches!(
            r.publish_bga(Some(key), Vec::new()),
            Err(BgaFault::Stale)
        ));
        assert_eq!(r.stage_bga_cells(key, cells()), Err(BgaFault::Stale));
        drop(r);
        let (mut again, fresh) = renderer();
        assert_ne!(fresh, key);
        assert_eq!(
            again.stage_bga_cells(key, cells()),
            Err(BgaFault::Stale),
            "the old key names nothing on the new renderer"
        );
        again.stage_bga_cells(fresh, cells()).unwrap();
        assert_eq!(until_done(&mut again), Ok(()));
        let table = again.publish_bga(Some(fresh), Vec::new()).unwrap();
        assert_eq!(table.key(), Some(fresh));
    }
}
