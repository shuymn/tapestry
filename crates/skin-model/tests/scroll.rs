//! The lane gain (`Gain`): the range it accepts, compared exactly, and
//! its pixels per position, finite and positive at any terms.

use skin_model::Gain;

/// A gain needs both terms positive and lies within 1/100 to 20.
#[test]
fn gain_refuses_zero_and_out_of_range() {
    for (num, den) in [(0, 1), (1, 0), (0, 0), (1, 101), (2_001, 100), (21, 1)] {
        assert_eq!(Gain::new(num, den), None, "{num}/{den}");
    }
}

#[test]
fn gain_accepts_both_ends() {
    for (num, den) in [(1, 100), (20, 1), (2_000, 100), (u64::MAX, u64::MAX)] {
        let g = Gain::new(num, den).unwrap();
        assert_eq!((g.num(), g.den()), (num, den));
    }
}

/// Terms near `u64::MAX` compare exactly and still give a finite,
/// positive scale.
#[test]
fn gain_with_large_terms_is_finite() {
    let g = Gain::new(u64::MAX - 1, (u64::MAX - 1) / 3).unwrap();
    let beat = (1_u128 << 80) as f64;
    for len in [1.0, 800.0, 1e6] {
        let p = g.px_per_pos(len);
        assert!(p.is_finite() && p > 0.0, "{len}: {p}");
        // H 3 over `len` px: a beat is `3 · len / 4` px.
        assert!((p * beat / (0.75 * len) - 1.0).abs() < 1e-12, "{len}");
    }
    // One unit past either end at the largest terms, and the end itself.
    let (k, h) = (u64::MAX / 20, u64::MAX / 100);
    assert_eq!(Gain::new(20 * k + 1, k), None);
    assert!(Gain::new(20 * k, k).is_some());
    assert_eq!(Gain::new(h - 1, 100 * h), None);
    assert!(Gain::new(h, 100 * h).is_some());
}
