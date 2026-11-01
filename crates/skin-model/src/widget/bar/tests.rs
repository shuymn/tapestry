//! The bar groups' windows and walk against an exhaustive oracle: every
//! bar's quad of every group built and tested by its actual bounds, in
//! chart order then group order.

#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use super::*;
use crate::place::{Affine, LaneMap};
use crate::quad::RunKey;

const FRAME: FrameUv = FrameUv {
    uv: [0.0, 0.0, 1.0, 1.0],
    layer: 1,
};

/// A group's template: `[x, w, h]` at `base`, on `affine`, turned by
/// `angle` about its pivot fractions `pivot`, kept at `native`.
fn template(
    base: f64,
    [x, w, h]: [f32; 3],
    affine: Affine,
    pivot: [f32; 2],
    angle: f32,
    native: Option<Native>,
) -> Template {
    Template {
        brush: Brush {
            key: RunKey {
                blend: Blend::Alpha,
                clip: None,
            },
            affine,
            map: LaneMap::IDENTITY,
            pivot,
            angle,
            color: [1.0; 4],
            native,
            scale: 1.5,
        },
        x,
        w,
        h,
        base,
        frame: FrameUv { layer: 2, ..FRAME },
        filter: Filter::Linear,
    }
}

/// A drawn bar: its bar's index, its group's and its rect's bits.
type Drawn = (usize, usize, [u32; 4]);

/// Every pair of bar and group whose quad meets the band, in chart order
/// then group order.
fn oracle(groups: &[Option<Template>], f: &Field<'_>) -> Vec<Drawn> {
    let mut out = Vec::new();
    for (b, &p) in f.bars.iter().enumerate() {
        for (g, t) in groups.iter().enumerate() {
            let Some(t) = t else { continue };
            let q = t.quad(travel(f.scale, p));
            if meets(extent(&q), f.band) {
                out.push((b, g, q.rect.map(f32::to_bits)));
            }
        }
    }
    out
}

/// The walk's drawn pairs and every candidate it met, stopping after
/// `visits` candidates or `quads` drawn ones.
fn walked(
    groups: &[Option<Template>],
    f: &Field<'_>,
    visits: usize,
    quads: usize,
) -> (Vec<Drawn>, Vec<(usize, usize)>) {
    let mut s = BarScratch::new(groups.len());
    for (g, t) in groups.iter().enumerate() {
        s.set(g, *t, f);
    }
    let (mut drawn, mut met) = (Vec::new(), Vec::new());
    s.walk(f, |c| {
        if met.len() == visits {
            return false;
        }
        met.push((c.bar, c.group));
        if let Some(q) = c.quad {
            if drawn.len() == quads {
                return false;
            }
            drawn.push((c.bar, c.group, q.rect.map(f32::to_bits)));
        }
        true
    });
    (drawn, met)
}

fn affine(scale: [f32; 2], offset: [f32; 2]) -> Affine {
    Affine { scale, offset }
}

/// The groups the oracle draws: tall, displaced far above and below,
/// turned, natively sized, scaled, translated, hidden.
fn groups() -> Vec<Option<Template>> {
    let id = Affine::IDENTITY;
    vec![
        Some(template(
            879.0,
            [160.0, 544.0, 2.0],
            id,
            [0.0, 0.0],
            0.0,
            None,
        )),
        Some(template(
            600.0,
            [160.0, 544.0, 400.0],
            id,
            [0.5, 0.5],
            0.0,
            None,
        )),
        None,
        Some(template(
            -900.0,
            [0.0, 50.0, 10.0],
            id,
            [0.0, 0.0],
            0.0,
            None,
        )),
        Some(template(
            2_400.0,
            [0.0, 50.0, 10.0],
            id,
            [1.0, 1.0],
            0.0,
            None,
        )),
        Some(template(
            879.0,
            [160.0, 544.0, 2.0],
            id,
            [0.5, 0.5],
            1.2,
            None,
        )),
        Some(template(
            879.0,
            [300.0, 4.0, 200.0],
            id,
            [0.0, 1.0],
            4.0,
            None,
        )),
        Some(template(
            879.0,
            [160.0, 544.0, 2.0],
            id,
            [0.5, 0.5],
            0.3,
            Some(Native::Exact([64, 300])),
        )),
        Some(template(
            879.0,
            [160.0, 544.0, 20.0],
            id,
            [0.0, 0.0],
            0.0,
            Some(Native::NoLarger([8, 8])),
        )),
        Some(template(
            440.0,
            [80.0, 272.0, 1.0],
            affine([0.5, 2.0], [10.0, -3.25]),
            [0.0, 0.0],
            0.0,
            None,
        )),
        Some(template(
            -10.0,
            [80.0, 272.0, -6.0],
            affine([1.0, 0.75], [0.0, 400.0]),
            [0.25, 0.75],
            2.5,
            None,
        )),
    ]
}

/// Bar lists with gaps, equal positions and extremes.
fn bar_lists() -> Vec<Vec<Position>> {
    let p = Position;
    vec![
        Vec::new(),
        (0..40).map(|i| p(i * 37 - 500)).collect(),
        vec![p(-3), p(0), p(0), p(0), p(5), p(400), p(401), p(5_000)],
        (0..30).map(|i| p(i * i * 11)).collect(),
        vec![p(i128::MIN), p(-1), p(0), p(1), p(i128::MAX)],
        vec![p(i128::MIN + 7), p(i128::MAX - 7)],
    ]
}

/// Scales: positions per pixel across magnitudes, playheads at and away
/// from 0 and near the ends of the range.
fn scales() -> Vec<Scale> {
    let mut out = Vec::new();
    for at in [0, 3, -250, 1_000, i128::MAX - 2, i128::MIN + 2] {
        for k in [1.0, 0.37, 4.0, 1e-9, 1e12] {
            out.push(Scale {
                at_line: Position(at),
                px_per_pos: k,
                line_y: 880.0,
            });
        }
    }
    out
}

#[test]
fn the_walk_draws_exactly_what_the_oracle_draws() {
    let groups = groups();
    for bars in bar_lists() {
        for scale in scales() {
            for band in [[80.0, 960.0], [0.0, 1_200.0], [500.0, 500.5]] {
                let f = Field {
                    bars: &bars,
                    scale: &scale,
                    band,
                };
                let want = oracle(&groups, &f);
                let (got, met) = walked(&groups, &f, usize::MAX, usize::MAX);
                assert_eq!(got, want, "{scale:?} {band:?} {bars:?}");
                // Candidates in strictly increasing (bar, group) order.
                assert!(met.windows(2).all(|w| w[0] < w[1]), "{met:?}");
                // A hidden group is never a candidate.
                assert!(met.iter().all(|&(_, g)| g != 2));
                // Every budget prefix draws a prefix of the oracle.
                for quads in [0, 1, 2, 5] {
                    let (got, _) = walked(&groups, &f, usize::MAX, quads);
                    assert_eq!(got, want[..quads.min(want.len())]);
                }
                for visits in [0, 1, 3, 17] {
                    let (got, _) = walked(&groups, &f, visits, usize::MAX);
                    assert_eq!(got, want[..got.len()]);
                }
            }
        }
    }
}

#[test]
fn a_quad_merely_touching_the_band_draws_nothing() {
    let t = template(
        78.0,
        [0.0, 10.0, 2.0],
        Affine::IDENTITY,
        [0.0, 0.0],
        0.0,
        None,
    );
    let scale = Scale {
        at_line: Position(0),
        px_per_pos: 1.0,
        line_y: 880.0,
    };
    // Travel 0 ends the quad at 80, the band's top; one pixel down it
    // overlaps by one.
    let bars = [Position(0), Position(-1), Position(-882)];
    let f = Field {
        bars: &bars,
        scale: &scale,
        band: [80.0, 960.0],
    };
    let (got, _) = walked(&[Some(t)], &f, usize::MAX, usize::MAX);
    let drawn: Vec<usize> = got.iter().map(|&(b, ..)| b).collect();
    assert_eq!(drawn, [1]);
}

/// The margin bounds the whole construction: under a region far from
/// its reference, the quad `f32` builds can meet the band though the
/// exact rect does not.
#[test]
fn the_window_covers_rounding_far_from_the_reference() {
    let t = template(
        1_000_000.0,
        [0.0, 10.0, 1.01],
        affine([1.0, 2.0], [0.0, -2_000_000.0]),
        [0.0, 0.0],
        0.0,
        None,
    );
    // Travel −1.03 at 103 positions ahead, past the exact window's last
    // whole position, 101.
    let scale = Scale {
        at_line: Position(0),
        px_per_pos: 0.01,
        line_y: 0.0,
    };
    let bars = [Position(103)];
    let f = Field {
        bars: &bars,
        scale: &scale,
        band: [0.0, 1_200.0],
    };
    // The exact bottom is 2·(1e6 − 1.03 + 1.01) − 2e6 = −0.04; the quad
    // built in `f32` ends near +0.02.
    let q = t.quad(travel(&scale, Position(103)));
    let [_, e1] = extent(&q);
    assert!(e1 > 0.0 && e1 < 0.05, "{e1}");
    let (got, _) = walked(&[Some(t)], &f, usize::MAX, usize::MAX);
    assert_eq!(got.len(), 1);
    assert_eq!(oracle(&[Some(t)], &f), got);
}

#[test]
fn a_group_that_cannot_move_has_no_window() {
    let still = template(
        879.0,
        [0.0, 10.0, 2.0],
        affine([1.0, 0.0], [0.0, 0.0]),
        [0.0, 0.0],
        0.0,
        None,
    );
    let scale = Scale {
        at_line: Position(0),
        px_per_pos: 1.0,
        line_y: 880.0,
    };
    let bars = [Position(0)];
    let f = Field {
        bars: &bars,
        scale: &scale,
        band: [80.0, 960.0],
    };
    assert_eq!(window(&still, &f), None);
}
