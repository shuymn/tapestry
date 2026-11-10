//! Tracks and stretch: a destination's keys read with the
//! source's inheritance, sorted, accepted or refused, and the stretch
//! modes baked into each key or, for 8 and 9, kept for placement.

use skin_model::{End, MAX_KEY_US, Native};

use super::asset::Axis;
use super::geom::{Q, Rect};
use super::report::TrackWhy;
use crate::error::ImportError;
use crate::resolve::Definition;

/// A key as the source keeps it after inheritance: its time in ms, its
/// rect in source pixels (y up), its colour `[a, r, g, b]` clamped to
/// 0..=255, and its angle in degrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SrcKey {
    pub(crate) at: i64,
    pub(crate) rect: [i32; 4],
    pub(crate) color: [i32; 4],
    pub(crate) angle: i32,
}

impl SrcKey {
    /// A key at 0 drawing nothing of its own: rect 0, white, opaque.
    pub(crate) const NEUTRAL: Self = Self {
        at: 0,
        rect: [0; 4],
        color: [255; 4],
        angle: 0,
    };

    fn same_values(&self, o: &Self) -> bool {
        self.rect == o.rect && self.color == o.color && self.angle == o.angle
    }
}

/// A destination's keys, in source order, with what they set apart from
/// the track.
#[derive(Debug)]
pub(crate) struct Keys {
    pub(crate) keys: Vec<SrcKey>,
    /// The first non-zero `acc` in source order (`SkinObject.java:217`).
    pub(crate) acc: i32,
    /// Some key sets all four clip fields.
    pub(crate) clip: bool,
    /// The first key whose colour was clamped.
    pub(crate) clamped: Option<u32>,
}

/// Reads the keys `ids` of a destination with the source's inheritance
/// (`JSONSkinLoader.java:425-482` at `0.8.9`): the first key's omitted
/// fields default to 0 (255 for colour), a later key's to the previous
/// key's. Colour is clamped per key, as libGDX's `Color` constructor
/// does (`Color.java:88` at 1.9.9), so an imported skin looks as its
/// author saw it rather than being refused.
///
/// With `digits`, as for a judge's combo, whose raw `x` moves left by
/// half its raw `w` times its digits (`JsonPlaySkinObjectLoader.java:
/// 268-270`), a key gives `x` and `w` together or omits both, so the
/// moved `x` it inherits is the moved one; one omitted alone is a range
/// error. The move itself is exact, at placement.
pub(crate) fn read(
    def: &mut Definition,
    ids: &[u32],
    digits: Option<i32>,
) -> Result<Keys, ImportError> {
    const FIELDS: [&str; 15] = [
        "time", "x", "y", "w", "h", "acc", "a", "r", "g", "b", "angle", "clip_x", "clip_y",
        "clip_w", "clip_h",
    ];
    const DEFAULTS: [i32; 15] = [
        0,
        0,
        0,
        0,
        0,
        0,
        255,
        255,
        255,
        255,
        0,
        i32::MIN,
        i32::MIN,
        i32::MIN,
        i32::MIN,
    ];
    let mut keys = Vec::with_capacity(ids.len());
    let mut prev: Option<[i32; 15]> = None;
    let (mut acc, mut clip, mut clamped) = (0, false, None);
    for &id in ids {
        let fields = def.fields(id, FIELDS);
        let mut v = [i32::MIN; 15];
        for ((slot, field), name) in v.iter_mut().zip(fields).zip(FIELDS) {
            if let Some(f) = field {
                *slot = def.int(f, name)?;
            }
        }
        if let (Some(_), [_, x, _, w, ..]) = (digits, &v)
            && (*x == i32::MIN) != (*w == i32::MIN)
        {
            return Err(ImportError::Range {
                at: def.loc(id),
                field: "x",
            });
        }
        for (i, slot) in v.iter_mut().enumerate() {
            if *slot == i32::MIN {
                *slot = match prev {
                    Some(p) => p.get(i).copied().unwrap_or(i32::MIN),
                    None => DEFAULTS.get(i).copied().unwrap_or(i32::MIN),
                };
            }
        }
        let [t, x, y, w, h, a_cc, a, r, g, b, angle, cx, cy, cw, ch] = v;
        if acc == 0 {
            acc = a_cc;
        }
        clip |= [cx, cy, cw, ch].iter().all(|&c| c != i32::MIN);
        let color = [a, r, g, b];
        if clamped.is_none() && color.iter().any(|c| !(0..=255).contains(c)) {
            clamped = Some(id);
        }
        keys.push(SrcKey {
            at: i64::from(t),
            rect: [x, y, w, h],
            color: color.map(|c| c.clamp(0, 255)),
            angle,
        });
        prev = Some(v);
    }
    Ok(Keys {
        keys,
        acc,
        clip,
        clamped,
    })
}

/// An accepted track: keys non-decreasing from 0 or later, at most two
/// at one time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Accepted {
    pub(crate) keys: Vec<SrcKey>,
    pub(crate) end: End,
    /// What stays the same over every key as authored, keys `keys` no
    /// longer holds included.
    fixed: Fixed,
}

/// What stays the same over a list of keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fixed {
    /// The colour, alpha included.
    color: bool,
    /// The rect, the angle and the colour apart from alpha.
    but_alpha: bool,
}

impl Fixed {
    fn of(keys: &[SrcKey]) -> Self {
        let all = |f: fn(&SrcKey, &SrcKey) -> bool| {
            keys.windows(2).all(|w| match w {
                [a, b] => f(a, b),
                _ => true,
            })
        };
        Self {
            color: all(|a, b| a.color == b.color),
            but_alpha: all(|a, b| {
                let ([_, rgb_a @ ..], [_, rgb_b @ ..]) = (a.color, b.color);
                a.rect == b.rect && a.angle == b.angle && rgb_a == rgb_b
            }),
        }
    }
}

impl Accepted {
    /// Whether every key shows the same values.
    pub(crate) fn identical(&self) -> bool {
        // Between them the two cover the rect, the angle and the colour.
        self.fixed.color && self.fixed.but_alpha
    }

    /// Whether the colour is the same at every key.
    pub(crate) fn fixed_color(&self) -> bool {
        self.fixed.color
    }

    /// Whether every key shows the same placement (rect and angle) and
    /// the same colour apart from alpha.
    pub(crate) fn fixed_but_alpha(&self) -> bool {
        self.fixed.but_alpha
    }

    /// Leaves one key at 0: the last, whose rect, colour and angle stand
    /// for the whole track.
    pub(crate) fn keep_last_from_start(&mut self) {
        self.keys.drain(..self.keys.len().saturating_sub(1));
        for k in &mut self.keys {
            k.at = 0;
        }
        self.fixed = Fixed::of(&self.keys);
    }
}

/// Sorts `keys` stably by time and accepts them with loop point `l`:
/// `Ok(None)` for a track the source never draws.
///
/// # Errors
///
/// Why the model cannot show the track as the source does.
pub(crate) fn accept(mut keys: Vec<SrcKey>, l: i32) -> Result<Option<Accepted>, TrackWhy> {
    // `SkinObject.java:240-255` inserts before the first later key.
    keys.sort_by_key(|k| k.at);
    // The source decides what stays fixed over every key it was given
    // (`SkinObject.java:198-216`), the keys pruned below included.
    let fixed = Fixed::of(&keys);
    // Of keys sharing a time, the source draws the last at it and moves
    // toward the first before it (`SkinObject.java:545-575`): the keys
    // between them never draw. A run whose first and last show the same
    // values keeps one.
    let mut out: Vec<SrcKey> = Vec::with_capacity(keys.len());
    let mut iter = keys.into_iter().peekable();
    while let Some(k) = iter.next() {
        let keep = match out.last().filter(|p| p.at == k.at) {
            // The first of its time.
            None => true,
            // The last of its time, unless it shows the first's values.
            Some(first) => iter.peek().is_none_or(|n| n.at != k.at) && !first.same_values(&k),
        };
        if keep {
            out.push(k);
        }
    }
    let (Some(first), Some(last)) = (out.first(), out.last()) else {
        return Ok(None);
    };
    let (k0, kn, l) = (first.at, last.at, i64::from(l));
    if k0 < 0 {
        return Err(TrackWhy::NegativeStart);
    }
    if kn.saturating_mul(1000) > MAX_KEY_US {
        return Err(TrackWhy::Limits);
    }
    // The source's sampler (`SkinObject.java:361-376`).
    let end = if l == -1 {
        End::Hide
    } else if kn == 0 || l > kn {
        // No loop applies with the last key at 0, and none reaches past
        // the last key: after it the source's search finds no segment and
        // shows the first key.
        if first.same_values(last) {
            End::Hold
        } else {
            return Err(TrackWhy::FallbackToFirst);
        }
    } else if l == kn {
        End::Hold
    } else if k0 == kn {
        // Every key at one time after its loop point (`l < kn` here):
        // every wrapped time is before it, so the source never draws it.
        return Ok(None);
    } else {
        // A loop point before the first key, a negative one included,
        // wraps into a gap where nothing draws (`SkinObject.java:358-374`).
        let from = l * 1000;
        if from < -MAX_KEY_US {
            return Err(TrackWhy::Limits);
        }
        End::Loop {
            from: bms_model::TimeUs(from),
        }
    };
    Ok(Some(Accepted {
        keys: out,
        end,
        fixed,
    }))
}

/// A stretch mode (`StretchType.java:16-105`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stretch {
    /// −1, 0 and unknown modes.
    Identity,
    Inner,
    Outer,
    FitWidth,
    FitHeight,
    NoExpanding,
    NoResize,
    /// 3, 5, 7, 10: they crop source columns or rows in `f32`.
    Trimmed,
}

impl Stretch {
    pub(crate) fn of(mode: i32) -> Self {
        match mode {
            1 => Self::Inner,
            2 => Self::Outer,
            4 => Self::FitWidth,
            6 => Self::FitHeight,
            8 => Self::NoExpanding,
            9 => Self::NoResize,
            3 | 5 | 7 | 10 => Self::Trimmed,
            _ => Self::Identity,
        }
    }
}

impl Stretch {
    /// The size a quad of a `cell`-texel frame keeps in target pixels:
    /// stretch 8 and 9 size it after the source scales the destination
    /// to the window (`skin/SkinObject.java:626-629`,
    /// `skin/StretchType.java:85-98`), so the model applies them at
    /// placement instead of baking them into the keys.
    pub(crate) fn native(self, cell: [u32; 2]) -> Option<Native> {
        match self {
            Self::NoExpanding => Some(Native::NoLarger(cell)),
            Self::NoResize => Some(Native::Exact(cell)),
            _ => None,
        }
    }
}

/// The branches a key's stretch can take, a bit per branch; an exact tie
/// takes either.
pub(crate) type Branches = u8;

/// Every branch: where the intersection of the keys' branches starts.
pub(crate) const ANY_BRANCH: Branches = 0xFF;

fn fit_height(r: Rect, h: Q) -> Option<Rect> {
    Some(Rect {
        y: r.y.add(r.h.sub(h)?.half()?)?,
        h,
        ..r
    })
}

fn fit_width(r: Rect, w: Q) -> Option<Rect> {
    Some(Rect {
        x: r.x.add(r.w.sub(w)?.half()?)?,
        w,
        ..r
    })
}

/// `r` stretched against a native frame of `[iw, ih]` texels, and the
/// branches it took; `None` on overflow.
pub(crate) fn stretch(mode: Stretch, r: Rect, [iw, ih]: [u32; 2]) -> Option<(Rect, Branches)> {
    let (iw, ih) = (Q::int(iw), Q::int(ih));
    let sx = r.w.div(iw)?;
    let sy = r.h.div(ih)?;
    let cmp = |a: Q, b: Q| match a.cmp(&b) {
        std::cmp::Ordering::Less => 0b01,
        std::cmp::Ordering::Greater => 0b10,
        std::cmp::Ordering::Equal => 0b11,
    };
    Some(match mode {
        Stretch::Identity | Stretch::Trimmed => (r, 0b1),
        Stretch::Inner | Stretch::Outer => {
            let b = cmp(sx, sy);
            let height_first = if mode == Stretch::Inner {
                b & 0b01 != 0
            } else {
                b & 0b10 != 0
            };
            let out = if height_first {
                fit_height(r, ih.mul(sx)?)?
            } else {
                fit_width(r, iw.mul(sy)?)?
            };
            (out, b)
        }
        Stretch::FitWidth => (fit_height(r, ih.mul(r.w)?.div(iw)?)?, 0b1),
        Stretch::FitHeight => (fit_width(r, iw.mul(r.h)?.div(ih)?)?, 0b1),
        Stretch::NoExpanding => {
            let one = Q::int(1);
            let scale = one.min(sx).min(sy);
            // Which of 1, sx, sy is the minimum, ties counting for each.
            let b = u8::from(scale == one)
                | (u8::from(scale == sx) << 1)
                | (u8::from(scale == sy) << 2);
            let r = fit_width(r, iw.mul(scale)?)?;
            (fit_height(r, ih.mul(scale)?)?, b)
        }
        Stretch::NoResize => (fit_height(fit_width(r, iw)?, ih)?, 0b1),
    })
}

/// A trimmed stretch that maps (`StretchType.java:47-80`); 10, which
/// crops at the frame's native size, has no case and stays unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrimMode {
    /// 3: the larger scale covers the destination.
    Cover,
    /// 5: the width's scale.
    Width,
    /// 7: the height's scale.
    Height,
}

impl TrimMode {
    pub(crate) fn of(mode: i32) -> Option<Self> {
        match mode {
            3 => Some(Self::Cover),
            5 => Some(Self::Width),
            7 => Some(Self::Height),
            _ => None,
        }
    }
}

/// What a trimmed stretch makes of one key's destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Trim {
    /// The scaled frame fits: the destination shrinks and recentres to
    /// it, and the whole frame draws.
    Fit(Rect),
    /// The scaled frame overflows `axis`: the destination stays, and the
    /// frame keeps `kept` texels about its middle on that axis.
    Crop { axis: Axis, kept: Q },
}

/// The texels a frame `[i_len, i_other]` texels long keeps on the axis
/// where the destination is `len` long, when a trimmed stretch takes its
/// scale from the other axis, `other` long, exactly
/// (`skin/StretchType.java:131-153`): it trims when the destination is
/// shorter than the scaled frame, `len < other · i_len / i_other`, and
/// keeps `len · i_other / other` of them. The test is made as
/// `len · i_other < other · i_len`, so a negative or zero `other` takes
/// the source's branch; a zero `other` trims only a negative `len`, which
/// the source cuts by a zero scale: its kept texels are `len`, negative,
/// a crop of nothing. `Some(None)` when nothing is trimmed; `None` on
/// overflow.
pub(crate) fn kept(len: Q, other: Q, [i_len, i_other]: [u32; 2]) -> Option<Option<Q>> {
    let (i_len, i_other) = (Q::int(i_len), Q::int(i_other));
    if len.mul(i_other)? >= other.mul(i_len)? {
        return Some(None);
    }
    if other == Q::ZERO {
        return Some(Some(len));
    }
    Some(Some(len.mul(i_other)?.div(other)?))
}

/// The texels a frame at `at`, `len` long on one axis, keeps when it
/// keeps `kept` texels about its middle, its edges rounded half up as D1
/// rounds where the source truncates in `f32`: the first and the count.
/// `None` when nothing is left or on overflow.
pub(crate) fn crop_middle(at: u32, len: u32, kept: Q) -> Option<[u32; 2]> {
    let first = Q::int(at).add(Q::int(len).sub(kept)?.half()?)?;
    let (start, end) = (first.round()?, first.add(kept)?.round()?);
    let n = u32::try_from(end.checked_sub(start)?)
        .ok()
        .filter(|&n| n > 0)?;
    Some([u32::try_from(start).ok()?, n])
}

/// `r` under trimmed stretch `mode` against a native frame of `[iw, ih]`
/// texels (`skin/StretchType.java:47-80, 119-153`); `None` on overflow.
/// Mode 3 takes the larger scale, the width's on a tie, and so always
/// crops (or meets the destination exactly); 5 and 7 crop when the scaled
/// frame overflows and fit otherwise, an exact meeting fitting.
pub(crate) fn trim(mode: TrimMode, r: Rect, cell: [u32; 2]) -> Option<Trim> {
    let [iw, ih] = cell;
    let rows = |r: Rect| -> Option<Trim> {
        Some(match kept(r.h, r.w, [ih, iw])? {
            Some(kept) => Trim::Crop {
                axis: Axis::Rows,
                kept,
            },
            None => Trim::Fit(fit_height(r, Q::int(ih).mul(r.w)?.div(Q::int(iw))?)?),
        })
    };
    let cols = |r: Rect| -> Option<Trim> {
        Some(match kept(r.w, r.h, [iw, ih])? {
            Some(kept) => Trim::Crop {
                axis: Axis::Cols,
                kept,
            },
            None => Trim::Fit(fit_width(r, Q::int(iw).mul(r.h)?.div(Q::int(ih))?)?),
        })
    };
    match mode {
        // scaleX ≥ scaleY, `r.w / iw ≥ r.h / ih`.
        TrimMode::Cover if r.w.mul(Q::int(ih))? >= r.h.mul(Q::int(iw))? => rows(r),
        TrimMode::Cover | TrimMode::Height => cols(r),
        TrimMode::Width => rows(r),
    }
}

/// Whether the intersection of every key's branches keeps one: a
/// piecewise mode is baked only then (the modes are continuous at branch
/// boundaries).
pub(crate) fn one_branch(shared: Branches) -> bool {
    shared != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chart image's stretch at placement (`skin_model::chart_image`)
    /// meets a destination as an imported image's baked stretch does, for
    /// every mode it keeps, wide and tall images, and boxes of every
    /// aspect; 8 and 9 keep the same native size at placement.
    #[test]
    fn chart_image_stretches_agree_with_the_baked_ones() {
        use skin_model::chart_image::ChartStretch;
        let modes = [
            (0, ChartStretch::Fill),
            (1, ChartStretch::Inner),
            (2, ChartStretch::Outer),
            (4, ChartStretch::FitWidth),
            (6, ChartStretch::FitHeight),
        ];
        // Model boxes, y down. A 2 × 1 or 1 × 2 image in the small boxes
        // puts an edge on a half pixel, which both round half up.
        let boxes = [
            [100, 100, 400, 100],
            [-30, 20, 100, 400],
            [0, 0, 640, 480],
            [5, 7, 333, 333],
            [10, 10, 2, 2],
            [11, 13, 3, 2],
            // A square image in 7 × 8: the exact top is a half pixel that
            // floating division misses.
            [0, 0, 7, 8],
        ];
        let natives = [
            [200, 100],
            [100, 200],
            [640, 480],
            [1, 1],
            [1_920, 1_080],
            [2, 1],
            [1, 2],
            [25, 25],
        ];
        let height = i32::try_from(super::super::geom::HEIGHT).unwrap();
        for (mode, chart) in modes {
            for b in boxes {
                for n in natives {
                    // The importer bakes in the source's y up, then reflects
                    // and rounds the edges into the model's integer rect.
                    let [x, y, w, h] = b;
                    let up = Rect {
                        x: Q::int(x),
                        y: Q::int(height - y - h),
                        w: Q::int(w),
                        h: Q::int(h),
                    };
                    let (baked, _) = stretch(Stretch::of(mode), up, n).unwrap();
                    let baked = baked.model().unwrap().map(|v| v as f32);
                    let (placed, native) = chart.place(b.map(|v| v as f32), n);
                    assert_eq!(native, None);
                    assert_eq!(placed, baked, "mode {mode} {b:?} {n:?}");
                }
            }
        }
        for n in natives {
            let r = [0.0, 0.0, 10.0, 10.0];
            assert_eq!(
                ChartStretch::NoExpanding.place(r, n).1,
                Stretch::of(8).native(n)
            );
            assert_eq!(
                ChartStretch::NoResize.place(r, n).1,
                Stretch::of(9).native(n)
            );
        }
    }

    fn key(at: i64, x: i32) -> SrcKey {
        SrcKey {
            at,
            rect: [x, 0, 1, 1],
            color: [255; 4],
            angle: 0,
        }
    }

    #[test]
    fn end_modes_follow_the_sampler() {
        let one = |at, l| accept(vec![key(at, 0)], l);
        assert_eq!(one(0, -1).unwrap().unwrap().end, End::Hide);
        assert_eq!(one(0, -5).unwrap().unwrap().end, End::Hold);
        assert_eq!(one(0, 5).unwrap().unwrap().end, End::Hold);
        assert_eq!(one(10, 10).unwrap().unwrap().end, End::Hold);
        assert_eq!(one(10, 20).unwrap().unwrap().end, End::Hold);
        assert_eq!(one(10, 5), Ok(None));
        assert_eq!(one(10, -5), Ok(None));
        let two = |l| accept(vec![key(10, 0), key(20, 5)], l);
        assert_eq!(
            two(15).unwrap().unwrap().end,
            End::Loop {
                from: bms_model::TimeUs(15_000)
            }
        );
        assert_eq!(two(30), Err(TrackWhy::FallbackToFirst));
        // A loop point before the first key wraps into a gap the model
        // samples as the source does.
        assert_eq!(
            two(5).unwrap().unwrap().end,
            End::Loop {
                from: bms_model::TimeUs(5_000)
            }
        );
        assert_eq!(
            two(0).unwrap().unwrap().end,
            End::Loop {
                from: bms_model::TimeUs(0)
            }
        );
        // Below −1 the source wraps into negative times too.
        assert_eq!(
            two(-5).unwrap().unwrap().end,
            End::Loop {
                from: bms_model::TimeUs(-5_000)
            }
        );
        let earliest = i32::try_from(MAX_KEY_US / 1000).unwrap();
        assert_eq!(
            two(-earliest).unwrap().unwrap().end,
            End::Loop {
                from: bms_model::TimeUs(-MAX_KEY_US)
            }
        );
        assert_eq!(two(-earliest - 1), Err(TrackWhy::Limits));
    }

    #[test]
    fn equal_times_keep_the_first_and_last_of_a_run() {
        let xs = |t: &Accepted| t.keys.iter().map(|k| (k.at, k.rect[0])).collect::<Vec<_>>();
        let t = accept(vec![key(0, 0), key(10, 0), key(10, 0)], 0)
            .unwrap()
            .unwrap();
        assert_eq!(xs(&t), [(0, 0), (10, 0)]);
        let t = accept(
            vec![key(0, 0), key(10, 1), key(10, 2), key(10, 3), key(20, 4)],
            -1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(xs(&t), [(0, 0), (10, 1), (10, 3), (20, 4)]);
        // A run whose ends agree keeps one, whatever lies between.
        let t = accept(vec![key(10, 1), key(10, 2), key(10, 1)], 10)
            .unwrap()
            .unwrap();
        assert_eq!(xs(&t), [(10, 1)]);
        // Every key at 0: the last shows at 0 and the first after, unless
        // they agree or the track hides.
        let zero = |l| accept(vec![key(0, 0), key(0, 5)], l);
        assert_eq!(zero(0), Err(TrackWhy::FallbackToFirst));
        assert_eq!(zero(-5), Err(TrackWhy::FallbackToFirst));
        assert_eq!(zero(-1).unwrap().unwrap().end, End::Hide);
        // Every key at one later time behaves as a single key there.
        let late = |l| accept(vec![key(10, 0), key(10, 5)], l);
        assert_eq!(late(10).unwrap().unwrap().end, End::Hold);
        assert_eq!(late(5), Ok(None));
        assert_eq!(late(-5), Ok(None));
        assert_eq!(
            accept(vec![key(-10, 0), key(10, 20)], -1),
            Err(TrackWhy::NegativeStart)
        );
    }

    #[test]
    fn piecewise_stretch_records_its_branch() {
        let r = |w: i32, h: i32| Rect {
            x: Q::ZERO,
            y: Q::ZERO,
            w: Q::int(w),
            h: Q::int(h),
        };
        let (_, a) = stretch(Stretch::Inner, r(2, 4), [1, 1]).unwrap();
        let (_, b) = stretch(Stretch::Inner, r(4, 2), [1, 1]).unwrap();
        let (_, c) = stretch(Stretch::Inner, r(3, 3), [1, 1]).unwrap();
        assert!(!one_branch(ANY_BRANCH & a & b));
        assert!(one_branch(ANY_BRANCH & a & c) && one_branch(ANY_BRANCH & b & c));
    }
}
