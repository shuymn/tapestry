//! Measure positions to integer microseconds with exact rationals (D3).

use bms_model::{ScrollChange, TimeUs};
use num_traits::{CheckedAdd, CheckedDiv, CheckedMul, CheckedSub, Signed, Zero};

use crate::decimal::Rational;
use crate::parse::{ObjectKind, Source};
use crate::{Limits, LoadError, Warning, WarningKind};

/// The BPM assumed when `#BPM` is missing.
const DEFAULT_BPM: i128 = 130;
const US_PER_MINUTE: i128 = 60_000_000;
/// A STOP value counts 1/192 of a 4/4 measure, so 1/48 of a beat.
const STOP_UNITS_PER_BEAT: i128 = 48;

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
    /// Notes, long-note objects, mines and BGM, sorted by time, then by file
    /// order. Timing objects are consumed.
    pub objects: Vec<Timed>,
    pub initial_bpm_micro: i64,
    pub scroll: Vec<ScrollChange>,
    pub bar_lines: Vec<TimeUs>,
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
        bpm.value
    } else {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::MissingBpm,
        });
        Rational::from_integer(DEFAULT_BPM)
    };
    let cap = Rational::from_integer(i128::from(limits.max_duration.0));

    // Beat at the start of every measure, through the end of the last one.
    let last_measure = source
        .objects
        .iter()
        .map(|o| o.measure)
        .chain(source.measure_lengths.keys().copied())
        .max()
        .unwrap_or(0);
    let mut measure_start = Vec::with_capacity(usize::from(last_measure) + 2);
    measure_start.push(Rational::zero());
    for m in 0..=last_measure {
        let length = source
            .measure_lengths
            .get(&m)
            .map_or_else(|| Rational::from_integer(1), |l| l.value);
        let next = length
            .checked_mul(&Rational::from_integer(4))
            .and_then(|beats| measure_start[usize::from(m)].checked_add(&beats))
            .ok_or(LoadError::TimeOverflow { measure: m })?;
        measure_start.push(next);
    }

    let mut events = Vec::with_capacity(source.objects.len() + usize::from(last_measure) + 1);
    for m in 0..=last_measure {
        events.push((measure_start[usize::from(m)], Event::Bar(m)));
    }
    for (i, o) in source.objects.iter().enumerate() {
        let m = usize::from(o.measure);
        let pos = measure_start[m + 1]
            .checked_sub(&measure_start[m])
            .and_then(|span| {
                Rational::new(i128::from(o.index), i128::from(o.count)).checked_mul(&span)
            })
            .and_then(|offset| measure_start[m].checked_add(&offset))
            .ok_or(LoadError::TimeOverflow { measure: o.measure })?;
        events.push((pos, Event::Object(i)));
    }
    // Stable: equal positions keep bar lines first, then file order.
    events.sort_by_key(|e| e.0);

    let mut timeline = Timeline {
        objects: Vec::with_capacity(source.objects.len()),
        initial_bpm_micro: bpm_micro(initial_bpm, source.bpm.as_ref().map_or(0, |b| b.line))?,
        scroll: Vec::new(),
        bar_lines: Vec::with_capacity(usize::from(last_measure) + 1),
    };
    let mut bpm = initial_bpm;
    let mut beat = Rational::zero();
    let mut time = Rational::zero();
    let mut rest = events.as_slice();
    while let Some(&(pos, _)) = rest.first() {
        let len = rest.iter().take_while(|e| e.0 == pos).count();
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
        let overflow = LoadError::TimeOverflow { measure };

        time = pos
            .checked_sub(&beat)
            .and_then(|d| d.checked_mul(&Rational::from_integer(US_PER_MINUTE)))
            .and_then(|d| d.checked_div(&bpm))
            .and_then(|d| time.checked_add(&d))
            .ok_or(overflow.clone())?;
        beat = pos;
        let at = to_micros(time, cap, measure)?;

        // A BPM change applies first; channel 08 wins over 03; the later
        // object in the file wins within a channel.
        let pick = |want_ref: bool| {
            objects().filter_map(move |o| match o.kind {
                ObjectKind::BpmRef(id) if want_ref => {
                    Some((o.line, source.bpm_defs.get(&id).copied()))
                }
                ObjectKind::BpmHex(v) if !want_ref => {
                    Some((o.line, Some(Rational::from_integer(i128::from(v)))))
                }
                _ => None,
            })
        };
        let mut new_bpm = None;
        for (l, value) in pick(false).chain(pick(true)) {
            match value {
                Some(v) => new_bpm = Some((l, v)),
                None => warnings.push(Warning {
                    line: Some(l),
                    kind: WarningKind::UndefinedReference,
                }),
            }
        }
        if let Some((l, v)) = new_bpm {
            bpm = v;
            timeline.scroll.push(ScrollChange::Bpm {
                at,
                bpm_micro: bpm_micro(v, l)?,
            });
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
                    Some(v) => units = units.checked_add(v).ok_or(overflow.clone())?,
                    None => warnings.push(Warning {
                        line: Some(o.line),
                        kind: WarningKind::UndefinedReference,
                    }),
                }
            }
        }
        if units.is_positive() {
            let length = units
                .checked_mul(&Rational::from_integer(US_PER_MINUTE))
                .and_then(|d| d.checked_div(&Rational::from_integer(STOP_UNITS_PER_BEAT)))
                .and_then(|d| d.checked_div(&bpm))
                .ok_or(overflow.clone())?;
            time = time.checked_add(&length).ok_or(overflow)?;
            let end = to_micros(time, cap, measure)?;
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

/// Floors a non-negative time to microseconds after checking the cap, so
/// nothing is sized from a time past it.
fn to_micros(time: Rational, cap: Rational, measure: u16) -> Result<TimeUs, LoadError> {
    if time > cap {
        return Err(LoadError::DurationExceeded { measure });
    }
    i64::try_from(time.floor().to_integer())
        .map(TimeUs)
        .map_err(|_| LoadError::TimeOverflow { measure })
}

/// A BPM in millionths; exact because BPM text has at most 6 fractional
/// digits.
fn bpm_micro(bpm: Rational, line: usize) -> Result<i64, LoadError> {
    let micro = bpm * Rational::from_integer(1_000_000);
    if !micro.is_integer() {
        return Err(LoadError::InvalidValue {
            line,
            what: "BPM",
            reason: "more than 6 fractional digits",
        });
    }
    i64::try_from(micro.to_integer()).map_err(|_| LoadError::InvalidValue {
        line,
        what: "BPM",
        reason: "too large",
    })
}
