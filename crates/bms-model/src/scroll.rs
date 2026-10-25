//! Display position as a piecewise linear function of chart time.

use crate::{ModelError, TimeUs};

/// A display position, in millionths of a beat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position(pub i64);

/// A change that shapes the scroll map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollChange {
    /// From `at` on, the chart scrolls at `bpm_micro` millionths of a BPM.
    Bpm {
        /// Time of the change.
        at: TimeUs,
        /// The new BPM, in millionths.
        bpm_micro: i64,
    },
    /// The chart does not scroll from `at` for `len`.
    Stop {
        /// Start of the STOP.
        at: TimeUs,
        /// Length of the STOP.
        len: TimeUs,
    },
}

impl ScrollChange {
    const fn at(self) -> TimeUs {
        match self {
            Self::Bpm { at, .. } | Self::Stop { at, .. } => at,
        }
    }
}

/// A segment at `bpm_micro` advances `bpm_micro / RATE_DIVISOR` millionths of
/// a beat per microsecond: 60 seconds in microseconds, times millionths of a
/// BPM, over millionths of a beat.
const RATE_DIVISOR: i128 = 60_000_000;

/// One linear piece. From `start` on, the scaled position (position times
/// [`RATE_DIVISOR`]) grows from `scaled` by `bpm_micro` per microsecond. A
/// STOP has `bpm_micro == 0`. Scaled positions are exact, so no rounding
/// carries from one segment to the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    start: i64,
    scaled: i128,
    bpm_micro: i64,
}

impl Segment {
    /// The position at `t`, unbounded.
    fn position(self, t: i128) -> i128 {
        let advance = (t - i128::from(self.start)).saturating_mul(i128::from(self.bpm_micro));
        self.scaled.saturating_add(advance).div_euclid(RATE_DIVISOR)
    }

    /// The smallest `t` with `position(t) >= p`. Needs `bpm_micro > 0`.
    fn earliest_at_or_past(self, p: i128) -> i128 {
        i128::from(self.start)
            + ceil_div(p * RATE_DIVISOR - self.scaled, i128::from(self.bpm_micro))
    }

    /// The largest `t` with `position(t) <= p`. Needs `bpm_micro > 0`.
    fn latest_not_past(self, p: i128) -> i128 {
        i128::from(self.start)
            + ceil_div(
                (p + 1) * RATE_DIVISOR - self.scaled,
                i128::from(self.bpm_micro),
            )
            - 1
    }
}

/// The piecewise linear map from chart time to display position.
///
/// Position 0 is at time 0. Before time 0 the map extends at the initial BPM;
/// after the last change it extends at the last BPM. STOP segments are flat,
/// and the map never decreases. Positions are exact up to one final floor,
/// so splitting a segment with a redundant BPM change moves nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollMap {
    /// The scroll before time 0.
    before_zero: Segment,
    /// Sorted by strictly increasing `start`; the first starts at time 0.
    segments: Vec<Segment>,
}

impl ScrollMap {
    /// Builds the map from the initial BPM and the BPM changes and STOPs,
    /// with BPMs in millionths.
    ///
    /// The changes may come in any order. At one time, BPM changes apply
    /// before a STOP, so the scroll after the STOP uses the new BPM; of
    /// several BPM changes at one time, the last one given wins. A STOP may
    /// start where another ends.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-positive BPM or STOP length, a change before
    /// time 0, a change strictly inside a STOP, or a time or segment start
    /// position outside `i64`.
    pub fn new(initial_bpm_micro: i64, changes: &[ScrollChange]) -> Result<Self, ModelError> {
        if initial_bpm_micro <= 0 {
            return Err(ModelError::InvalidBpm {
                at: TimeUs(0),
                bpm_micro: initial_bpm_micro,
            });
        }
        let mut sorted = changes.to_vec();
        sorted.sort_by_key(|c| (c.at(), matches!(c, ScrollChange::Stop { .. })));

        let first = Segment {
            start: 0,
            scaled: 0,
            bpm_micro: initial_bpm_micro,
        };
        let mut map = Self {
            before_zero: first,
            segments: vec![first],
        };
        let mut bpm_micro = initial_bpm_micro;
        let mut resume_at: Option<i64> = None;
        for change in sorted {
            let at = change.at();
            if at.0 < 0 {
                return Err(ModelError::NegativeTime { at });
            }
            if let Some(resume) = resume_at.take() {
                if at.0 < resume {
                    return Err(ModelError::ChangeDuringStop { at });
                }
                map.push(resume, bpm_micro)?;
            }
            match change {
                ScrollChange::Bpm { bpm_micro: b, .. } => {
                    if b <= 0 {
                        return Err(ModelError::InvalidBpm { at, bpm_micro: b });
                    }
                    bpm_micro = b;
                    map.push(at.0, b)?;
                }
                ScrollChange::Stop { len, .. } => {
                    if len.0 <= 0 {
                        return Err(ModelError::InvalidStop { at, len });
                    }
                    map.push(at.0, 0)?;
                    resume_at = Some(at.0.checked_add(len.0).ok_or(ModelError::Overflow)?);
                }
            }
        }
        if let Some(resume) = resume_at {
            map.push(resume, bpm_micro)?;
        }
        Ok(map)
    }

    /// The BPM at time 0 before any change, in millionths.
    #[must_use]
    pub const fn initial_bpm_micro(&self) -> i64 {
        self.before_zero.bpm_micro
    }

    /// Starts a segment at `start`, or retunes the last one if it starts there.
    fn push(&mut self, start: i64, bpm_micro: i64) -> Result<(), ModelError> {
        let last = self.segments.last_mut().ok_or(ModelError::Overflow)?;
        if last.start == start {
            last.bpm_micro = bpm_micro;
            return Ok(());
        }
        let scaled = (i128::from(start) - i128::from(last.start))
            .checked_mul(i128::from(last.bpm_micro))
            .and_then(|advance| last.scaled.checked_add(advance))
            .ok_or(ModelError::Overflow)?;
        if i64::try_from(scaled.div_euclid(RATE_DIVISOR)).is_err() {
            return Err(ModelError::Overflow);
        }
        self.segments.push(Segment {
            start,
            scaled,
            bpm_micro,
        });
        Ok(())
    }

    /// The segment that holds `t`.
    fn segment_at(&self, t: i64) -> Segment {
        if t < 0 {
            self.before_zero
        } else {
            self.segments[self.segments.partition_point(|s| s.start <= t) - 1]
        }
    }

    /// The display position at `t`, rounded down. Saturates at the `i64`
    /// bounds far outside any chart.
    #[must_use]
    pub fn position_at(&self, t: TimeUs) -> Position {
        Position(saturate(self.segment_at(t.0).position(i128::from(t.0))))
    }

    /// The times whose position lies in `[from, to]`, as an inclusive pair
    /// `(earliest, latest)`.
    ///
    /// `earliest` is the first time whose [`position_at`](Self::position_at)
    /// is at or past `from`, and `latest` the last time whose position is not
    /// past `to`, both including the saturated tails. A flat STOP segment
    /// inside the range contributes its whole span, both ends included. When
    /// no time maps into the range (a position skipped between two
    /// microseconds, `from > to`, or a range beyond every `i64` time),
    /// `earliest > latest`.
    #[must_use]
    pub fn time_range(&self, from: Position, to: Position) -> (TimeUs, TimeUs) {
        let lo = if from.0 == i64::MIN {
            i128::MIN
        } else {
            self.earliest_at_or_past(i128::from(from.0))
        };
        let hi = if to.0 == i64::MAX {
            i128::MAX
        } else {
            self.latest_not_past(i128::from(to.0))
        };
        let (min, max) = (i128::from(i64::MIN), i128::from(i64::MAX));
        let (lo, hi) = if lo > max {
            (max, max - 1)
        } else if hi < min {
            (min + 1, min)
        } else {
            (lo.max(min), hi.min(max))
        };
        (TimeUs(saturate(lo)), TimeUs(saturate(hi)))
    }

    /// The smallest `t` with an unbounded position at or past `p`.
    fn earliest_at_or_past(&self, p: i128) -> i128 {
        let next = self
            .segments
            .partition_point(|s| s.scaled < p * RATE_DIVISOR);
        if next == 0 {
            // p <= 0: before time 0.
            return self.before_zero.earliest_at_or_past(p).min(0);
        }
        let seg = self.segments[next - 1];
        let next_start = self.segments.get(next).map(|s| i128::from(s.start));
        if seg.bpm_micro == 0 {
            // A STOP below p is always followed by a resume segment.
            return next_start.unwrap_or(i128::MAX);
        }
        let t = seg.earliest_at_or_past(p);
        next_start.map_or(t, |n| t.min(n))
    }

    /// The largest `t` with an unbounded position not past `p`.
    fn latest_not_past(&self, p: i128) -> i128 {
        let next = self
            .segments
            .partition_point(|s| s.scaled < (p + 1) * RATE_DIVISOR);
        if next == 0 {
            // p < 0: before time 0.
            return self.before_zero.latest_not_past(p).min(-1);
        }
        let seg = self.segments[next - 1];
        let next_end = self.segments.get(next).map(|s| i128::from(s.start) - 1);
        if seg.bpm_micro == 0 {
            return next_end.unwrap_or(i128::MAX);
        }
        let t = seg.latest_not_past(p);
        next_end.map_or(t, |n| t.min(n))
    }
}

/// `ceil(a / b)` for `b > 0`.
fn ceil_div(a: i128, b: i128) -> i128 {
    -(-a).div_euclid(b)
}

fn saturate(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

#[cfg(test)]
mod tests {
    use super::{Position, ScrollChange, ScrollMap};
    use crate::{ModelError, TimeUs};

    /// 120 BPM: 2 millionths of a beat per microsecond.
    const BPM_120: i64 = 120_000_000;

    fn pos(map: &ScrollMap, t: i64) -> i64 {
        map.position_at(TimeUs(t)).0
    }

    fn range(map: &ScrollMap, from: i64, to: i64) -> (i64, i64) {
        let (a, b) = map.time_range(Position(from), Position(to));
        (a.0, b.0)
    }

    fn stop(at: i64, len: i64) -> ScrollChange {
        ScrollChange::Stop {
            at: TimeUs(at),
            len: TimeUs(len),
        }
    }

    fn bpm(at: i64, bpm_micro: i64) -> ScrollChange {
        ScrollChange::Bpm {
            at: TimeUs(at),
            bpm_micro,
        }
    }

    #[test]
    fn scroll_position_is_flat_during_stop() {
        let map = ScrollMap::new(BPM_120, &[stop(1_000_000, 500_000)]).unwrap();
        let at_start = pos(&map, 1_000_000);
        assert_eq!(at_start, 2_000_000);
        assert!(pos(&map, 999_999) < at_start);
        for t in [1_000_001, 1_250_000, 1_499_999, 1_500_000] {
            assert_eq!(pos(&map, t), at_start, "t = {t}");
        }
        assert_eq!(pos(&map, 1_500_001), at_start + 2);
        assert_eq!(pos(&map, 2_500_000), at_start + 2_000_000);
    }

    #[test]
    fn stop_at_time_zero_holds_position_zero() {
        let map = ScrollMap::new(BPM_120, &[stop(0, 100)]).unwrap();
        assert_eq!(pos(&map, -1), -2);
        assert_eq!(pos(&map, 0), 0);
        assert_eq!(pos(&map, 100), 0);
        assert_eq!(pos(&map, 101), 2);
        assert_eq!(range(&map, 0, 0), (0, 100));
    }

    #[test]
    fn last_bpm_change_at_one_time_wins_and_applies_before_a_stop() {
        let changes = [
            stop(1_000, 500),
            bpm(1_000, 60_000_000),
            bpm(1_000, 240_000_000),
        ];
        let map = ScrollMap::new(BPM_120, &changes).unwrap();
        assert_eq!(pos(&map, 1_000), 2_000);
        assert_eq!(pos(&map, 1_500), 2_000);
        assert_eq!(pos(&map, 1_501), 2_004);
    }

    #[test]
    fn redundant_bpm_changes_keep_the_distance() {
        // 123.456 BPM covers 123_456_000 / 60_000_000 millionths of a beat
        // per microsecond, so time t sits at floor(t * 123_456 / 60_000).
        const BPM: i64 = 123_456_000;
        let changes: Vec<_> = (1..=60_000).map(|k| bpm(k * 1_001, BPM)).collect();
        let split = ScrollMap::new(BPM, &changes).unwrap();
        let plain = ScrollMap::new(BPM, &[]).unwrap();
        let end = 60_000 * 1_001;
        assert_eq!(pos(&split, end), 123_579_456);
        let mut checked = 0;
        for t in (0..=end).step_by(997) {
            let expected = i64::try_from(i128::from(t) * 123_456 / 60_000).unwrap();
            assert_eq!(pos(&split, t), expected, "t = {t}");
            assert_eq!(pos(&plain, t), expected, "t = {t}");
            checked += 1;
        }
        assert!(checked > 60_000);
    }

    #[test]
    fn bpm_keeps_six_fractional_digits() {
        // 0.0005 BPM: 500 / 60_000_000 millionths of a beat per microsecond.
        let map = ScrollMap::new(500, &[]).unwrap();
        assert_eq!(pos(&map, 479_999), 3);
        assert_eq!(pos(&map, 480_000), 4);
        assert_eq!(range(&map, 4, 4), (480_000, 599_999));
    }

    #[test]
    fn time_range_maps_a_stop_back_to_its_whole_span() {
        let map = ScrollMap::new(BPM_120, &[stop(1_000_000, 500_000)]).unwrap();
        assert_eq!(range(&map, 2_000_000, 2_000_000), (1_000_000, 1_500_000));
        assert_eq!(range(&map, 1_999_998, 2_000_002), (999_999, 1_500_001));
        assert_eq!(range(&map, 0, 4_000_000), (0, 2_500_000));
    }

    #[test]
    fn time_range_of_a_single_position() {
        let map = ScrollMap::new(BPM_120, &[]).unwrap();
        assert_eq!(range(&map, 4, 4), (2, 2));
        // Position 3 falls between microseconds 1 and 2.
        assert_eq!(range(&map, 3, 3), (2, 1));
        let slow = ScrollMap::new(30_000_000, &[]).unwrap();
        assert_eq!(range(&slow, 3, 3), (6, 7));
        assert_eq!(range(&slow, -3, -3), (-6, -5));
    }

    #[test]
    fn time_range_inverts_the_saturated_tails() {
        let map = ScrollMap::new(BPM_120, &[]).unwrap();
        let half = 1_i64 << 62;
        assert_eq!(pos(&map, half - 1), i64::MAX - 1);
        assert_eq!(pos(&map, half), i64::MAX);
        assert_eq!(range(&map, i64::MAX, i64::MAX), (half, i64::MAX));
        assert_eq!(pos(&map, -half), i64::MIN);
        assert_eq!(range(&map, i64::MIN, i64::MIN), (i64::MIN, -half));
        // 0.000001 BPM never reaches the top within i64 time.
        let crawl = ScrollMap::new(1, &[]).unwrap();
        let (lo, hi) = crawl.time_range(Position(i64::MAX), Position(i64::MAX));
        assert!(lo > hi);
        let (lo, hi) = crawl.time_range(Position(i64::MIN), Position(i64::MIN));
        assert!(lo > hi);
    }

    #[test]
    fn time_range_inverts_position_at() {
        let changes = [
            bpm(10_000, 155_555_555),
            stop(20_000, 7_777),
            bpm(27_777, 33_333_333),
            stop(27_777, 1),
            stop(27_778, 3),
        ];
        let map = ScrollMap::new(BPM_120, &changes).unwrap();
        let mut checked = 0;
        for t in -2_000..60_000 {
            let p = map.position_at(TimeUs(t));
            let (lo, hi) = map.time_range(p, p);
            assert!(lo.0 <= t && t <= hi.0, "t = {t}");
            assert!(map.position_at(TimeUs(lo.0 - 1)) < p, "t = {t}");
            assert!(map.position_at(TimeUs(hi.0 + 1)) > p, "t = {t}");
            checked += 1;
        }
        assert_eq!(checked, 62_000);
    }

    #[test]
    fn rejects_malformed_changes() {
        assert_eq!(
            ScrollMap::new(0, &[]),
            Err(ModelError::InvalidBpm {
                at: TimeUs(0),
                bpm_micro: 0
            })
        );
        assert_eq!(
            ScrollMap::new(BPM_120, &[stop(10, 0)]),
            Err(ModelError::InvalidStop {
                at: TimeUs(10),
                len: TimeUs(0)
            })
        );
        assert_eq!(
            ScrollMap::new(BPM_120, &[bpm(-1, BPM_120)]),
            Err(ModelError::NegativeTime { at: TimeUs(-1) })
        );
        assert_eq!(
            ScrollMap::new(BPM_120, &[stop(10, 10), bpm(15, BPM_120)]),
            Err(ModelError::ChangeDuringStop { at: TimeUs(15) })
        );
        assert_eq!(
            ScrollMap::new(BPM_120, &[stop(10, 10), stop(10, 5)]),
            Err(ModelError::ChangeDuringStop { at: TimeUs(10) })
        );
        assert_eq!(
            ScrollMap::new(BPM_120, &[stop(i64::MAX, 1)]),
            Err(ModelError::Overflow)
        );
    }
}
