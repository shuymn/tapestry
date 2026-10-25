//! Display position as a piecewise linear function of chart time.

use crate::{ModelError, TimeUs};

/// A display position, in millionths of a beat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position(pub i64);

/// A change that shapes the scroll map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollChange {
    /// From `at` on, the chart scrolls at `bpm_milli` thousandths of a BPM.
    Bpm {
        /// Time of the change.
        at: TimeUs,
        /// The new BPM, in thousandths.
        bpm_milli: i64,
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

/// Microseconds per minute divided by millionths per beat, times thousandths
/// per BPM: a segment at `bpm_milli` advances `bpm_milli / 60_000` millionths
/// of a beat per microsecond.
const RATE_DIVISOR: i128 = 60_000;

/// One linear piece: from `start` on, position grows from `pos` at
/// `bpm_milli / 60_000` per microsecond. A STOP has `bpm_milli == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    start: i64,
    pos: i64,
    bpm_milli: i64,
}

/// The piecewise linear map from chart time to display position.
///
/// Position 0 is at time 0. Before time 0 the map extends at the initial BPM;
/// after the last change it extends at the last BPM. STOP segments are flat,
/// and the map never decreases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollMap {
    initial_bpm_milli: i64,
    /// Sorted by strictly increasing `start`; the first starts at time 0.
    segments: Vec<Segment>,
}

impl ScrollMap {
    /// Builds the map from the initial BPM and the BPM changes and STOPs.
    ///
    /// The changes may come in any order. At one time, BPM changes apply
    /// before a STOP, so the scroll after the STOP uses the new BPM; of
    /// several BPM changes at one time, the last one given wins. A STOP may
    /// start where another ends.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-positive BPM or STOP length, a change before
    /// time 0, a change strictly inside a STOP, or an `i64` overflow.
    pub fn new(initial_bpm_milli: i64, changes: &[ScrollChange]) -> Result<Self, ModelError> {
        if initial_bpm_milli <= 0 {
            return Err(ModelError::InvalidBpm {
                at: TimeUs(0),
                bpm_milli: initial_bpm_milli,
            });
        }
        let mut sorted = changes.to_vec();
        sorted.sort_by_key(|c| (c.at(), matches!(c, ScrollChange::Stop { .. })));

        let mut map = Self {
            initial_bpm_milli,
            segments: vec![Segment {
                start: 0,
                pos: 0,
                bpm_milli: initial_bpm_milli,
            }],
        };
        let mut bpm_milli = initial_bpm_milli;
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
                map.push(resume, bpm_milli)?;
            }
            match change {
                ScrollChange::Bpm { bpm_milli: b, .. } => {
                    if b <= 0 {
                        return Err(ModelError::InvalidBpm { at, bpm_milli: b });
                    }
                    bpm_milli = b;
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
            map.push(resume, bpm_milli)?;
        }
        Ok(map)
    }

    /// The BPM at time 0 before any change, in thousandths.
    #[must_use]
    pub const fn initial_bpm_milli(&self) -> i64 {
        self.initial_bpm_milli
    }

    /// Starts a segment at `start`, or retunes the last one if it starts there.
    fn push(&mut self, start: i64, bpm_milli: i64) -> Result<(), ModelError> {
        let last = self.segments.last_mut().ok_or(ModelError::Overflow)?;
        if last.start == start {
            last.bpm_milli = bpm_milli;
            return Ok(());
        }
        let advance = (i128::from(start) - i128::from(last.start)) * i128::from(last.bpm_milli);
        let pos = i128::from(last.pos) + advance.div_euclid(RATE_DIVISOR);
        let pos = i64::try_from(pos).map_err(|_| ModelError::Overflow)?;
        self.segments.push(Segment {
            start,
            pos,
            bpm_milli,
        });
        Ok(())
    }

    /// The display position at `t`, rounded down. Saturates at the `i64`
    /// bounds far outside any chart.
    #[must_use]
    pub fn position_at(&self, t: TimeUs) -> Position {
        let (start, pos, bpm_milli) = if t.0 < 0 {
            (0, 0, self.initial_bpm_milli)
        } else {
            let seg = self.segments[self.segments.partition_point(|s| s.start <= t.0) - 1];
            (seg.start, seg.pos, seg.bpm_milli)
        };
        let advance = (i128::from(t.0) - i128::from(start)) * i128::from(bpm_milli);
        Position(saturate(i128::from(pos) + advance.div_euclid(RATE_DIVISOR)))
    }

    /// The times whose position lies in `[from, to]`, as an inclusive pair
    /// `(earliest, latest)`.
    ///
    /// `earliest` is the first time at or past `from`, and `latest` the last
    /// time not past `to`. A flat STOP segment inside the range contributes
    /// its whole span, both ends included. When no time maps into the range
    /// (a position skipped between two microseconds, or `from > to`),
    /// `earliest > latest`. Results saturate at the `i64` bounds.
    #[must_use]
    pub fn time_range(&self, from: Position, to: Position) -> (TimeUs, TimeUs) {
        (
            TimeUs(saturate(self.earliest_at_or_past(from.0))),
            TimeUs(saturate(self.latest_not_past(to.0))),
        )
    }

    /// The smallest `t` with `position_at(t) >= p`.
    fn earliest_at_or_past(&self, p: i64) -> i128 {
        let p = i128::from(p);
        let next = self.segments.partition_point(|s| i128::from(s.pos) < p);
        if next == 0 {
            // p <= 0: before time 0 at the initial BPM.
            let t = ceil_div(p * RATE_DIVISOR, i128::from(self.initial_bpm_milli));
            return t.min(0);
        }
        let seg = self.segments[next - 1];
        let next_start = self.segments.get(next).map(|s| i128::from(s.start));
        if seg.bpm_milli == 0 {
            return next_start.unwrap_or(i128::MAX);
        }
        let t = i128::from(seg.start)
            + ceil_div(
                (p - i128::from(seg.pos)) * RATE_DIVISOR,
                i128::from(seg.bpm_milli),
            );
        next_start.map_or(t, |n| t.min(n))
    }

    /// The largest `t` with `position_at(t) <= p`.
    fn latest_not_past(&self, p: i64) -> i128 {
        let p = i128::from(p);
        let next = self.segments.partition_point(|s| i128::from(s.pos) <= p);
        if next == 0 {
            // p < 0: before time 0 at the initial BPM.
            return ceil_div((p + 1) * RATE_DIVISOR, i128::from(self.initial_bpm_milli)) - 1;
        }
        let seg = self.segments[next - 1];
        let next_end = self.segments.get(next).map(|s| i128::from(s.start) - 1);
        if seg.bpm_milli == 0 {
            return next_end.unwrap_or(i128::MAX);
        }
        let t = i128::from(seg.start)
            + ceil_div(
                (p - i128::from(seg.pos) + 1) * RATE_DIVISOR,
                i128::from(seg.bpm_milli),
            )
            - 1;
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
    const BPM_120: i64 = 120_000;

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

    fn bpm(at: i64, bpm_milli: i64) -> ScrollChange {
        ScrollChange::Bpm {
            at: TimeUs(at),
            bpm_milli,
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
        let changes = [stop(1_000, 500), bpm(1_000, 60_000), bpm(1_000, 240_000)];
        let map = ScrollMap::new(BPM_120, &changes).unwrap();
        assert_eq!(pos(&map, 1_000), 2_000);
        assert_eq!(pos(&map, 1_500), 2_000);
        assert_eq!(pos(&map, 1_501), 2_004);
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
        let slow = ScrollMap::new(30_000, &[]).unwrap();
        assert_eq!(range(&slow, 3, 3), (6, 7));
        assert_eq!(range(&slow, -3, -3), (-6, -5));
    }

    #[test]
    fn time_range_inverts_position_at() {
        let changes = [
            bpm(10_000, 155_555),
            stop(20_000, 7_777),
            bpm(27_777, 33_333),
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
                bpm_milli: 0
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
