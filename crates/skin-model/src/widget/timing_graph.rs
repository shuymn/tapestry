//! The result's timing distribution: a view's deviations by ms within
//! ±150 ms as bars from the bottom, over the judge areas, with ticks
//! along the top and the mean and ±σ lines (design.md "Rendering and
//! Skins", result widgets).
//!
//! The source paints one texture of `gx` columns and `scale` rows and
//! draws it once under the destination's colour. Here the texture is a
//! set of disjoint quads, each tinted with its region's colour (layers
//! composed by [`over`]), so the destination's alpha applies once per
//! pixel: a run of columns holding only an area is one quad; a column
//! holding a tick, a line or a bar is cut at its row boundaries into
//! pieces, each its layers composed. Geometry is in the element's own
//! fractional units: column `j` of `gx` spans `[w·j/gx, w·(j+1)/gx)`, a
//! row is `h/scale` tall from the top, and ms `i` is column `gx/2 + i`.
//!
//! Each piece's layers are composed on every build from the view's areas
//! and statistics and the skin's colours: arithmetic only, allocating
//! nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::judge_graph::{GraphFit, cut, drawn_pivot, fitted_region};
use super::{Brush, Frame, ImageRef, tint};
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::pixmap::{Rgba8, over};
use crate::quad::QuadBatch;
use crate::result::view::{DEVIATION_SLOTS, TimingStats};

/// The ms the deviations reach either side of 0.
const REACH: i32 = 150;
/// The least scale, in rows.
const MIN_SCALE: u32 = 10;
/// The ticks' colour: black at a quarter.
const TICK: [f32; 4] = [0.0, 0.0, 0.0, 0.25];
/// The rows a tick covers from the top.
const TICK_ROWS: u32 = 2;

/// A timing distribution as a front end builds it. Its element's track
/// places the whole graph and gives its colour and alpha; its
/// [`GraphFit`] fits the whole graph as the source fits its texture of
/// `gx` × scale texels, and the track's angle turns the whole graph about
/// the element's pivot in the region it draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingGraphDef {
    /// The columns, one ms each, the centre at `gx / 2`.
    pub gx: u16,
    /// The bars, the average line, the ±σ lines, then the areas PERFECT,
    /// GREAT, GOOD, BAD and POOR.
    pub colors: [Rgba8; 8],
    /// Draw the average line.
    pub average: bool,
    /// Draw the ±σ lines.
    pub dev: bool,
    /// A white image every piece tints.
    pub solid: u32,
    pub fit: GraphFit,
}

impl TimingGraphDef {
    /// The quads one frame can push at worst: with `K` columns holding a
    /// tick, a line or a bar (at most the 301 bars, a tick every 10
    /// columns and 3 lines), each in at most 4 pieces and splitting a run
    /// in two, over at most 11 runs of the areas: `5K + 11`.
    #[must_use]
    pub fn quads(&self) -> u32 {
        let gx = u32::from(self.gx);
        let k = gx.min(304 + gx.div_ceil(10));
        5 * k + 11
    }
}

/// A timing distribution as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TimingGraph {
    pub(crate) def: TimingGraphDef,
    pub(crate) solid: ImageRef,
}

/// What one timing distribution's frame reads besides its element's.
pub(crate) struct TimingGraphDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The view's deviations by ms, `None` without them.
    pub(crate) deviations: Option<&'a [u32; DEVIATION_SLOTS]>,
    /// The view's timing statistics, `None` without a sample.
    pub(crate) timing: Option<TimingStats>,
    /// The view's judge areas, `None` without them.
    pub(crate) areas: Option<[[i16; 2]; 5]>,
    /// Whether the view is ready; nothing draws otherwise.
    pub(crate) ready: bool,
}

/// The scale in rows for the largest slot `largest`: 10, or the next
/// step of 10 above it. It depends on the largest slot alone, never on
/// the slots' order.
pub(crate) fn scale(largest: u32) -> u32 {
    if largest <= MIN_SCALE {
        MIN_SCALE
    } else {
        largest / 10 * 10 + 10
    }
}

/// What one frame of the graph lays out: its columns, centre and scale,
/// the slots, the lines' columns and the areas.
struct Layout<'a> {
    c: i32,
    scale: u32,
    deviations: Option<&'a [u32; DEVIATION_SLOTS]>,
    average: Option<i32>,
    /// The ±σ lines' columns, each drawn.
    dev: Option<[i32; 2]>,
    /// The areas' columns relative to the centre, clamped to `±c`.
    areas: Option<[[i32; 2]; 5]>,
    colors: [[f32; 4]; 8],
}

impl Layout<'_> {
    /// The bar at column `j`: its slot's count, 0 outside ±150 ms.
    fn bar(&self, j: i32) -> u32 {
        let i = j - self.c;
        if !(-REACH..=REACH).contains(&i) {
            return 0;
        }
        let slot = usize::try_from(i + REACH).unwrap_or(usize::MAX);
        self.deviations
            .and_then(|d| d.get(slot).copied())
            .unwrap_or(0)
    }

    /// Whether column `j` holds a tick: every 10 columns from `c mod 10`,
    /// up to `2c`.
    fn tick(&self, j: i32) -> bool {
        j >= self.c % 10 && j <= 2 * self.c && (j - self.c % 10) % 10 == 0
    }

    /// The colour of column `j`'s area, transparent outside them: the
    /// centre PERFECT's, then each area's columns outside those before
    /// it, outward.
    fn area(&self, j: i32) -> [f32; 4] {
        let Some(areas) = self.areas else {
            return [0.0; 4];
        };
        let i = j - self.c;
        let colors = self.colors.get(3..).unwrap_or(&[]);
        let k = if i == 0 {
            Some(0)
        } else {
            areas
                .iter()
                .position(|&[lo, hi]| if i < 0 { lo <= i } else { hi >= i })
        };
        k.and_then(|k| colors.get(k).copied()).unwrap_or([0.0; 4])
    }

    /// Whether column `j` holds anything over its area.
    fn overlaid(&self, j: i32) -> bool {
        self.tick(j)
            || self.average == Some(j)
            || self.dev.is_some_and(|d| d.contains(&j))
            || self.bar(j) > 0
    }

    /// The colour of column `j` over rows `[top, bottom)` (from the top),
    /// which lie wholly inside or outside the tick's rows and the bar's.
    fn color(&self, j: i32, top: u32) -> [f32; 4] {
        let [bars, average, dev, ..] = self.colors;
        let mut c = self.area(j);
        if top < TICK_ROWS && self.tick(j) {
            c = over(TICK, c);
        }
        if self.average == Some(j) {
            c = over(average, c);
        }
        if let Some(d) = self.dev {
            for line in d {
                if line == j {
                    c = over(dev, c);
                }
            }
        }
        let bar = self.bar(j);
        if bar > 0 && top >= self.scale.saturating_sub(bar) {
            c = over(bars, c);
        }
        c
    }
}

/// Mean or σ in hundredths of a ms to whole ms, rounded half up.
fn round_ms(x100: i64) -> i32 {
    i32::try_from((x100 + 50).div_euclid(100)).unwrap_or(i32::MAX)
}

impl TimingGraph {
    /// Draws the graph over the reference rect `rect` through `brush`:
    /// nothing unless the view is ready or without a positive width and
    /// height; the areas and ticks, and with
    /// samples the bars and the lines. The rect is placed (lane map,
    /// affine) and fitted by the graph's [`GraphFit`] as one region, a
    /// trimmed fit cut to the rect; every quad turns with the brush's
    /// angle about the brush's pivot in the region drawn. A refused push
    /// stops the graph.
    pub(crate) fn draw(
        &self,
        d: &TimingGraphDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        if !d.ready {
            return;
        }
        let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let def = &self.def;
        let gx = i32::from(def.gx);
        if gx == 0 {
            return;
        }
        let c = gx / 2;
        // Bars only with samples; the scale from every slot, whatever
        // range the graph shows.
        let deviations = d.deviations.filter(|_| d.timing.is_some());
        let largest = deviations
            .and_then(|s| s.iter().copied().max())
            .unwrap_or(0);
        let scale = scale(largest);
        let average = d.timing.map(|t| c + round_ms(i64::from(t.mean_x100)));
        let dev = d.timing.and_then(|t| {
            let a = c + round_ms(i64::from(t.mean_x100));
            let s = round_ms(i64::from(t.sd_x100));
            def.dev.then_some([a + s, a - s])
        });
        let l = Layout {
            c,
            scale,
            deviations,
            average: average.filter(|_| def.average),
            dev,
            areas: d.areas.map(|a| {
                a.map(|[lo, hi]| [i32::from(lo).clamp(-c, c), i32::from(hi).clamp(-c, c)])
            }),
            colors: def.colors.map(Rgba8::unit),
        };
        let n = u32::from(def.gx);
        let (fitted @ [x, y, w, h], clip) = fitted_region(rect, brush, def.fit, [n, scale]);
        // A region with no positive width and height (a track may reach
        // one, or interpolate through 0 into one) draws nothing.
        if !(w > 0.0 && h > 0.0) {
            return;
        }
        let g = Grid {
            x: f64::from(x),
            y: f64::from(y),
            w: f64::from(w),
            h: f64::from(h),
            gx: f64::from(n),
            scale: f64::from(scale),
            pivot: drawn_pivot(fitted, clip, brush.pivot),
            clip,
            brush,
            f,
            image: self.solid,
        };
        // Runs of columns holding only an area, and cut columns.
        let mut run: Option<(i32, [f32; 4])> = None;
        for j in 0..=gx {
            let overlaid = j < gx && l.overlaid(j);
            let area = if j < gx { l.area(j) } else { [0.0; 4] };
            if let Some((from, color)) = run
                && (j == gx || overlaid || color != area)
            {
                if !g.push([from, j], [0, scale], color, out) {
                    return;
                }
                run = None;
            }
            if j == gx {
                break;
            }
            if overlaid {
                if !g.column(&l, j, out) {
                    return;
                }
            } else if run.is_none() {
                run = Some((j, area));
            }
        }
    }
}

/// The graph's region in canvas pixels and how it divides.
struct Grid<'a> {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    gx: f64,
    scale: f64,
    pivot: [f32; 2],
    /// The rect a trimmed fit cuts every piece to.
    clip: Option<[f32; 4]>,
    brush: &'a Brush,
    f: FrameUv,
    image: ImageRef,
}

impl Grid<'_> {
    /// Pushes columns `[from, to)` over rows `[top, bottom)` (from the
    /// top) in the straight colour `color`; nothing when empty or fully
    /// transparent; `false` when the batch refused it.
    fn push(
        &self,
        [from, to]: [i32; 2],
        [top, bottom]: [u32; 2],
        color: [f32; 4],
        out: &mut QuadBatch,
    ) -> bool {
        let [.., alpha] = color;
        if to <= from || bottom <= top || alpha <= 0.0 {
            return true;
        }
        let col = |j: i32| self.x + self.w * f64::from(j) / self.gx;
        let row = |r: u32| self.y + self.h * f64::from(r) / self.scale;
        let rect = [
            f32_of(col(from)),
            f32_of(row(top)),
            f32_of(col(to) - col(from)),
            f32_of(row(bottom) - row(top)),
        ];
        let b = Brush {
            color: tint(self.brush.color, color),
            ..*self.brush
        };
        let Some((rect, uv)) = cut(rect, self.f.uv, self.clip) else {
            return true;
        };
        let f = FrameUv {
            uv,
            layer: self.f.layer,
        };
        b.push_placed(out, rect, self.pivot, f, self.image.filter)
    }

    /// Pushes column `j` cut at its rows' boundaries (the tick's rows and
    /// the bar's top), each piece its layers composed; `false` when the
    /// batch refused one.
    fn column(&self, l: &Layout<'_>, j: i32, out: &mut QuadBatch) -> bool {
        let scale = l.scale;
        let bar_top = scale.saturating_sub(l.bar(j));
        let mut cuts = [0, TICK_ROWS.min(scale), bar_top, scale];
        cuts.sort_unstable();
        for pair in cuts.windows(2) {
            let &[top, bottom] = pair else {
                continue;
            };
            if bottom > top && !self.push([j, j + 1], [top, bottom], l.color(j, top), out) {
                return false;
            }
        }
        true
    }
}
