//! Measure positions to integer microseconds with exact rationals (D3).

use std::cmp::Ordering;

use bms_model::{ScrollChange, TimeUs};
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

/// The running time in µs as a reduced fraction whose denominator is
/// capped (D3). An addition takes greatest common divisors only against the
/// added term's denominator, which the text limits keep small, and builds
/// the reduced sum without normalizing it again (Knuth, TAOCP vol. 2,
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
    pub kind: ObjectKind,
    pub line: usize,
    /// Position of the object in the file, the tie-breaker among objects
    /// that share a time.
    pub seq: usize,
}

/// The chart's objects in time order with the scroll changes.
#[derive(Debug)]
pub struct Timeline {
    /// Notes, long-note objects, mines and BGM in measure-position order,
    /// file order within one position. Positions that floor to the same µs
    /// keep position order; file order travels in [`Timed::seq`]. Timing
    /// objects are consumed.
    pub objects: Vec<Timed>,
    pub initial_bpm_micro: i64,
    pub scroll: Vec<ScrollChange>,
    pub bar_lines: Vec<TimeUs>,
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
    Bar(u16),
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

    // Each measure's starting beat and length in beats. These values and
    // the segments below are bounded by the text limits (D3), so only the
    // running time needs a cap.
    let last_measure = source
        .objects
        .iter()
        .map(|o| o.measure)
        .chain(source.measure_lengths.keys().copied())
        .max()
        .unwrap_or(0);
    let mut starts = Vec::with_capacity(usize::from(last_measure) + 1);
    let mut spans = Vec::with_capacity(usize::from(last_measure) + 1);
    let mut start = Rational::zero();
    for m in 0..=last_measure {
        let length = source
            .measure_lengths
            .get(&m)
            .cloned()
            .unwrap_or_else(|| int(1));
        let span = length * BigInt::from(4);
        starts.push(start.clone());
        start += &span;
        spans.push(span);
    }
    // The beat position of a spot.
    let position = |spot: &Spot| {
        let m = usize::from(spot.measure);
        // `count` is at least 1: a line without pairs has no objects.
        let fraction = Rational::new(BigInt::from(spot.index), BigInt::from(spot.count));
        &starts[m] + fraction * &spans[m]
    };

    let mut events = Vec::with_capacity(source.objects.len() + usize::from(last_measure) + 1);
    for m in 0..=last_measure {
        events.push((Spot::start(m), Event::Bar(m)));
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

    let mut timeline = Timeline {
        objects: Vec::with_capacity(source.objects.len()),
        initial_bpm_micro: bpm_micro(&initial_bpm),
        scroll: Vec::new(),
        bar_lines: Vec::with_capacity(usize::from(last_measure) + 1),
    };
    let mut us_per_beat = int(US_PER_MINUTE) / &initial_bpm;
    // A measure's length in µs at the current BPM, kept while neither the
    // measure nor the BPM changes.
    let mut rate: Option<(u16, Rational)> = None;
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
                Event::Bar(_) => None,
            })
        };
        let measure = objects().next().map_or_else(
            || match group[0].1 {
                Event::Bar(m) => m,
                Event::Object(_) => 0,
            },
            |o| o.measure,
        );
        let overflow = || LoadError::TimeOverflow { measure };

        let segment = if spot.measure == beat.measure {
            // Within one measure: the fraction between, of the measure's µs.
            // Below 2^46 each, and not negative in sorted order.
            let numer = u64::from(spot.index) * u64::from(beat.count)
                - u64::from(beat.index) * u64::from(spot.count);
            let denom = u64::from(spot.count) * u64::from(beat.count);
            if rate.as_ref().is_none_or(|(m, _)| *m != spot.measure) {
                let span = &spans[usize::from(spot.measure)];
                rate = Some((spot.measure, span * &us_per_beat));
            }
            let fraction = Rational::new(BigInt::from(numer), BigInt::from(denom));
            rate.as_ref()
                .map_or_else(Rational::zero, |(_, measure_us)| fraction * measure_us)
        } else {
            (position(spot) - position(&beat)) * &us_per_beat
        };
        time.add(&segment, max_bits).map_err(|()| overflow())?;
        beat = *spot;
        let at = to_micros(&time, &cap, measure)?;

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
            timeline.scroll.push(ScrollChange::Bpm {
                at,
                bpm_micro: bpm_micro(&v),
            });
            us_per_beat = int(US_PER_MINUTE) / v;
            rate = None;
        }

        for (_, event) in group {
            match *event {
                Event::Bar(_) => timeline.bar_lines.push(at),
                Event::Object(i) => {
                    let o = &source.objects[i];
                    if !matches!(
                        o.kind,
                        ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_)
                    ) {
                        timeline.objects.push(Timed {
                            time: at,
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
            let length = units * &us_per_beat / BigInt::from(STOP_UNITS_PER_BEAT);
            time.add(&length, max_bits).map_err(|()| overflow())?;
            let end = to_micros(&time, &cap, measure)?;
            if end > at {
                timeline.scroll.push(ScrollChange::Stop {
                    at,
                    len: TimeUs(end.0 - at.0),
                });
            }
        }
    }
    Ok(timeline)
}

/// Floors the time to microseconds (mathematical floor) after checking the
/// duration cap, so nothing is sized from a time past it.
fn to_micros(time: &Time, cap: &BigInt, measure: u16) -> Result<TimeUs, LoadError> {
    let (floor, rest) = time.numer.div_mod_floor(&time.denom);
    if floor > *cap || (floor == *cap && !rest.is_zero()) {
        return Err(LoadError::DurationExceeded { measure });
    }
    // The floor is at most the cap, an `i64`, so this never fails.
    i64::try_from(floor)
        .map(TimeUs)
        .map_err(|_| LoadError::TimeOverflow { measure })
}

/// A BPM in millionths, rounded down. Every BPM is two hex digits, the
/// default, or a decimal of at most 12 integer digits and at least
/// 0.000001, so the value is from 1 to below 10^18. Saturates if that ever
/// changes.
fn bpm_micro(bpm: &Rational) -> i64 {
    i64::try_from((bpm * BigInt::from(1_000_000)).floor().to_integer()).unwrap_or(i64::MAX)
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
    fn time_denominator_cap_is_inclusive() {
        // 1/255 has an 8-bit denominator; adding 1/257 makes it 16 bits.
        let mut time = Time::zero();
        assert_eq!(time.add(&r(1, 255), 8), Ok(()));
        assert_eq!(time.add(&r(1, 257), 15), Err(()));
    }
}
