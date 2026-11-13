//! The play skin's hit-error visualizer: the ring's newest offsets as
//! lines, a centre line and a moving average, each element keeping its own
//! average (`skin/SkinHitErrorVisualizer.java:56-195` at `0.8.9`; design.md
//! "Rendering and Skins", play visualizers).
//!
//! The source rasterizes a `width × 2·wl` pixmap whenever the ring's head
//! moves, source-over onto transparent (the newest `wl` lines newest first,
//! the centre line, the average's line and triangle), and draws it once
//! under the destination's state. Here the same fills are composed into
//! disjoint runs per column ([`compose`](super::compose)) whenever the
//! ring's count moves, into room reserved at preparation, and the runs are
//! drawn under the destination's state.
//!
//! The average consumes every offset recorded since the element last read
//! the ring, in order, that is still in the ring (the source applies only
//! the newest a frame): `ema += α · (v − ema)` for each one strictly
//! inside the BAD window, kept unrounded.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::compose::{self, Fill, Raster, Run};
use super::{Brush, Frame, ImageRef};
use crate::pixmap::Rgba8;
use crate::play::Offsets;
use crate::quad::QuadBatch;

/// The widest hit-error pixmap, in texels.
pub const MAX_HIT_ERROR_WIDTH: u32 = 4_096;
/// The most lines a hit-error visualizer keeps.
pub const MAX_HIT_ERROR_WINDOW: u32 = 100;

/// How a hit-error visualizer marks its moving average.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmaMode {
    /// None; the average is not kept.
    Off,
    /// A full-height line.
    Line,
    /// A triangle from the top.
    Triangle,
    /// Both.
    Both,
}

/// A hit-error visualizer as a front end builds it. Its element's track
/// places the pixmap and gives its colour, alpha and angle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitErrorDef {
    /// The pixmap's width in texels, 1 to [`MAX_HIT_ERROR_WIDTH`].
    pub width: u32,
    /// The ms either side of 0 it spans, 1 to
    /// [`MAX_JUDGE_WIDTH`](super::timing_vis::MAX_JUDGE_WIDTH).
    pub judge_width: u32,
    /// The lines' width in texels, 1 to 4.
    pub line_width: u32,
    /// The newest lines drawn, 1 to [`MAX_HIT_ERROR_WINDOW`]; the pixmap is
    /// twice as tall.
    pub window: u32,
    /// Draw the lines (the source's `hiterrorMode`).
    pub lines: bool,
    /// Colour each line by its window band, else by age in `line`.
    pub band_colors: bool,
    pub ema_mode: EmaMode,
    /// The average's weight of each new offset: any finite value, as the
    /// source takes any (`ema += alpha · (v − ema)`, so a weight outside 0
    /// to 1 overshoots, the average held within ±2^53 ms, an infinite
    /// weight's step at that bound).
    pub alpha: f32,
    pub line: Rgba8,
    pub center: Rgba8,
    pub ema: Rgba8,
    /// PERFECT, GREAT, GOOD, BAD and beyond; a transparent one draws nothing.
    pub bands: [Rgba8; 5],
    /// Lines shrink with age, centred.
    pub decay: bool,
    /// A white image every run tints.
    pub solid: u32,
}

impl HitErrorDef {
    /// The triangle's half width at most: `⌊width/100⌋` made even.
    const fn half(&self) -> u32 {
        let w = self.width / 100;
        w + w % 2
    }

    /// Column fills at most: each line's, the centre's and the average
    /// line's columns, and the triangle's.
    const fn column_fills(&self) -> u32 {
        self.line_width * (self.window + 2) + 2 * self.half() + 1
    }

    /// The quads one frame pushes at worst: a column with `k` fills holds
    /// at most `2k` runs that are not transparent.
    #[must_use]
    pub const fn quads(&self) -> u32 {
        2 * self.column_fills()
    }
}

/// A hit-error visualizer as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HitError {
    pub(crate) def: HitErrorDef,
    pub(crate) solid: ImageRef,
}

/// One hit-error element's state in a play: its average, the ring count
/// it last read and composed, and its runs, each buffer reserved at its
/// bound.
#[derive(Clone, Debug)]
pub(crate) struct HitErrorState {
    def: HitErrorDef,
    ema: f64,
    seen: u32,
    composed: Option<u32>,
    runs: Vec<Run>,
    fills: Vec<(u32, u32, Fill)>,
    cols: Vec<u32>,
    column: Vec<Fill>,
    edges: Vec<u32>,
}

/// `n` as a buffer length.
fn room(n: u32) -> usize {
    usize::try_from(n).unwrap_or(0)
}

impl HitErrorState {
    /// A fresh state for `def`: the average at 0, nothing read.
    pub(crate) fn new(def: HitErrorDef) -> Self {
        let fills = def.window + 2 + 2 * def.half() + 1;
        Self {
            def,
            ema: 0.0,
            seen: 0,
            composed: None,
            runs: Vec::with_capacity(room(def.quads())),
            fills: Vec::with_capacity(room(fills)),
            cols: Vec::with_capacity(room(def.column_fills())),
            column: Vec::with_capacity(room(def.window + 3)),
            edges: Vec::with_capacity(room(2 * (def.window + 3))),
        }
    }

    /// Starts over as [`new`](Self::new) does, keeping the buffers.
    pub(crate) fn reset(&mut self) {
        self.ema = 0.0;
        self.seen = 0;
        self.composed = None;
        self.runs.clear();
    }

    /// The bytes its buffers hold.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.runs.capacity() * size_of::<Run>()
            + self.fills.capacity() * size_of::<(u32, u32, Fill)>()
            + self.cols.capacity() * size_of::<u32>()
            + self.column.capacity() * size_of::<Fill>()
            + self.edges.capacity() * size_of::<u32>()
    }

    /// The composed runs.
    pub(crate) fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Reads the ring: the average takes every offset recorded since the
    /// last read that is still in it, and the pixmap is composed again
    /// when the ring's count moved. `areas` are the key judge areas.
    pub(crate) fn update(&mut self, offsets: Offsets<'_>, areas: [[i16; 2]; 5]) {
        let [bad_lo, bad_hi] = areas.get(3).copied().unwrap_or([0; 2]).map(i64::from);
        if self.def.ema_mode != EmaMode::Off {
            for v in offsets.since(self.seen) {
                if bad_lo < i64::from(v) && i64::from(v) < bad_hi {
                    self.ema = ema_step(self.ema, f64::from(v), self.def.alpha);
                }
            }
        }
        self.seen = offsets.count;
        if self.composed != Some(offsets.count) {
            self.compose(offsets, areas);
            self.composed = Some(offsets.count);
        }
    }

    /// The source's x of an offset `v`: the centre less `v` scaled to the
    /// width, truncated toward 0; early is to the left.
    fn x_of(&self, v: i64) -> i64 {
        let d = &self.def;
        let jw = i64::from(d.judge_width);
        let w = i64::from(d.width);
        (w - i64::from(d.line_width)) / 2 + (-v.clamp(-jw, jw) * w) / (2 * jw + 1)
    }

    /// The x of the average `ema`, as [`x_of`](Self::x_of) places an
    /// offset: its scaled distance truncated toward 0. Within ±`jw` the
    /// quotient is within ±`w`, so the cast is exact.
    fn x_of_ema(&self, ema: f64) -> i64 {
        let d = &self.def;
        let jw = f64::from(d.judge_width);
        let w = f64::from(d.width);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a quotient within ±w, truncated toward 0"
        )]
        let off = ((-ema.clamp(-jw, jw) * w) / (2.0 * jw + 1.0)).trunc() as i64;
        (i64::from(d.width) - i64::from(d.line_width)) / 2 + off
    }

    /// The pixmap's fills in paint order, composed per column into `runs`.
    fn compose(&mut self, offsets: Offsets<'_>, areas: [[i16; 2]; 5]) {
        let d = self.def;
        let (w, wl, lw) = (d.width, d.window, d.line_width);
        let rows = 2 * wl;
        self.fills.clear();
        let push = |fills: &mut Vec<(u32, u32, Fill)>,
                    x: i64,
                    width: u32,
                    y0: u32,
                    y1: u32,
                    color: [f32; 4]| {
            let x0 = u32::try_from(x.clamp(0, i64::from(w))).unwrap_or(0);
            let x1 = u32::try_from((x + i64::from(width)).clamp(0, i64::from(w))).unwrap_or(0);
            let (y0, y1) = (y0.min(rows), y1.min(rows));
            if x1 > x0 && y1 > y0 && color[3] > 0.0 && fills.len() < fills.capacity() {
                fills.push((x0, x1, Fill { y0, y1, color }));
            }
        };
        if d.lines {
            for i in (1..=wl).rev() {
                let Some(v) = offsets.back(wl - i) else {
                    continue;
                };
                let color = if d.band_colors {
                    let k = areas
                        .iter()
                        .take(4)
                        .position(|&[lo, hi]| i32::from(lo) < v && v < i32::from(hi))
                        .unwrap_or(4);
                    d.bands.get(k).copied().unwrap_or(d.line).unit()
                } else {
                    let [r, g, b, a] = d.line.unit();
                    #[expect(clippy::cast_precision_loss, reason = "counts to 100")]
                    let alpha = a * i as f32 / (wl as f32 / 2.0);
                    [r, g, b, alpha.min(1.0)]
                };
                let (y0, y1) = if d.decay {
                    (wl - i, wl + i)
                } else {
                    (0, 2 * 100)
                };
                let x = self.x_of(i64::from(v));
                push(&mut self.fills, x, lw, y0, y1, color);
            }
        }
        let center = (i64::from(w) - i64::from(lw)) / 2;
        push(&mut self.fills, center, lw, 0, rows, d.center.unit());
        if d.ema_mode != EmaMode::Off {
            let x = self.x_of_ema(self.ema);
            let color = d.ema.unit();
            if matches!(d.ema_mode, EmaMode::Line | EmaMode::Both) {
                push(&mut self.fills, x, lw, 0, rows, color);
            }
            if matches!(d.ema_mode, EmaMode::Triangle | EmaMode::Both) {
                let tip = x + i64::from(lw / 2);
                let half = i64::from(d.half());
                let depth = i64::from(rows / 3);
                for c in tip - half..=tip + half {
                    let top = if half == 0 {
                        depth
                    } else {
                        depth * (half - (c - tip).abs()) / half
                    };
                    let top = u32::try_from(top + 1).unwrap_or(0);
                    push(&mut self.fills, c, 1, 0, top, color);
                }
            }
        }
        self.cols.clear();
        for &(x0, x1, _) in &self.fills {
            for c in x0..x1 {
                if self.cols.len() < self.cols.capacity() {
                    self.cols.push(c);
                }
            }
        }
        self.cols.sort_unstable();
        self.cols.dedup();
        self.runs.clear();
        for &c in &self.cols {
            self.column.clear();
            for &(x0, x1, f) in &self.fills {
                if x0 <= c && c < x1 && self.column.len() < self.column.capacity() {
                    self.column.push(f);
                }
            }
            if self.runs.len() + 2 * self.column.len() > self.runs.capacity() {
                break;
            }
            compose::column(c, &self.column, &mut self.edges, &mut self.runs);
        }
    }
}

/// The farthest the average goes from 0, in ms (2^53): a weight outside 0
/// to 1 can push it past any offset, and this keeps every step finite.
pub(crate) const EMA_BOUND: f64 = 9_007_199_254_740_992.0;

/// One step of the average, `ema + alpha · (value − ema)`, unrounded and
/// held within ±[`EMA_BOUND`]. With `|ema|` at most 2^53 and `alpha` a
/// finite `f32`, the product is finite; an infinite weight's step is
/// infinite and held at the bound, and a step that is no number (an
/// infinite weight times no difference) leaves the average as it was.
pub(crate) fn ema_step(ema: f64, value: f64, alpha: f32) -> f64 {
    let next = ema + f64::from(alpha) * (value - ema);
    if next.is_nan() {
        ema
    } else {
        next.clamp(-EMA_BOUND, EMA_BOUND)
    }
}

/// What one hit-error visualizer's frame reads besides its element's.
pub(crate) struct HitErrorDraw<'a> {
    pub(crate) at: Frame<'a>,
    pub(crate) state: &'a HitErrorState,
}

impl HitError {
    /// Draws the composed runs over the placed rect under the brush.
    pub(crate) fn draw(
        &self,
        d: &HitErrorDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let region = super::judge_graph::placed(rect, brush);
        let r = Raster {
            region,
            cols: self.def.width,
            rows: 2 * self.def.window,
            from_bottom: false,
            pivot: super::judge_graph::drawn_pivot(region, None, brush.pivot),
        };
        r.draw(d.state.runs(), u32::MAX, brush, f, self.solid.filter, out);
    }
}
