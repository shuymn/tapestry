//! Composition of a pixmap's fills into disjoint runs: the play graphs and
//! the hit-error visualizer paint their fills, in order, source-over onto
//! a transparent pixmap the source then draws once under its destination's
//! state. Here each pixel column is cut at its fills' edges and each piece's
//! colour composed by straight-alpha over ([`over`]), so the runs never
//! overlap and the destination's alpha applies once per pixel.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Brush, tint};
use crate::atlas::uv::FrameUv;
use crate::canvas::f32_of;
use crate::pixmap::over;
use crate::quad::{Filter, QuadBatch};

/// One fill of a column: rows `[y0, y1)` in a colour, straight RGBA.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Fill {
    pub(crate) y0: u32,
    pub(crate) y1: u32,
    pub(crate) color: [f32; 4],
}

/// A composed run: rows `[y0, y1)` of column `col`, its colour straight
/// RGBA with alpha above 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Run {
    pub(crate) col: u32,
    pub(crate) y0: u32,
    pub(crate) y1: u32,
    pub(crate) color: [f32; 4],
}

/// Composes column `col`'s `fills`, in paint order, into `out`: the
/// column cut at every fill's edge, each piece the fills over it composed
/// over transparent, transparent pieces left out and equal neighbours
/// joined. `edges` is scratch for the edges, cleared first; nothing grows
/// while it holds `2 · fills.len()` and `out` the runs' room.
pub(crate) fn column(col: u32, fills: &[Fill], edges: &mut Vec<u32>, out: &mut Vec<Run>) {
    edges.clear();
    for f in fills.iter().filter(|f| f.y1 > f.y0) {
        edges.push(f.y0);
        edges.push(f.y1);
    }
    edges.sort_unstable();
    edges.dedup();
    let mut last: Option<Run> = None;
    for pair in edges.windows(2) {
        let [y0, y1] = [pair.first().copied(), pair.get(1).copied()];
        let (Some(y0), Some(y1)) = (y0, y1) else {
            continue;
        };
        let color = fills
            .iter()
            .filter(|f| f.y0 <= y0 && y1 <= f.y1)
            .fold([0.0; 4], |acc, f| over(f.color, acc));
        if color[3] <= 0.0 {
            if let Some(r) = last.take() {
                out.push(r);
            }
            continue;
        }
        last = match last {
            Some(r) if r.y1 == y0 && r.color == color => Some(Run { y1, ..r }),
            Some(r) => {
                out.push(r);
                Some(Run { col, y0, y1, color })
            }
            None => Some(Run { col, y0, y1, color }),
        };
    }
    if let Some(r) = last {
        out.push(r);
    }
}

/// Where a pixmap of `cols` × `rows` texels is drawn: the canvas rect
/// `region`, row 0 at its top, or at its bottom when `from_bottom`, turned
/// with the brush's angle about the canvas point `pivot`.
#[derive(Clone, Copy)]
pub(crate) struct Raster {
    pub(crate) region: [f32; 4],
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    pub(crate) from_bottom: bool,
    pub(crate) pivot: [f32; 2],
}

impl Raster {
    /// Pushes `runs` of columns below `upto` as quads of the white `f`,
    /// each tinted by its colour under the brush's; `false` when the batch
    /// refused one.
    pub(crate) fn draw(
        &self,
        runs: &[Run],
        upto: u32,
        brush: &Brush,
        f: FrameUv,
        filter: Filter,
        out: &mut QuadBatch,
    ) -> bool {
        let [x, y, w, h] = self.region.map(f64::from);
        let (cols, rows) = (f64::from(self.cols.max(1)), f64::from(self.rows.max(1)));
        for r in runs.iter().filter(|r| r.col < upto) {
            let left = x + w * f64::from(r.col) / cols;
            let right = x + w * f64::from(r.col + 1) / cols;
            let (top, bottom) = if self.from_bottom {
                (
                    y + h - h * f64::from(r.y1) / rows,
                    y + h - h * f64::from(r.y0) / rows,
                )
            } else {
                (
                    y + h * f64::from(r.y0) / rows,
                    y + h * f64::from(r.y1) / rows,
                )
            };
            let rect = [left, top, right - left, bottom - top].map(f32_of);
            let b = Brush {
                color: tint(brush.color, r.color),
                ..*brush
            };
            if !b.push_placed(out, rect, self.pivot, f, filter) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{Fill, Run, column};

    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
    const HALF_BLUE: [f32; 4] = [0.0, 0.0, 1.0, 0.5];

    /// Overlapping fills compose source-over into disjoint runs; a
    /// transparent gap is no run, and equal neighbours join.
    #[test]
    fn fills_compose_into_disjoint_runs() {
        let fills = [
            Fill {
                y0: 0,
                y1: 4,
                color: RED,
            },
            Fill {
                y0: 2,
                y1: 6,
                color: HALF_BLUE,
            },
            Fill {
                y0: 8,
                y1: 9,
                color: RED,
            },
            Fill {
                y0: 9,
                y1: 10,
                color: RED,
            },
        ];
        let (mut edges, mut out) = (Vec::new(), Vec::new());
        column(3, &fills, &mut edges, &mut out);
        assert_eq!(
            out,
            [
                Run {
                    col: 3,
                    y0: 0,
                    y1: 2,
                    color: RED
                },
                Run {
                    col: 3,
                    y0: 2,
                    y1: 4,
                    color: [0.5, 0.0, 0.5, 1.0]
                },
                Run {
                    col: 3,
                    y0: 4,
                    y1: 6,
                    color: HALF_BLUE
                },
                Run {
                    col: 3,
                    y0: 8,
                    y1: 10,
                    color: RED
                },
            ]
        );
    }
}
