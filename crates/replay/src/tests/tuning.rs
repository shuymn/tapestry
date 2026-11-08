//! The starting tuning (design.md "Input Log Format"): its section, its
//! value checks, and its presence exactly when the record played.

use super::{golden, join_sections, limits, sample, split_sections, wrap};
use crate::{
    CoverRecord, DecodeError, EndStatus, HiSpeedRatio, PlayRecord, Section, StartTuning,
    body_bytes, decode, encode,
};

/// A change to the starting tuning.
type Change = fn(&mut StartTuning);

/// `sample`'s record with its starting tuning changed by `change`.
fn with(change: impl FnOnce(&mut StartTuning)) -> PlayRecord {
    let mut rec = sample(3);
    let mut t = StartTuning::default();
    change(&mut t);
    rec.start_tuning = Some(t);
    rec
}

fn decoded(rec: &PlayRecord) -> Result<PlayRecord, DecodeError> {
    decode(&encode(rec), &limits())
}

#[test]
fn the_default_and_every_boundary_round_trip() {
    let cases: [Change; 11] = [
        |_| {},
        |t| t.classic_hispeed_x100 = 1,
        |t| t.classic_hispeed_x100 = 2000,
        |t| t.classic_step_x100 = 1,
        |t| t.classic_step_x100 = 100,
        |t| t.floating_step_x100 = 5,
        |t| t.floating_step_x100 = 100,
        |t| t.green_target = 1,
        |t| t.green_target = 6000,
        |t| {
            t.lift = CoverRecord {
                amount: 1000,
                on: false,
            };
        },
        |t| {
            t.hispeed = Some(HiSpeedRatio { num: 1, den: 100 });
            t.target = Some(crate::TuningCover::Hidden);
        },
    ];
    for change in cases {
        let rec = with(change);
        assert_eq!(decoded(&rec), Ok(rec));
    }
    let rec = with(|t| t.hispeed = Some(HiSpeedRatio { num: 20, den: 1 }));
    assert_eq!(decoded(&rec), Ok(rec));
}

#[test]
fn a_value_off_its_range_or_grid_is_refused() {
    let invalid = DecodeError::InvalidValue;
    let cases: [(Change, &str); 16] = [
        (|t| t.classic_hispeed_x100 = 0, "classic hi-speed"),
        (|t| t.classic_hispeed_x100 = 2001, "classic hi-speed"),
        (|t| t.classic_step_x100 = 0, "classic step"),
        (|t| t.classic_step_x100 = 101, "classic step"),
        (|t| t.floating_step_x100 = 0, "floating step"),
        (|t| t.floating_step_x100 = 105, "floating step"),
        // In range, off the grid of fives.
        (|t| t.floating_step_x100 = 7, "floating step"),
        (|t| t.green_target = 0, "green target"),
        (|t| t.green_target = 6001, "green target"),
        (|t| t.sudden.amount = 1001, "cover amount"),
        (|t| t.hidden.amount = 1001, "cover amount"),
        // A zero denominator.
        (
            |t| t.hispeed = Some(HiSpeedRatio { num: 1, den: 0 }),
            "hi-speed",
        ),
        // Not in lowest terms.
        (
            |t| t.hispeed = Some(HiSpeedRatio { num: 4, den: 2 }),
            "hi-speed",
        ),
        // Below 0.01 and above 20.
        (
            |t| t.hispeed = Some(HiSpeedRatio { num: 1, den: 101 }),
            "hi-speed",
        ),
        (
            |t| t.hispeed = Some(HiSpeedRatio { num: 21, den: 1 }),
            "hi-speed",
        ),
        (
            |t| t.hispeed = Some(HiSpeedRatio { num: 0, den: 1 }),
            "hi-speed",
        ),
    ];
    for (change, field) in cases {
        assert_eq!(decoded(&with(change)), Err(invalid(field)), "{field}");
    }
}

#[test]
fn an_unknown_tag_is_refused() {
    let sections = split_sections(&body_bytes(&sample(3)));
    // The default tuning's section: Floating, START, 100, 25, 50, 300,
    // three covers off, no target, no multiplier.
    let base: Vec<u8> = vec![1, 0, 100, 25, 50, 0xac, 0x02, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(sections[Section::Tuning as usize], base);
    let cases = [
        (0, 2, "speed mode"),
        (1, 4, "hi-speed reference"),
        (8, 2, "cover switch"),
        (13, 4, "cover target"),
        (14, 2, "hi-speed"),
    ];
    for (at, tag, field) in cases {
        let mut forged = sections.clone();
        let mut t = base.clone();
        t[at] = tag;
        forged[Section::Tuning as usize] = t;
        assert_eq!(
            decode(&wrap(&join_sections(&forged), 3), &limits()),
            Err(DecodeError::InvalidValue(field)),
            "{field}"
        );
    }
}

#[test]
fn a_played_record_without_its_tuning_section_is_refused() {
    let mut sections = split_sections(&body_bytes(&sample(3)));
    sections[Section::Tuning as usize].clear();
    assert_eq!(
        decode(&wrap(&join_sections(&sections), 3), &limits()),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn a_capture_cancelled_before_playback_writes_and_reads_no_tuning() {
    let mut rec = golden();
    rec.status.end = EndStatus::CancelledBeforePlayback;
    rec.status.finalized = bms_model::TimeUs(0);
    rec.start = eval::InputStart::default();
    rec.events.clear();
    rec.arrivals.clear();
    // A tuning given with no played attempt is not written.
    let mut sections = split_sections(&body_bytes(&rec));
    assert!(sections[Section::Tuning as usize].is_empty());
    // A tuning section on such a record is refused.
    sections[Section::Tuning as usize] =
        split_sections(&body_bytes(&golden()))[Section::Tuning as usize].clone();
    assert_eq!(
        decode(&wrap(&join_sections(&sections), 3), &limits()),
        Err(DecodeError::TrailingBytes)
    );
}

#[test]
#[should_panic(expected = "a played record has its starting tuning")]
fn a_played_record_must_have_its_tuning() {
    let mut rec = sample(3);
    rec.start_tuning = None;
    let _ = body_bytes(&rec);
}

#[test]
fn a_record_of_format_8_is_refused() {
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&8_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(8))
    );
}
