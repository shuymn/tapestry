//! Tracks (§4.4) and stretch (§4.5): a destination's keys read with the
//! source's inheritance, sorted, accepted or refused, and the stretch
//! modes baked into each key.

use skin_model::{End, MAX_KEY_US};

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
}

/// Reads the keys `ids` of a destination with the source's inheritance
/// (`JSONSkinLoader.java:425-482` at `0.8.9`): the first key's omitted
/// fields default to 0 (255 for colour), a later key's to the previous
/// key's. Colour is clamped per key, as libGDX's `Color` constructor
/// does (`Color.java:88` at 1.9.9).
pub(crate) fn read(def: &mut Definition, ids: &[u32]) -> Result<Keys, ImportError> {
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
    let (mut acc, mut clip) = (0, false);
    for &id in ids {
        let fields = def.fields(id, FIELDS);
        let mut v = [i32::MIN; 15];
        for ((slot, field), name) in v.iter_mut().zip(fields).zip(FIELDS) {
            if let Some(f) = field {
                *slot = def.int(f, name)?;
            }
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
        keys.push(SrcKey {
            at: i64::from(t),
            rect: [x, y, w, h],
            color: [a, r, g, b].map(|c| c.clamp(0, 255)),
            angle,
        });
        prev = Some(v);
    }
    Ok(Keys { keys, acc, clip })
}

/// An accepted track: keys strictly increasing from 0 or later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Accepted {
    pub(crate) keys: Vec<SrcKey>,
    pub(crate) end: End,
}

impl Accepted {
    /// Whether every key shows the same values.
    pub(crate) fn identical(&self) -> bool {
        self.keys.windows(2).all(|w| match w {
            [a, b] => a.same_values(b),
            _ => true,
        })
    }

    /// Whether the colour is the same at every key.
    pub(crate) fn fixed_color(&self) -> bool {
        self.keys.windows(2).all(|w| match w {
            [a, b] => a.color == b.color,
            _ => true,
        })
    }
}

/// Sorts `keys` stably by time and accepts them with loop point `l`
/// (§4.4): `Ok(None)` for a track the source never draws.
///
/// # Errors
///
/// Why the model cannot show the track as the source does.
pub(crate) fn accept(mut keys: Vec<SrcKey>, l: i32) -> Result<Option<Accepted>, TrackWhy> {
    // `SkinObject.java:240-255` inserts before the first later key.
    keys.sort_by_key(|k| k.at);
    let mut out: Vec<SrcKey> = Vec::with_capacity(keys.len());
    for k in keys {
        match out.last() {
            Some(last) if last.at == k.at => {
                if !last.same_values(&k) {
                    return Err(TrackWhy::EqualTime);
                }
            }
            _ => out.push(k),
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
    let single = out.len() == 1;
    // The source's sampler (`SkinObject.java:361-376`).
    let end = if l == -1 {
        End::Hide
    } else if kn == 0 || l == kn {
        End::Hold
    } else if (k0..kn).contains(&l) {
        End::Loop {
            from: bms_model::TimeUs(l * 1000),
        }
    } else if l > kn {
        if first.same_values(last) {
            End::Hold
        } else {
            return Err(TrackWhy::FallbackToFirst);
        }
    } else if single {
        return Ok(None);
    } else if l >= 0 {
        return Err(TrackWhy::Gap);
    } else {
        return Err(TrackWhy::NegativeLoop);
    };
    Ok(Some(Accepted { keys: out, end }))
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
    /// 3, 5, 7, 10: they crop source rows in `f32`.
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

/// The branches a key's stretch can take, a bit per branch; an exact tie
/// takes either.
type Branches = u8;

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

/// Whether every key's branches share one: a piecewise mode is baked only
/// then (the modes are continuous at branch boundaries).
pub(crate) fn one_branch(branches: impl IntoIterator<Item = Branches>) -> bool {
    branches.into_iter().fold(0xFF, |acc, b| acc & b) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(two(5), Err(TrackWhy::Gap));
        assert_eq!(two(-5), Err(TrackWhy::NegativeLoop));
    }

    #[test]
    fn equal_times_keep_one_identical_key() {
        let t = accept(vec![key(0, 0), key(10, 0), key(10, 0)], 0)
            .unwrap()
            .unwrap();
        assert_eq!(t.keys.len(), 2);
        assert_eq!(
            accept(vec![key(0, 0), key(10, 0), key(10, 100)], 0),
            Err(TrackWhy::EqualTime)
        );
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
        assert!(!one_branch([a, b]));
        assert!(one_branch([a, c]) && one_branch([b, c]));
    }
}
