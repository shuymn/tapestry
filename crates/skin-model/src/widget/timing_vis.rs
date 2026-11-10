//! The play skin's timing visualizer: a band of the judge windows and the
//! ring's offsets as lines over it (`skin/SkinTimingVisualizer.java:56-151`
//! at `0.8.9`; design.md "Rendering and Skins", play visualizers).
//!
//! The background is the source's `2·jw + 1` texels across, drawn under the
//! destination's colour, alpha, blend and angle as disjoint composed runs.
//! Each line is a quad of its own in the line colour at alpha `a·(i+1)/100`,
//! oldest first, unrotated and untinted, under the destination's blend
//! alone, as the source draws it with its own colour and angle 0.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::compose::Run;
use super::{Brush, Frame, ImageRef};
use crate::canvas::f32_of;
use crate::pixmap::{Rgba8, over};
use crate::play::{OFFSET_SLOTS, Offsets};
use crate::quad::QuadBatch;

/// The widest judge range either side of 0, in ms.
pub const MAX_JUDGE_WIDTH: u32 = 1_000;
/// The lines a visualizer draws at most: one per ring slot.
const LINES: u32 = 100;
const _: () = assert!(LINES as usize == OFFSET_SLOTS);
/// The grid lines' colour: black at a quarter.
const GRID: [f32; 4] = [0.0, 0.0, 0.0, 0.25];

/// A timing visualizer as a front end builds it. Its element's track places
/// the band and gives its colour, alpha and angle, which the lines ignore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimingVisualizerDef {
    /// The ms either side of 0 the band spans, 1 to [`MAX_JUDGE_WIDTH`].
    pub judge_width: u32,
    /// The source's `width`, in reference pixels: a ms moves a line
    /// `width / (2·jw + 1)` of them.
    pub width: u32,
    /// The lines' width in reference pixels, 1 to 4.
    pub line_width: u32,
    pub line: Rgba8,
    pub center: Rgba8,
    /// PERFECT, GREAT, GOOD, BAD and the empty POOR range's colours; a
    /// transparent one leaves its band clear.
    pub bands: [Rgba8; 5],
    /// Lines shrink with age, centred.
    pub decay: bool,
    /// A white image the band's runs and the lines tint.
    pub solid: u32,
}

impl TimingVisualizerDef {
    /// The band's texels.
    const fn texels(&self) -> u32 {
        2 * self.judge_width + 1
    }

    /// The grid lines: every 10th texel from `jw % 10`.
    const fn grid_lines(&self) -> u32 {
        (self.texels() - 1 - self.judge_width % 10) / 10 + 1
    }

    /// The quads one frame pushes at worst: the band's runs (11 colour
    /// intervals, each grid line cutting one in two and adding itself),
    /// then a line per slot.
    #[must_use]
    pub const fn quads(&self) -> u32 {
        11 + 2 * self.grid_lines() + LINES
    }

    /// The band's texel runs, each `[y0, y1)` texels across, for the key
    /// judge areas `areas` (none draws
    /// every band clear): the centre texel, each window's added texels
    /// outward in turn, then the grid lines over them, each texel composed
    /// over transparent.
    pub(crate) fn band(&self, areas: Option<[[i16; 2]; 5]>) -> Box<[Run]> {
        let jw = i64::from(self.judge_width);
        let n = self.texels();
        let mut texels = vec![[0.0_f32; 4]; usize::try_from(n).unwrap_or(0)];
        let mut fill = |from: i64, to: i64, color: [f32; 4]| {
            for x in from.max(0)..to.min(i64::from(n)) {
                if let Some(t) = usize::try_from(x).ok().and_then(|x| texels.get_mut(x)) {
                    *t = over(color, *t);
                }
            }
        };
        fill(jw, jw + 1, self.center.unit());
        let (mut left, mut right) = (jw, jw + 1);
        let areas = areas.unwrap_or([[0; 2]; 5]);
        for (area, color) in areas.iter().zip(self.bands) {
            let [lo, hi] = area.map(|v| i64::from(v).clamp(-jw, jw));
            let (x1, x2) = (jw + lo, jw + hi + 1);
            if left > x1 {
                fill(x1, left, color.unit());
                left = x1;
            }
            if x2 > right {
                fill(right, x2, color.unit());
                right = x2;
            }
        }
        let mut x = jw % 10;
        while x < i64::from(n) {
            fill(x, x + 1, GRID);
            x += 10;
        }
        let mut runs: Vec<Run> = Vec::new();
        for (x, &c) in (0_u32..).zip(&texels) {
            if c[3] <= 0.0 {
                continue;
            }
            match runs.last_mut() {
                Some(r) if r.y1 == x && r.color == c => r.y1 = x + 1,
                _ => runs.push(Run {
                    col: 0,
                    y0: x,
                    y1: x + 1,
                    color: c,
                }),
            }
        }
        runs.into_boxed_slice()
    }
}

/// A timing visualizer as a build draws it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TimingVisualizer {
    pub(crate) def: TimingVisualizerDef,
    pub(crate) solid: ImageRef,
}

/// What one timing visualizer's frame reads besides its element's.
pub(crate) struct TimingDraw<'a> {
    pub(crate) at: Frame<'a>,
    /// The band's texel runs, prepared with the attempt.
    pub(crate) band: &'a [Run],
    pub(crate) offsets: Offsets<'a>,
}

impl TimingVisualizer {
    /// Draws the band under the brush, unless its alpha is 0, then every
    /// slot's offset within `±jw`, oldest first.
    pub(crate) fn draw(
        &self,
        d: &TimingDraw<'_>,
        rect: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let Some(f) = self.solid.frame(d.at.starts, d.at.now, d.at.uvs) else {
            return;
        };
        let def = &self.def;
        let filter = self.solid.filter;
        // The source draws nothing of the band at alpha 0.
        if brush.color[3] > 0.0 {
            let [px, py, pw, ph] = super::judge_graph::placed(rect, brush);
            let [fx, fy] = brush.pivot;
            let pivot = [px + fx * pw, py + fy * ph];
            let (x, w) = (f64::from(px), f64::from(pw));
            let n = f64::from(def.texels());
            for run in d.band {
                let left = x + w * f64::from(run.y0) / n;
                let right = x + w * f64::from(run.y1) / n;
                let b = Brush {
                    color: super::tint(brush.color, run.color),
                    ..*brush
                };
                let quad = [f32_of(left), py, f32_of(right - left), ph];
                if !b.push_placed(out, quad, pivot, f, filter) {
                    return;
                }
            }
        }
        let [x, y, w, h] = rect.map(f64::from);
        let lw = f64::from(def.line_width);
        let jw = i64::from(def.judge_width);
        let rate = f64::from(def.width) / f64::from(def.texels());
        let [r, g, b, a] = def.line.unit();
        let slots = u32::try_from(OFFSET_SLOTS).unwrap_or(100);
        for i in 0..slots {
            let Some(v) = d.offsets.back(slots - 1 - i) else {
                continue;
            };
            if !(-jw..=jw).contains(&i64::from(v)) {
                continue;
            }
            let lx = x + (w - lw) / 2.0 + f64::from(v) * rate;
            let k = f64::from(i);
            let n = f64::from(slots);
            let (ly, lh) = if def.decay {
                (y + (h - h * k / n) / 2.0, h * k / n)
            } else {
                (y, h)
            };
            let color = [r, g, b, a / 100.0 * f32_of(k + 1.0)];
            let line = Brush {
                color,
                angle: 0.0,
                native: None,
                ..*brush
            };
            if !line.push(out, [lx, ly, lw, lh].map(f32_of), f, filter) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Rgba8, TimingVisualizerDef};

    fn def(jw: u32) -> TimingVisualizerDef {
        TimingVisualizerDef {
            judge_width: jw,
            width: 301,
            line_width: 1,
            line: Rgba8::rgb(0xff_ffff, 0xff),
            center: Rgba8::rgb(0xff_ffff, 0xff),
            bands: [Rgba8::rgb(0, 0); 5],
            decay: false,
            solid: 0,
        }
    }

    /// 31 grid lines at jw 150, from texel 0; 201 at jw 1 000.
    #[test]
    fn the_grid_counts_its_lines() {
        assert_eq!(def(150).grid_lines(), 31);
        assert_eq!(def(1_000).grid_lines(), 201);
        assert_eq!(def(155).grid_lines(), 31);
    }
}
