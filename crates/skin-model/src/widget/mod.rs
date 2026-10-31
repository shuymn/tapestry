//! Widgets: elements that draw many quads from player logic. Stage 3 S5
//! has the lanes, whose scroll math is a copy of the simple skin's; S5b
//! adds the glyph strips of `number` and the judge's combo, the `judge`
//! and the `gauge`.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub(crate) mod digits;
pub(crate) mod gauge;
pub(crate) mod judge;
pub(crate) mod lanes;
pub(crate) mod scroll;

use bms_model::TimeUs;

use crate::atlas::uv::{Cycle, FrameUv, UvTable};
use crate::place::{Affine, LaneMap};
use crate::play::Play;
use crate::play::timers::elapsed_us;
use crate::quad::{Filter, Quad, QuadBatch, RunKey};

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
        let shown = self.group(play, now, self.frames);
        self.first.checked_add(shown).and_then(|i| uvs.get(i))
    }

    /// Which of `groups` equal runs of the image's frames its cycle shows
    /// at `now`: run 0 while the cycle's timer is off, without a cycle, or
    /// with one run. With `groups` the frame count, a run is a frame.
    pub(crate) fn group(&self, play: &Play, now: TimeUs, groups: u16) -> u32 {
        self.cycle.map_or(0, |c| {
            play.timer_at(c.slot)
                .map_or(0, |at| c.frame(elapsed_us(now, at), groups))
        })
    }
}

/// What every widget reads of this frame: the play state as of the last
/// build, the frame's clock, which image cycles age on, and the uv table.
#[derive(Clone, Copy)]
pub(crate) struct Frame<'a> {
    pub(crate) play: &'a Play,
    pub(crate) now: TimeUs,
    pub(crate) uvs: &'a UvTable,
}

/// What the quads of one widget part share: the run key, the transform
/// of the element's region or fill, the lane map of its binding, the
/// pivot's fractions of each quad's rect, the angle and the color.
#[derive(Clone, Copy)]
pub(crate) struct Brush {
    pub(crate) key: RunKey,
    pub(crate) affine: Affine,
    pub(crate) map: LaneMap,
    pub(crate) pivot: [f32; 2],
    /// Radians in `[0, 2π)`.
    pub(crate) angle: f32,
    pub(crate) color: [f32; 4],
}

impl Brush {
    /// Pushes the quad showing `f` over the reference rect `[x, y, w, h]`:
    /// moved with its lane (when bound), then by the affine, turned about
    /// its own pivot moved alike. `false` when the batch refused it.
    pub(crate) fn push(
        &self,
        out: &mut QuadBatch,
        [x, y, w, h]: [f32; 4],
        f: FrameUv,
        filter: Filter,
    ) -> bool {
        let [fx, fy] = self.pivot;
        let pivot = [x + fx * w, y + fy * h];
        let (x, w, px) = (self.map.x(x), self.map.w(w), self.map.x(pivot[0]));
        out.push(
            self.key,
            Quad {
                rect: self.affine.rect([x, y, w, h]),
                uv: f.uv,
                color: self.color,
                pivot: self.affine.point([px, pivot[1]]),
                angle: self.angle,
                layer: f.layer,
                filter,
            },
        )
    }
}

/// Two colors multiplied component by component.
pub(crate) fn tint(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [a0, a1, a2, a3] = a;
    let [b0, b1, b2, b3] = b;
    [a0 * b0, a1 * b1, a2 * b2, a3 * b3]
}
