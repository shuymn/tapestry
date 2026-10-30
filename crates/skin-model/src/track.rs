//! Keyframe tracks: an element's geometry, color and angle over its timer's
//! local time.
//!
//! Key times are integer µs and key values integers; [`Track::sample`]
//! interpolates in `i128` and converts to `f32` only for the fraction left
//! after the exact integer part, so a value at a key is bit-exact.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;

use crate::MAX_ELAPSED_MS;

/// The most keys one track may hold.
pub const MAX_TRACK_KEYS: usize = 256;
/// The latest key time, in µs: the longest a timer reads (4 h).
pub const MAX_KEY_US: i64 = MAX_ELAPSED_MS as i64 * 1000;
/// The largest magnitude of a key's x, y, w, h or angle: every integer up
/// to it is exact in `f32`.
pub const MAX_GEOMETRY: i32 = 1 << 24;

/// The props one key sets, in the index order of [`KeyDef::props`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prop {
    X,
    Y,
    W,
    H,
    Alpha,
    R,
    G,
    B,
    /// Degrees, clockwise.
    Angle,
}

impl Prop {
    /// Every prop, in index order.
    pub const ALL: [Self; 9] = [
        Self::X,
        Self::Y,
        Self::W,
        Self::H,
        Self::Alpha,
        Self::R,
        Self::G,
        Self::B,
        Self::Angle,
    ];

    /// The values a key may give the prop.
    const fn range(self) -> (i32, i32) {
        match self {
            Self::Alpha | Self::R | Self::G | Self::B => (0, 255),
            Self::X | Self::Y | Self::W | Self::H | Self::Angle => (-MAX_GEOMETRY, MAX_GEOMETRY),
        }
    }

    /// The value key 0 takes when it leaves the prop out: geometry and
    /// angle 0, color and alpha 255.
    const fn default(self) -> i32 {
        match self {
            Self::Alpha | Self::R | Self::G | Self::B => 255,
            Self::X | Self::Y | Self::W | Self::H | Self::Angle => 0,
        }
    }
}

/// One key as authored: its time and the props it sets. A prop left out
/// keeps the previous key's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyDef {
    pub at: TimeUs,
    pub props: [Option<i32>; 9],
}

/// How a track moves between keys. `In` and `Out` are quadratic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ease {
    Linear,
    In,
    Out,
    /// Each key's values hold until the next key.
    Step,
}

/// What a track shows after its last key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The last key's values.
    Hold,
    /// Nothing: drawn while `t ≤ kn`.
    Hide,
    /// Loops back to `from` (`k0 ≤ from < kn`): for `t > from`, the track
    /// shows `from + (t − from) mod (kn − from)`, so `kn` already shows
    /// `from`.
    Loop { from: TimeUs },
}

/// A validated track: key times strictly increasing from `k0 ≥ 0`, values
/// densified (every key holds all nine props).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    at: Box<[i64]>,
    v: Box<[[i32; 9]]>,
    ease: Ease,
    end: End,
}

/// One sample of a track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Left, top, width, height.
    pub rect: [f32; 4],
    /// RGBA, each in `[0, 1]`.
    pub color: [f32; 4],
    /// Degrees, clockwise.
    pub angle: f32,
}

/// Why [`Track::new`] refused a track; `index` is a key's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrackError {
    #[error("the track has no keys")]
    NoKeys,
    #[error("the track has {n} keys, more than {MAX_TRACK_KEYS}")]
    TooManyKeys { n: usize },
    #[error("the first key is at {at} µs, before 0")]
    NegativeStart { at: i64 },
    #[error("key {index} is not after the key before it")]
    NotIncreasing { index: usize },
    #[error("key {index} is after {MAX_KEY_US} µs")]
    KeyTooLate { index: usize },
    #[error("the loop point {from} µs is outside [{first}, {last}) µs")]
    LoopOutside { from: i64, first: i64, last: i64 },
    #[error("key {index}'s {prop:?} is out of range")]
    OutOfRange { index: usize, prop: Prop },
}

impl Track {
    /// Validates and densifies `keys` (stage3.md R2): 1 to
    /// [`MAX_TRACK_KEYS`] keys, checked before anything is allocated, at
    /// strictly increasing times from 0 to [`MAX_KEY_US`]; a loop point in
    /// `[k0, kn)`; color and alpha in `0..=255`, geometry and angle within
    /// ±[`MAX_GEOMETRY`].
    ///
    /// # Errors
    ///
    /// The first rule broken, in that order.
    pub fn new(keys: &[KeyDef], ease: Ease, end: End) -> Result<Self, TrackError> {
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return Err(TrackError::NoKeys);
        };
        if keys.len() > MAX_TRACK_KEYS {
            return Err(TrackError::TooManyKeys { n: keys.len() });
        }
        if first.at.0 < 0 {
            return Err(TrackError::NegativeStart { at: first.at.0 });
        }
        for (index, pair) in keys.windows(2).enumerate() {
            if let [a, b] = pair
                && b.at <= a.at
            {
                return Err(TrackError::NotIncreasing { index: index + 1 });
            }
        }
        if let Some(index) = keys.iter().position(|k| k.at.0 > MAX_KEY_US) {
            return Err(TrackError::KeyTooLate { index });
        }
        if let End::Loop { from } = end
            && !(first.at <= from && from < last.at)
        {
            return Err(TrackError::LoopOutside {
                from: from.0,
                first: first.at.0,
                last: last.at.0,
            });
        }
        for (index, key) in keys.iter().enumerate() {
            for (prop, value) in Prop::ALL.into_iter().zip(key.props) {
                let (lo, hi) = prop.range();
                if value.is_some_and(|v| !(lo..=hi).contains(&v)) {
                    return Err(TrackError::OutOfRange { index, prop });
                }
            }
        }
        let mut prev = Prop::ALL.map(Prop::default);
        let v = keys
            .iter()
            .map(|key| {
                for (p, value) in prev.iter_mut().zip(key.props) {
                    if let Some(value) = value {
                        *p = value;
                    }
                }
                prev
            })
            .collect();
        Ok(Self {
            at: keys.iter().map(|k| k.at.0).collect(),
            v,
            ease,
            end,
        })
    }

    /// The number of keys.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.at.len()
    }

    /// Whether any key has an angle other than 0.
    #[must_use]
    pub fn has_rotation(&self) -> bool {
        self.v
            .iter()
            .any(|k| k.get(Prop::Angle as usize).is_some_and(|&a| a != 0))
    }

    /// The track at local time `t_us` (µs since its timer started), or
    /// `None` when it is not drawn: before the first key, or after the last
    /// under [`End::Hide`]. Allocates nothing and never panics.
    #[must_use]
    pub fn sample(&self, t_us: i64) -> Option<Sample> {
        let (&k0, &kn) = (self.at.first()?, self.at.last()?);
        if t_us < k0 {
            return None;
        }
        let t = match self.end {
            End::Hold if t_us >= kn => return self.v.last().map(Self::exact),
            End::Hold => t_us,
            End::Hide if t_us > kn => return None,
            End::Hide => t_us,
            // `from ≥ k0 ≥ 0` and `t_us > from`, so neither subtraction
            // overflows, and `kn − from > 0`.
            End::Loop { from } if t_us > from.0 => from.0 + (t_us - from.0).rem_euclid(kn - from.0),
            End::Loop { .. } => t_us,
        };
        // `t ≥ k0`, so at least one key is at or before it.
        let i = self.at.partition_point(|&k| k <= t).checked_sub(1)?;
        let a = self.v.get(i)?;
        let (Some(&at_a), Some(&at_b), Some(b)) =
            (self.at.get(i), self.at.get(i + 1), self.v.get(i + 1))
        else {
            return Some(Self::exact(a));
        };
        // Keys lie in [0, MAX_KEY_US], so 0 < d < 2^34 and 0 ≤ x < d.
        let d = i128::from(at_b - at_a);
        let x = i128::from(t - at_a);
        let (num, den) = match self.ease {
            Ease::Linear => (x, d),
            Ease::In => (x * x, d * d),
            Ease::Out => (x * (2 * d - x), d * d),
            Ease::Step => return Some(Self::exact(a)),
        };
        let mut out = [0.0; 9];
        for ((o, &va), &vb) in out.iter_mut().zip(a).zip(b) {
            *o = lerp(va, vb, num, den);
        }
        Some(Self::from_values(out))
    }

    /// A key's values, exactly.
    fn exact(v: &[i32; 9]) -> Sample {
        // Every value is within ±2^24, so exact in `f32`.
        Self::from_values(v.map(|x| x as f32))
    }

    fn from_values([x, y, w, h, alpha, r, g, b, angle]: [f32; 9]) -> Sample {
        Sample {
            rect: [x, y, w, h],
            color: [r / 255.0, g / 255.0, b / 255.0, alpha / 255.0],
            angle,
        }
    }
}

/// `a + (b − a) · num / den`, for `0 ≤ num < den`.
///
/// `|b − a| ≤ 2^25` and `num < 2^68`, so `|δ| < 2^93` fits `i128`. The
/// integer part `a + q` lies between `a` and `b`, so it is exact in `f32`;
/// only the fraction `r / den` is rounded.
fn lerp(a: i32, b: i32, num: i128, den: i128) -> f32 {
    if a == b {
        // Most props hold between keys; `a` is within ±2^24, exact in
        // `f32`, and needs no division.
        return a as f32;
    }
    let delta = (i128::from(b) - i128::from(a)) * num;
    let q = delta.div_euclid(den);
    let r = delta - q * den;
    // `a + q` lies between two values within ±2^24, exact in `f32`.
    let whole = (i128::from(a) + q) as f32;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the fraction r / den is in [0, 1); rounding it is the only loss"
    )]
    let frac = (r as f64 / den as f64) as f32;
    whole + frac
}
