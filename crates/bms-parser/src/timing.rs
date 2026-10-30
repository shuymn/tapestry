//! Measure positions to integer microseconds and display positions with exact
//! rationals (design.md "Chart Time").

use std::cmp::Ordering;

use bms_model::{DOMAIN, FRAC_BITS, Position, ScrollSegment, Speed, TimeUs};
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
    /// The time at 1×: the exact time floored to µs.
    pub time: TimeUs,
    /// The real time at the load's speed: the exact time scaled by `20/k`,
    /// then floored to µs; `time` at 1×.
    pub real: TimeUs,
    /// The display position: the exact beat count floored to 2^-80 beat.
    pub pos: Position,
    pub kind: ObjectKind,
    pub line: usize,
    /// Position of the object in the file, the tie-breaker among objects
    /// that share a position.
    pub seq: usize,
    /// The ordinal of the object's exact position among the chart's
    /// positions: equal for objects at one position, ordered as positions
    /// and times are. Identity is decided on it, never on the floored time
    /// or the quantized position, which may merge distinct positions.
    pub rank: usize,
}

/// The chart's objects in time order with the scroll map's parts.
#[derive(Debug)]
pub struct Timeline {
    /// Notes, long-note objects, mines and BGM in measure-position order,
    /// file order within one position. The position travels in
    /// [`Timed::rank`] and file order in [`Timed::seq`]. Timing objects are
    /// consumed.
    pub objects: Vec<Timed>,
    /// The scroll rate before time 0, from the `#BPM` header.
    pub initial_rate: i128,
    /// The scroll map's segments, coalesced by start in exact event order.
    pub segments: Vec<ScrollSegment>,
    /// The scroll map on the real timeline at a speed other than 1×.
    pub real: Option<Scroll>,
    pub bar_lines: Vec<Position>,
    /// The measure of the last position kept, which errors from compiling
    /// the scroll map name.
    pub last_measure: u16,
}

/// A scroll map's parts: the rate before time 0 and the segments.
#[derive(Debug)]
pub struct Scroll {
    pub initial_rate: i128,
    pub segments: Vec<ScrollSegment>,
}

/// Chart time to real time at a speed `k/20`: the exact time times
/// `num / den = 20/k` in lowest terms, floored once (design.md "Practice
/// Speed Changes"). Flooring the 1× time again would round twice.
struct Mapping {
    num: BigInt,
    den: BigInt,
    ratio: Rational,
}

impl Mapping {
    /// `None` at 1×, where real time is the 1× time.
    fn of(speed: Speed) -> Option<Self> {
        (speed != Speed::NORMAL).then(|| {
            let (k, twenty) = speed.ratio();
            let (num, den) = (BigInt::from(twenty), BigInt::from(k));
            Self {
                ratio: Rational::new(num.clone(), den.clone()),
                num,
                den,
            }
        })
    }

    /// The real time of `time`: its floor in µs, the remainder and the
    /// denominator it is over. Past the scroll map's domain the chart is
    /// too long to play at this speed.
    fn real(&self, time: &Time, measure: u16) -> Result<(TimeUs, BigInt, BigInt), LoadError> {
        let denom = &time.denom * &self.den;
        let (floor, rest) = (&time.numer * &self.num).div_mod_floor(&denom);
        let floor = i64::try_from(floor)
            .ok()
            .filter(|&f| f < DOMAIN || (f == DOMAIN && rest.is_zero()))
            .ok_or(LoadError::TooLongAtSpeed { measure })?;
        Ok((TimeUs(floor), rest, denom))
    }
}

/// The real timeline's scroll state at a speed other than 1×.
struct RealScroll {
    mapping: Mapping,
    /// Real µs per beat at the current BPM.
    us_per_beat: Rational,
    /// Its rate in units per µs, floored.
    rate: i128,
    scroll: Scroll,
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

/// The chart's timeline at 1× and, at another `speed`, on the real
/// timeline too, both from one exact time.
pub fn compute(
    source: &Source,
    limits: &Limits,
    speed: Speed,
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
    let first = |slope| ScrollSegment {
        start: TimeUs(0),
        anchor: Position(0),
        slope,
    };
    // The real timeline's scroll map, from the mapped exact time: its rates
    // are the mapped BPM's, so hi-speed starts from the mapped start BPM.
    let mut real = match Mapping::of(speed) {
        Some(mapping) => {
            let us_per_beat = &us_per_beat * &mapping.ratio;
            let rate = slope(&us_per_beat).ok_or(LoadError::PositionOverflow { measure: 0 })?;
            Some(RealScroll {
                mapping,
                us_per_beat,
                rate,
                scroll: Scroll {
                    initial_rate: rate,
                    segments: vec![first(rate)],
                },
            })
        }
        None => None,
    };
    let mut timeline = Timeline {
        objects: Vec::with_capacity(source.objects.len()),
        initial_rate,
        segments: vec![first(initial_rate)],
        real: None,
        bar_lines: Vec::with_capacity(usize::from(last_measure) + 1),
        last_measure: end.measure,
    };
    // The current measure's length in µs at the current BPM, cleared when
    // either changes.
    let mut measure_us: Option<Rational> = None;
    let mut beat = Spot::start(0);
    let mut time = Time::zero();
    let mut rest = events.as_slice();
    let mut rank = 0;
    while let Some((spot, _)) = rest.first() {
        rank += 1;
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
        let (at, remainder) = to_micros(&time, &cap, measure)?;
        // The exact beat count, which BPM changes and STOPs do not move.
        let beats = Beats::at(
            &starts[usize::from(measure)],
            &spans[usize::from(measure)],
            spot,
        );
        let (pos, pos_rest) = beats.quantize().ok_or_else(out_of_range)?;
        // Where a BPM change or a STOP's start at this position happens.
        let here = Change {
            at,
            rest: &remainder,
            denom: &time.denom,
            beats: &beats,
            pos,
            pos_rest: &pos_rest,
        };
        let real_here = match &real {
            Some(r) => Some(r.mapping.real(&time, measure)?),
            None => None,
        };

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
            let change = segment(&here, &us_per_beat, scroll_rate).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, change);
            if let (Some(r), Some(t)) = (&mut real, &real_here) {
                r.us_per_beat = &us_per_beat * &r.mapping.ratio;
                r.rate = slope(&r.us_per_beat).ok_or_else(out_of_range)?;
                let change = segment(
                    &Change::real(t, &beats, pos, &pos_rest),
                    &r.us_per_beat,
                    r.rate,
                )
                .ok_or_else(out_of_range)?;
                push(&mut r.scroll.segments, change);
            }
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
                            real: real_here.as_ref().map_or(at, |r| r.0),
                            pos,
                            kind: o.kind,
                            line: o.line,
                            seq: i,
                            rank,
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
            let stop = segment(&here, &us_per_beat, 0).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, stop);
            let length = units * &us_per_beat / BigInt::from(STOP_UNITS_PER_BEAT);
            time.add(&length, max_bits).map_err(|()| overflow())?;
            let (end, remainder) = to_micros(&time, &cap, measure)?;
            let change = Change {
                at: end,
                rest: &remainder,
                denom: &time.denom,
                beats: &beats,
                pos,
                pos_rest: &pos_rest,
            };
            let resume = segment(&change, &us_per_beat, scroll_rate).ok_or_else(out_of_range)?;
            push(&mut timeline.segments, resume);
            if let (Some(r), Some(t)) = (&mut real, &real_here) {
                let stop = segment(&Change::real(t, &beats, pos, &pos_rest), &r.us_per_beat, 0)
                    .ok_or_else(out_of_range)?;
                push(&mut r.scroll.segments, stop);
                let end = r.mapping.real(&time, measure)?;
                let resume = segment(
                    &Change::real(&end, &beats, pos, &pos_rest),
                    &r.us_per_beat,
                    r.rate,
                )
                .ok_or_else(out_of_range)?;
                push(&mut r.scroll.segments, resume);
            }
        }
    }
    timeline.real = real.map(|r| r.scroll);
    Ok(timeline)
}

/// Floors the time to microseconds (mathematical floor) after checking the
/// duration cap, so nothing is sized from a time past it, and the scroll
/// map's domain, which a raised duration cap may pass. Returns the floor and
/// the remainder over the time's denominator.
fn to_micros(time: &Time, cap: &BigInt, measure: u16) -> Result<(TimeUs, BigInt), LoadError> {
    let (floor, rest) = time.numer.div_mod_floor(&time.denom);
    if floor > *cap || (floor == *cap && !rest.is_zero()) {
        return Err(LoadError::DurationExceeded { measure });
    }
    // The floor is at most the cap, an `i64`, so this never fails.
    let floor = i64::try_from(floor).map_err(|_| LoadError::TimeOverflow { measure })?;
    if floor > DOMAIN || (floor == DOMAIN && !rest.is_zero()) {
        return Err(LoadError::PositionOverflow { measure });
    }
    Ok((TimeUs(floor), rest))
}

/// A beat count as a fraction left unreduced: it is only narrowed to a
/// position or used in an anchor, so no greatest common divisor is worth
/// taking.
struct Beats {
    numer: BigInt,
    /// Positive.
    denom: BigInt,
}

impl Beats {
    /// `start + span × index / count`.
    fn at(start: &Rational, span: &Rational, spot: &Spot) -> Self {
        let count = BigInt::from(spot.count);
        let numer = start.numer() * span.denom() * &count
            + span.numer() * BigInt::from(spot.index) * start.denom();
        Self {
            numer,
            denom: start.denom() * span.denom() * count,
        }
    }

    /// `floor(2^80 × beats)` and the remainder over `denom`, or `None`
    /// outside `i128`.
    fn quantize(&self) -> Option<(Position, BigInt)> {
        let (units, rest) = (&self.numer << FRAC_BITS).div_mod_floor(&self.denom);
        Some((Position(i128::try_from(units).ok()?), rest))
    }
}

/// The scroll rate in units per µs, floored: `2^80` beats over the µs per
/// beat.
fn slope(us_per_beat: &Rational) -> Option<i128> {
    i128::try_from((us_per_beat.denom() << FRAC_BITS).div_floor(us_per_beat.numer())).ok()
}

/// Where a scroll change happens: the exact time `at + rest / denom` and the
/// beat count `beats`, whose display position is `pos` with `pos_rest` left
/// over `beats.denom`.
struct Change<'a> {
    at: TimeUs,
    rest: &'a BigInt,
    denom: &'a BigInt,
    beats: &'a Beats,
    pos: Position,
    pos_rest: &'a BigInt,
}

impl<'a> Change<'a> {
    /// The change at the real time `(floor, rest, denom)` from
    /// [`Mapping::real`], at the beat count `beats`.
    const fn real(
        (at, rest, denom): &'a (TimeUs, BigInt, BigInt),
        beats: &'a Beats,
        pos: Position,
        pos_rest: &'a BigInt,
    ) -> Self {
        Self {
            at: *at,
            rest,
            denom,
            beats,
            pos,
            pos_rest,
        }
    }
}

/// The segment of `change`, after which the chart scrolls at `us_per_beat`
/// (`rate` units per µs, 0 for a STOP): it starts at the first whole µs,
/// anchored on the exact curve there. The time was checked against the
/// domain, so the start fits.
fn segment(change: &Change<'_>, us_per_beat: &Rational, rate: i128) -> Option<ScrollSegment> {
    let at = change.at.0;
    let (start, anchor) = if change.rest.is_zero() {
        (at, change.pos)
    } else if rate == 0 {
        (at + 1, change.pos)
    } else {
        // floor(2^80 × (beats + gap / us_per_beat)) for the gap
        // `(denom - rest) / denom` to the next µs and `us_per_beat = a / b`,
        // as the position of the beats plus the floor of the advance, plus
        // one where their fractions carry: one division by a product of the
        // running time's denominator.
        let (a, b) = (us_per_beat.numer(), us_per_beat.denom());
        let bd = &change.beats.denom;
        let whole = a * change.denom;
        let gap = change.denom - change.rest;
        let (advance, advance_rest) = ((gap * b) << FRAC_BITS).div_mod_floor(&whole);
        let carry = (bd - change.pos_rest) * &whole <= advance_rest * bd;
        let units = i128::try_from(advance).ok()?;
        let anchor = change
            .pos
            .0
            .checked_add(units)?
            .checked_add(i128::from(carry))?;
        (at + 1, Position(anchor))
    };
    Some(ScrollSegment {
        start: TimeUs(start),
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
