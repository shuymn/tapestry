//! The app's share of a frame, everything before wgpu is called: the
//! batch a frame fills, its staging into [`Instance`]s, and the play
//! skin's projection with its clips' scissors, all sized before play so a
//! frame allocates nothing (design.md "Scope of No Allocation"). A play
//! frame's batch holds the skin's sprites, then the measurement overlay's
//! from a reserve the skin cannot fill.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use skin_model::{BuildError, Canvas, ClipId};

use crate::overlay::{self, OVERLAY_RUNS, OVERLAY_SPRITES};
use crate::projection::{ClipTable, Projection};
use crate::{Blend, FrameInput, Instance, LoadingFrame, PlaySkin, Run, Sprite, SpriteBatch};

/// A frame's batch and staging, sized for `capacity` sprites of the skin
/// and the overlay's reserve after them, and the play skin's projection
/// and clip scissors for the applied target.
#[derive(Debug)]
pub struct Staging {
    batch: SpriteBatch,
    instances: Vec<Instance>,
    /// Layer 0's uv scale: the built-in atlas's side over the page side.
    font_scale: f32,
    projection: Projection,
    clips: ClipTable,
    /// Each clip's scissor under `projection`; one entry per clip.
    scissors: Vec<Option<[u32; 4]>>,
}

/// Where a run draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scissor {
    /// The whole target: an unclipped run.
    Full,
    /// These target pixels, `[x, y, width, height]`.
    Rect([u32; 4]),
    /// Nowhere: its clip keeps no pixel, or is not in the table.
    Empty,
}

impl Staging {
    /// Room for `capacity` sprites of the skin in as many runs, and the
    /// overlay's reserve, projected onto `canvas`'s target, with no clips
    /// and the built-in atlas alone.
    #[must_use]
    pub fn new(capacity: u32, canvas: &Canvas) -> Self {
        Self::with_runs(capacity, capacity, canvas)
    }

    /// As [`new`](Self::new), with room for `runs` runs.
    #[must_use]
    pub fn with_runs(capacity: u32, runs: u32, canvas: &Canvas) -> Self {
        let batch = Self::sized(capacity, runs);
        Self {
            instances: Vec::with_capacity(batch.total_capacity()),
            batch,
            font_scale: 1.0,
            projection: Projection::new(canvas),
            clips: ClipTable::default(),
            scissors: Vec::new(),
        }
    }

    /// A batch of `sprites` sprites in `runs` runs and the overlay's
    /// reserve.
    fn sized(sprites: u32, runs: u32) -> SpriteBatch {
        SpriteBatch::with_reserve(sprites, runs, OVERLAY_SPRITES, OVERLAY_RUNS)
    }

    /// The most sprites a frame of the skin stages, the overlay's reserve
    /// left out.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.batch.capacity()
    }

    /// The most runs a frame of the skin takes, the overlay's left out.
    #[must_use]
    pub fn run_capacity(&self) -> usize {
        self.batch.run_capacity()
    }

    /// The most instances a frame stages, the overlay's reserve included:
    /// what the instance buffer holds.
    #[must_use]
    pub fn instance_capacity(&self) -> usize {
        self.batch.total_capacity()
    }

    /// Makes room for `capacity` sprites of the skin in `runs` runs, and
    /// the overlay's reserve, emptying the batch and the instances, whose
    /// storage stays while their capacity does: allocates, so before play.
    pub fn set_capacity(&mut self, capacity: u32, runs: u32) {
        let batch = Self::sized(capacity, runs);
        if batch.total_capacity() == self.batch.total_capacity() {
            self.instances.clear();
        } else {
            self.instances = Vec::with_capacity(batch.total_capacity());
        }
        self.batch = batch;
    }

    /// The fixed clips taken.
    #[must_use]
    pub const fn clips(&self) -> &ClipTable {
        &self.clips
    }

    /// The bytes it holds: the batch's sprites and runs at their whole
    /// capacity, the reserve included, and the instances staged for it
    /// twice, on the CPU and in the GPU's instance buffer of the same
    /// size; the clips and their scissors.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        let sprites = self.batch.total_capacity() * size_of::<crate::Sprite>();
        let runs = self.batch.total_run_capacity() * size_of::<crate::Run>();
        let instances = 2 * self.instances.capacity() * size_of::<Instance>();
        let clips = self.clips.len() * (size_of::<[f32; 4]>() + size_of::<Option<[u32; 4]>>());
        u64::try_from(sprites + runs + instances + clips).unwrap_or(u64::MAX)
    }

    /// Takes `clips` and projects them: allocates, so before play.
    pub fn set_clips(&mut self, clips: ClipTable) {
        self.scissors = vec![None; clips.len()];
        self.clips = clips;
        self.clips.project(&self.projection, &mut self.scissors);
    }

    /// Sets layer 0's uv scale ([`Instance::of`]).
    pub fn set_font_scale(&mut self, font_scale: f32) {
        self.font_scale = font_scale;
    }

    /// Projects the play skin's frames onto `canvas`'s target, and every
    /// clip's scissor with them, once; allocates nothing, so a resize can
    /// be applied between any two frames.
    pub fn project(&mut self, canvas: &Canvas) {
        self.projection = Projection::new(canvas);
        self.clips.project(&self.projection, &mut self.scissors);
    }

    /// The play skin's projection.
    #[must_use]
    pub fn projection(&self) -> Projection {
        self.projection
    }

    /// Where a run with `clip` draws.
    #[must_use]
    pub fn scissor(&self, clip: Option<ClipId>) -> Scissor {
        let Some(id) = clip else {
            return Scissor::Full;
        };
        match self.scissors.get(usize::from(id.0)) {
            Some(Some(rect)) => Scissor::Rect(*rect),
            _ => Scissor::Empty,
        }
    }

    /// Where `run` draws: under its fixed clip's scissor
    /// ([`scissor`](Self::scissor)) and, when it carries an inline clip
    /// rect, under that rect too, projected as a fixed clip is (each edge
    /// placed as the viewport places it, rounded half up and clamped to
    /// the target, D1) and intersected with it. Rounding each edge on its
    /// own is monotonic, so intersecting the projected rectangles is the
    /// projection of their canvas intersection. An empty intersection
    /// draws nothing. Allocates nothing: the renderer resolves every run
    /// this way as it draws.
    #[must_use]
    pub fn run_scissor(&self, run: &Run) -> Scissor {
        let fixed = self.scissor(run.key.clip);
        let Some(rect) = run.clip_rect else {
            return fixed;
        };
        let Some(inline) = self.projection.scissor(rect.rect()) else {
            return Scissor::Empty;
        };
        match fixed {
            Scissor::Full => Scissor::Rect(inline),
            Scissor::Rect(fixed) => intersect(fixed, inline).map_or(Scissor::Empty, Scissor::Rect),
            Scissor::Empty => Scissor::Empty,
        }
    }

    /// The skin builds `f` into the emptied batch, the overlay's lines
    /// follow from the reserve, and the batch is staged in drawing order:
    /// a play frame's app work, which calls nothing in wgpu and allocates
    /// nothing.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame: the batch and the staging are
    /// left empty, so nothing of the frame, the sprites the skin pushed
    /// before refusing included, can be drawn.
    pub fn build(&mut self, skin: &mut dyn PlaySkin, f: &FrameInput<'_>) -> Result<(), BuildError> {
        self.batch.clear();
        self.instances.clear();
        if let Err(e) = skin.build(f, &mut self.batch) {
            self.batch.clear();
            return Err(e);
        }
        overlay::draw(f.overlay, &mut self.batch);
        self.batch.stage(&mut self.instances, self.font_scale);
        Ok(())
    }

    /// The skin builds the loading frame `f` into the emptied batch, which
    /// is then staged: a loading frame's app work, which calls nothing in
    /// wgpu and allocates nothing.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame: the batch and the staging are
    /// left empty, as for a play frame's ([`build`](Self::build)).
    pub fn build_loading(
        &mut self,
        skin: &mut dyn PlaySkin,
        f: &LoadingFrame<'_>,
    ) -> Result<(), BuildError> {
        self.batch.clear();
        self.instances.clear();
        if let Err(e) = skin.build_loading(f, &mut self.batch) {
            self.batch.clear();
            return Err(e);
        }
        self.batch.stage(&mut self.instances, self.font_scale);
        Ok(())
    }

    /// `build` fills the emptied batch's quads (a screen skin's frame other
    /// than play's), `reserve`'s sprites follow from the reserve the skin
    /// cannot fill (the selection screen's strip), and the batch is staged:
    /// a frame's app work, which calls nothing in wgpu and allocates
    /// nothing.
    ///
    /// # Errors
    ///
    /// `build`'s refusal: the batch and the staging are left empty, as for
    /// a play frame's ([`build`](Self::build)).
    pub fn build_with(
        &mut self,
        build: impl FnOnce(&mut skin_model::QuadBatch) -> Result<(), BuildError>,
        reserve: &[Sprite],
    ) -> Result<(), BuildError> {
        self.batch.clear();
        self.instances.clear();
        if let Err(e) = build(self.batch.quads_mut()) {
            self.batch.clear();
            return Err(e);
        }
        self.batch.open_reserve();
        for s in reserve {
            self.batch.push(Blend::Alpha, *s);
        }
        self.batch.stage(&mut self.instances, self.font_scale);
        Ok(())
    }

    /// `push` fills the emptied batch, which is then staged: the built-in
    /// screens' frames.
    pub fn fill(&mut self, push: impl FnOnce(&mut SpriteBatch)) {
        self.batch.clear();
        push(&mut self.batch);
        self.batch.stage(&mut self.instances, self.font_scale);
    }

    /// The staged instances, in drawing order.
    #[must_use]
    pub fn instances(&self) -> &[Instance] {
        &self.instances
    }

    /// The batch last built or filled.
    #[must_use]
    pub fn batch(&self) -> &SpriteBatch {
        &self.batch
    }
}

/// The pixels both target rectangles `[x, y, width, height]` cover;
/// `None` when they share none.
fn intersect(a: [u32; 4], b: [u32; 4]) -> Option<[u32; 4]> {
    let [ax, ay, aw, ah] = a;
    let [bx, by, bw, bh] = b;
    let (x0, y0) = (ax.max(bx), ay.max(by));
    let x1 = ax.saturating_add(aw).min(bx.saturating_add(bw));
    let y1 = ay.saturating_add(ah).min(by.saturating_add(bh));
    (x0 < x1 && y0 < y1).then(|| [x0, y0, x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use skin_model::{Canvas, ClipId, ClipRect, Fit, RunKey};

    use super::{Scissor, Staging};
    use crate::projection::ClipTable;
    use crate::{Blend, Run, Sprite};

    /// A clip's scissor follows the projection: the same canvas clip on
    /// 1920×1080 and, letterboxed at half scale, on 960×540; an unclipped
    /// run draws on the whole target, and a clip with no pixel left or
    /// past the table nowhere.
    #[test]
    fn scissors_follow_the_projection() {
        let mut s = Staging::new(8, &Canvas::new([1920, 1080], Fit::Fixed));
        s.set_clips(
            ClipTable::new(vec![[100.0, 200.0, 300.0, 400.0], [10.0, 10.0, 0.4, 5.0]]).unwrap(),
        );
        assert_eq!(s.scissor(None), Scissor::Full);
        assert_eq!(
            s.scissor(Some(ClipId(0))),
            Scissor::Rect([100, 200, 300, 400])
        );
        assert_eq!(s.scissor(Some(ClipId(1))), Scissor::Empty);
        assert_eq!(s.scissor(Some(ClipId(2))), Scissor::Empty);
        s.project(&Canvas::new([960, 540], Fit::Fixed));
        assert_eq!(
            s.scissor(Some(ClipId(0))),
            Scissor::Rect([50, 100, 150, 200])
        );
        assert_eq!(s.projection().target(), [960, 540]);
    }

    /// A run's inline clip rect is projected as a fixed clip is, rounded
    /// half up edge by edge (D1) and clamped to the target, then
    /// intersected with its fixed clip's scissor: alone it is its own
    /// scissor; with a fixed clip only the overlap draws; an overlap of no
    /// pixel, a rect rounding to none and a fixed clip of none draw
    /// nothing; a run without either draws on the whole target.
    #[test]
    fn inline_clips_intersect_the_fixed_clip() {
        let run = |clip: Option<u16>, rect: Option<[f32; 4]>| Run {
            key: RunKey {
                blend: Blend::Alpha,
                clip: clip.map(ClipId),
            },
            clip_rect: rect.map(|r| ClipRect::new(r).unwrap()),
            start: 0,
            end: 1,
        };
        let mut s = Staging::new(8, &Canvas::new([1920, 1080], Fit::Fixed));
        s.set_clips(
            ClipTable::new(vec![[100.0, 200.0, 300.0, 400.0], [10.0, 10.0, 0.4, 5.0]]).unwrap(),
        );
        assert_eq!(s.run_scissor(&run(None, None)), Scissor::Full);
        assert_eq!(
            s.run_scissor(&run(None, Some([50.0, 60.0, 70.0, 80.0]))),
            Scissor::Rect([50, 60, 70, 80])
        );
        assert_eq!(
            s.run_scissor(&run(Some(0), Some([250.0, 150.0, 1_000.0, 100.0]))),
            Scissor::Rect([250, 200, 150, 50])
        );
        // Wholly right of the fixed clip, or touching its right edge: no
        // pixel shared.
        assert_eq!(
            s.run_scissor(&run(Some(0), Some([1_000.0, 900.0, 10.0, 10.0]))),
            Scissor::Empty
        );
        assert_eq!(
            s.run_scissor(&run(Some(0), Some([400.0, 200.0, 10.0, 10.0]))),
            Scissor::Empty
        );
        assert_eq!(
            s.run_scissor(&run(Some(1), Some([0.0, 0.0, 1_920.0, 1_080.0]))),
            Scissor::Empty
        );
        // Past the target: clamped to it.
        assert_eq!(
            s.run_scissor(&run(None, Some([-10.0, 1_000.0, 3_000.0, 500.0]))),
            Scissor::Rect([0, 1_000, 1_920, 80])
        );
        // At half scale: x 101..501 is 50.5..250.5, rounded to 51..251, and
        // y 201..402 to 101..201, then cut by clip 0's x 50..200 and
        // y 100..300 to 51..200.
        s.project(&Canvas::new([960, 540], Fit::Fixed));
        assert_eq!(
            s.run_scissor(&run(Some(0), Some([101.0, 201.0, 400.0, 201.0]))),
            Scissor::Rect([51, 101, 149, 100])
        );
        assert_eq!(
            s.run_scissor(&run(None, Some([10.0, 10.0, 0.5, 10.0]))),
            Scissor::Empty
        );
    }

    #[test]
    fn filling_stages_with_the_font_scale() {
        let mut s = Staging::new(4, &Canvas::new([1920, 1080], Fit::Fixed));
        s.set_font_scale(0.25);
        s.fill(|b| {
            b.push(
                Blend::Alpha,
                Sprite {
                    uv: [0.5; 4],
                    ..Sprite::default()
                },
            );
        });
        assert_eq!(s.instances().len(), 1);
        assert_eq!(s.instances()[0].uv, [0.125; 4]);
        assert_eq!(s.batch().len(), 1);
        assert_eq!(s.capacity(), 4);
    }
}
