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
use crate::{FrameInput, Instance, PlaySkin, SpriteBatch};

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
        let batch = Self::sized(capacity, capacity);
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
    /// the overlay's reserve, emptying the batch: allocates, so before
    /// play.
    pub fn set_capacity(&mut self, capacity: u32, runs: u32) {
        self.batch = Self::sized(capacity, runs);
        self.instances = Vec::with_capacity(self.batch.total_capacity());
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

#[cfg(test)]
mod tests {
    use skin_model::{Canvas, ClipId, Fit};

    use super::{Scissor, Staging};
    use crate::projection::ClipTable;
    use crate::{Blend, Sprite};

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
