//! Widgets: elements that draw many quads from player logic. Stage 3 S5
//! has the lanes; the scroll math they use is a copy of the simple skin's.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub(crate) mod lanes;
pub(crate) mod scroll;

use bms_model::TimeUs;

use crate::atlas::uv::{Cycle, FrameUv, UvTable};
use crate::play::Play;
use crate::play::timers::elapsed_us;
use crate::quad::Filter;

/// An image as a build reads it: its first frame in the uv table, its
/// frame count, cycle and filter, copied so a build never reads the sheet.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageRef {
    pub(crate) first: u32,
    pub(crate) frames: u16,
    pub(crate) cycle: Option<Cycle>,
    pub(crate) filter: Filter,
}

impl ImageRef {
    /// The frame the image shows at `now`, from its cycle's timer: frame 0
    /// while that timer is off, without a cycle, or with one frame.
    pub(crate) fn frame(&self, play: &Play, now: TimeUs, uvs: &UvTable) -> Option<FrameUv> {
        let shown = self.cycle.map_or(0, |c| {
            play.timer_at(c.slot)
                .map_or(0, |at| c.frame(elapsed_us(now, at), self.frames))
        });
        self.first.checked_add(shown).and_then(|i| uvs.get(i))
    }
}
