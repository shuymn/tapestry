//! The result's judge graph: a view's notes over chart time, one column
//! a timeline bin, stacked in chips from the bottom, each note without a
//! counted judgment grey and the others by their judgment (design.md
//! "Rendering and Skins", result widgets).
//!
//! The source paints two textures and draws each once under the
//! destination's colour: a background of stripes and lines, then the
//! chips, revealed left to right. Here each texture is a set of disjoint
//! quads, each tinted with its region's colour (layers composed by
//! [`over`](crate::pixmap::over)), so the destination's alpha applies once
//! per pixel. Geometry is in the element's own fractional units: bin `i`
//! of `n` spans `[w·i/n, w·(i+1)/n)`, a cell is `h/scale` tall.
//!
//! A chip run is one quad over a generated chip grid
//! ([`SolidImageDef::grid`](crate::SolidImageDef)) whose uv covers the
//! run's cells, so the gaps between chips are the grid's transparent
//! texels.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;

use super::{Brush, Frame, ImageRef, tint};
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::pixmap::{Rgba8, over};
use crate::place::Native;
use crate::quad::QuadBatch;
use crate::result::view::{MAX_TIMELINE_BINS, TIMELINE_CHANNELS, Timelines};

/// The cells a generated chip grid holds down its height: a graph's
/// largest scale.
pub const CHIP_GRID_CELLS: u32 = 100;
/// The least scale, in cells.
const MIN_SCALE: u32 = 20;
/// The background's stripes and lines, at most: ten stripes of up to 53
/// pieces between 52 lines, and the lines.
const BACK_QUADS: u32 = 10 * 53 + 52;
/// The most bins.
#[expect(
    clippy::cast_possible_truncation,
    reason = "MAX_TIMELINE_BINS fits u32, asserted below"
)]
const BINS: u32 = MAX_TIMELINE_BINS as u32;
const _: () = assert!(MAX_TIMELINE_BINS <= u32::MAX as usize);

/// What a judge graph's chips count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeGraphKind {
    /// The source's type 1: by grade, early and late together.
    Judgment,
    /// The source's type 2: PERFECT, then each other grade early, then
    /// late.
    EarlyLate,
}

/// How a judge graph's texture fits its element's rect: the source's
/// stretch modes, applied to the whole graph as the source applies them to
/// an image, the texture being 5 texels a bin across and 5 a cell down.
/// The aspect-keeping modes depend on the bins and the scale, so the graph
/// fits itself each frame. A trimmed mode shows only the part of the
/// fitted graph inside the rect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GraphFit {
    /// The rect (stretch 0, and the source's unknown modes).
    #[default]
    Stretch,
    /// The texture's aspect, inside the rect (1).
    Inner,
    /// The texture's aspect, covering the rect (2; 3 trimmed).
    Outer { trim: bool },
    /// The rect's width, the aspect's height (4; 5 trimmed).
    Width { trim: bool },
    /// The rect's height, the aspect's width (6; 7 trimmed).
    Height { trim: bool },
    /// The texture's size in target pixels, shrunk with its aspect to fit
    /// the rect, never enlarged (8).
    NoLarger,
    /// The texture's size in target pixels (9; 10 trimmed).
    Exact { trim: bool },
}

/// A judge graph as a front end builds it. Its element's track places the
/// whole graph and gives its colour and alpha; its [`GraphFit`] fits the
/// whole graph in that rect, and the track's angle turns the whole graph
/// about the element's pivot in the region it draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JudgeGraphDef {
    pub kind: JudgeGraphKind,
    /// How long the chips take to reveal, left to right, on the screen
    /// timer; not positive shows them at once.
    pub delay: TimeUs,
    /// Stack the channels from the last instead of from the first.
    pub reverse: bool,
    /// Draw the background (the source's `backTexOff` 0).
    pub back: bool,
    /// The chips' image: a white generated grid of [`CHIP_GRID_CELLS`]
    /// cells down, whose gaps are the graph's; sampled at its filter,
    /// [`Filter::NearestBase`](crate::Filter::NearestBase) for crisp gaps.
    pub chips: u32,
    /// A white image the background's stripes and lines tint.
    pub solid: u32,
    pub fit: GraphFit,
}

impl JudgeGraphDef {
    /// The quads one frame can push at worst: the background's, then one
    /// per channel of every bin.
    #[must_use]
    pub fn quads(&self) -> u32 {
        let back = if self.back { BACK_QUADS } else { 0 };
        back + BINS * channels(self.kind)
    }
}

/// The channels a bin stacks: not judged first.
fn channels(kind: JudgeGraphKind) -> u32 {
    match kind {
        JudgeGraphKind::Judgment => 6,
        JudgeGraphKind::EarlyLate => 10,
    }
}

/// Each channel's colour, not judged first (the source's fixed colours).
const JUDGMENT_COLORS: [Rgba8; 6] = [
    Rgba8::rgb(0x55_5555, 0xff),
    Rgba8::rgb(0x00_88ff, 0xff),
    Rgba8::rgb(0x00_ff88, 0xff),
    Rgba8::rgb(0xff_ff00, 0xff),
    Rgba8::rgb(0xff_8800, 0xff),
    Rgba8::rgb(0xff_0000, 0xff),
];
const EARLY_LATE_COLORS: [Rgba8; 10] = [
    Rgba8::rgb(0x55_5555, 0xff),
    Rgba8::rgb(0x44_ff44, 0xff),
    Rgba8::rgb(0x00_88ff, 0xff),
    Rgba8::rgb(0x00_66cc, 0xff),
    Rgba8::rgb(0x00_4488, 0xff),
    Rgba8::rgb(0x00_2244, 0xff),
    Rgba8::rgb(0xff_8800, 0xff),
    Rgba8::rgb(0xcc_6600, 0xff),
    Rgba8::rgb(0x88_4400, 0xff),
    Rgba8::rgb(0x44_2200, 0xff),
];

/// The background's base, below cell 10.
const BASE: Rgba8 = Rgba8::rgb(0, 0xcc);
/// The line every 10th bin, and every 60th.
const LINE: Rgba8 = Rgba8::rgb(0x1f_1f1f, 0xff);
const LINE_60: Rgba8 = Rgba8::rgb(0x3f_3f3f, 0xff);

/// The stripe of cells `10·m` to `10·m + 10`: the base, then an opaque
/// band whose red and green are `⌊0.007 · 10m · 255⌋`.
fn stripe(m: u32) -> Rgba8 {
    if m == 0 {
        return BASE;
    }
    let c = u8::try_from(7 * 10 * m * 255 / 1_000).unwrap_or(u8::MAX);
    Rgba8([c, c, 0, 0xff])
}

/// A judge graph as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JudgeGraph {
    pub(crate) def: JudgeGraphDef,
    pub(crate) chips: ImageRef,
    pub(crate) solid: ImageRef,
}

/// What one judge graph's frame reads besides its element's.
pub(crate) struct GraphDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The view's timelines; nothing draws without them.
    pub(crate) timelines: Option<&'a Timelines>,
    /// The screen timer's elapsed µs, `None` while it is off.
    pub(crate) screen: Option<i64>,
}

/// One bin's counts by the graph's channels: first the notes without a
/// counted judgment (`notes` less the bin's counted judgments, covered
/// bins or not; [`Timelines::new`] refuses more judgments than notes),
/// then the judgments.
fn bin_counts(kind: JudgeGraphKind, b: &[u32; TIMELINE_CHANNELS], notes: u32) -> [u32; 10] {
    let [pg, gr_e, gd_e, bd_e, pr_e, gr_l, gd_l, bd_l, pr_l] = *b;
    let counted = b.iter().fold(0_u32, |s, &c| s.saturating_add(c));
    let grey = notes.saturating_sub(counted);
    match kind {
        JudgeGraphKind::Judgment => [
            grey,
            pg,
            gr_e.saturating_add(gr_l),
            gd_e.saturating_add(gd_l),
            bd_e.saturating_add(bd_l),
            pr_e.saturating_add(pr_l),
            0,
            0,
            0,
            0,
        ],
        JudgeGraphKind::EarlyLate => [grey, pg, gr_e, gd_e, bd_e, pr_e, gr_l, gd_l, bd_l, pr_l],
    }
}

/// The scale in cells for the largest bin count `largest`: 20, or the
/// next step of 10 above it, at most [`CHIP_GRID_CELLS`]. It depends on
/// the largest count alone, never on the bins' order.
pub(crate) fn scale(largest: u32) -> u32 {
    if largest <= MIN_SCALE {
        MIN_SCALE
    } else {
        (largest / 10 * 10 + 10).min(CHIP_GRID_CELLS)
    }
}

impl JudgeGraph {
    /// Draws the graph over the reference rect `rect` through `brush`:
    /// nothing without timelines; the background whole when on, then the
    /// chips revealed up to `w·min(1, t/delay)` of the screen timer's `t`.
    /// The rect is placed (lane map, affine) and fitted by the graph's
    /// [`GraphFit`] as one region, a trimmed fit cut to the rect; every
    /// quad turns with the brush's angle about the brush's pivot in the
    /// region drawn. A refused push stops the graph.
    pub(crate) fn draw(
        &self,
        d: &GraphDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let Some(tl) = d.timelines else {
            return;
        };
        let kind = self.def.kind;
        let bins = tl.bins();
        let notes = tl.notes();
        // A bin's chips, grey included, are its notes: `Timelines::new`
        // refuses more counted judgments than notes.
        let largest = notes.iter().copied().max().unwrap_or(0);
        let scale = scale(largest);
        let n = u32::try_from(bins.len()).unwrap_or(BINS);
        let texels = [n.saturating_mul(5), scale.saturating_mul(5)];
        let (fitted, clip) = fitted_region(rect, brush, self.def.fit, texels);
        let g = Geometry::new(fitted, clip, brush.pivot, n, scale);
        if self.def.back
            && let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs)
            && !self.back(&g, f, brush, out)
        {
            return;
        }
        let Some(f) = self.chips.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let reveal = self.reveal(d.screen);
        let clip = g.x + g.w * reveal;
        let colors: &[Rgba8] = match kind {
            JudgeGraphKind::Judgment => &JUDGMENT_COLORS,
            JudgeGraphKind::EarlyLate => &EARLY_LATE_COLORS,
        };
        let used = colors.len();
        for (i, (b, &notes)) in (0_u32..).zip(bins.iter().zip(notes)) {
            let column @ [left, right] = g.column(i);
            if left >= clip {
                break;
            }
            let counts = bin_counts(kind, b, notes);
            let mut cell = 0_u32;
            for c in 0..used {
                let ch = if self.def.reverse { used - 1 - c } else { c };
                let k = counts
                    .get(ch)
                    .copied()
                    .unwrap_or(0)
                    .min(scale.saturating_sub(cell));
                if k == 0 {
                    continue;
                }
                let color = colors.get(ch).copied().unwrap_or(BASE).unit();
                if !g.chips(
                    column,
                    [cell, k],
                    right.min(clip),
                    f,
                    self.chips,
                    brush,
                    color,
                    out,
                ) {
                    return;
                }
                cell += k;
                if cell >= scale {
                    break;
                }
            }
        }
    }

    /// The chips' revealed fraction of the width at the screen timer's
    /// elapsed µs: all of it without a positive delay, none while the
    /// timer is off.
    fn reveal(&self, screen: Option<i64>) -> f32 {
        let delay = self.def.delay.0;
        if delay <= 0 {
            return 1.0;
        }
        screen.map_or(0.0, |t| f32_of((f64_of(t) / f64_of(delay)).min(1.0)))
    }

    /// The background: each stripe between the lines, then the lines over
    /// the full height; `false` when the batch refused a quad.
    fn back(&self, g: &Geometry, f: FrameUv, brush: &Brush, out: &mut QuadBatch) -> bool {
        let filter = self.solid.filter;
        let lines = g.n.div_ceil(10);
        for m in 0..g.scale / 10 {
            let color = tint(brush.color, stripe(m).unit());
            let [top, height] = g.rows(10 * m, 10);
            let mut from = g.x;
            for l in 0..=lines {
                let [left, right] = if l < lines {
                    g.line(10 * l)
                } else {
                    [g.x + g.w; 2]
                };
                if left > from
                    && !g.push(
                        brush,
                        color,
                        [from, top, left - from, height],
                        f,
                        filter,
                        out,
                    )
                {
                    return false;
                }
                from = right;
            }
        }
        for l in 0..lines {
            let i = 10 * l;
            let line = if i % 60 == 0 { LINE_60 } else { LINE };
            // Opaque over every stripe: the line's own colour.
            let color = tint(brush.color, over(line.unit(), stripe(0).unit()));
            let [left, right] = g.line(i);
            if !g.push(brush, color, [left, g.y, right - left, g.h], f, filter, out) {
                return false;
            }
        }
        true
    }
}

/// The reference `rect` placed: through the brush's lane map and affine.
fn placed(rect: [f32; 4], brush: &Brush) -> [f32; 4] {
    let [x, y, w, h] = rect;
    brush.affine.rect([brush.map.x(x), y, brush.map.w(w), h])
}

/// A result graph's region in canvas pixels: the reference `rect` placed,
/// then fitted to the brush's native size, if any, with the graph's
/// texture of `texels` as the frame (the timing distribution's, D8).
#[allow(
    dead_code,
    reason = "the timing distribution draws through it (S10-09h-1); used or not, so no expect"
)]
pub(crate) fn region(rect: [f32; 4], brush: &Brush, texels: [u32; 2]) -> [f32; 4] {
    let placed = placed(rect, brush);
    brush.native.map_or(placed, |native| {
        native.sized(texels).fit(placed, brush.scale)
    })
}

/// A judge graph's region in canvas pixels, and the rect a trimmed fit
/// cuts it to: the reference `rect` placed, then fitted by `fit` with the
/// graph's texture of `texels` as the image (D9: 5 texels a bin across
/// and 5 a cell down).
fn fitted_region(
    rect: [f32; 4],
    brush: &Brush,
    fit: GraphFit,
    texels: [u32; 2],
) -> ([f32; 4], Option<[f32; 4]>) {
    let placed = placed(rect, brush);
    let [px, py, pw, ph] = placed.map(f64::from);
    let [tw, th] = texels.map(|t| f64::from(t.max(1)));
    let (sx, sy) = (pw / tw, ph / th);
    let width = |nw: f64| [px + (pw - nw) / 2.0, py, nw, ph].map(f32_of);
    let height = |nh: f64| [px, py + (ph - nh) / 2.0, pw, nh].map(f32_of);
    let (fitted, trim) = match fit {
        GraphFit::Stretch => (placed, false),
        GraphFit::Inner if sx <= sy => (height(th * sx), false),
        GraphFit::Inner => (width(tw * sy), false),
        GraphFit::Outer { trim } if sx >= sy => (height(th * sx), trim),
        GraphFit::Outer { trim } => (width(tw * sy), trim),
        GraphFit::Width { trim } => (height(th * sx), trim),
        GraphFit::Height { trim } => (width(tw * sy), trim),
        GraphFit::NoLarger => (Native::NoLarger(texels).fit(placed, brush.scale), false),
        GraphFit::Exact { trim } => (Native::Exact(texels).fit(placed, brush.scale), trim),
    };
    (fitted, trim.then_some(placed))
}

/// The part of the rect `a` inside the rect `b`, `None` when empty.
fn intersect(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let [ax, ay, aw, ah] = a;
    let [bx, by, bw, bh] = b;
    let (x0, y0) = (ax.max(bx), ay.max(by));
    let (x1, y1) = ((ax + aw).min(bx + bw), (ay + ah).min(by + bh));
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// A graph's region in canvas pixels, the rect a trimmed fit cuts it to,
/// its pivot, and its bins and cells.
struct Geometry {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    clip: Option<[f32; 4]>,
    pivot: [f32; 2],
    n: u32,
    scale: u32,
}

fn f64_of(v: i64) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a time far below 2^53 µs")]
    let f = v as f64;
    f
}

impl Geometry {
    /// The region `fitted`, cut to `clip` when trimmed, its pivot at
    /// `[fx, fy]` of the region it draws.
    fn new(
        fitted @ [x, y, w, h]: [f32; 4],
        clip: Option<[f32; 4]>,
        [fx, fy]: [f32; 2],
        n: u32,
        scale: u32,
    ) -> Self {
        let [dx, dy, dw, dh] = clip
            .map_or(Some(fitted), |c| intersect(fitted, c))
            .unwrap_or(fitted);
        Self {
            x,
            y,
            w,
            h,
            clip,
            pivot: [dx + fx * dw, dy + fy * dh],
            n: n.max(1),
            scale: scale.max(1),
        }
    }

    /// Pushes the canvas rect `rect` of the graph in `color`, cut with
    /// its uv to a trimmed fit's rect, turned with the brush's angle about
    /// the graph's pivot; `false` when refused.
    fn push(
        &self,
        brush: &Brush,
        color: [f32; 4],
        rect: [f32; 4],
        f: FrameUv,
        filter: crate::quad::Filter,
        out: &mut QuadBatch,
    ) -> bool {
        let (rect, f) = match self.clip {
            None => (rect, f),
            Some(clip) => {
                let Some(cut @ [cx, cy, cw, ch]) = intersect(rect, clip) else {
                    return true;
                };
                let [x, y, w, h] = rect;
                let [l, t, r, b] = f.uv;
                let u = |at: f32| l + (r - l) * (at - x) / w;
                let v = |at: f32| t + (b - t) * (at - y) / h;
                let uv = [u(cx), v(cy), u(cx + cw), v(cy + ch)];
                (cut, FrameUv { uv, layer: f.layer })
            }
        };
        let b = Brush { color, ..*brush };
        b.push_placed(out, rect, self.pivot, f, filter)
    }

    /// Bin `i`'s left and right edges.
    fn column(&self, i: u32) -> [f32; 2] {
        let at = |i: u32| {
            f32_of(f64::from(self.x) + f64::from(self.w) * f64::from(i) / f64::from(self.n))
        };
        [at(i), at(i + 1)]
    }

    /// The line at bin `i`'s left: a fifth of the bin wide, as the
    /// source's one texel of the bin's five.
    fn line(&self, i: u32) -> [f32; 2] {
        let [left, right] = self.column(i);
        [left, left + (right - left) / 5.0]
    }

    /// The top and height of `k` cells from cell `from` up.
    fn rows(&self, from: u32, k: u32) -> [f32; 2] {
        let cell = f64::from(self.h) / f64::from(self.scale);
        let bottom = f64::from(self.y) + f64::from(self.h);
        let top = bottom - cell * f64::from(from + k);
        [f32_of(top), f32_of(cell * f64::from(k))]
    }

    /// Pushes the run of `k` cells from cell `from` up of the bin whose
    /// column is `[left, bin_right)`, drawn up to `right` (the reveal's
    /// cut), its uv the grid's bottom `k` cells, cut alike; `false` when
    /// refused.
    #[expect(clippy::too_many_arguments, reason = "one run's parts")]
    fn chips(
        &self,
        [left, bin_right]: [f32; 2],
        [from, k]: [u32; 2],
        right: f32,
        f: FrameUv,
        image: ImageRef,
        brush: &Brush,
        color: [f32; 4],
        out: &mut QuadBatch,
    ) -> bool {
        if right <= left || bin_right <= left {
            return true;
        }
        let [l, t, r, b] = f.uv;
        let cut = (right - left) / (bin_right - left);
        let fk = f32_of(f64::from(k) / f64::from(CHIP_GRID_CELLS));
        let uv = FrameUv {
            uv: [l, b - (b - t) * fk, l + (r - l) * cut, b],
            layer: f.layer,
        };
        let [top, height] = self.rows(from, k);
        let color = tint(brush.color, color);
        self.push(
            brush,
            color,
            [left, top, right - left, height],
            uv,
            image.filter,
            out,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{BASE, Brush, LINE, LINE_60, Rgba8, region, scale, stripe};
    use crate::place::{Affine, LaneMap, Native};
    use crate::quad::{Blend, RunKey};

    /// The shared region fits a graph to the brush's native size with the
    /// texture's own dimensions: a 301 × 30 texture (a distribution's)
    /// kept 301 × 30 in a 600 × 100 rect, centred; none keeps the rect.
    #[test]
    fn the_shared_region_fits_the_actual_texture() {
        let brush = |native| Brush {
            key: RunKey {
                blend: Blend::Alpha,
                clip: None,
            },
            affine: Affine::IDENTITY,
            map: LaneMap::IDENTITY,
            pivot: [0.0; 2],
            angle: 0.0,
            color: [1.0; 4],
            native,
            scale: 1.0,
        };
        let rect = [100.0, 50.0, 600.0, 100.0];
        let exact = region(rect, &brush(Some(Native::Exact([0, 0]))), [301, 30]);
        assert_eq!(exact, [249.5, 85.0, 301.0, 30.0]);
        assert_eq!(region(rect, &brush(None), [301, 30]), rect);
    }

    /// The source's background colours: the base, then bands `111100`,
    /// `232300`, …; lines `1f1f1f` and `3f3f3f`.
    #[test]
    fn background_colours_are_the_sources() {
        assert_eq!(BASE, Rgba8([0, 0, 0, 0xcc]));
        assert_eq!(stripe(0), BASE);
        assert_eq!(stripe(1), Rgba8([0x11, 0x11, 0, 0xff]));
        assert_eq!(stripe(2), Rgba8([0x23, 0x23, 0, 0xff]));
        assert_eq!(stripe(9), Rgba8([0xa0, 0xa0, 0, 0xff]));
        assert_eq!(
            (LINE.0, LINE_60.0),
            ([0x1f, 0x1f, 0x1f, 0xff], [0x3f, 0x3f, 0x3f, 0xff])
        );
    }

    /// 20 cells, or the next step of 10 above the largest count, at most
    /// 100.
    #[test]
    fn the_scale_steps_by_ten() {
        let got: Vec<u32> = [0, 20, 21, 29, 30, 95, 99, 100, 113, 150]
            .into_iter()
            .map(scale)
            .collect();
        assert_eq!(got, [20, 20, 30, 30, 40, 100, 100, 100, 100, 100]);
    }
}
