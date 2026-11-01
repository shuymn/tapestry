//! Bar groups: the lanes' bar lines, each group an image of its own drawn
//! at every chart bar line, with its own track, timer, visibility, blend
//! and placement.
//!
//! A group's rect is its sampled rect moved by the covers it follows,
//! then by the bar line's travel from the chart position at the display
//! head, `−(p − at_line) × px_per_pos`: at the judgment line's position
//! it is where the group authors it. Its track, visibility and image
//! cycle run on the frame's clock and their timers' starts, so the
//! display offset moves the bars but not their animation. Each frame
//! samples every group once; the bars draw in chart order, a bar's groups
//! in group order, through a bounded heap of the groups' cursors
//! ([`BarScratch`]), on the lanes' budgets.
//!
//! A bar draws its full quad while its vertical extent overlaps the
//! lanes' band `[top, bottom]` by a positive height, after the region's
//! transform; touching it draws nothing. The candidates a group visits
//! are the bars its conservative window holds, so a bar inside the window
//! but outside the band spends a visit and no quad.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use bms_model::Position;

use super::scroll::Scale;
use super::{Brush, ImageRef};
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::expr::Program;
use crate::place::{Bind, Follow, Native, Place};
use crate::quad::{Blend, Filter, Quad};
use crate::skin::{MAX_COORD, MAX_ELEMENTS, TimerRef};
use crate::track::Track;

/// The most bar groups one skin's lanes may hold.
pub const MAX_BAR_GROUPS: usize = MAX_ELEMENTS;

// A group's index fits the heap's `u16`.
const _: () = assert!(MAX_BAR_GROUPS <= 1 << 16);

/// One bar group as a skin authors it.
#[derive(Clone, Debug, PartialEq)]
pub struct BarGroupDef {
    /// The sheet image it shows, at the frame the image's cycle shows.
    pub image: u32,
    /// Corner-origin, like an element's: the rect, color, alpha and angle
    /// of the bar at the judgment line's position.
    pub track: Track,
    pub timer: TimerRef,
    /// Drawn only while this is true; compiled against the play schema.
    pub visible: Option<Program>,
    pub blend: Blend,
    /// In the lanes element's region, without a fill; its pivot, binding,
    /// native size and following as an image element's.
    pub place: Place,
}

/// Why a bar group was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BarGroupError {
    #[error("the group's timer does not exist")]
    UnknownTimer,
    #[error("the group's visibility is not a bool")]
    VisibleNotBool,
    #[error("the group's keys leave ±{MAX_COORD}")]
    Coordinates,
    #[error("the group is outside the lanes element's region")]
    Region,
    #[error("the group fills the canvas")]
    Fill,
}

/// A bar group as a build reads it.
#[derive(Clone, Debug)]
pub(crate) struct BarGroup {
    pub(crate) image: ImageRef,
    pub(crate) track: Track,
    /// The timer's slot.
    pub(crate) slot: usize,
    pub(crate) visible: Option<Program>,
    pub(crate) blend: Blend,
    /// The pivot's fractions of the rect.
    pub(crate) pivot: [f32; 2],
    pub(crate) bind: Bind,
    pub(crate) native: Option<Native>,
    pub(crate) follow: Follow,
}

/// One group as one frame draws it: its sampled quad at no travel.
#[derive(Clone, Copy)]
pub(crate) struct Template {
    pub(crate) brush: Brush,
    /// The rect's x, width and height; its y is `base`.
    pub(crate) x: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
    /// The rect's y at no travel, moved by the covers it follows.
    pub(crate) base: f64,
    pub(crate) frame: FrameUv,
    pub(crate) filter: Filter,
}

impl Template {
    /// The quad of the bar `travel` reference pixels down.
    pub(crate) fn quad(&self, travel: f64) -> Quad {
        let y = f32_of(self.base + travel);
        self.brush
            .quad([self.x, y, self.w, self.h], self.frame, self.filter)
    }
}

/// A group's frame record: its template and the bars left in its window,
/// `cursor..end` of the chart's bar lines.
#[derive(Clone, Copy)]
struct GroupFrame {
    template: Template,
    cursor: usize,
    end: usize,
}

/// The preallocated work of one attempt's bar drawing: a frame record and
/// a heap slot per group. [`Skin::prepare`](crate::Skin::prepare) makes
/// it once for a skin with groups; a build lends it to the lanes.
#[derive(Clone)]
pub(crate) struct BarScratch {
    frames: Box<[Option<GroupFrame>]>,
    heap: Box<[u16]>,
}

impl BarScratch {
    /// The bytes its buffers hold: the groups' frame records and the heap.
    pub(crate) fn heap_bytes(&self) -> usize {
        size_of_val(&*self.frames) + size_of_val(&*self.heap)
    }
}

impl fmt::Debug for BarScratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BarScratch")
            .field("groups", &self.frames.len())
            .finish_non_exhaustive()
    }
}

impl BarScratch {
    /// The work for `groups` groups.
    pub(crate) fn new(groups: usize) -> Self {
        Self {
            frames: vec![None; groups].into_boxed_slice(),
            heap: vec![0; groups].into_boxed_slice(),
        }
    }

    /// The groups it holds work for.
    pub(crate) fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// The bytes one group's frame record and heap slot take.
    #[must_use]
    pub(crate) const fn group_bytes() -> usize {
        size_of::<Option<GroupFrame>>() + size_of::<u16>()
    }
}

/// What a frame's bar walk reads: the chart's bars, the scale and the
/// lanes' band after the region's transform, `[top, bottom]`.
#[derive(Clone, Copy)]
pub(crate) struct Field<'a> {
    pub(crate) bars: &'a [Position],
    pub(crate) scale: &'a Scale,
    pub(crate) band: [f32; 2],
}

/// The vertical extent of `q` as drawn: its corners turned clockwise by
/// its angle about its pivot (the renderer's `vs`), in `f32`.
pub(crate) fn extent(q: &Quad) -> [f32; 2] {
    let [x, y, w, h] = q.rect;
    if q.angle == 0.0 {
        return [y.min(y + h), y.max(y + h)];
    }
    let (s, k) = q.angle.sin_cos();
    let [px, py] = q.pivot;
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for cx in [x, x + w] {
        for cy in [y, y + h] {
            let ty = py + ((cx - px) * s + (cy - py) * k);
            lo = lo.min(ty);
            hi = hi.max(ty);
        }
    }
    [lo, hi]
}

/// Whether `[e0, e1]` overlaps `[b0, b1]` by a positive length.
pub(crate) fn meets([e0, e1]: [f32; 2], [b0, b1]: [f32; 2]) -> bool {
    e1.min(b1) > e0.max(b0)
}

/// The travel, in reference pixels down, of the bar at `p`.
pub(crate) fn travel(scale: &Scale, p: Position) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a position difference, as the scale's own `y`"
    )]
    let ahead = p.minus(scale.at_line) as f64;
    -(ahead * scale.px_per_pos)
}

/// The indices `[start, end)` of `bars` whose quad of `t` may meet the
/// band: the travels for which the template's extent, moved by the
/// region's vertical scale times the travel, overlaps it, widened by the
/// construction's worst rounding error and rounded outward to whole
/// positions. `None` when the group cannot move or a bound is not finite.
pub(crate) fn window(t: &Template, f: &Field<'_>) -> Option<[usize; 2]> {
    let sy = f64::from(t.brush.affine.scale[1]);
    let k = f.scale.px_per_pos;
    if !(sy > 0.0 && k > 0.0 && sy.is_finite() && k.is_finite()) {
        return None;
    }
    let q0 = t.quad(0.0);
    let [e0, e1] = extent(&q0).map(f64::from);
    let [b0, b1] = f.band.map(f64::from);
    // The canvas shifts `c = sy·travel` for which `[e0 + c, e1 + c]` meets
    // `[b0, b1]`.
    let (c0, c1) = (b0 - e1, b1 - e0);
    // Every operand and intermediate the construction meets: the
    // reference rect and pivot, the affine's products and offset, the
    // template quad and its turned corners, the band and the travel at
    // both ends of the window.
    let [qx, qy, qw, qh] = q0.rect.map(f64::from);
    let [px, py] = q0.pivot.map(f64::from);
    let [ax, ay] = t.brush.affine.offset.map(f64::from);
    let sx = f64::from(t.brush.affine.scale[0]);
    let (x, w, h) = (f64::from(t.x), f64::from(t.w), f64::from(t.h));
    let (t0, t1) = (c0 / sy, c1 / sy);
    let terms = [
        1.0,
        x,
        x + w,
        t.base,
        t.base + h,
        sx * x,
        sx * (x + w),
        sy * t.base,
        sy * (t.base + h),
        ax,
        ay,
        qx,
        qx + qw,
        qy,
        qy + qh,
        px,
        py,
        2.0 * (qx.abs() + qw.abs() + px.abs()),
        2.0 * (qy.abs() + qh.abs() + py.abs()),
        e0,
        e1,
        b0,
        b1,
        c0,
        c1,
        t0,
        t1,
        t.base + t0,
        t.base + t1,
        sy * (t.base + t0),
        sy * (t.base + t1),
    ];
    let m = terms.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    if !m.is_finite() {
        return None;
    }
    let margin = 256.0 * f64::from(f32::EPSILON) * m;
    // Travel `−(p − at)·k`: the positions `p − at` in
    // `[−(c1 + margin)/(sy·k), −(c0 − margin)/(sy·k)]`, rounded outward.
    let lo = (-(c1 + margin) / (sy * k)).floor();
    let hi = (-(c0 - margin) / (sy * k)).ceil();
    if !(lo.is_finite() && hi.is_finite()) {
        return None;
    }
    // A relative bound past `i128` leaves its end open; otherwise `as`
    // saturates the far side of each bound and the sum saturates past
    // the range, either only widening the window.
    let at = f.scale.at_line.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "checked against the range first; `as` saturates, which only widens"
    )]
    let (from, to) = (
        if lo < i128::MIN as f64 {
            i128::MIN
        } else {
            at.saturating_add(lo as i128)
        },
        if hi >= i128::MAX as f64 {
            i128::MAX
        } else {
            at.saturating_add(hi as i128)
        },
    );
    let start = f.bars.partition_point(|p| p.0 < from);
    let end = f.bars.partition_point(|p| p.0 <= to);
    (start < end).then_some([start, end])
}

/// A candidate the walk met: the bar's index, its group's index and, when
/// its quad meets the band, the quad.
pub(crate) struct Candidate {
    #[cfg_attr(not(test), expect(dead_code, reason = "the oracle reads it"))]
    pub(crate) bar: usize,
    pub(crate) group: usize,
    pub(crate) quad: Option<Quad>,
}

impl BarScratch {
    /// Sets group `group`'s template for this frame, or none: a group
    /// without one draws nothing this frame.
    pub(crate) fn set(&mut self, group: usize, t: Option<Template>, f: &Field<'_>) {
        let record = t.and_then(|template| {
            let [cursor, end] = window(&template, f)?;
            Some(GroupFrame {
                template,
                cursor,
                end,
            })
        });
        if let Some(slot) = self.frames.get_mut(group) {
            *slot = record;
        }
    }

    /// Visits every group's bars in chart order, a bar's groups in group
    /// order, each candidate once through `each` until it returns `false`.
    /// Builds the heap from the groups set this frame, so a walk follows
    /// [`set`](Self::set) for every group.
    pub(crate) fn walk(&mut self, f: &Field<'_>, mut each: impl FnMut(Candidate) -> bool) {
        let mut len = 0;
        for g in 0..self.frames.len() {
            if self.frames.get(g).is_some_and(Option::is_some)
                && let Ok(i) = u16::try_from(g)
            {
                self.push(&mut len, i);
            }
        }
        while let Some(g) = self.pop(&mut len) {
            let gi = usize::from(g);
            let Some(Some(rec)) = self.frames.get_mut(gi) else {
                return;
            };
            let bar = rec.cursor;
            let Some(&p) = f.bars.get(bar) else {
                return;
            };
            let q = rec.template.quad(travel(f.scale, p));
            let quad = meets(extent(&q), f.band).then_some(q);
            rec.cursor += 1;
            let more = rec.cursor < rec.end;
            if !each(Candidate {
                bar,
                group: gi,
                quad,
            }) {
                return;
            }
            if more {
                self.push(&mut len, g);
            }
        }
    }

    /// The heap key of group `g`: its cursor, then its index.
    fn key(&self, g: u16) -> (usize, u16) {
        let cursor = self
            .frames
            .get(usize::from(g))
            .and_then(|r| r.as_ref())
            .map_or(usize::MAX, |r| r.cursor);
        (cursor, g)
    }

    fn push(&mut self, len: &mut usize, g: u16) {
        let mut i = *len;
        let Some(slot) = self.heap.get_mut(i) else {
            return;
        };
        *slot = g;
        *len += 1;
        while i > 0 {
            let parent = (i - 1) / 2;
            let (Some(&a), Some(&b)) = (self.heap.get(i), self.heap.get(parent)) else {
                return;
            };
            if self.key(a) >= self.key(b) {
                return;
            }
            self.heap.swap(i, parent);
            i = parent;
        }
    }

    fn pop(&mut self, len: &mut usize) -> Option<u16> {
        if *len == 0 {
            return None;
        }
        let top = *self.heap.first()?;
        *len -= 1;
        self.heap.swap(0, *len);
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut least = i;
            for c in [l, r] {
                if c < *len
                    && let (Some(&a), Some(&b)) = (self.heap.get(c), self.heap.get(least))
                    && self.key(a) < self.key(b)
                {
                    least = c;
                }
            }
            if least == i {
                return Some(top);
            }
            self.heap.swap(i, least);
            i = least;
        }
    }
}

#[cfg(test)]
mod tests;
