//! The skin boundary (section 2, "Skins"): what a skin is given each frame
//! and what it must do.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, TimeUs};
use eval::{JudgeEvent, Section};
use game::PlayView;
use skin_model::{BuildError, Canvas, Fit};

use crate::{Gpu, MAX_SPRITES, SpriteBatch};

/// What one frame draws from.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// Chart time when the frame's work starts, read from QPC.
    pub now: TimeUs,
    /// The display latency offset: how much later than `now` the frame is
    /// expected to reach the screen (design.md "Rendering and Skins").
    pub display_offset: TimeUs,
    /// Hi-speed as the visible time (design.md "Algorithms"): how long a note's
    /// center takes from the top of the lane to the judgment line while the
    /// chart scrolls at its initial BPM. The note's edge shows slightly
    /// earlier.
    pub visible_time: TimeUs,
    /// The newest play state.
    pub view: &'a PlayView,
    /// The effects received since the previous frame, in emission order.
    pub effects: &'a [JudgeEvent],
    /// Index into [`PlayView::gauges`] of the gauge to show.
    pub gauge: usize,
    /// Lines of the measurement overlay (design.md "Latency Figures"), drawn
    /// over the play after the skin's sprites, by the staging and not the
    /// skin ([`Staging::build`](crate::Staging::build)); empty when it is
    /// hidden.
    pub overlay: &'a [&'a str],
    /// The target's size in pixels as the renderer last applied it
    /// ([`Renderer::target`](crate::Renderer::target)), never zero: the
    /// size the frame is projected onto ([`PlaySkin::canvas`]).
    /// [`Renderer::build`](crate::Renderer::build) sets it so.
    pub target: [u32; 2],
}

impl FrameInput<'_> {
    /// The chart time the frame is expected to appear on screen: `now` plus
    /// the display latency offset. Note positions follow it.
    #[must_use]
    pub const fn display_time(&self) -> TimeUs {
        TimeUs(self.now.0.saturating_add(self.display_offset.0))
    }
}

/// A skin that cannot draw a chart.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct SkinError {
    /// Why, for the player.
    pub reason: String,
}

/// A play screen layout. The renderer calls `prepare` before the countdown
/// and `build` once per frame. `build` must not allocate: everything it
/// needs is sized in `prepare`.
pub trait PlaySkin: Send {
    /// Takes what the skin needs from `chart` and the GPU, and resets any
    /// per-play state, so a retry calls it again. With a practice round's
    /// `section`, only its targets are drawn ([`Section::is_target`]).
    ///
    /// # Errors
    ///
    /// Returns why the skin cannot draw this chart.
    fn prepare(
        &mut self,
        gpu: &Gpu,
        chart: &Chart,
        section: Option<&Section>,
    ) -> Result<(), SkinError>;

    /// Fills `out`, which arrives empty, with the frame's sprites, or
    /// refuses the frame with the runtime model's error. A refused frame
    /// is drawn by nothing: the renderer drops whatever was pushed, submits
    /// nothing, and calls `build` no more until the next
    /// [`prepare`](Self::prepare), so the attempt stops. Allocation-free,
    /// the error included.
    ///
    /// # Errors
    ///
    /// The model's [`BuildError`] for a frame it cannot build.
    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch) -> Result<(), BuildError>;

    /// The most sprites a frame of the skin pushes: the renderer's batch,
    /// staging and instance buffer hold this many, and never fewer than
    /// [`MAX_SPRITES`], which the built-in screens use, and the
    /// measurement overlay's reserve after them
    /// ([`OVERLAY_SPRITES`](crate::OVERLAY_SPRITES)). The default is
    /// [`MAX_SPRITES`].
    fn capacity(&self) -> u32 {
        MAX_SPRITES as u32
    }

    /// The most runs a frame of the skin's sprites take, at least one per
    /// sprite when it may change blend or clip at every sprite; the
    /// overlay's run is reserved apart. The default is
    /// [`capacity`](Self::capacity).
    fn run_capacity(&self) -> u32 {
        self.capacity()
    }

    /// The canvas the skin draws on for a `target`-pixel target, which the
    /// renderer projects its frames and clips onto. The default is the
    /// fixed 1920×1080 layout, letterboxed.
    fn canvas(&self, target: [u32; 2]) -> Canvas {
        Canvas::new(target, Fit::Fixed)
    }

    /// Fills `out`, which arrives empty, with the play screen's frame while
    /// a chart loads before the countdown (design.md "Play Lifecycle"):
    /// the lanes and the rest of the screen without notes, and nothing of
    /// any chart: the skin may still hold the previous chart's
    /// preparation, whose values the loading frame must not show. The
    /// renderer draws the load's text, the chosen chart's, over it. The
    /// default draws nothing.
    fn build_loading(&mut self, out: &mut SpriteBatch) {
        let _ = out;
    }
}
