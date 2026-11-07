//! The result's gauge graph: a view's emphasized gauge over the play's
//! time, a back in two bands split at the border and a line over it
//! (design.md "Rendering and Skins", result widgets).
//!
//! The source paints the back and the line into two textures and draws
//! each once under the destination's colour, the line revealed left to
//! right. Here each texture is a set of disjoint quads, each tinted with
//! its region's colour (layers composed by [`over`]), so the
//! destination's alpha applies once per pixel. Geometry is in the
//! element's reference units, the graph's rows counted up from its
//! bottom: column `c` of the history owns `[w·c/288, w·(c+1)/288)`, a
//! value `v` sits at `v/max·(h − 2)`, and the line is 2 units thick.
//!
//! The layers' colours are composed on every build, from the view's look
//! and the skin's colours, not into a fixed table filled at a view change
//! as the S10-09h design (D6) planned: a deviation that is arithmetic
//! only and allocates nothing, as the judge graph composes its own.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Brush, Frame, ImageRef, tint};
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::pixmap::{Rgba8, over};
use crate::quad::{Filter, QuadBatch};
use crate::result::view::{GRAPH_COLUMNS, GaugeColumn, GaugeHistory};
use crate::widget::gauge::GaugeLook;

/// The line's thickness in reference units.
const LINE: f64 = 2.0;
/// How long the line takes to reveal, in µs of the screen timer.
const REVEAL_US: f64 = 1_500_000.0;
/// The history's columns as a float.
#[expect(
    clippy::cast_precision_loss,
    reason = "GRAPH_COLUMNS is 288, exact in f64"
)]
const COLUMNS: f64 = GRAPH_COLUMNS as f64;
/// The quads one frame can push at worst: the back's two, five a column
/// of the line, and the no-input mark.
#[expect(
    clippy::cast_possible_truncation,
    reason = "GRAPH_COLUMNS is 288, which fits u32"
)]
const QUADS: u32 = 2 + 5 * GRAPH_COLUMNS as u32 + 1;

/// One look's colours, as the source keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeGraphColors {
    /// The line at or above the border.
    pub border_line: Rgba8,
    /// The back above the border, over [`back`](Self::back).
    pub border_back: Rgba8,
    /// The line below the border.
    pub line: Rgba8,
    /// The back over the whole graph.
    pub back: Rgba8,
}

/// A gauge graph as a front end builds it. Its element's track places
/// the graph and gives its colour and alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeGraphDef {
    /// Each look's colours, by [`GaugeLook::index`].
    pub colors: [GaugeGraphColors; GaugeLook::COUNT],
    /// A white image the back, the line and the mark tint.
    pub solid: u32,
}

impl GaugeGraphDef {
    /// The quads one frame can push at worst.
    pub(crate) fn quads(&self) -> u32 {
        QUADS
    }
}

/// A gauge graph as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GaugeGraph {
    pub(crate) def: GaugeGraphDef,
    pub(crate) solid: ImageRef,
}

/// What one gauge graph's frame reads besides its element's.
pub(crate) struct GaugeGraphDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The view's emphasized gauge; nothing draws without it.
    pub(crate) gauge: Option<&'a GaugeHistory>,
    /// The screen timer's elapsed µs, `None` while it is off.
    pub(crate) screen: Option<i64>,
}

/// The graph's region: its reference rect, its placement and its pivot
/// in canvas pixels, and the solid image's frame.
struct Region<'a> {
    /// The reference rect after the lane map: `[x, y, w, h]`.
    rect: [f64; 4],
    brush: &'a Brush,
    pivot: [f32; 2],
    f: FrameUv,
    filter: Filter,
}

impl Region<'_> {
    /// Pushes the piece `[left, right) × [bottom, top)` of the graph (its
    /// x from the region's left, its rows up from its bottom, in
    /// reference units) in the straight colour `color`, cut at `clip`
    /// from the left and to the graph's rows `[0, h)`, so a graph shorter
    /// than the line keeps every quad inside its rect; `false` when the
    /// batch refused it. An empty or fully transparent piece pushes
    /// nothing.
    fn piece(
        &self,
        [left, right]: [f64; 2],
        [bottom, top]: [f64; 2],
        clip: f64,
        color: [f32; 4],
        out: &mut QuadBatch,
    ) -> bool {
        let [x, y, _, h] = self.rect;
        let right = right.min(clip);
        let (bottom, top) = (bottom.max(0.0), top.min(h));
        let [.., alpha] = color;
        if right <= left || top <= bottom || alpha <= 0.0 {
            return true;
        }
        let reference = [
            f32_of(x + left),
            f32_of(y + h - top),
            f32_of(right - left),
            f32_of(top - bottom),
        ];
        let placed = self.brush.affine.rect(reference);
        let b = Brush {
            color: tint(self.brush.color, color),
            ..*self.brush
        };
        b.push_placed(out, placed, self.pivot, self.f, self.filter)
    }
}

impl GaugeGraph {
    /// Draws the graph over the reference rect `rect` through `brush`:
    /// nothing without a gauge or without a positive width and height; the back whole, then the line's columns
    /// up to `reached`, cut at `w·min(1, t/1.5 s)` of the screen timer's
    /// `t`, then the no-input mark. The rect is placed (lane map, affine)
    /// as one region; every quad turns with the brush's angle about the
    /// brush's pivot in that region. A refused push stops the graph.
    pub(crate) fn draw(
        &self,
        d: &GaugeGraphDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let Some(g) = d.gauge else {
            return;
        };
        let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let [x, y, w, h] = rect;
        let mapped = [
            f64::from(brush.map.x(x)),
            f64::from(y),
            f64::from(brush.map.w(w)),
            f64::from(h),
        ];
        let [mx, my, mw, mh] = mapped.map(f32_of);
        let [px, py, pw, ph] = brush.affine.rect([mx, my, mw, mh]);
        // A rect with no positive width and height (a track may reach
        // one, or interpolate through 0 into one) draws nothing.
        if !(mw > 0.0 && mh > 0.0 && pw > 0.0 && ph > 0.0) {
            return;
        }
        let [fx, fy] = brush.pivot;
        let r = Region {
            rect: mapped,
            brush,
            pivot: [px + fx * pw, py + fy * ph],
            f,
            filter: self.solid.filter,
        };
        let Some(colors) = self.def.colors.get(g.look.index()).copied() else {
            return;
        };
        let [_, _, w, h] = mapped;
        let max = value(g.max.0).max(1.0);
        let border = value(g.border.0);
        // The back: the below-border colour, then the above-border colour
        // over it from the border to the top.
        let back = colors.back.unit();
        let hb = h * border / max;
        if !r.piece([0.0, w], [0.0, hb], w, back, out)
            || !r.piece(
                [0.0, w],
                [hb, h],
                w,
                over(colors.border_back.unit(), back),
                out,
            )
        {
            return;
        }
        let clip = w * reveal(d.screen);
        let line = Line {
            span: h - LINE,
            max,
            border,
            below: colors.line.unit(),
            above: colors.border_line.unit(),
        };
        let reached = usize::from(g.reached).min(GRAPH_COLUMNS);
        for (c, col) in (0_u16..).zip(g.columns.iter().take(reached)) {
            let [left, right] = column(w, c);
            if left >= clip {
                break;
            }
            if !line.column(&r, col, [left, right], clip, out) {
                return;
            }
        }
        // The no-input mark: one reference unit wide at its column's left,
        // full height, after the line.
        if let Some(n) = g.no_input_from
            && usize::from(n) < GRAPH_COLUMNS
        {
            let [left, _] = column(w, n);
            let white = Rgba8::rgb(0xff_ffff, 0xff).unit();
            r.piece([left, left + 1.0], [0.0, h], clip, white, out);
        }
    }
}

/// Column `c`'s left and right edges of a graph `w` wide.
fn column(w: f64, c: u16) -> [f64; 2] {
    let at = |c: u16| w * f64::from(c) / COLUMNS;
    [at(c), at(c + 1)]
}

/// A gauge value as a float, exact below 2^53.
fn value(v: i64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a gauge value far below 2^53")]
    let f = v as f64;
    f
}

/// The line's revealed fraction of the width at the screen timer's
/// elapsed µs: none while the timer is off.
fn reveal(screen: Option<i64>) -> f64 {
    screen.map_or(0.0, |t| (value(t) / REVEAL_US).clamp(0.0, 1.0))
}

/// The line's scale and colours.
struct Line {
    /// The rows a value spans: `h − 2`.
    span: f64,
    max: f64,
    border: f64,
    /// The colour below the border, and at or above it.
    below: [f32; 4],
    above: [f32; 4],
}

impl Line {
    /// The row of value `v`: `v/max·(h − 2)`, within the graph.
    fn y(&self, v: i64) -> f64 {
        (value(v) / self.max * self.span).clamp(0.0, self.span.max(0.0))
    }

    /// Pushes one column's pieces: the vertical at its left from `low` to
    /// `high` (in one colour unless the two cross the border, split at
    /// the border's row only then), the horizontal at `last` across the
    /// column in `last`'s colour, the corner where they meet the
    /// horizontal over the vertical, each cut at `clip`; `false` when the
    /// batch refused a piece.
    fn column(
        &self,
        r: &Region<'_>,
        col: &GaugeColumn,
        [left, right]: [f64; 2],
        clip: f64,
        out: &mut QuadBatch,
    ) -> bool {
        let yb = (self.border / self.max * self.span).clamp(0.0, self.span.max(0.0));
        let (low, high, last) = (col.low.0, col.high.0, col.last.0);
        let [y0, y1, yl] = [self.y(low), self.y(high) + LINE, self.y(last)];
        let border = self.border;
        // The vertical's colour at a row: one colour unless it crosses.
        let split = value(low) < border && border <= value(high);
        let vertical = |row_above_border: bool| {
            if split {
                if row_above_border {
                    self.above
                } else {
                    self.below
                }
            } else if value(high) < border {
                self.below
            } else {
                self.above
            }
        };
        let horizontal = if value(last) < border {
            self.below
        } else {
            self.above
        };
        let vx = [left, left + LINE.min(right - left)];
        // The vertical's rows, each cut at the border's row when split,
        // the corner's rows composed with the horizontal over them.
        let corner = [yl, yl + LINE];
        let spans = [
            ([y0, corner[0]], false),
            (corner, true),
            ([corner[1], y1], false),
        ];
        for ([from, to], in_corner) in spans {
            let cuts: [[f64; 2]; 2] = if split && from < yb && yb < to {
                [[from, yb], [yb, to]]
            } else {
                [[from, to], [to, to]]
            };
            for [a, b] in cuts {
                if b <= a {
                    continue;
                }
                let v = vertical(a >= yb);
                let color = if in_corner { over(horizontal, v) } else { v };
                if !r.piece(vx, [a, b], clip, color, out) {
                    return false;
                }
            }
        }
        r.piece([vx[1], right], corner, clip, horizontal, out)
    }
}
