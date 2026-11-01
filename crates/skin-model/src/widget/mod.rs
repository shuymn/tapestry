//! Widgets: elements that draw many quads from player logic. Stage 3 S5
//! has the lanes, whose scroll math is a copy of the simple skin's; S5b
//! adds the glyph strips of `number` and the judge's combo, the `judge`
//! and the `gauge`; S5c adds `text`; S6c-04 adds expression-selected
//! images (`image_choice`); S6c-05 paints SUDDEN+ apart from the lanes
//! (`cover_paint`); S6c-06 draws the lanes' bar groups (`bar`).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub(crate) mod bar;
pub(crate) mod cover_paint;
pub(crate) mod digits;
pub(crate) mod gauge;
pub(crate) mod glyphs;
pub(crate) mod image_choice;
pub(crate) mod judge;
pub(crate) mod lanes;
pub(crate) mod scroll;
pub(crate) mod text;

use bms_model::TimeUs;

use self::digits::DigitOffset;
use crate::atlas::uv::{Cycle, FrameUv, UvTable};
use crate::canvas::f32_of;
use crate::expr::{Program, State};
use crate::place::{Affine, LaneMap, Native};
use crate::play::Play;
use crate::play::timers::elapsed_us;
use crate::quad::{ClipRect, Filter, Quad, QuadBatch, RunKey};
use crate::track::{Sample, Track};

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
/// pivot's fractions of each quad's rect, the angle, the color, and the
/// native size each quad keeps at the canvas's scale.
#[derive(Clone, Copy)]
pub(crate) struct Brush {
    pub(crate) key: RunKey,
    pub(crate) affine: Affine,
    pub(crate) map: LaneMap,
    pub(crate) pivot: [f32; 2],
    /// Radians in `[0, 2π)`.
    pub(crate) angle: f32,
    pub(crate) color: [f32; 4],
    pub(crate) native: Option<Native>,
    /// Target pixels per canvas pixel.
    pub(crate) scale: f32,
}

impl Brush {
    /// The quad showing `f` over the reference rect `[x, y, w, h]`: moved
    /// with its lane (when bound), then by the affine, resized to its
    /// native size (when it has one), turned about its own pivot in that
    /// rect.
    pub(crate) fn quad(&self, [x, y, w, h]: [f32; 4], f: FrameUv, filter: Filter) -> Quad {
        let [fx, fy] = self.pivot;
        let Some(native) = self.native else {
            let pivot = [x + fx * w, y + fy * h];
            return self.quad_about([x, y, w, h], pivot, f, filter);
        };
        let placed = self.affine.rect([self.map.x(x), y, self.map.w(w), h]);
        self.quad_at(native.fit(placed, self.scale), f, filter)
    }

    /// The quad showing `f` over the placed canvas rect `rect`, turned
    /// about its own pivot in it.
    fn quad_at(&self, rect @ [qx, qy, qw, qh]: [f32; 4], f: FrameUv, filter: Filter) -> Quad {
        let [fx, fy] = self.pivot;
        Quad {
            rect,
            uv: f.uv,
            color: self.color,
            pivot: [qx + fx * qw, qy + fy * qh],
            angle: self.angle,
            layer: f.layer,
            filter,
        }
    }

    /// [`quad`](Self::quad) with a strip cell's paint delta `d` added to
    /// the placed rect, `d / scale` canvas pixels in `f64`, before its
    /// native size and its pivot, as the source adds a digit's offset
    /// before it draws the glyph (`skin/SkinNumber.java:196-205`,
    /// `skin/SkinObject.java:649-667`). A zero delta is `quad`.
    pub(crate) fn quad_offset(
        &self,
        rect: [f32; 4],
        d: DigitOffset,
        f: FrameUv,
        filter: Filter,
    ) -> Quad {
        if d == DigitOffset::default() || self.scale <= 0.0 || self.scale.is_nan() {
            return self.quad(rect, f, filter);
        }
        let [x, y, w, h] = rect;
        let [px, py, pw, ph] = self.affine.rect([self.map.x(x), y, self.map.w(w), h]);
        let s = f64::from(self.scale);
        let add = |v: f32, dv: i32| f32_of(f64::from(v) + f64::from(dv) / s);
        let moved = [add(px, d.dx), add(py, d.dy), add(pw, d.dw), add(ph, d.dh)];
        let rect = match self.native {
            Some(native) => native.fit(moved, self.scale),
            None => moved,
        };
        self.quad_at(rect, f, filter)
    }

    /// The quad showing `f` over the reference rect `[x, y, w, h]` turned
    /// about the reference point `pivot`, both moved as
    /// [`quad`](Self::quad) moves them.
    pub(crate) fn quad_about(
        &self,
        [x, y, w, h]: [f32; 4],
        pivot: [f32; 2],
        f: FrameUv,
        filter: Filter,
    ) -> Quad {
        let (x, w, px) = (self.map.x(x), self.map.w(w), self.map.x(pivot[0]));
        Quad {
            rect: self.affine.rect([x, y, w, h]),
            uv: f.uv,
            color: self.color,
            pivot: self.affine.point([px, pivot[1]]),
            angle: self.angle,
            layer: f.layer,
            filter,
        }
    }

    /// Pushes [`quad`](Self::quad); `false` when the batch refused it.
    pub(crate) fn push(
        &self,
        out: &mut QuadBatch,
        rect: [f32; 4],
        f: FrameUv,
        filter: Filter,
    ) -> bool {
        out.push(self.key, self.quad(rect, f, filter))
    }

    /// Pushes [`quad_offset`](Self::quad_offset); `false` when the batch
    /// refused it.
    pub(crate) fn push_offset(
        &self,
        out: &mut QuadBatch,
        rect: [f32; 4],
        d: DigitOffset,
        f: FrameUv,
        filter: Filter,
    ) -> bool {
        out.push(self.key, self.quad_offset(rect, d, f, filter))
    }

    /// [`push`](Self::push) clipped to `clip` as well when one.
    pub(crate) fn push_clipped(
        &self,
        out: &mut QuadBatch,
        rect: [f32; 4],
        f: FrameUv,
        filter: Filter,
        clip: Option<ClipRect>,
    ) -> bool {
        let quad = self.quad(rect, f, filter);
        match clip {
            Some(c) => out.push_clipped(self.key, quad, c),
            None => out.push(self.key, quad),
        }
    }

    /// Pushes [`quad_about`](Self::quad_about); `false` when the batch
    /// refused it.
    pub(crate) fn push_about(
        &self,
        out: &mut QuadBatch,
        rect: [f32; 4],
        pivot: [f32; 2],
        f: FrameUv,
        filter: Filter,
    ) -> bool {
        out.push(self.key, self.quad_about(rect, pivot, f, filter))
    }
}

/// `track` on the timer in `slot` at `now`, with the µs since that timer
/// started: nothing while the timer is off, `visible` is false, or the
/// track does not draw.
pub(crate) fn sample(
    play: &Play,
    now: TimeUs,
    state: &State<'_>,
    slot: usize,
    visible: Option<&Program>,
    track: &Track,
) -> Option<(i64, Sample)> {
    let start = play.timer_at(slot)?;
    if visible.is_some_and(|v| !v.eval_bool(state)) {
        return None;
    }
    let t = elapsed_us(now, start);
    Some((t, track.sample(t)?))
}

/// Two colors multiplied component by component.
pub(crate) fn tint(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [a0, a1, a2, a3] = a;
    let [b0, b1, b2, b3] = b;
    [a0 * b0, a1 * b1, a2 * b2, a3 * b3]
}
