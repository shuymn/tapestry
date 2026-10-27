//! Measure positions to integer microseconds and display positions with exact
//! rationals (design.md "Chart Time").

use std::cmp::Ordering;

use bms_model::{DOMAIN, FRAC_BITS, Position, ScrollSegment, TimeUs};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, Zero};

use crate::decimal::Rational;
use crate::parse::{ObjectKind, Source};
use crate::{Limits, LoadError, Warning, WarningKind};

/// The BPM assumed when `#BPM` is missing.
const DEFAULT_BPM: i64 = 130;
const US_PER_MINUTE: i64 = 60_000_000;
/// A STOP value counts 1/192 of a 4/4 measure, so 1/48 of a beat.
const STOP_UNITS_PER_BEAT: i64 = 48;

fn int(v: i64) -> Rational {
    Rational::from_integer(BigInt::from(v))
}

/// The running time in µs as a reduced fraction whose denominator is capped
/// (design.md "Chart Time"). An addition takes greatest common divisors only
/// against the added term's denominator, which the text limits keep small, and
/// builds the reduced sum without normalizing it again (Knuth, TAOCP vol. 2,
/// 4.5.1), so its cost grows linearly with the time's size.
struct Time {
    numer: BigInt,
    /// Positive, and coprime with `numer`.
    denom: BigInt,
}

impl Time {
    fn zero() -> Self {
        Self {
            numer: BigInt::zero(),
            denom: BigInt::one(),
        }
    }

    /// Adds `term`, then refuses a denominator of more than `max_bits`.
    fn add(&mut self, term: &Rational, max_bits: u64) -> Result<(), ()> {
        // A `Ratio` is reduced, with a positive denominator.
        let (p, q) = (term.numer(), term.denom());
        let g = (&self.denom % q).gcd(q);
        if g.is_one() {
            self.numer = &self.numer * q + p * &self.denom;
            self.denom = &self.denom * q;
        } else {
            let denom_g = &self.denom / &g;
            let t = &self.numer * (q / &g) + p * &denom_g;
            let g2 = (&t % &g).gcd(&g);
            self.numer = t / &g2;
            self.denom = denom_g * (q / &g2);
        }
        if self.denom.bits() > max_bits {
            return Err(());
        }
        Ok(())
    }
}

/// An object with its time.
#[derive(Clone, Copy, Debug)]
pub struct Timed {
    pub time: TimeUs,
    /// The display position: the exact beat count floored to 2^-80 beat.
    pub pos: Position,
    pub kind: ObjectKind,
    pub line: usize,
    /// Position of the object in the file, the tie-breaker among objects
    /// that share a time.
    pub seq: usize,
}

/// The chart's objects in time order with the scroll map's parts.
#[derive(Debug)]
pub struct Timeline {
    /// Notes, long-note objects, mines and BGM in measure-position order,
    /// file order within one position. Positions that floor to the same µs
    /// keep position order; file order travels in [`Timed::seq`]. Timing
    /// objects are consumed.
    pub objects: Vec<Timed>,
    /// The scroll rate before time 0, from the `#BPM` header.
    pub initial_rate: i128,
    /// The scroll map's segments, coalesced by start in exact event order.
    pub segments: Vec<ScrollSegment>,
    pub bar_lines: Vec<Position>,
    /// The measure of the last position kept, which errors from compiling
    /// the scroll map name.
    pub last_measure: u16,
}

/// Where an event sits: `index / count` of the way through `measure`.
#[derive(Clone, Copy)]
struct Spot {
    measure: u16,
    index: u32,
    count: u32,
}

impl Spot {
    const fn start(measure: u16) -> Self {
        Self {
            measure,
            index: 0,
            count: 1,
        }
    }

    /// The order of the beat positions. Every measure length is positive
    /// and `index < count`, so positions grow with the measure and, within
    /// one, with the fraction; comparing those needs no rationals.
    fn order(&self, other: &Self) -> Ordering {
        self.measure.cmp(&other.measure).then_with(|| {
            let lhs = u64::from(self.index) * u64::from(other.count);
            let rhs = u64::from(other.index) * u64::from(self.count);
            lhs.cmp(&rhs)
        })
    }
}

/// One position's worth of work: a bar line or an object.
#[derive(Clone, Copy)]
enum Event {
    Bar,
    Object(usize),
}

pub fn compute(
    source: &Source,
    limits: &Limits,
    warnings: &mut Vec<Warning>,
) -> Result<Timeline, LoadError> {
    let initial_bpm = if let Some(bpm) = &source.bpm {
        bpm.clone()
    } else {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::MissingBpm,
        });
        int(DEFAULT_BPM)
    };
    let cap = BigInt::from(limits.max_duration.0);
    let max_bits = limits.max_time_bits;

    // Each measure's length in beats. These values and the segments below are
    // bounded by the text limits (design.md "Chart Time"), so only the running
    // time needs a cap.
    let last_measure = source
        .objects
        .iter()
        .map(|o| o.measure)
        .chain(source.measure_lengths.keys().copied())
        .max()
        .unwrap_or(0);
    let spans: Vec<Rational> = (0..=last_measure)
        .map(|m| {
            let length = source
                .measure_lengths
                .get(&m)
                .cloned()
                .unwrap_or_else(|| int(1));
            length * BigInt::from(4)
        })
        .collect();
    // Each measure's start in beats, exact: positions are narrowed only for
    // what the chart keeps (design.md "Chart Time").
    let mut sum = Rational::zero();
    let starts: Vec<Rational> = spans
        .iter()
        .map(|span| {
            let start = sum.clone();
            sum += span;
            start
        })
        .collect();

    let mut events = Vec::with_capacity(source.objects.len() + usize::from(last_measure) + 1);
    for m in 0..=last_measure {
        events.push((Spot::start(m), Event::Bar));
    }
    for (i, o) in source.objects.iter().enumerate() {
        let spot = Spot {
            measure: o.measure,
            index: o.index,
            count: o.count,
        };
        events.push((spot, Event::Object(i)));
    }
    // Stable: equal positions keep bar lines first, then file order. The
    // time cap is checked on this canonical order of additions.
    events.sort_by(|a, b| a.0.order(&b.0));
    // The chart ends with its last note, mine or BGM object: bar lines and BPM
    // changes or STOPs after it are dropped, so empty trailing measures neither
    // count toward the duration cap nor draw bar lines (design.md "Loading
    // Limits").
    let end = events
        .iter()
        .rev()
        .find(|(_, e)| match *e {
            Event::Object(i) => !matches!(
                source.objects[i].kind,
                ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_)
            ),
            Event::Bar => false,
        })
        .map_or_else(|| Spot::start(0), |(spot, _)| *spot);
    events.truncate(events.partition_point(|e| e.0.order(&end) != Ordering::Greater));

    let mut us_per_beat = int(US_PER_MINUTE) / &initial_bpm;
    let initial_rate = slope(&us_per_beat).ok_or(LoadError::PositionOverflow { measure: 0 })?;
    let mut scroll_rate = initial_rate;
    let mut timeline = Timeline {
        objects: Vec::with_capacity(source.objects.len()),
        initial_rate,
        segments: vec![ScrollSegment {
            start: TimeUs(0),
            anchor: Position(0),
            slope: initial_rate,
        }],
        bar_lines: Vec::with_capacity(usize::from(last_measure) + 1),
        last_measure: 0,
    };
    // The current measure's length in µs at the current BPM, cleared when
    // either changes.
    let mut measure_us: Option<Rational> = None;
    let mut beat = Spot::start(0);
    let mut time = Time::zero();
    let mut rest = events.as_slice();
    while let Some((spot, _)) = rest.first() {
        let len = rest
            .iter()
            .take_while(|e| e.0.order(spot) == Ordering::Equal)
            .count();
        let (group, tail) = rest.split_at(len);
        rest = tail;
        let objects = || {
            group.iter().filter_map(|(_, e)| match *e {
                Event::Object(i) => Some(&source.objects[i]),
                Event::Bar => None,
            })
        };
        let measure = spot.measure;
        let overflow = || LoadError::TimeOverflow { measure };
        let out_of_range = || LoadError::PositionOverflow { measure };
        timeline.last_measure = measure;

        // The fraction of the previous position's measure up to this
        // position. Every measure has a bar line at its start, so a group in
        // another measure is the start of the next one, and the fraction is
        // the rest of the previous measure. Each term is below 2^46 and not
        // negative in sorted order; `count` is at least 1, since a line
        // without pairs has no objects.
        let (numer, denom) = if measure == beat.measure {
            (
                u64::from(spot.index) * u64::from(beat.count)
                    - u64::from(beat.index) * u64::from(spot.count),
                u64::from(spot.count) * u64::from(beat.count),
            )
        } else {
            (u64::from(beat.count - beat.index), u64::from(beat.count))
        };
        let fraction = Rational::new(BigInt::from(numer), BigInt::from(denom));
        let rate =
            measure_us.get_or_insert_with(|| &spans[usize::from(beat.measure)] * &us_per_beat);
        time.add(&(fraction * &*rate), max_bits)
            .map_err(|()| overflow())?;
        if measure != beat.measure {
            measure_us = None;
        }
        beat = *spot;
        let at = to_micros(&time, &cap, measure)?;
        // The exact beat count, which BPM changes and STOPs do not move.
        let beats = &starts[usize::from(measure)]
            + &spans[usize::from(measure)]
                * Rational::new(BigInt::from(spot.index), BigInt::from(spot.count));
        let pos = to_position(&beats).ok_or_else(out_of_range)?;

        // A BPM change applies first; channel 08 wins over 03; the later
        // object in the file wins within a channel.
        let pick = |want_ref: bool| {
            objects().filter_map(move |o| match o.kind {
                ObjectKind::BpmRef(id) if want_ref => {
                    Some((o.line, source.bpm_defs.get(&id).cloned()))
                }
                ObjectKind::BpmHex(v) if !want_ref => Some((o.line, Some(int(i64::from(v))))),
                _ => None,
            })
        };
        let mut new_bpm = None;
        for (l, value) in pick(false).chain(pick(true)) {
            match value {
                Some(v) => new_bpm = Some(v),
                None => warnings.push(Warning {
                    line: Some(l),
                    kind: WarningKind::UndefinedReference,
                }),
            }
        }
        if let Some(v) = new_bpm {
            us_per_beat = int(US_PER_MINUTE) / v;
            measure_us = None;
            scroll_rate = slope(&us_per_beat).ok_or_else(out_of_range)?;
            let segment =
                segment(&time, &beats, pos, &us_per_beat, scroll_rate).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, segment);
        }

        for (_, event) in group {
            match *event {
                Event::Bar => timeline.bar_lines.push(pos),
                Event::Object(i) => {
                    let o = &source.objects[i];
                    if !matches!(
                        o.kind,
                        ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_)
                    ) {
                        timeline.objects.push(Timed {
                            time: at,
                            pos,
                            kind: o.kind,
                            line: o.line,
                            seq: i,
                        });
                    }
                }
            }
        }

        // Notes at a STOP's position sound at its start; the STOP length
        // uses the BPM after any change at the same position.
        let mut units = Rational::zero();
        for o in objects() {
            if let ObjectKind::StopRef(id) = o.kind {
                match source.stop_defs.get(&id) {
                    Some(v) if v.is_zero() => warnings.push(Warning {
                        line: Some(o.line),
                        kind: WarningKind::ZeroStop,
                    }),
                    Some(v) => units += v,
                    None => warnings.push(Warning {
                        line: Some(o.line),
                        kind: WarningKind::UndefinedReference,
                    }),
                }
            }
        }
        if units.is_positive() {
            // Flat from the STOP's start, then the current rate again from
            // its end, each anchored on the exact curve.
            let stop = segment(&time, &beats, pos, &us_per_beat, 0).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, stop);
            let length = units * &us_per_beat / BigInt::from(STOP_UNITS_PER_BEAT);
            time.add(&length, max_bits).map_err(|()| overflow())?;
            to_micros(&time, &cap, measure)?;
            let resume =
                segment(&time, &beats, pos, &us_per_beat, scroll_rate).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, resume);
        }
    }
    Ok(timeline)
}

/// Floors the time to microseconds (mathematical floor) after checking the
/// duration cap, so nothing is sized from a time past it, and the scroll
/// map's domain, which a raised duration cap may pass.
fn to_micros(time: &Time, cap: &BigInt, measure: u16) -> Result<TimeUs, LoadError> {
    let (floor, rest) = time.numer.div_mod_floor(&time.denom);
    let past = |bound: &BigInt| floor > *bound || (floor == *bound && !rest.is_zero());
    if past(cap) {
        return Err(LoadError::DurationExceeded { measure });
    }
    if past(&BigInt::from(DOMAIN)) {
        return Err(LoadError::PositionOverflow { measure });
    }
    // The floor is at most the cap, an `i64`, so this never fails.
    i64::try_from(floor)
        .map(TimeUs)
        .map_err(|_| LoadError::TimeOverflow { measure })
}

/// `floor(2^80 × beats)`, or `None` outside `i128`.
fn to_position(beats: &Rational) -> Option<Position> {
    let units = (beats.numer() << FRAC_BITS).div_floor(beats.denom());
    i128::try_from(units).ok().map(Position)
}

/// The scroll rate in units per µs, floored: `2^80` beats over the µs per
/// beat.
fn slope(us_per_beat: &Rational) -> Option<i128> {
    i128::try_from((us_per_beat.denom() << FRAC_BITS).div_floor(us_per_beat.numer())).ok()
}

/// The segment of a change at exact time `time` and beat count `beats`
/// (display position `pos`), after which the chart scrolls at
/// `us_per_beat` (`rate` units per µs, 0 for a STOP): it starts at
/// `ceil(time)`, anchored on the exact curve there. The time was checked
/// against the domain, so the start fits.
fn segment(
    time: &Time,
    beats: &Rational,
    pos: Position,
    us_per_beat: &Rational,
    rate: i128,
) -> Option<ScrollSegment> {
    let (floor, rest) = time.numer.div_mod_floor(&time.denom);
    let (start, anchor) = if rest.is_zero() {
        (floor, pos)
    } else if rate == 0 {
        (floor + 1, pos)
    } else {
        // beats + (ceil(time) - time) / us_per_beat, with the gap
        // `(denom - rest) / denom` and `us_per_beat = a / b`: one product
        // of the running time's denominator, never reduced.
        let gap = &time.denom - &rest;
        let (a, b) = (us_per_beat.numer(), us_per_beat.denom());
        let (bn, bd) = (beats.numer(), beats.denom());
        let numer = (bn * &time.denom * a + gap * b * bd) << FRAC_BITS;
        let units = numer.div_floor(&(bd * &time.denom * a));
        (floor + 1, Position(i128::try_from(units).ok()?))
    };
    Some(ScrollSegment {
        start: TimeUs(i64::try_from(start).ok()?),
        anchor,
        slope: rate,
    })
}

/// Appends `segment`, replacing the last one if it starts at the same µs:
/// changes coalesce in exact event order, the later winning.
fn push(segments: &mut Vec<ScrollSegment>, segment: ScrollSegment) {
    match segments.last_mut() {
        Some(last) if last.start == segment.start => *last = segment,
        _ => segments.push(segment),
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;

    use super::{Rational, Time};

    fn r(n: i64, d: i64) -> Rational {
        Rational::new(BigInt::from(n), BigInt::from(d))
    }

    #[test]
    fn time_sums_stay_reduced() {
        // The steps take each path: coprime denominators (0 + 1/6), a shared
        // factor the sum cancels (1/6 + 1/3 = 1/2, 55/42 + 3/14 = 32/21),
        // and one it keeps (17/14 + 2/21 = 55/42).
        let mut time = Time::zero();
        let mut expected = r(0, 1);
        for term in [r(1, 6), r(1, 3), r(5, 7), r(2, 21), r(3, 14), r(11, 2)] {
            time.add(&term, 64).unwrap();
            // num-rational's own addition normalizes with a full GCD.
            expected += &term;
            assert_eq!(
                (&time.numer, &time.denom),
                (expected.numer(), expected.denom()),
                "{term}"
            );
        }
    }

    #[test]
    fn time_sums_take_zero_equal_negative_and_cancelling_terms() {
        let mut time = Time::zero();
        let mut expected = r(0, 1);
        // A zero term; equal denominators (1/6 + 1/6 = 1/3); cancellation
        // to zero (1/3 - 1/3); a negative term; cancellation of a negative
        // sum (-2/5 + 2/5); and a zero term onto a fraction.
        for term in [
            r(0, 1),
            r(1, 6),
            r(1, 6),
            r(-1, 3),
            r(-2, 5),
            r(2, 5),
            r(7, 4),
            r(0, 1),
        ] {
            time.add(&term, 64).unwrap();
            expected += &term;
            assert_eq!(
                (&time.numer, &time.denom),
                (expected.numer(), expected.denom()),
                "{term}"
            );
        }
    }

    #[test]
    fn time_denominator_cap_is_inclusive() {
        // 1/255 has an 8-bit denominator; adding 1/257 makes it 16 bits.
        let mut time = Time::zero();
        assert_eq!(time.add(&r(1, 255), 8), Ok(()));
        assert_eq!(time.add(&r(1, 257), 15), Err(()));
    }
}
