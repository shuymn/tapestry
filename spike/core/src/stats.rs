//! Exact percentiles (nearest-rank, no interpolation or sketches).

/// Nearest-rank percentile `pct` (1..=100) of `values`, or `None` if empty.
///
/// The rank is `ceil(pct * n / 100)` in integer arithmetic, so p99 of 100
/// values is the 99th smallest and p99 of 1,000 values is the 990th.
pub fn percentile<T: Copy, F: FnMut(&T, &T) -> std::cmp::Ordering>(
    values: &[T],
    pct: u32,
    cmp: F,
) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    let (_, x, _) = v.select_nth_unstable_by(rank_index(values.len(), pct), cmp);
    Some(*x)
}

/// Index of the nearest-rank percentile `pct` (1..=100) in `n` sorted
/// values (`n > 0`): rank `ceil(pct * n / 100)`, minus one.
pub fn rank_index(n: usize, pct: u32) -> usize {
    assert!((1..=100).contains(&pct) && n > 0);
    (pct as usize * n).div_ceil(100) - 1
}

pub fn percentile_f64(values: &[f64], pct: u32) -> Option<f64> {
    percentile(values, pct, f64::total_cmp)
}

pub fn percentile_i64(values: &[i64], pct: u32) -> Option<i64> {
    percentile(values, pct, Ord::cmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_is_exact() {
        let v: Vec<i64> = (1..=100).rev().collect();
        assert_eq!(percentile_i64(&v, 99), Some(99));
        assert_eq!(percentile_i64(&v, 100), Some(100));
        assert_eq!(percentile_i64(&v, 1), Some(1));
        let v: Vec<i64> = (1..=1000).collect();
        assert_eq!(percentile_i64(&v, 99), Some(990));
        // 101 values: rank ceil(99.99) = 100.
        let v: Vec<i64> = (1..=101).collect();
        assert_eq!(percentile_i64(&v, 99), Some(100));
        assert_eq!(percentile_i64(&[], 99), None);
        assert_eq!(percentile_f64(&[3.0, 1.0, 2.0], 50), Some(2.0));
    }
}
