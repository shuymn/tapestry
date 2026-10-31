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

fn gcd(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
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
        let g = gcd(n, d);
        let (mut n, mut d) = (n.checked_div(g)?, d.checked_div(g)?);
        if d < 0 {
            n = n.checked_neg()?;
            d = d.checked_neg()?;
        }
        Some(Self { n, d })
    }

    pub(crate) fn add(self, o: Self) -> Option<Self> {
        let g = gcd(self.d, o.d);
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
        let g1 = gcd(self.n, o.d).max(1);
        let g2 = gcd(o.n, self.d).max(1);
        let n = (self.n / g1).checked_mul(o.n / g2)?;
        let d = (self.d / g2).checked_mul(o.d / g1)?;
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
