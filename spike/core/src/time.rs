//! QPC tick conversion.

/// Converts a raw QPC value to µs relative to `origin` (floor division).
///
/// Multiplies before dividing in `i128`, so there is neither overflow nor the
/// precision loss of converting through `f64`.
pub fn qpc_to_us(qpc: i64, origin: i64, freq: i64) -> i64 {
    assert!(freq > 0, "QPC frequency must be positive");
    let ticks = i128::from(qpc) - i128::from(origin);
    (ticks * 1_000_000).div_euclid(i128::from(freq)) as i64
}

/// Converts a millisecond value printed by PresentMon to whole µs (rounded).
pub fn ms_to_us(ms: f64) -> i64 {
    (ms * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpc_conversion_floors_and_handles_negative_offsets() {
        assert_eq!(qpc_to_us(10_000_000, 0, 10_000_000), 1_000_000);
        assert_eq!(qpc_to_us(15, 0, 10_000_000), 1);
        assert_eq!(qpc_to_us(-1, 0, 10_000_000), -1);
        // Large values that overflow i64 if multiplied first in i64.
        let big = 9_000_000_000_000_000;
        assert_eq!(qpc_to_us(big, big - 3_000_000, 3_000_000), 1_000_000);
    }
}
