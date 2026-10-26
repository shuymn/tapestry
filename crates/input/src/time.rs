//! QPC ticks to µs, and the input thread's wake interval distribution.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;

/// A QPC frequency in ticks per second; always positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QpcFrequency(i64);

impl QpcFrequency {
    /// The frequency `hz`, or `None` unless it is positive.
    #[must_use]
    pub const fn new(hz: i64) -> Option<Self> {
        if hz > 0 { Some(Self(hz)) } else { None }
    }

    /// Ticks per second.
    #[must_use]
    pub const fn hz(self) -> i64 {
        self.0
    }
}

/// The µs from `origin` to `ticks`: multiplied before dividing in `i128`
/// and floored, toward the smaller side for negative values too
/// (design.md "Handling QPC"). Saturates at the `i64` range, which only
/// spans beyond it for frequencies below 1 MHz.
#[must_use]
pub fn qpc_to_us(ticks: i64, origin: i64, freq: QpcFrequency) -> TimeUs {
    let elapsed = i128::from(ticks) - i128::from(origin);
    let us = (elapsed * 1_000_000).div_euclid(i128::from(freq.0));
    let saturated = if us < 0 { i64::MIN } else { i64::MAX };
    TimeUs(i64::try_from(us).unwrap_or(saturated))
}

/// The widest wake interval [`WakeHistogram`] keeps exactly, in µs.
pub const WAKE_HISTOGRAM_LIMIT: u64 = 20_000;

/// A percentile of [`WakeHistogram`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Percentile {
    /// No interval was recorded.
    Empty,
    /// The percentile, in µs.
    At(u64),
    /// The percentile lies above [`WAKE_HISTOGRAM_LIMIT`].
    Above,
}

/// The distribution of the input thread's wake intervals, in 1 µs buckets
/// up to [`WAKE_HISTOGRAM_LIMIT`]; longer ones are only counted, with the
/// exact maximum. Recording allocates nothing.
#[derive(Clone, Debug)]
pub struct WakeHistogram {
    buckets: Box<[u64]>,
    count: u64,
    max: u64,
}

impl Default for WakeHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeHistogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: vec![0; WAKE_HISTOGRAM_LIMIT as usize + 1].into_boxed_slice(),
            count: 0,
            max: 0,
        }
    }

    /// Records one interval in µs.
    pub fn record(&mut self, interval_us: u64) {
        if let Some(b) = usize::try_from(interval_us)
            .ok()
            .and_then(|i| self.buckets.get_mut(i))
        {
            *b += 1;
        }
        self.count += 1;
        self.max = self.max.max(interval_us);
    }

    /// Intervals recorded.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The longest interval, in µs; 0 when empty.
    #[must_use]
    pub const fn max(&self) -> u64 {
        self.max
    }

    /// The 99th percentile: the smallest interval at or below which at
    /// least 99 % of the intervals lie.
    #[must_use]
    pub fn p99(&self) -> Percentile {
        if self.count == 0 {
            return Percentile::Empty;
        }
        // The rank of the percentile, rounded up; at least 1 for a nonzero
        // count.
        let rank = (u128::from(self.count) * 99).div_ceil(100);
        let mut seen = 0u128;
        for (us, &n) in self.buckets.iter().enumerate() {
            seen += u128::from(n);
            if seen >= rank {
                return Percentile::At(us as u64);
            }
        }
        Percentile::Above
    }
}
