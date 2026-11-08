//! The history of cancelled READY openings and captures cancelled before
//! playback (design.md "Input Log Format", cancelled openings).

use std::ops::Range;

use bms_model::TimeUs;
use eval::{ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId};

use super::{control, join_sections, limits, sample, split_sections, wrap};
use crate::wire::Writer;
use crate::{
    Arrival, CancelReason, CancelledOpening, DecodeError, DeviceKind, EndStatus, PlayRecord,
    ReadyHistory, body_bytes, decode, encode, write_history,
};

/// The history section's index in the body.
const HISTORY: usize = crate::Section::History as usize;

fn opening(
    opened: i64,
    origin: i64,
    closed: i64,
    events: Range<u32>,
    arrivals: Range<u32>,
    reason: CancelReason,
) -> CancelledOpening {
    CancelledOpening {
        start: InputStart {
            opened_at: TimeUs(opened),
            held: Vec::new(),
        },
        origin: TimeUs(origin),
        closed: TimeUs(closed),
        events,
        arrivals,
        reason,
    }
}

/// Two openings cancelled by a hold before the winning one of
/// [`sample`], which opens at 5 s: the first 3 s before it, with a held
/// control, two events and an arrival, the second 1.5 s before it with
/// one event. On the winning opening's timeline they close at −2 s and
/// −0.5 s.
fn history() -> ReadyHistory {
    let mut first = opening(0, -3_000_000, 1_000_000, 0..2, 0..1, CancelReason::Held);
    first.start.held = vec![(SourceId(1), ControlId(0x21), 1)];
    ReadyHistory {
        openings: vec![
            first,
            opening(0, -1_500_000, 1_000_000, 2..3, 1..1, CancelReason::Held),
        ],
        events: vec![
            control(0, 100, 0x1e, 1),
            control(2, 200, 1, 1),
            control(1, 50, 0x20, 1),
        ],
        arrivals: vec![Arrival {
            at: TimeUs(700),
            kind: DeviceKind::Keyboard,
        }],
    }
}

fn played() -> PlayRecord {
    let mut rec = sample(9);
    rec.history = history();
    rec
}

/// A capture cancelled before playback: [`history`] with its last opening
/// quit at origin 0 and the first moved to keep its place.
fn cancelled() -> PlayRecord {
    let mut rec = sample(9);
    rec.history = history();
    rec.history.openings[0].origin = TimeUs(-1_500_000);
    rec.history.openings[1].origin = TimeUs(0);
    rec.history.openings[1].reason = CancelReason::Quit;
    rec.history.openings[1].events = 2..3;
    rec.start = InputStart::default();
    rec.events.clear();
    rec.arrivals.clear();
    rec.status.end = EndStatus::CancelledBeforePlayback;
    rec.start_tuning = None;
    rec.status.finalized = TimeUs(0);
    rec
}

#[test]
fn the_history_section_layout_is_pinned() {
    let mut w = Writer::default();
    write_history(&mut w, &history());
    assert_eq!(
        w.0,
        [
            2, // openings
            // The first: opened at 0, one held control (source 1, 0x21,
            // value 1), origin −3 s, closed at 1 s, events 0..2,
            // arrivals 0..1, held.
            0, 1, 1, 0x21, 2, //
            0xff, 0x9a, 0xee, 0x02, //
            0x80, 0x89, 0x7a, //
            0, 2, 0, 1, 0, //
            // The second: origin −1.5 s, events 2..3, arrivals 1..1.
            0, 0, //
            0xbf, 0x8d, 0xb7, 0x01, //
            0x80, 0x89, 0x7a, //
            2, 3, 1, 1, 0, //
            // Three events, each opening's from its own opening time.
            3, //
            0, 0xc8, 0x01, 0, 0x1e, 2, //
            2, 0x90, 0x03, 0, 1, 2, //
            1, 100, 0, 0x20, 2, //
            // One arrival at 700 µs, a keyboard.
            1, 0xf8, 0x0a, 0,
        ]
    );
}

#[test]
fn a_played_record_keeps_its_cancelled_openings() {
    let rec = played();
    assert!(rec.played());
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn a_capture_cancelled_before_playback_has_no_winning_opening() {
    let rec = cancelled();
    assert!(!rec.played());
    let body = body_bytes(&rec);
    let sections = split_sections(&body);
    // No start, tuning, events or arrivals, and the status holds no
    // watermark: the end tag and the reason bits (suspend).
    assert!(sections[5..9].iter().all(Vec::is_empty));
    assert_eq!(sections[9], [2, 4]);
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn a_cancelled_capture_with_a_winning_opening_is_refused() {
    let mut sections = split_sections(&body_bytes(&cancelled()));
    // A start snapshot: opened at 0, nothing held.
    sections[5] = vec![0, 0];
    assert_eq!(
        decode(&wrap(&join_sections(&sections), 3), &limits()),
        Err(DecodeError::TrailingBytes)
    );
}

#[test]
fn a_history_disagreeing_with_the_end_status_is_refused() {
    let invalid = DecodeError::InvalidHistory;
    type Change = fn(&mut PlayRecord);
    let cases: [(Change, DecodeError); 6] = [
        // A played record's openings were all cancelled by a hold.
        (
            |r| r.history.openings[1].reason = CancelReason::Quit,
            invalid("cancel reason"),
        ),
        // Its last cancelled origin lies before the winning one (0).
        (
            |r| r.history.openings[1].origin = TimeUs(0),
            invalid("origin"),
        ),
        // Its last cancelled opening closed before the winning one opened.
        (
            |r| r.history.openings[1].closed = TimeUs(6_500_001),
            invalid("chronology"),
        ),
        // A capture cancelled before playback has an opening.
        (
            |r| {
                *r = cancelled();
                r.history = ReadyHistory::default();
            },
            invalid("no opening"),
        ),
        // Its last opening is at origin 0.
        (
            |r| {
                *r = cancelled();
                r.history.openings[0].origin = TimeUs(-2_500_000);
                r.history.openings[1].origin = TimeUs(-1);
            },
            invalid("origin"),
        ),
        // Every opening but its last was cancelled by a hold.
        (
            |r| {
                *r = cancelled();
                r.history.openings[0].reason = CancelReason::Retry;
            },
            invalid("cancel reason"),
        ),
    ];
    for (i, (change, expected)) in cases.into_iter().enumerate() {
        let mut rec = played();
        change(&mut rec);
        assert_eq!(decode(&encode(&rec), &limits()), Err(expected), "case {i}");
    }
    // An unknown reason: one opening whose reason tag is 7.
    let mut sections = split_sections(&body_bytes(&played()));
    sections[HISTORY] = forged(|w| {
        w.uint(1);
        w.raw(&[0, 0]);
        w.int(-1);
        w.int(0);
        w.raw(&[0, 0, 0, 0]);
        w.uint(7);
        w.raw(&[0, 0]);
    });
    assert_eq!(
        decode(&wrap(&join_sections(&sections), 3), &limits()),
        Err(DecodeError::InvalidValue("cancel reason"))
    );
}

/// A history payload written by `write`.
fn forged(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::default();
    write(&mut w);
    w.0
}

/// One opening at `origin`, opened at 0 and closed at `closed`, with the
/// given ranges and a hold as its reason; no held control.
fn opening_bytes(w: &mut Writer, origin: i64, closed: i64, events: [u64; 2], arrivals: [u64; 2]) {
    w.raw(&[0, 0]);
    w.int(origin);
    w.int(closed);
    for v in events.into_iter().chain(arrivals) {
        w.uint(v);
    }
    w.uint(0);
}

#[test]
fn invalid_ranges_origins_and_counts_are_refused_before_allocating() {
    let sections = split_sections(&body_bytes(&sample(9)));
    let huge = u64::MAX >> 4;
    let invalid = DecodeError::InvalidHistory;
    let cases: Vec<(&str, Vec<u8>, DecodeError)> = vec![
        (
            "openings beyond the limit",
            forged(|w| w.uint(65)),
            DecodeError::TooManyOpenings(65),
        ),
        (
            "openings beyond the limit, huge",
            forged(|w| w.uint(huge)),
            DecodeError::TooManyOpenings(huge),
        ),
        (
            "openings beyond the data",
            forged(|w| w.uint(64)),
            DecodeError::CountBeyondData(64),
        ),
        (
            "an event range not starting where the last ended",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [1, 1], [0, 0]);
            }),
            invalid("range"),
        ),
        (
            "an event range ending before it starts",
            forged(|w| {
                w.uint(2);
                opening_bytes(w, -2, 0, [0, 2], [0, 0]);
                opening_bytes(w, -1, 0, [2, 1], [0, 0]);
            }),
            invalid("range"),
        ),
        (
            "an arrival range leaving a gap",
            forged(|w| {
                w.uint(2);
                opening_bytes(w, -2, 0, [0, 0], [0, 1]);
                opening_bytes(w, -1, 0, [0, 0], [2, 2]);
            }),
            invalid("range"),
        ),
        (
            "events beyond the limit",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 65_537], [0, 0]);
            }),
            DecodeError::TooManyHistoryEvents(65_537),
        ),
        (
            "arrivals beyond one opening's limit",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 0], [0, 17]);
            }),
            DecodeError::TooManyHistoryEvents(17),
        ),
        (
            "an event count other than the ranges'",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 1], [0, 0]);
                w.uint(2);
            }),
            invalid("event count"),
        ),
        (
            "events within the limit beyond the data",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 65_536], [0, 0]);
                w.uint(65_536);
            }),
            DecodeError::CountBeyondData(65_536),
        ),
        (
            "an arrival count other than the ranges'",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 0], [0, 1]);
                w.uint(0);
                w.uint(0);
            }),
            invalid("arrival count"),
        ),
        (
            "an arrival count beyond the ranges', checked before the data",
            forged(|w| {
                w.uint(0);
                w.uint(0);
                w.uint(huge);
            }),
            invalid("arrival count"),
        ),
        (
            "an origin repeated",
            forged(|w| {
                w.uint(2);
                opening_bytes(w, -1, 0, [0, 0], [0, 0]);
                opening_bytes(w, -1, 0, [0, 0], [0, 0]);
            }),
            invalid("origin"),
        ),
        (
            "an opening closed before it opened",
            forged(|w| {
                w.uint(1);
                w.int(5);
                w.uint(0);
                w.int(-10);
                w.int(4);
                w.raw(&[0, 0, 0, 0, 0]);
            }),
            invalid("closing time"),
        ),
        (
            "an opening overlapping the next",
            forged(|w| {
                w.uint(2);
                opening_bytes(w, -10, 6, [0, 0], [0, 0]);
                opening_bytes(w, -5, 0, [0, 0], [0, 0]);
            }),
            invalid("chronology"),
        ),
        (
            "a held control of an unknown source",
            forged(|w| {
                w.uint(1);
                w.raw(&[0, 1, 3, 1, 2]);
                w.int(-1);
                w.raw(&[0, 0, 0, 0, 0, 0]);
            }),
            DecodeError::UnknownSource(3),
        ),
        (
            "held controls beyond the data",
            forged(|w| {
                w.uint(1);
                w.uint(0);
                w.uint(huge);
            }),
            DecodeError::CountBeyondData(huge),
        ),
        (
            "an event before the last of its source",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 10, [0, 2], [0, 0]);
                w.uint(2);
                w.raw(&[0, 8, 0, 1, 2]);
                w.raw(&[0, 1, 0, 1, 2]);
                w.uint(0);
            }),
            invalid("event time"),
        ),
        (
            "an event after its opening closed",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 10, [0, 1], [0, 0]);
                w.uint(1);
                w.raw(&[0, 22, 0, 1, 2]);
                w.uint(0);
            }),
            invalid("event time"),
        ),
        (
            "an event wrapping past the largest time",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 10, [0, 2], [0, 0]);
                w.uint(2);
                w.raw(&[0, 10, 0, 1, 2]);
                w.raw(&[0]);
                w.int(i64::MAX);
                w.raw(&[0, 1, 2]);
                w.uint(0);
            }),
            invalid("event time"),
        ),
        (
            "an arrival before its opening opened",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 10, [0, 0], [0, 1]);
                w.uint(0);
                w.uint(1);
                w.int(-1);
                w.uint(0);
            }),
            invalid("arrival time"),
        ),
        (
            "an arrival after its opening closed",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 10, [0, 0], [0, 1]);
                w.uint(0);
                w.uint(1);
                w.int(11);
                w.uint(0);
            }),
            invalid("arrival time"),
        ),
        (
            "an event of an unknown source",
            forged(|w| {
                w.uint(1);
                opening_bytes(w, -1, 0, [0, 1], [0, 0]);
                w.uint(1);
                w.raw(&[3, 0, 0, 1, 2]);
            }),
            DecodeError::UnknownSource(3),
        ),
    ];
    for (what, payload, expected) in cases {
        let mut forged = sections.clone();
        forged[HISTORY] = payload;
        assert_eq!(
            decode(&wrap(&join_sections(&forged), 3), &limits()),
            Err(expected),
            "{what}"
        );
    }
}

#[test]
fn a_full_history_decodes() {
    // 64 openings, 65,536 events and 16 arrivals each, the most a history
    // holds.
    let mut rec = sample(9);
    let per = 1024_u32;
    rec.history.openings = (0..64)
        .map(|i| {
            opening(
                0,
                -64_000_000 + i64::from(i) * 1_000_000,
                i64::from(per),
                i * per..(i + 1) * per,
                i * 16..(i + 1) * 16,
                CancelReason::Held,
            )
        })
        .collect();
    rec.history.events = (0..64 * per)
        .map(|i| PhysicalInputEvent {
            received: TimeUs(i64::from(i % per)),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x1e),
                value: i32::from(i % 2 == 0),
            },
        })
        .collect();
    rec.history.arrivals = vec![
        Arrival {
            at: TimeUs(5),
            kind: DeviceKind::Controller,
        };
        64 * 16
    ];
    assert!(body_bytes(&rec).len() < crate::HISTORY_MAX_BYTES + crate::RECORD_OVERHEAD_BYTES);
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn a_record_from_before_the_ready_history_is_refused() {
    // Format 7 had no history and no cancelled status; its files are
    // refused by their version, with no conversion.
    let mut old = encode(&sample(9));
    old[4..6].copy_from_slice(&7_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(7))
    );
}
