//! Floor division and modulo, total: a zero divisor gives 0.
//!
//! These are not `div_euclid` and `rem_euclid`, which differ from floor
//! division for a negative divisor (`7.rem_euclid(-2) == 1`, while the floor
//! modulo is `-1`). The `i64` twins compute the compile-time intervals with
//! the same rounding.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// `a / b` rounded toward negative infinity; 0 when `b == 0`. Wraps only
/// for `i32::MIN / -1`, which the range check refuses at compile time.
pub(crate) fn floor_div(a: i32, b: i32) -> i32 {
    if b == 0 {
        return 0;
    }
    let q = a.wrapping_div(b);
    if a.wrapping_rem(b) != 0 && ((a < 0) != (b < 0)) {
        q.wrapping_sub(1)
    } else {
        q
    }
}

/// `a - b * floor(a / b)`, with the sign of `b`; 0 when `b == 0`. Cannot
/// overflow: `i32::MIN % -1` is 0.
pub(crate) fn floor_mod(a: i32, b: i32) -> i32 {
    if b == 0 {
        return 0;
    }
    let r = a.wrapping_rem(b);
    if r != 0 && ((r < 0) != (b < 0)) {
        r.wrapping_add(b)
    } else {
        r
    }
}

/// [`floor_div`] over `i64`, for interval ends that lie in `i32`.
pub(crate) fn floor_div_wide(a: i64, b: i64) -> i64 {
    if b == 0 {
        return 0;
    }
    let q = a.wrapping_div(b);
    if a.wrapping_rem(b) != 0 && ((a < 0) != (b < 0)) {
        q.wrapping_sub(1)
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_toward_negative_infinity() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(7, -2), -4);
        assert_eq!(floor_div(-7, -2), 3);
        assert_eq!(floor_mod(7, -2), -1);
        assert_eq!(floor_mod(-7, 2), 1);
        assert_eq!(floor_mod(7, 0), 0);
        assert_eq!(floor_div(7, 0), 0);
        assert_eq!(floor_mod(i32::MIN, -1), 0);
        assert_eq!(floor_div_wide(i64::from(i32::MIN), -1), 1 << 31);
        assert_eq!(floor_div_wide(-7, 2), -4);
        assert_eq!(floor_div_wide(-7, 0), 0);
    }
}
