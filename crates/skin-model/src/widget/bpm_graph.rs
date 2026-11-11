//! The play skin's BPM graph: the chart's tempo rows over its 1× time
//! (`skin/SkinBPMGraph.java:51-54, 167-219` at `0.8.9`; design.md
//! "Rendering and Skins", play graphs).
//!
//! The source paints its strokes into one pixmap of the destination's size
//! on its first frame and draws it once under the destination's state. Here
//! the pixmap is sized at preparation from the element's first key, at most
//! [`MAX_BPM_COLUMNS`] columns, and its strokes composed into disjoint runs
//! per column ([`compose`](super::compose)), at most [`RUNS_PER_COLUMN`] a
//! column: a busier column keeps its first three and draws the rest of its
//! span as one run in the transition colour. The floating-point geometry
//! (the source's `log10` and its `double` products, truncated as it
//! truncates them) stays in this preparation.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{SpeedRow, Tempo, TimeUs};

use super::compose::{self, Fill, Raster, Run};
use super::{Brush, Frame, ImageRef};
use crate::pixmap::Rgba8;
use crate::quad::QuadBatch;

/// The most columns a BPM graph's pixmap takes.
pub const MAX_BPM_COLUMNS: u32 = 2_048;
/// The most runs a column keeps.
pub const RUNS_PER_COLUMN: u32 = 4;
/// The widest stroke a BPM graph takes, in texels (provisional, owner
/// pattern 7): it bounds how many columns a stroke overhangs its row's
/// span, so how many strokes one column holds in a preparation, and keeps
/// the source's `int` stroke arithmetic exact.
pub const MAX_BPM_LINE_WIDTH: u32 = 16;

/// A BPM graph's colours, the source's defaults first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BpmColors {
    /// A row at the chart's MAIN tempo (`00ff00`).
    pub main: Rgba8,
    /// A row at its lowest positive tempo (`0000ff`).
    pub min: Rgba8,
    /// A row at its highest tempo (`ff0000`).
    pub max: Rgba8,
    /// Any other row (`ffff00`).
    pub other: Rgba8,
    /// A stopped row (`ff00ff`).
    pub stop: Rgba8,
    /// A change's vertical stroke (`7f7f7f`).
    pub transition: Rgba8,
}

impl BpmColors {
    /// The source's defaults (`skin/SkinBPMGraph.java:39-44`).
    pub const DEFAULT: Self = Self {
        main: Rgba8::rgb(0x00_ff00, 0xff),
        min: Rgba8::rgb(0x00_00ff, 0xff),
        max: Rgba8::rgb(0xff_0000, 0xff),
        other: Rgba8::rgb(0xff_ff00, 0xff),
        stop: Rgba8::rgb(0xff_00ff, 0xff),
        transition: Rgba8::rgb(0x7f_7f7f, 0xff),
    };
}

/// A BPM graph as a front end builds it. Its element's first key sizes
/// its pixmap; its track places the whole graph and gives its colour,
/// alpha and angle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BpmGraphDef {
    /// How long the graph takes to reveal, left to right, on the screen
    /// timer; not positive shows it at once.
    pub delay: TimeUs,
    /// The strokes' width in texels, 1 to [`MAX_BPM_LINE_WIDTH`] (the
    /// source's default 2).
    pub line_width: u32,
    pub colors: BpmColors,
    /// A white image every run tints.
    pub solid: u32,
}

/// The columns of a BPM graph whose element's first key is `width`
/// reference pixels wide.
#[must_use]
pub fn bpm_columns(width: i32) -> u32 {
    width.unsigned_abs().min(MAX_BPM_COLUMNS)
}

/// A BPM graph as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BpmGraph {
    pub(crate) def: BpmGraphDef,
    pub(crate) solid: ImageRef,
}

/// One BPM graph element's prepared pixmap: its size and its runs.
#[derive(Clone, Debug, Default)]
pub(crate) struct BpmRaster {
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    pub(crate) runs: Box<[Run]>,
}

/// What a BPM graph is prepared from.
pub(crate) struct BpmInput<'a> {
    /// The tempo rows, in time order.
    pub(crate) rows: &'a [SpeedRow],
    /// The whole chart's MAIN tempo.
    pub(crate) main: Tempo,
    /// The chart's length at 1×, in whole ms.
    pub(crate) length_ms: i64,
}

/// A tempo's BPM in `f64`.
fn bpm(t: Option<Tempo>) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "the source's double tempo")]
    let v = t.map_or(0.0, |t| t.units() as f64 / Tempo::UNITS_PER_BPM as f64);
    v
}

/// `v` truncated toward 0 as Java's `(int)` of a `double` does, saturating.
fn int_of(v: f64) -> i64 {
    if v.is_nan() {
        return 0;
    }
    #[expect(clippy::cast_possible_truncation, reason = "Java's (int) truncation")]
    let i = v.trunc().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i64;
    i
}

/// A row's class colour, in the source's order: MAIN, the lowest positive,
/// the highest, a stop, else other.
fn class(row: Option<Tempo>, main: Tempo, [min, max]: [Option<Tempo>; 2], c: &BpmColors) -> Rgba8 {
    match row {
        Some(t) if t == main => c.main,
        Some(t) if Some(t) == min => c.min,
        Some(t) if Some(t) == max => c.max,
        None => c.stop,
        Some(_) => c.other,
    }
}

impl BpmGraphDef {
    /// The pixmap of `input` at `cols` × `rows` texels: the source's
    /// strokes, composed into runs. Empty with fewer than two rows.
    pub(crate) fn prepare(&self, input: &BpmInput<'_>, cols: u32, rows: u32) -> BpmRaster {
        let mut raster = BpmRaster {
            cols,
            rows,
            runs: Box::new([]),
        };
        let data = input.rows;
        let Some(last) = data.last() else {
            return raster;
        };
        if data.len() < 2 || cols == 0 || rows == 0 {
            return raster;
        }
        // The lowest positive and the highest tempo; a stop is 0, below
        // every tempo.
        let tempos = data.iter().filter_map(|r| r.tempo);
        let range = [tempos.clone().min(), tempos.max()];
        let main = bpm(Some(input.main));
        let lw = i64::from(self.line_width.max(1));
        let (w, h) = (i64::from(cols), i64::from(rows));
        let mut last_time = i64::from(last.ms);
        if input.length_ms < last_time {
            last_time = input.length_ms;
        }
        #[expect(clippy::cast_precision_loss, reason = "a time in ms, far below 2^53")]
        let last_time = (last_time + 1_000) as f64;
        // The source's y of a row: its tempo over MAIN, clamped to 1/8 to 8,
        // on a log scale over the height less a stroke.
        let (lo, hi) = ((1.0_f64 / 8.0).log10(), 8.0_f64.log10());
        #[expect(clippy::cast_precision_loss, reason = "sizes within 2^31")]
        let (wf, hl) = (w as f64, (h - lw) as f64);
        let y = |t: Option<Tempo>| {
            let r = (bpm(t) / main).clamp(1.0 / 8.0, 8.0);
            int_of((r.log10() - lo) / (hi - lo) * hl)
        };
        let x = |ms: u32| int_of(wf * f64::from(ms) / last_time);
        let colors = &self.colors;
        // The fills in paint order: (left, right, bottom, top, colour), in
        // texels from the bottom left.
        let mut fills: Vec<(i64, i64, i64, i64, Rgba8)> = Vec::with_capacity(2 * data.len());
        for pair in data.windows(2) {
            let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            let (y1, y2) = (y(a.tempo), y(b.tempo));
            let x1 = x(b.ms);
            if (y2 - y1).abs() - lw > 0 {
                let bottom = y1.min(y2) + lw;
                fills.push((
                    x1,
                    x1 + lw,
                    bottom,
                    bottom + (y2 - y1).abs() - lw,
                    colors.transition,
                ));
            }
            let x0 = x(a.ms);
            let color = class(a.tempo, input.main, range, colors);
            fills.push((x0, x1 + lw, y1, y1 + lw, color));
        }
        let yl = y(last.tempo);
        let color = class(last.tempo, input.main, range, colors);
        fills.push((x(last.ms), w + lw, yl, yl + lw, color));
        raster.runs = compose_columns(&fills, cols, rows, colors.transition);
        raster
    }

    /// The quads one frame pushes at worst for `cols` columns.
    #[must_use]
    pub fn quads(cols: u32) -> u32 {
        cols.saturating_mul(RUNS_PER_COLUMN)
    }
}

/// `fills` clipped to `cols` × `rows`, composed per column, each column
/// capped at [`RUNS_PER_COLUMN`] runs: its first three, then one run in
/// `transition` from the fourth's bottom to its last's top.
///
/// The columns are swept left to right holding only the fills that cover
/// the current column, in paint order, so the storage is the most fills
/// one column holds, never a copy of every fill per column it covers. The
/// work is each column's fills, which [`compose::column`] rescans once for
/// each distinct vertical interval between their edges.
fn compose_columns(
    fills: &[(i64, i64, i64, i64, Rgba8)],
    cols: u32,
    rows: u32,
    transition: Rgba8,
) -> Box<[Run]> {
    let clamp = |v: i64, top: u32| u32::try_from(v.clamp(0, i64::from(top))).unwrap_or(0);
    // Each drawable fill as (left, right, its fill), in paint order.
    let clipped: Vec<(u32, u32, Fill)> = fills
        .iter()
        .filter_map(|&(l, r, b, t, color)| {
            let (l, r) = (clamp(l, cols), clamp(r, cols));
            let (y0, y1) = (clamp(b, rows), clamp(t, rows));
            (y1 > y0 && r > l).then_some((
                l,
                r,
                Fill {
                    y0,
                    y1,
                    color: color.unit(),
                },
            ))
        })
        .collect();
    // The fills by their left column, in paint order within a column.
    let mut by_left: Vec<usize> = (0..clipped.len()).collect();
    by_left.sort_by_key(|&k| clipped.get(k).map_or(0, |f| f.0));
    let mut next = by_left.iter().copied().peekable();
    let mut active: Vec<usize> = Vec::new();
    let mut column: Vec<Fill> = Vec::new();
    let mut out = Vec::new();
    let (mut edges, mut runs) = (Vec::new(), Vec::new());
    let cap = usize::try_from(RUNS_PER_COLUMN).unwrap_or(4);
    for c in 0..cols {
        active.retain(|&k| clipped.get(k).is_some_and(|f| f.1 > c));
        let before = active.len();
        while let Some(k) = next.next_if(|&k| clipped.get(k).is_some_and(|f| f.0 <= c)) {
            active.push(k);
        }
        if active.len() > before {
            active.sort_unstable();
        }
        column.clear();
        column.extend(active.iter().filter_map(|&k| clipped.get(k).map(|f| f.2)));
        runs.clear();
        compose::column(c, &column, &mut edges, &mut runs);
        if runs.len() > cap {
            let rest = runs.get(cap - 1..).unwrap_or_default();
            let (Some(from), Some(to)) = (rest.first(), rest.last()) else {
                continue;
            };
            let merged = Run {
                col: c,
                y0: from.y0,
                y1: to.y1,
                color: transition.unit(),
            };
            out.extend(runs.iter().take(cap - 1).copied());
            out.push(merged);
        } else {
            out.extend(runs.iter().copied());
        }
    }
    out.into_boxed_slice()
}

/// What one BPM graph's frame reads besides its element's.
pub(crate) struct BpmDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The element's prepared pixmap.
    pub(crate) raster: &'a BpmRaster,
    /// The screen timer's elapsed µs, `None` while it is off.
    pub(crate) screen: Option<i64>,
}

impl BpmGraph {
    /// Draws the runs of the columns the reveal shows (`⌊cols · min(1,
    /// t/delay)⌋` at the screen timer's `t`) over the placed rect.
    pub(crate) fn draw(&self, d: &BpmDraw<'_>, rect: [f32; 4], brush: &Brush, out: &mut QuadBatch) {
        let raster = d.raster;
        if raster.runs.is_empty() {
            return;
        }
        let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let delay = self.def.delay.0;
        let upto = if delay <= 0 {
            raster.cols
        } else {
            let t = d.screen.unwrap_or(0).clamp(0, delay);
            let shown = i128::from(raster.cols) * i128::from(t) / i128::from(delay);
            u32::try_from(shown).unwrap_or(raster.cols)
        };
        let region = super::judge_graph::placed(rect, brush);
        let r = Raster {
            region,
            cols: raster.cols,
            rows: raster.rows,
            from_bottom: true,
            pivot: super::judge_graph::drawn_pivot(region, None, brush.pivot),
        };
        r.draw(&raster.runs, upto, brush, f, self.solid.filter, out);
    }
}
