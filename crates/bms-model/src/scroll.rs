//! Display positions and the map from chart time to the playhead (design.md
//! "Chart Time").

use crate::{ModelError, TimeUs};

/// Fractional bits of a [`Position`].
pub const FRAC_BITS: u32 = 80;

/// One beat, in [`Position`] units.
pub const BEAT: i128 = 1 << FRAC_BITS;

/// The map's domain: queries are clamped to `±DOMAIN` µs, and segments start
/// within it.
pub const DOMAIN: i64 = 1 << 32;

/// Microseconds per minute times millionths per unit of BPM: a BPM in
/// millionths over this is beats per microsecond.
const MICRO_BPM_PER_BEAT_PER_US: i128 = 60_000_000_000_000;

/// A display position, in units of 2^-80 beat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position(pub i128);

impl Position {
    /// `self - other` in units, saturating at the `i128` bounds, which no
    /// two positions of one chart reach.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn minus(self, other: Self) -> i128 {
        self.0.saturating_sub(other.0)
    }
}

/// A change in integer microseconds, the input of [`ScrollMap::new`] for
/// charts built in code.
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

/// One linear piece of a [`ScrollMap`]: from `start` on, the playhead
/// grows from `anchor` by `slope` units per microsecond.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollSegment {
    /// The first microsecond the segment covers.
    pub start: TimeUs,
    /// The position at `start`.
    pub anchor: Position,
    /// Units per microsecond; 0 during a STOP.
    pub slope: i128,
}

impl ScrollSegment {
    /// The position at `t`, for `t` from `start` to the last microsecond the
    /// segment covers, where [`ScrollMap::from_segments`] checked it fits.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn value(self, t: i64) -> i128 {
        self.anchor.0 + self.slope * (i128::from(t) - i128::from(self.start.0))
    }

    /// The position at `t`, or `None` if it leaves `i128`.
    fn checked_value(self, t: i64) -> Option<i128> {
        let dt = i128::from(t) - i128::from(self.start.0);
        self.slope
            .checked_mul(dt)
            .and_then(|v| v.checked_add(self.anchor.0))
    }
}

/// The piecewise linear map from integer chart time to the playhead's
/// display position.
///
/// Position 0 is at time 0. Before time 0 the map runs at the initial rate;
/// from 0 on it follows its segments, each covering the microseconds up to
/// the next one's start. The map never decreases. It is defined on
/// `±DOMAIN` µs and clamps a query outside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScrollMap {
    /// Units per microsecond before time 0, positive.
    initial_rate: i128,
    /// Strictly increasing starts; the first is `{0, 0, _}`.
    segments: Vec<ScrollSegment>,
}

impl ScrollMap {
    /// Takes a compiled map: the rate before time 0 and the segments.
    ///
    /// # Errors
    ///
    /// [`ModelError::InitialRateNotPositive`] for an initial rate of 0 or
    /// less; [`ModelError::ScrollNotMonotone`] for a missing first segment
    /// or one not at time 0 and position 0, starts that do not strictly
    /// increase, a negative slope, or a segment that passes the next
    /// anchor by the last microsecond it covers; and
    /// [`ModelError::PositionOverflow`] for a start beyond `DOMAIN` or a
    /// position within the domain that leaves `i128`.
    pub fn from_segments(
        initial_rate: i128,
        segments: Vec<ScrollSegment>,
    ) -> Result<Self, ModelError> {
        if initial_rate <= 0 {
            return Err(ModelError::InitialRateNotPositive);
        }
        initial_rate
            .checked_mul(i128::from(DOMAIN))
            .ok_or(ModelError::PositionOverflow)?;
        match segments.first() {
            Some(first) if first.start == TimeUs(0) && first.anchor == Position(0) => {}
            _ => return Err(ModelError::ScrollNotMonotone { index: 0 }),
        }
        let mut prev: Option<ScrollSegment> = None;
        for (index, &s) in segments.iter().enumerate() {
            if s.slope < 0 {
                return Err(ModelError::ScrollNotMonotone { index });
            }
            if s.start.0 > DOMAIN {
                return Err(ModelError::PositionOverflow);
            }
            if let Some(p) = prev {
                if s.start <= p.start {
                    return Err(ModelError::ScrollNotMonotone { index });
                }
                let reach = p
                    .checked_value(s.start.0 - 1)
                    .ok_or(ModelError::PositionOverflow)?;
                if reach > s.anchor.0 {
                    return Err(ModelError::ScrollNotMonotone { index });
                }
            }
            prev = Some(s);
        }
        if let Some(last) = prev {
            last.checked_value(DOMAIN)
                .ok_or(ModelError::PositionOverflow)?;
        }
        Ok(Self {
            initial_rate,
            segments,
        })
    }

    /// Builds the map from an initial BPM and BPM changes and STOPs in
    /// integer microseconds, with BPMs in millionths. Each segment is
    /// anchored on the exact curve at its start, and its slope is the
    /// BPM's rate floored to units.
    ///
    /// The changes may come in any order. At one time, BPM changes apply
    /// before a STOP, so the scroll after the STOP uses the new BPM; of
    /// several BPM changes at one time, the last one given wins. A STOP may
    /// start where another ends.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-positive BPM or STOP length, a change before
    /// time 0, a change strictly inside a STOP, a time overflow, or a change
    /// beyond `DOMAIN`.
    pub fn new(initial_bpm_micro: i64, changes: &[ScrollChange]) -> Result<Self, ModelError> {
        if initial_bpm_micro <= 0 {
            return Err(ModelError::InvalidBpm {
                at: TimeUs(0),
                bpm_micro: initial_bpm_micro,
            });
        }
        let mut sorted = changes.to_vec();
        sorted.sort_by_key(|c| (c.at(), matches!(c, ScrollChange::Stop { .. })));

        // Breakpoints of (start, BPM in millionths), the last one at a time
        // winning.
        let mut points: Vec<(i64, i64)> = vec![(0, initial_bpm_micro)];
        let mut push = |start: i64, bpm_micro: i64| match points.last_mut() {
            Some(last) if last.0 == start => last.1 = bpm_micro,
            _ => points.push((start, bpm_micro)),
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
                push(resume, bpm_micro);
            }
            match change {
                ScrollChange::Bpm { bpm_micro: b, .. } => {
                    if b <= 0 {
                        return Err(ModelError::InvalidBpm { at, bpm_micro: b });
                    }
                    bpm_micro = b;
                    push(at.0, b);
                }
                ScrollChange::Stop { len, .. } => {
                    if len.0 <= 0 {
                        return Err(ModelError::InvalidStop { at, len });
                    }
                    push(at.0, 0);
                    resume_at = Some(at.checked_add(len).ok_or(ModelError::Overflow)?.0);
                }
            }
        }
        if let Some(resume) = resume_at {
            push(resume, bpm_micro);
        }

        // The exact position in beats times MICRO_BPM_PER_BEAT_PER_US.
        let mut scaled: i128 = 0;
        let mut segments = Vec::with_capacity(points.len());
        let mut last: Option<(i64, i64)> = None;
        for &(start, bpm_micro) in &points {
            if start > DOMAIN {
                return Err(ModelError::PositionOverflow);
            }
            if let Some((s, b)) = last {
                scaled = (i128::from(start) - i128::from(s))
                    .checked_mul(i128::from(b))
                    .and_then(|advance| scaled.checked_add(advance))
                    .ok_or(ModelError::PositionOverflow)?;
            }
            segments.push(ScrollSegment {
                start: TimeUs(start),
                anchor: Position(
                    to_units(scaled, MICRO_BPM_PER_BEAT_PER_US)
                        .ok_or(ModelError::PositionOverflow)?,
                ),
                slope: to_units(i128::from(bpm_micro), MICRO_BPM_PER_BEAT_PER_US)
                    .ok_or(ModelError::PositionOverflow)?,
            });
            last = Some((start, bpm_micro));
        }
        let initial_rate = to_units(i128::from(initial_bpm_micro), MICRO_BPM_PER_BEAT_PER_US)
            .ok_or(ModelError::PositionOverflow)?;
        Self::from_segments(initial_rate, segments)
    }

    /// Units per microsecond before time 0: the start BPM's rate, which
    /// sets the hi-speed scale.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn initial_rate(&self) -> i128 {
        self.initial_rate
    }

    /// The segments, by strictly increasing start.
    #[must_use]
    pub fn segments(&self) -> &[ScrollSegment] {
        &self.segments
    }

    /// The playhead at `t`, clamped to `±DOMAIN` µs.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn position_at(&self, t: TimeUs) -> Position {
        let t = t.0.clamp(-DOMAIN, DOMAIN);
        if t < 0 {
            // `from_segments` checked `initial_rate × DOMAIN`.
            return Position(self.initial_rate * i128::from(t));
        }
        let next = self.segments.partition_point(|s| s.start.0 <= t);
        // The first segment starts at 0 <= t, so `next` is at least 1, and
        // `from_segments` checked that each segment fits up to the next.
        match next.checked_sub(1).and_then(|i| self.segments.get(i)) {
            Some(s) => Position(s.value(t)),
            None => Position(0),
        }
    }
}

/// `floor(numer × 2^80 / denom)` for `numer >= 0` and `denom > 0`, split
/// into quotient and remainder so no intermediate leaves `i128` while the
/// result fits; `None` otherwise. `denom` must be below 2^47.
fn to_units(numer: i128, denom: i128) -> Option<i128> {
    let (q, r) = (numer.div_euclid(denom), numer.rem_euclid(denom));
    q.checked_mul(BEAT)?.checked_add((r << FRAC_BITS) / denom)
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;
    use num_integer::Integer;
    use proptest::prelude::*;

    use super::{BEAT, DOMAIN, Position, ScrollChange, ScrollMap, ScrollSegment};
    use crate::{ModelError, TimeUs};

    /// 120 BPM in millionths.
    const BPM_120: i64 = 120_000_000;

    fn pos(map: &ScrollMap, t: i64) -> i128 {
        map.position_at(TimeUs(t)).0
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

    fn seg(start: i64, anchor: i128, slope: i128) -> ScrollSegment {
        ScrollSegment {
            start: TimeUs(start),
            anchor: Position(anchor),
            slope,
        }
    }

    /// `floor(2^80 × n / d)` in big integers.
    fn floor_units(n: &BigInt, d: &BigInt) -> i128 {
        i128::try_from((n << 80u32).div_floor(d)).unwrap()
    }

    /// The exact curve of integer-µs changes, in beats times 6e13 (BPM in
    /// millionths times µs): the BPM in effect over each µs summed, STOPs
    /// flat. `changes` are sorted, at distinct times, inside no STOP.
    fn exact_scaled(initial: i64, changes: &[ScrollChange], t: i64) -> BigInt {
        let rate_at = |u: i64| {
            let mut rate = initial;
            let mut last = initial;
            for c in changes {
                match *c {
                    ScrollChange::Bpm { at, bpm_micro } if at.0 <= u => {
                        rate = bpm_micro;
                        last = bpm_micro;
                    }
                    ScrollChange::Stop { at, len } if at.0 <= u => {
                        rate = if u < at.0 + len.0 { 0 } else { last };
                    }
                    _ => {}
                }
            }
            rate
        };
        if t < 0 {
            return BigInt::from(initial) * t;
        }
        // Piecewise: the rate is constant between change boundaries.
        let mut cuts: Vec<i64> = vec![0, t];
        for c in changes {
            match *c {
                ScrollChange::Bpm { at, .. } => cuts.push(at.0),
                ScrollChange::Stop { at, len } => {
                    cuts.push(at.0);
                    cuts.push(at.0 + len.0);
                }
            }
        }
        cuts.retain(|&c| (0..=t).contains(&c));
        cuts.sort_unstable();
        cuts.dedup();
        cuts.windows(2)
            .map(|w| BigInt::from(rate_at(w[0])) * (w[1] - w[0]))
            .sum()
    }

    fn exact_units(initial: i64, changes: &[ScrollChange], t: i64) -> i128 {
        floor_units(
            &exact_scaled(initial, changes, t),
            &BigInt::from(60_000_000_000_000_i64),
        )
    }

    #[test]
    fn scroll_position_is_flat_during_stop() {
        let map = ScrollMap::new(BPM_120, &[stop(1_000_000, 500_000)]).unwrap();
        // 120 BPM is 2 beats a second.
        let at_start = pos(&map, 1_000_000);
        assert_eq!(at_start, 2 * BEAT);
        assert!(pos(&map, 999_999) < at_start);
        for t in [1_000_001, 1_250_000, 1_499_999, 1_500_000] {
            assert_eq!(pos(&map, t), at_start, "t = {t}");
        }
        assert!(pos(&map, 1_500_001) > at_start);
        // One more second of scroll: 4 beats exactly, reached from the
        // anchor at the STOP's end at the floored rate.
        let d = 4 * BEAT - pos(&map, 2_500_000);
        assert!((0..=1_000_000).contains(&d), "{d}");
    }

    #[test]
    fn stop_at_time_zero_holds_position_zero_and_runs_at_the_initial_rate_before() {
        let map = ScrollMap::new(BPM_120, &[stop(0, 100)]).unwrap();
        assert_eq!(map.segments()[0].slope, 0);
        let rate = map.initial_rate();
        assert!(rate > 0);
        assert_eq!(pos(&map, -1), -rate);
        assert_eq!(pos(&map, 0), 0);
        assert_eq!(pos(&map, 100), 0);
        assert_eq!(pos(&map, 101), rate);
    }

    #[test]
    fn last_bpm_change_at_one_time_wins_and_applies_before_a_stop() {
        let changes = [
            stop(1_000, 500),
            bpm(1_000, 60_000_000),
            bpm(1_000, 240_000_000),
        ];
        let map = ScrollMap::new(BPM_120, &changes).unwrap();
        let at = pos(&map, 1_000);
        assert_eq!(pos(&map, 1_500), at);
        // 240 BPM after the STOP: 4 beats a second, 4e-6 beats a µs.
        let rate_240 = map.segments().last().unwrap().slope;
        assert_eq!(rate_240, (4 * BEAT) / 1_000_000);
        assert_eq!(pos(&map, 1_501), at + rate_240);
    }

    #[test]
    fn redundant_bpm_changes_stay_within_a_unit_per_microsecond_of_the_exact_curve() {
        // Each segment is anchored exactly and runs at the floored rate, so
        // splitting one BPM into many segments moves the playhead by less
        // than a unit per µs since the last anchor, never past the exact
        // curve.
        const BPM: i64 = 123_456_000;
        let changes: Vec<_> = (1..=60_000).map(|k| bpm(k * 1_001, BPM)).collect();
        let split = ScrollMap::new(BPM, &changes).unwrap();
        let plain = ScrollMap::new(BPM, &[]).unwrap();
        let mut checked = 0;
        for t in (0..=60_000 * 1_001).step_by(997) {
            let exact = exact_units(BPM, &[], t);
            let since = t % 1_001;
            let d = exact - pos(&split, t);
            assert!((0..=i128::from(since)).contains(&d), "t = {t}: {d}");
            let d = exact - pos(&plain, t);
            assert!((0..=i128::from(t)).contains(&d), "t = {t}: {d}");
            checked += 1;
        }
        assert!(checked > 60_000);
    }

    #[test]
    fn negative_queries_run_at_the_initial_rate_within_a_unit_per_microsecond() {
        // 0.000001 BPM: 2^80 / 6e13 units per µs, about 20 billion; at
        // -5 µs the floored rate sits 2 units above the exact curve.
        let map = ScrollMap::new(1, &[]).unwrap();
        let exact = floor_units(&BigInt::from(-5), &BigInt::from(60_000_000_000_000_i64));
        assert_eq!(pos(&map, -5) - exact, 2);
        // The domain boundary, and a query past it clamped there.
        let edge = floor_units(
            &BigInt::from(-DOMAIN),
            &BigInt::from(60_000_000_000_000_i64),
        );
        let d = pos(&map, -DOMAIN) - edge;
        assert!((0..=i128::from(DOMAIN)).contains(&d), "{d}");
        assert_eq!(pos(&map, i64::MIN), pos(&map, -DOMAIN));
        assert_eq!(pos(&map, i64::MAX), pos(&map, DOMAIN));
    }

    #[test]
    fn largest_bpm_reaches_the_domain_end() {
        // Just under 10^12 BPM, the most the loader takes, in millionths.
        let top = 999_999_999_999_999_999;
        let map = ScrollMap::new(top, &[]).unwrap();
        let exact = exact_units(top, &[], DOMAIN);
        let d = exact - pos(&map, DOMAIN);
        assert!((0..=i128::from(DOMAIN)).contains(&d), "{d}");
        assert!(pos(&map, DOMAIN) > 0);
    }

    #[test]
    fn from_segments_rejects_malformed_maps_and_takes_their_corrected_forms() {
        let rate = BEAT / 1_000_000;
        let ok = ScrollMap::from_segments(rate, vec![seg(0, 0, rate), seg(10, 10 * rate, 0)]);
        assert!(ok.is_ok());
        let cases: [(i128, Vec<ScrollSegment>, ModelError); 8] = [
            (0, vec![seg(0, 0, rate)], ModelError::InitialRateNotPositive),
            (rate, vec![], ModelError::ScrollNotMonotone { index: 0 }),
            (
                rate,
                vec![seg(1, 0, rate)],
                ModelError::ScrollNotMonotone { index: 0 },
            ),
            (
                rate,
                vec![seg(0, 1, rate)],
                ModelError::ScrollNotMonotone { index: 0 },
            ),
            (
                rate,
                vec![seg(0, 0, rate), seg(0, 0, rate)],
                ModelError::ScrollNotMonotone { index: 1 },
            ),
            (
                rate,
                vec![seg(0, 0, -1)],
                ModelError::ScrollNotMonotone { index: 0 },
            ),
            // Covering 0..=9, the first reaches 9 × rate; an anchor below it
            // would step back.
            (
                rate,
                vec![seg(0, 0, rate), seg(10, 9 * rate - 1, 0)],
                ModelError::ScrollNotMonotone { index: 1 },
            ),
            (
                rate,
                vec![seg(0, 0, rate), seg(DOMAIN + 1, 0, 0)],
                ModelError::PositionOverflow,
            ),
        ];
        for (initial, segments, err) in cases {
            assert_eq!(
                ScrollMap::from_segments(initial, segments.clone()),
                Err(err),
                "{segments:?}"
            );
        }
        // The matching accepted forms: a flat first segment, an anchor
        // exactly where the previous segment reaches, a start at the
        // domain's end.
        for segments in [
            vec![seg(0, 0, 0)],
            vec![seg(0, 0, rate), seg(10, 9 * rate, 0)],
            vec![seg(0, 0, rate), seg(DOMAIN, i128::from(DOMAIN) * rate, 0)],
        ] {
            assert!(
                ScrollMap::from_segments(rate, segments.clone()).is_ok(),
                "{segments:?}"
            );
        }
        // A slope whose product leaves `i128` within the domain.
        assert_eq!(
            ScrollMap::from_segments(rate, vec![seg(0, 0, i128::MAX / i128::from(DOMAIN) + 1)]),
            Err(ModelError::PositionOverflow)
        );
        assert_eq!(
            ScrollMap::from_segments(i128::MAX / i128::from(DOMAIN) + 1, vec![seg(0, 0, 0)]),
            Err(ModelError::PositionOverflow)
        );
        assert!(
            ScrollMap::from_segments(
                i128::MAX / i128::from(DOMAIN),
                vec![seg(0, 0, i128::MAX / i128::from(DOMAIN))]
            )
            .is_ok()
        );
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
        assert_eq!(
            ScrollMap::new(BPM_120, &[stop(DOMAIN, 1)]),
            Err(ModelError::PositionOverflow)
        );
        assert!(ScrollMap::new(BPM_120, &[stop(DOMAIN - 1, 1)]).is_ok());
    }

    fn changes() -> impl Strategy<Value = (i64, Vec<ScrollChange>)> {
        let bpm_micro = 1..1_000_000_000_000_000_000_i64;
        (
            bpm_micro.clone(),
            prop::collection::vec(
                (1..2_000_000_i64, bpm_micro, 0..3_000_i64, any::<bool>()),
                0..8,
            ),
        )
            .prop_map(|(initial, steps)| {
                let mut at = 0;
                let mut out = Vec::new();
                for (gap, b, len, is_stop) in steps {
                    at += gap;
                    if is_stop && len > 0 {
                        out.push(stop(at, len));
                        at += len;
                    } else {
                        out.push(bpm(at, b));
                    }
                }
                (initial, out)
            })
    }

    proptest! {
        /// The playhead never decreases, stays at or below the exact curve
        /// from time 0 within a unit per µs since the last anchor (at most
        /// the time itself), and above it before time 0 within a unit per
        /// µs.
        #[test]
        fn position_at_brackets_the_exact_curve(
            (initial, changes) in changes(),
            queries in prop::collection::vec(-3_000_000_i64..20_000_000, 1..64),
        ) {
            let map = ScrollMap::new(initial, &changes).unwrap();
            let mut queries = queries;
            queries.sort_unstable();
            let mut prev = i128::MIN;
            for t in queries {
                let p = pos(&map, t);
                prop_assert!(p >= prev, "t = {}", t);
                prev = p;
                let exact = exact_units(initial, &changes, t);
                let d = exact - p;
                let span = if t < 0 { -i128::from(t) } else { i128::from(t) };
                if t < 0 {
                    prop_assert!((-span..=0).contains(&d), "t = {}: {}", t, d);
                } else {
                    let anchor = map
                        .segments()
                        .iter()
                        .rev()
                        .find(|s| s.start.0 <= t)
                        .map_or(0, |s| s.start.0);
                    prop_assert!((0..=i128::from(t - anchor)).contains(&d), "t = {}: {}", t, d);
                }
            }
        }
    }
}
