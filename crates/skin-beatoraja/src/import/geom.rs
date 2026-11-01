//! The one geometry rule (owner decision 1, D1): every position is an
//! exact rational in reference pixels (1920×1080, y up as the source
//! draws), scaled, offset and stretched without rounding, then reflected
//! to y down and rounded half up once per edge.
//!
//! The source scales at load and offsets and stretches at draw, in `f32`
//! (`SkinObject.java:384-411`, `StretchType.java:16-105` at `0.8.9`); this
//! does the same arithmetic exactly. Every operation is checked: an
//! overflow, or a result outside the model's coordinates, is a range
//! error, never a panic.

use std::cmp::Ordering;

use skin_model::MAX_COORD;

/// The reference height, the axis the source's y counts up from.
pub(crate) const HEIGHT: i128 = 1080;
/// The reference width.
pub(crate) const WIDTH: i128 = 1920;

/// An exact rational: reduced, with a positive denominator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Q {
    n: i128,
    d: i128,
}

/// The greatest common divisor of the magnitudes; `None` when it is
/// `2^127` (both operands `i128::MIN` or zero).
fn gcd(a: i128, b: i128) -> Option<i128> {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    i128::try_from(a).ok()
}

impl Q {
    pub(crate) const ZERO: Self = Self { n: 0, d: 1 };

    pub(crate) fn int(v: impl Into<i128>) -> Self {
        Self { n: v.into(), d: 1 }
    }

    /// `n / d`, reduced; `None` for a zero denominator or an overflow.
    pub(crate) fn new(n: i128, d: i128) -> Option<Self> {
        if d == 0 {
            return None;
        }
        let g = gcd(n, d)?;
        let (mut n, mut d) = (n.checked_div(g)?, d.checked_div(g)?);
        if d < 0 {
            n = n.checked_neg()?;
            d = d.checked_neg()?;
        }
        Some(Self { n, d })
    }

    pub(crate) fn add(self, o: Self) -> Option<Self> {
        let g = gcd(self.d, o.d)?;
        let l = self.d.checked_div(g)?.checked_mul(o.d)?;
        let a = self.n.checked_mul(l.checked_div(self.d)?)?;
        let b = o.n.checked_mul(l.checked_div(o.d)?)?;
        Self::new(a.checked_add(b)?, l)
    }

    pub(crate) fn sub(self, o: Self) -> Option<Self> {
        self.add(Self {
            n: o.n.checked_neg()?,
            d: o.d,
        })
    }

    pub(crate) fn mul(self, o: Self) -> Option<Self> {
        let g1 = gcd(self.n, o.d)?.max(1);
        let g2 = gcd(o.n, self.d)?.max(1);
        let n = self.n.checked_div(g1)?.checked_mul(o.n.checked_div(g2)?)?;
        let d = self.d.checked_div(g2)?.checked_mul(o.d.checked_div(g1)?)?;
        Self::new(n, d)
    }

    /// `self / o`; `None` for `o == 0`.
    pub(crate) fn div(self, o: Self) -> Option<Self> {
        if o.n == 0 {
            return None;
        }
        self.mul(Self::new(o.d, o.n)?)
    }

    pub(crate) fn half(self) -> Option<Self> {
        self.mul(Self { n: 1, d: 2 })
    }

    /// `(k + self) / 10` for a digit `k` and `0 ≤ self < 1`, the next
    /// fraction of a decimal read from its last digit. With `self = n/d`
    /// reduced, the sum is `N / 10d` for `N = kd + n`; `N` and `d` are
    /// coprime, so the only common factor is `g = gcd(N, 10)`, found from
    /// the last digits, and `N / g` is built from `d / g`, `n / g` and
    /// their remainders. No intermediate passes the reduced result.
    fn digit_in(self, k: u8) -> Option<Self> {
        let (n, d, k) = (self.n, self.d, i128::from(k));
        let g = gcd(k.checked_mul(d % 10)?.checked_add(n % 10)?, 10)?;
        let rem = k.checked_mul(d % g)?.checked_add(n % g)?.checked_div(g)?;
        let num = k
            .checked_mul(d.checked_div(g)?)?
            .checked_add(n.checked_div(g)?)?
            .checked_add(rem)?;
        Self::new(num, d.checked_mul(10_i128.checked_div(g)?)?)
    }

    /// A number token as written, exactly: an optional sign, digits with
    /// an optional point, and an optional exponent, as the source's float
    /// fields accept (`20`, `20.0`, `2e1` and `+200E-1` are one value).
    /// `None` for another form, or a value whose reduced numerator or
    /// denominator passes `i128`.
    ///
    /// The exponent moves the point among the digits. The whole part
    /// before it is read from its first digit; the fraction after it from
    /// its last, each step [`Q::digit_in`], so no step's numerator or
    /// denominator passes the fraction's own reduced ones. A point moved
    /// past either end multiplies or divides by ten a step at a time,
    /// which grows a nonzero value or its denominator each step and so
    /// overflows within a few hundred steps however large the exponent.
    pub(crate) fn decimal(raw: &str) -> Option<Self> {
        let (negative, rest) = match raw.as_bytes().first() {
            Some(b'-') => (true, raw.get(1..)?),
            Some(b'+') => (false, raw.get(1..)?),
            _ => (false, raw),
        };
        let (mantissa, exp) = match rest.find(['e', 'E']) {
            Some(i) => (rest.get(..i)?, rest.get(i + 1..)?.parse::<i64>().ok()?),
            None => (rest, 0),
        };
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = || int.bytes().chain(frac.bytes());
        if int.len() + frac.len() == 0 || !digits().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if digits().all(|b| b == b'0') {
            return Some(Self::ZERO);
        }
        let ten = Self::int(10);
        let digit = |b: u8| Some(Self::int(b.checked_sub(b'0')?));
        let len = int.len().checked_add(frac.len())?;
        // The point's place among the digits, and where it splits them.
        let point = i64::try_from(int.len()).ok()?.checked_add(exp)?;
        let split = usize::try_from(point.clamp(0, i64::try_from(len).ok()?)).ok()?;
        let mut whole = Self::ZERO;
        for b in digits().take(split) {
            whole = whole.mul(ten)?.add(digit(b)?)?;
        }
        let mut part = Self::ZERO;
        for i in (split..len).rev() {
            let b = match i.checked_sub(int.len()) {
                Some(j) => frac.as_bytes().get(j),
                None => int.as_bytes().get(i),
            };
            part = part.digit_in(b?.checked_sub(b'0')?)?;
        }
        let len = i64::try_from(len).ok()?;
        for _ in len..point {
            whole = whole.mul(ten)?;
        }
        for _ in point..0 {
            part = part.digit_in(0)?;
        }
        let v = whole.add(part)?;
        if negative { Self::ZERO.sub(v) } else { Some(v) }
    }

    /// The nearest `f32`, for geometry the model keeps unrounded: exact
    /// for the integers and halves of ordinary skins.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "the model keeps this geometry in f32"
    )]
    pub(crate) fn to_f32(self) -> f32 {
        (self.n as f64 / self.d as f64) as f32
    }

    /// The value when it is a whole number.
    pub(crate) fn integer(self) -> Option<i128> {
        (self.d == 1).then_some(self.n)
    }

    /// The nearest integer, halves rounded up: `⌊self + ½⌋`.
    pub(crate) fn round(self) -> Option<i128> {
        let num = self.n.checked_mul(2)?.checked_add(self.d)?;
        Some(num.div_euclid(self.d.checked_mul(2)?))
    }
}

impl PartialOrd for Q {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Q {
    /// Cross-multiplied in i128 with the operands' magnitudes far inside
    /// it (each is bounded by the source's `i32` fields and small
    /// denominators); a product that would overflow compares by sign.
    fn cmp(&self, o: &Self) -> Ordering {
        match (self.n.checked_mul(o.d), o.n.checked_mul(self.d)) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => self.n.signum().cmp(&o.n.signum()),
        }
    }
}

/// A rectangle in reference pixels, y up: `(x, y)` is its bottom-left
/// corner as the source stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: Q,
    pub(crate) y: Q,
    pub(crate) w: Q,
    pub(crate) h: Q,
}

/// The sum of a destination's offsets in reference pixels and degrees
/// (the source adds `x − w/2`, `y − h/2`, `w`, `h` and `r`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Offset {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) w: i64,
    pub(crate) h: i64,
    pub(crate) r: i64,
}

impl Rect {
    /// The source's `(xs, ys, ws, hs)` scaled by `r` into reference
    /// pixels.
    pub(crate) fn scaled(src: [i32; 4], r: Q) -> Option<Self> {
        let [x, y, w, h] = src.map(|v| Q::int(v).mul(r));
        Some(Self {
            x: x?,
            y: y?,
            w: w?,
            h: h?,
        })
    }

    /// With the offsets applied; `relative` objects skip x and y
    /// (`SkinObject.java:403-411`).
    pub(crate) fn offset(self, o: Offset) -> Option<Self> {
        let (ow, oh) = (Q::int(o.w), Q::int(o.h));
        Some(Self {
            x: self.x.add(Q::int(o.x))?.sub(ow.half()?)?,
            y: self.y.add(Q::int(o.y))?.sub(oh.half()?)?,
            w: self.w.add(ow)?,
            h: self.h.add(oh)?,
        })
    }

    /// With the offsets of a relative object applied: only its size
    /// (`SkinObject.java:403-411`).
    pub(crate) fn offset_relative(self, o: Offset) -> Option<Self> {
        Some(Self {
            w: self.w.add(Q::int(o.w))?,
            h: self.h.add(Q::int(o.h))?,
            ..self
        })
    }

    /// The edges reflected to y down and rounded half up: left, top,
    /// right, bottom.
    pub(crate) fn edges(self) -> Option<[i128; 4]> {
        let h = Q::int(HEIGHT);
        let left = self.x.round()?;
        let right = self.x.add(self.w)?.round()?;
        let top = h.sub(self.y.add(self.h)?)?.round()?;
        let bottom = h.sub(self.y)?.round()?;
        Some([left, top, right, bottom])
    }

    /// The model rect `[x, y, w, h]` (y down) from the rounded edges,
    /// every value within ±[`MAX_COORD`].
    pub(crate) fn model(self) -> Option<[i32; 4]> {
        let [l, t, r, b] = self.edges()?;
        coords([l, t, r.checked_sub(l)?, b.checked_sub(t)?])
    }
}

/// Values within ±[`MAX_COORD`] as `i32`.
pub(crate) fn coords(v: [i128; 4]) -> Option<[i32; 4]> {
    let max = i128::from(MAX_COORD);
    let mut out = [0_i32; 4];
    for (o, v) in out.iter_mut().zip(v) {
        if !(-max..=max).contains(&v) {
            return None;
        }
        *o = i32::try_from(v).ok()?;
    }
    Some(out)
}

/// The scale from a skin `src_w` pixels wide to the reference width.
pub(crate) fn scale(src_w: u32) -> Option<Q> {
    Q::new(WIDTH, i128::from(src_w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(n: i128, d: i128) -> Q {
        Q::new(n, d).unwrap()
    }

    #[test]
    fn decimals_are_exact() {
        let dec = Q::decimal;
        for raw in [
            "20",
            "20.0",
            "2e1",
            "+200E-1",
            "0.2e2",
            "20.",
            "000020.000000",
        ] {
            assert_eq!(dec(raw), Some(Q::int(20)), "{raw}");
        }
        assert_eq!(dec("0.1"), Some(q(1, 10)));
        assert_eq!(dec("-.5"), Some(q(-1, 2)));
        assert_eq!(dec("-0"), Some(Q::ZERO));
        assert_eq!(dec("0e999999999999"), Some(Q::ZERO));
        // Two values one `f32` rounds together stay apart.
        assert_ne!(dec("20.000001"), dec("20.0000011"));
        // Trailing zeros far past i128 are only an exponent.
        let long = format!("1{}e-60", "0".repeat(60));
        assert_eq!(dec(&long), Some(Q::int(1)));
        // Digits or a power of ten past i128 whose reduced value fits:
        // 3125 / 10^38 = 1 / (2^5 · 10^33), 3125 / 10^40 = 1 / (2^5 · 10^35).
        let fine = 32 * 10_i128.pow(33);
        assert_eq!(
            dec("20.00000000000000000000000000000000003125"),
            Some(q(20 * fine + 1, fine))
        );
        assert_eq!(dec("-3125e-40"), Some(q(-1, 32 * 10_i128.pow(35))));
        let tiny = format!("0.{}3125e2", "0".repeat(36));
        assert_eq!(dec(&tiny), Some(q(1, fine)));
        assert_eq!(dec("2.5e-38"), Some(q(1, 4 * 10_i128.pow(37))));
        // A fraction from its last digit up, the integer from its first.
        assert_eq!(dec("123.456"), Some(q(15_432, 125)));
        assert_eq!(dec("1234.5e-2"), Some(q(2_469, 200)));
        assert_eq!(dec("0.0012e3"), Some(q(6, 5)));
        // (9 + 2^-124) / 10: a fraction whose last step would pass
        // i128 before its factor of 5 cancels.
        let edge = "0.90000000000000000000000000000000000000470197740328915003187494614888898271127466222708835008603500682511366903781890869140625";
        assert_eq!(
            dec(edge),
            Some(q(
                38_281_766_278_605_577_139_629_643_336_073_923_789,
                1 << 125
            ))
        );
        // Past the rationals after reduction: a numerator, a denominator,
        // an exponent.
        assert_eq!(dec("1e39"), None);
        assert_eq!(dec("1e-39"), None);
        assert_eq!(dec("20.000000000000000000000000000000000000001"), None);
        assert_eq!(dec("2.00000000000000000000000000000000000001"), None);
        assert_eq!(dec("1e-9223372036854775807"), None);
        assert_eq!(dec("1e99999999999999999999"), None);
        assert_eq!(dec("170141183460469231731687303715884105728"), None);
        for bad in ["", ".", "e5", "1e", "1.2.3", "--1", "0x10", "1_0"] {
            assert_eq!(dec(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_digit_step_is_the_reduced_sum() {
        // Every reduced n/d in [0, 1) with d ≤ 400, every digit.
        for d in 1..=400_i128 {
            for n in (0..d).filter(|&n| gcd(n, d) == Some(1) || n == 0) {
                let part = q(n, d);
                for k in 0..=9_u8 {
                    let want = Q::new(i128::from(k) * d + n, 10 * d);
                    assert_eq!(part.digit_in(k), want, "{k} + {n}/{d}");
                }
            }
        }
        // Near the bound: the factor cancels before anything is built.
        let top = 1_i128 << 124;
        assert_eq!(q(1, top).digit_in(9), Some(q(9 * (top / 5) + 2, top * 2)));
        assert_eq!(q(1, top).digit_in(8), None);
    }

    #[test]
    fn rounding_is_half_up() {
        assert_eq!(q(5, 2).round(), Some(3));
        assert_eq!(q(-5, 2).round(), Some(-2));
        assert_eq!(q(-7, 3).round(), Some(-2));
        assert_eq!(q(3233, 3).round(), Some(1078));
    }

    #[test]
    fn arithmetic_reduces_and_overflows_to_none() {
        assert_eq!(q(1, 3).add(q(1, 6)), Some(q(1, 2)));
        assert_eq!(q(2, 3).mul(q(3, 4)), Some(q(1, 2)));
        assert_eq!(q(1, 2).div(Q::ZERO), None);
        assert_eq!(Q::int(i128::MAX).add(Q::int(1)), None);
        assert!(q(1, 3) < q(1, 2));
    }

    #[test]
    fn the_negative_boundary_is_checked() {
        // −2^127 has no positive counterpart.
        assert_eq!(Q::new(i128::MIN, 1), Some(Q::int(i128::MIN)));
        assert_eq!(Q::new(i128::MIN, 2), Some(q(i128::MIN / 2, 1)));
        assert_eq!(Q::new(i128::MIN, -1), None);
        assert_eq!(Q::new(i128::MIN, i128::MIN), None);
        assert_eq!(Q::int(i128::MIN).mul(q(1, 3)), Q::new(i128::MIN, 3));
        assert_eq!(Q::int(i128::MIN).mul(Q::int(-1)), None);
        assert_eq!(Q::int(i128::MIN).add(q(1, 2)), None);
        assert_eq!(Q::int(i128::MIN).round(), None);
    }

    #[test]
    fn edges_reflect_then_round() {
        let r = Rect {
            x: Q::ZERO,
            y: q(4, 3),
            w: Q::int(2),
            h: q(4, 3),
        };
        assert_eq!(r.edges(), Some([0, 1077, 2, 1079]));
        assert_eq!(r.model(), Some([0, 1077, 2, 2]));
    }
}
