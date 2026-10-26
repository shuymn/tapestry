//! The skin boundary (section 2, "Skins"): what a skin is given each frame
//! and what it must do.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, TimeUs};
use eval::JudgeEvent;
use game::PlayView;

use crate::{Gpu, SpriteBatch};

/// What one frame draws from.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// Chart time when the frame's work starts, read from QPC.
    pub now: TimeUs,
    /// The display latency offset: how much later than `now` the frame is
    /// expected to reach the screen (design.md "Rendering and Skins").
    pub display_offset: TimeUs,
    /// Hi-speed as the visible time (D9): how long a note's center takes
    /// from the top of the lane to the judgment line while the chart scrolls
    /// at its initial BPM. The note's edge shows slightly earlier.
    pub visible_time: TimeUs,
    /// The newest play state.
    pub view: &'a PlayView,
    /// The effects received since the previous frame, in emission order.
    pub effects: &'a [JudgeEvent],
    /// Index into [`PlayView::gauges`] of the gauge to show.
    pub gauge: usize,
    /// Lines of the measurement overlay (D11), drawn over the play; empty
    /// when it is hidden.
    pub overlay: &'a [&'a str],
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkinError {
    /// Why, for the player.
    pub reason: String,
}

impl std::fmt::Display for SkinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for SkinError {}

/// A play screen layout. The renderer calls `prepare` before the countdown
/// and `build` once per frame. `build` must not allocate: everything it
/// needs is sized in `prepare`.
pub trait PlaySkin: Send {
    /// Takes what the skin needs from `chart` and the GPU, and resets any
    /// per-play state, so a retry calls it again.
    ///
    /// # Errors
    ///
    /// Returns why the skin cannot draw this chart.
    fn prepare(&mut self, gpu: &Gpu, chart: &Chart) -> Result<(), SkinError>;

    /// Fills `out`, which arrives empty, with the frame's sprites.
    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch);
}
