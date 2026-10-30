//! Closed integer intervals over `i64`.
//!
//! Every operand interval has been checked to lie in `i32`, so each
//! function below computes in `i64` without overflow: a product of two
//! `i32` values, and any sum or difference of two, fits.

use super::arith::floor_div_wide;

/// The values `[lo, hi]` an integer node can take, `lo <= hi`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Iv {
    pub(super) lo: i64,
    pub(super) hi: i64,
}

impl Iv {
    pub(super) fn new(lo: impl Into<i64>, hi: impl Into<i64>) -> Self {
        Self {
            lo: lo.into(),
            hi: hi.into(),
        }
    }

    pub(super) fn point(v: i32) -> Self {
        Self::new(v, v)
    }

    pub(super) fn in_i32(self) -> bool {
        self.lo >= i64::from(i32::MIN) && self.hi <= i64::from(i32::MAX)
    }

    pub(super) fn hull(self, o: Self) -> Self {
        Self::new(self.lo.min(o.lo), self.hi.max(o.hi))
    }

    fn contains(self, v: i64) -> bool {
        self.lo <= v && v <= self.hi
    }

    /// The hull of `f` over every pair of ends.
    fn corners(self, o: Self, f: impl Fn(i64, i64) -> i64) -> Self {
        let vs = [
            f(self.lo, o.lo),
            f(self.lo, o.hi),
            f(self.hi, o.lo),
            f(self.hi, o.hi),
        ];
        let lo = vs.iter().copied().min().unwrap_or(0);
        let hi = vs.iter().copied().max().unwrap_or(0);
        Self::new(lo, hi)
    }

    pub(super) fn neg(self) -> Self {
        Self::new(-self.hi, -self.lo)
    }

    pub(super) fn add(self, o: Self) -> Self {
        Self::new(self.lo + o.lo, self.hi + o.hi)
    }

    pub(super) fn sub(self, o: Self) -> Self {
        Self::new(self.lo - o.hi, self.hi - o.lo)
    }

    pub(super) fn mul(self, o: Self) -> Self {
        self.corners(o, |a, b| a * b)
    }

    /// The divisor's negative and positive parts.
    fn sign_parts(self) -> [Option<Self>; 2] {
        let neg = (self.lo <= -1).then(|| Self::new(self.lo, self.hi.min(-1)));
        let pos = (self.hi >= 1).then(|| Self::new(self.lo.max(1), self.hi));
        [neg, pos]
    }

    /// Floor division. Over one sign part of the divisor, `floor(a / b)` is
    /// monotone in `a` and in `b`, so its extremes lie at the corners; a
    /// zero divisor adds 0.
    pub(super) fn div(self, o: Self) -> Self {
        let zero = o.contains(0).then(|| Self::point(0));
        o.sign_parts()
            .into_iter()
            .flatten()
            .map(|part| self.corners(part, floor_div_wide))
            .chain(zero)
            .reduce(Self::hull)
            .unwrap_or(Self::point(0))
    }

    /// Floor modulo, which takes the divisor's sign: a positive part
    /// `[p, q]` gives `[0, q - 1]` (at most `a` when `a >= 0`), a negative
    /// one `[p + 1, 0]` (at least `a` when `a <= 0`); a zero divisor adds 0.
    pub(super) fn rem(self, o: Self) -> Self {
        let [neg, pos] = o.sign_parts();
        let neg = neg.map(|p| {
            let lo = if self.hi <= 0 {
                (p.lo + 1).max(self.lo)
            } else {
                p.lo + 1
            };
            Self::new(lo, 0)
        });
        let pos = pos.map(|p| {
            let hi = if self.lo >= 0 {
                (p.hi - 1).min(self.hi)
            } else {
                p.hi - 1
            };
            Self::new(0, hi)
        });
        let zero = o.contains(0).then(|| Self::point(0));
        [neg, pos, zero]
            .into_iter()
            .flatten()
            .reduce(Self::hull)
            .unwrap_or(Self::point(0))
    }

    pub(super) fn abs(self) -> Self {
        if self.lo >= 0 {
            self
        } else if self.hi <= 0 {
            self.neg()
        } else {
            Self::new(0, (-self.lo).max(self.hi))
        }
    }

    pub(super) fn min(self, o: Self) -> Self {
        Self::new(self.lo.min(o.lo), self.hi.min(o.hi))
    }

    pub(super) fn max(self, o: Self) -> Self {
        Self::new(self.lo.max(o.lo), self.hi.max(o.hi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(lo: i64, hi: i64) -> Iv {
        Iv::new(lo, hi)
    }

    /// Every value `f` takes over the two (small) intervals.
    fn brute(a: Iv, b: Iv, f: impl Fn(i64, i64) -> i64) -> Iv {
        let mut out: Option<Iv> = None;
        for x in a.lo..=a.hi {
            for y in b.lo..=b.hi {
                let v = Iv::new(f(x, y), f(x, y));
                out = Some(out.map_or(v, |o| o.hull(v)));
            }
        }
        out.unwrap()
    }

    fn floor_mod_wide(a: i64, b: i64) -> i64 {
        if b == 0 {
            0
        } else {
            a - b * floor_div_wide(a, b)
        }
    }

    #[test]
    fn div_and_rem_are_exact_hulls_on_small_intervals() {
        let ends = [-4, -1, 0, 1, 3];
        for alo in ends {
            for ahi in ends.into_iter().filter(|&h| h >= alo) {
                for blo in ends {
                    for bhi in ends.into_iter().filter(|&h| h >= blo) {
                        let (a, b) = (iv(alo, ahi), iv(blo, bhi));
                        assert_eq!(a.div(b), brute(a, b, floor_div_wide), "{a:?} / {b:?}");
                        let r = a.rem(b);
                        let exact = brute(a, b, floor_mod_wide);
                        assert!(
                            r.lo <= exact.lo && exact.hi <= r.hi,
                            "{a:?} % {b:?}: {r:?} does not cover {exact:?}"
                        );
                        assert_eq!(a.mul(b), brute(a, b, |x, y| x * y));
                        assert_eq!(a.abs(), brute(a, a, |x, _| x.abs()));
                    }
                }
            }
        }
    }
}
