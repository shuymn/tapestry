//! Where each region lands on the canvas this frame: the transform order
//! of a region (lane geometry, scale about its anchor point, translation,
//! anchor shift), the translation clamp, and the bind map of lane-bound
//! elements.
//!
//! **The clamp is a visibility rule.** Each region's logical box (the
//! unrotated union of its elements' key rects, bound envelopes moved with
//! their lanes) keeps an overlap with the canvas of at least
//! `min(extent, MIN_VISIBLE)` on each axis after scale, translation and
//! anchor shift. It does not keep rotated geometry or a pivot on screen;
//! hiding is the `visible` control's job. A range is widened to include 0
//! when the author's own box breaks the rule at the current canvas, so
//! the author's placement is never moved.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{LANE_COUNT, Lane};

use crate::canvas::{Canvas, Fit, f32_of};
use crate::layout::{Adjust, Anchor, Controls, Layout, Region, RegionDef};
use crate::widget::lanes::{Geometry, Lanes, Side};

/// The overlap with the canvas a region's box keeps on each axis, in
/// reference pixels (all of a smaller box).
pub const MIN_VISIBLE: i32 = 64;

/// Where an element sits: its region, the point of its rect it rotates
/// and scales about, its lane binding and its fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub region: Region,
    /// The 9-point pivot inside the sampled rect, whose `(x, y)` is its
    /// top-left corner: the rotation centre.
    pub pivot: Anchor,
    pub bind: Bind,
    pub fill: Fill,
}

impl Place {
    /// In `screen`, pivot at the top left, unbound, no fill.
    pub const SCREEN: Self = Self {
        region: Region::Screen,
        pivot: Anchor::TopLeft,
        bind: Bind::None,
        fill: Fill::None,
    };
}

/// What an element's x and width follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    None,
    /// The lane: authored against its authored rect, mapped to its current
    /// one (scratch side and lane width).
    Lane(Lane),
    /// The span of all lanes, likewise.
    Lanes,
}

/// How a `screen` element meets the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Moved with the screen's anchor.
    None,
    /// Scaled uniformly about the centre until it covers the canvas.
    Cover,
    /// Stretched to the canvas on each axis.
    Stretch,
}

/// `p' = scale · p + offset`, per axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub scale: [f32; 2],
    pub offset: [f32; 2],
}

impl Affine {
    pub const IDENTITY: Self = Self {
        scale: [1.0, 1.0],
        offset: [0.0, 0.0],
    };

    /// The point `p`.
    #[must_use]
    pub fn point(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        let ([sx, sy], [ox, oy]) = (self.scale, self.offset);
        [sx * x + ox, sy * y + oy]
    }

    /// The rect `[x, y, w, h]`.
    #[must_use]
    pub fn rect(&self, [x, y, w, h]: [f32; 4]) -> [f32; 4] {
        let [sx, sy] = self.scale;
        let [x, y] = self.point([x, y]);
        [x, y, sx * w, sy * h]
    }
}

/// One region this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionPlacement {
    /// From reference pixels (after lane geometry) to the canvas.
    pub affine: Affine,
    /// Whether it is drawn.
    pub visible: bool,
    /// Its logical box on the canvas, `[x, y, w, h]`, for outlines; `None`
    /// for a region without elements.
    pub bounds: Option<[f32; 4]>,
}

/// Every region's transform, the fills and the lanes' geometry for one
/// canvas, layout and scratch side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    regions: [RegionPlacement; Region::COUNT],
    cover: Affine,
    stretch: Affine,
    current: Geometry,
    authored: Geometry,
}

/// A box as `[x0, y0, x1, y1]`.
pub(crate) type Bounds = [i64; 4];

/// The union of two boxes.
pub(crate) fn union(a: Option<Bounds>, b: Bounds) -> Bounds {
    a.map_or(b, |[x0, y0, x1, y1]| {
        [x0.min(b[0]), y0.min(b[1]), x1.max(b[2]), y1.max(b[3])]
    })
}

/// What a skin knows of its regions for placing them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Regions {
    pub(crate) fit: Fit,
    pub(crate) defs: [RegionDef; Region::COUNT],
    /// Per region: the box of its unbound elements' keys.
    pub(crate) fixed: [Option<Bounds>; Region::COUNT],
    /// Per region: the box of all its elements at the authored geometry,
    /// whose anchor point is its scale pivot.
    pub(crate) authored: [Option<Bounds>; Region::COUNT],
    /// The keys of the elements bound to each lane, and to the span with
    /// the lanes widget's own rect, as authored.
    pub(crate) lanes: [Option<Bounds>; LANE_COUNT],
    pub(crate) span: Option<Bounds>,
    /// The region the lanes widget sits in.
    pub(crate) lanes_in: Option<Region>,
}

/// `[lo, hi]` translations keeping `[a, b]` overlapping `[0, c]` by
/// `min(b − a, MIN_VISIBLE)`.
fn interval(a: f64, b: f64, c: f64) -> [f64; 2] {
    let m = (b - a).min(f64::from(MIN_VISIBLE));
    [m - b, c - m - a]
}

fn contains_zero([lo, hi]: [f64; 2]) -> bool {
    lo <= 0.0 && 0.0 <= hi
}

/// `v` limited to `[lo, hi]` (`lo ≤ hi`), without `clamp`'s panic.
fn clamp(v: f64, [lo, hi]: [f64; 2]) -> f64 {
    v.max(lo).min(hi)
}

/// One axis of a region before its translation: the box's ends after
/// scale and shift with no translation, and the allowed translations.
#[derive(Clone, Copy)]
struct Axis {
    /// The offset with no translation: `P·(1 − s) + shift`.
    base: f64,
    range: [f64; 2],
}

impl Axis {
    const FREE: [f64; 2] = [f64::NEG_INFINITY, f64::INFINITY];

    /// The axis of a box `[b0, b1]` scaled by `s` about `p`, shifted by
    /// `shift`, on a canvas `c` long; the author's box `[a0, a1]` decides
    /// whether 0 must stay allowed.
    fn new(b: [f64; 2], a: [f64; 2], p: f64, s: f64, shift: f64, c: f64) -> Self {
        let base = p * (1.0 - s) + shift;
        let range = interval(s * b[0] + base, s * b[1] + base, c);
        let author = interval(a[0] + shift, a[1] + shift, c);
        let range = if contains_zero(author) {
            range
        } else {
            [range[0].min(0.0), range[1].max(0.0)]
        };
        Self { base, range }
    }
}

#[expect(clippy::cast_precision_loss, reason = "box edges are within ±2^22")]
fn f(v: i64) -> f64 {
    v as f64
}

impl Regions {
    /// The controls the skin offers for `r` (design: R5's table).
    pub(crate) fn controls(&self, r: Region) -> Controls {
        let has = self.authored.get(r.index()).is_some_and(Option::is_some);
        if !has || self.fit == Fit::Fixed {
            return Controls::default();
        }
        let optional = self.defs.get(r.index()).is_some_and(|d| d.optional);
        match r {
            Region::Playfield => Controls {
                position: true,
                scale: true,
                lane_width: self.lanes_in == Some(Region::Playfield),
                hide: false,
            },
            Region::Judge => Controls {
                position: true,
                scale: true,
                lane_width: false,
                hide: true,
            },
            Region::Left | Region::Right | Region::Top | Region::Bottom => Controls {
                position: true,
                scale: true,
                lane_width: false,
                hide: optional,
            },
            Region::Screen => Controls::default(),
        }
    }

    /// `r`'s adjustment with every control the skin does not offer at its
    /// identity.
    pub(crate) fn effective(&self, r: Region, layout: &Layout) -> Adjust {
        let c = self.controls(r);
        let a = layout.get(r);
        let id = Adjust::IDENTITY;
        Adjust {
            x: if c.position { a.x } else { id.x },
            y: if c.position { a.y } else { id.y },
            scale: if c.scale { a.scale } else { id.scale },
            visible: if c.hide { a.visible } else { id.visible },
        }
    }

    /// The lane width the lanes get, in percent.
    pub(crate) fn lane_width(&self, layout: &Layout) -> u16 {
        if self.controls(Region::Playfield).lane_width {
            layout.lane_width()
        } else {
            100
        }
    }

    fn anchor(&self, r: Region) -> Anchor {
        self.defs
            .get(r.index())
            .map_or(Anchor::TopLeft, |d| d.anchor)
    }

    /// `r`'s logical box at the `current` lane geometry.
    fn current(&self, r: Region, authored: &Geometry, current: &Geometry) -> Option<[f64; 4]> {
        let fixed = self.fixed.get(r.index()).copied().flatten();
        let mut out = fixed.map(|b| b.map(f));
        if self.lanes_in == Some(r) {
            let bound = self
                .lanes
                .iter()
                .zip(authored.lanes.iter().zip(&current.lanes))
                .map(|(b, (a, c))| (*b, *a, *c))
                .chain(std::iter::once((self.span, authored.span, current.span)));
            for (b, a, c) in bound {
                let Some([x0, y0, x1, y1]) = b else {
                    continue;
                };
                let map = |x: i64| c[0] + (f(x) - a[0]) * c[1] / a[1];
                let m = [map(x0), f(y0), map(x1), f(y1)];
                out = Some(out.map_or(m, |o| {
                    [
                        o[0].min(m[0]),
                        o[1].min(m[1]),
                        o[2].max(m[2]),
                        o[3].max(m[3]),
                    ]
                }));
            }
        }
        out
    }

    /// The placement on `canvas` for `layout` and `scratch`, with `lanes`
    /// the skin's lanes. Allocates nothing.
    pub(crate) fn place(
        &self,
        lanes: Option<&Lanes>,
        canvas: &Canvas,
        layout: &Layout,
        scratch: Side,
    ) -> Placement {
        let [cw, ch] = canvas.size().map(f64::from);
        let identity = RegionPlacement {
            affine: Affine::IDENTITY,
            visible: true,
            bounds: None,
        };
        let fx = self.anchor(Region::Playfield).halves()[0];
        let (authored, current) = lanes.map_or((Geometry::NONE, Geometry::NONE), |l| {
            (
                l.authored(),
                l.geometry(scratch, self.lane_width(layout), fx),
            )
        });
        let k = (cw / 1_920.0).max(ch / 1_080.0);
        let mut out = Placement {
            regions: [identity; Region::COUNT],
            cover: Affine {
                scale: [f32_of(k); 2],
                offset: [f32_of(cw / 2.0 - 960.0 * k), f32_of(ch / 2.0 - 540.0 * k)],
            },
            stretch: Affine {
                scale: [f32_of(cw / 1_920.0), f32_of(ch / 1_080.0)],
                offset: [0.0, 0.0],
            },
            current,
            authored,
        };
        if self.fit == Fit::Fixed {
            return out;
        }
        let inset = f64::from(layout.inset()).clamp(0.0, ((cw - 1_920.0) / 2.0).max(0.0));
        let shift = |r: Region| {
            let [fx, fy] = self.anchor(r).fractions().map(f64::from);
            let i = if r == Region::Screen { 0.0 } else { inset };
            [i + fx * (cw - 2.0 * i - 1_920.0), fy * (ch - 1_080.0)]
        };
        let size = [cw, ch];
        // Per region: its box now, its axes and its requested translation.
        let axes = |r: Region| -> Option<([f64; 4], [Axis; 2], f64)> {
            let now = self.current(r, &authored, &current)?;
            let author = self.authored.get(r.index()).copied().flatten()?.map(f);
            let a = self.effective(r, layout);
            let s = f64::from(a.scale) / 100.0;
            let [fx, fy] = self.anchor(r).fractions().map(f64::from);
            let p = [
                author[0] + fx * (author[2] - author[0]),
                author[1] + fy * (author[3] - author[1]),
            ];
            // The judge moves with the playfield's anchor shift.
            let sh = shift(if r == Region::Judge {
                Region::Playfield
            } else {
                r
            });
            let axis = |i: usize, (lo, hi): (usize, usize)| {
                let g = |b: &[f64; 4], j: usize| b.get(j).copied().unwrap_or(0.0);
                Axis::new(
                    [g(&now, lo), g(&now, hi)],
                    [g(&author, lo), g(&author, hi)],
                    p.get(i).copied().unwrap_or(0.0),
                    s,
                    sh.get(i).copied().unwrap_or(0.0),
                    size.get(i).copied().unwrap_or(0.0),
                )
            };
            Some((now, [axis(0, (0, 2)), axis(1, (1, 3))], s))
        };
        // A region whose position the skin does not offer stays put.
        let range_of = |r: Region, ax: &Axis| {
            if self.controls(r).position {
                ax.range
            } else {
                [0.0, 0.0]
            }
        };
        let requested = |r: Region| {
            let a = self.effective(r, layout);
            [f64::from(a.x), f64::from(a.y)]
        };
        let mut set = |r: Region, now: [f64; 4], s: f64, o: [f64; 2], visible: bool| {
            if let Some(slot) = out.regions.get_mut(r.index()) {
                let [x0, y0, x1, y1] = now;
                *slot = RegionPlacement {
                    affine: Affine {
                        scale: [f32_of(s); 2],
                        offset: o.map(f32_of),
                    },
                    visible,
                    bounds: Some([
                        f32_of(s * x0 + o[0]),
                        f32_of(s * y0 + o[1]),
                        f32_of(s * (x1 - x0)),
                        f32_of(s * (y1 - y0)),
                    ]),
                };
            }
        };
        // The playfield and the judge, translated jointly (C14): the
        // judge's own translation first, within what the playfield's
        // range leaves possible, then the playfield's within both.
        let play = axes(Region::Playfield);
        let judge = axes(Region::Judge).filter(|_| self.effective(Region::Judge, layout).visible);
        let mut t_play = [0.0; 2];
        let mut t_judge = [0.0; 2];
        let want_play = requested(Region::Playfield);
        let want_judge = requested(Region::Judge);
        for i in 0..2 {
            let p = play
                .as_ref()
                .and_then(|(_, ax, _)| ax.get(i))
                .map_or([0.0, 0.0], |ax| range_of(Region::Playfield, ax));
            let j = judge
                .as_ref()
                .and_then(|(_, ax, _)| ax.get(i))
                .map_or(Axis::FREE, |ax| ax.range);
            let local = clamp(
                want_judge.get(i).copied().unwrap_or(0.0),
                [j[0] - p[1], j[1] - p[0]],
            );
            let both = [p[0].max(j[0] - local), p[1].min(j[1] - local)];
            let tp = clamp(want_play.get(i).copied().unwrap_or(0.0), both);
            if let (Some(a), Some(b)) = (t_play.get_mut(i), t_judge.get_mut(i)) {
                *a = tp;
                *b = local;
            }
        }
        if let Some((now, [ax, ay], s)) = play {
            let o = [ax.base + t_play[0], ay.base + t_play[1]];
            set(Region::Playfield, now, s, o, true);
        }
        if let Some((now, [ax, ay], s)) = axes(Region::Judge) {
            let o = [
                ax.base + t_judge[0] + t_play[0],
                ay.base + t_judge[1] + t_play[1],
            ];
            let visible = self.effective(Region::Judge, layout).visible;
            set(Region::Judge, now, s, o, visible);
        }
        for r in [Region::Left, Region::Right, Region::Top, Region::Bottom] {
            let Some((now, [ax, ay], s)) = axes(r) else {
                continue;
            };
            let [wx, wy] = requested(r);
            let o = [
                ax.base + clamp(wx, range_of(r, &ax)),
                ay.base + clamp(wy, range_of(r, &ay)),
            ];
            set(r, now, s, o, self.effective(r, layout).visible);
        }
        let screen = shift(Region::Screen);
        if let Some(slot) = out.regions.get_mut(Region::Screen.index()) {
            slot.affine.offset = screen.map(f32_of);
        }
        out
    }
}

impl Placement {
    /// Region `r` this frame.
    #[must_use]
    pub fn region(&self, r: Region) -> RegionPlacement {
        self.regions
            .get(r.index())
            .copied()
            .unwrap_or(RegionPlacement {
                affine: Affine::IDENTITY,
                visible: true,
                bounds: None,
            })
    }

    /// `lane`'s current `(x, w)` before its region's transform: after the
    /// scratch side and the lane width. `(0, 0)` without lanes.
    #[must_use]
    pub fn lane(&self, lane: Lane) -> [f32; 2] {
        self.current
            .lanes
            .get(lane.index())
            .map_or([0.0; 2], |l| l.map(f32_of))
    }

    /// The transform of an element with `fill` in `region`.
    pub(crate) fn affine(&self, region: Region, fill: Fill) -> Affine {
        match fill {
            Fill::None => self.region(region).affine,
            Fill::Cover => self.cover,
            Fill::Stretch => self.stretch,
        }
    }

    /// The lanes' current geometry.
    pub(crate) fn geometry(&self) -> &Geometry {
        &self.current
    }

    /// `x` and `w` of an element bound by `bind`, moved from its authored
    /// lane to the current one: `x' = cx + (x − ax)·cw/aw`, `w' = w·cw/aw`.
    pub(crate) fn bind(&self, bind: Bind, x: f32, w: f32) -> [f32; 2] {
        let pair = match bind {
            Bind::None => return [x, w],
            Bind::Lane(l) => self
                .authored
                .lanes
                .get(l.index())
                .zip(self.current.lanes.get(l.index()))
                .map(|(a, c)| (*a, *c)),
            Bind::Lanes => Some((self.authored.span, self.current.span)),
        };
        let Some(([ax, aw], [cx, cw])) = pair else {
            return [x, w];
        };
        let r = cw / aw;
        [
            f32_of(cx + (f64::from(x) - ax) * r),
            f32_of(f64::from(w) * r),
        ]
    }
}
