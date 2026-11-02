//! The attempt's cancelled READY openings kept from the input side's
//! reused source table, and the record they make: origins against the
//! last opening, ranges tiling the arenas, a played record and a capture
//! cancelled before playback that the decoder takes back, and an arena
//! running out sealed rather than truncated.

mod common;

use bms_model::TimeUs;
use common::{control, device, lane};
use eval::{InputKind, InputStart, SourceId};
use game::{HistoryFull, HistoryRecorder, Reservation};
use input::{DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};
use replay::{
    BuildProvenance, CancelReason, ChartRecord, DecodeLimits, DeviceKind as Kind, DeviceRecord,
    EndStatus, Incomplete, MAX_CANCELLED_EVENTS, MAX_CANCELLED_OPENINGS, PlayRecord, PlayType,
    PresetRecord, ReadyHistory, ReceiveMethod, SourceRecord, Status, Versions, WakePolicy,
};
use rules::Implementation;

fn set(queue_capacity: usize) -> SourceSet {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 4,
        queue_capacity,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(1), vec![control(lane(1))]);
    set.end_batch(TimeUs(1));
    set
}

fn press(set: &mut SourceSet, at: i64, value: i32) {
    set.begin_batch(TimeUs(at));
    set.packet(&Packet {
        device: DeviceId(1),
        control: control(lane(1)),
        value,
        background: false,
    });
    set.end_batch(TimeUs(at + 1));
}

fn record(
    history: ReadyHistory,
    played: Option<(InputStart, Vec<eval::PhysicalInputEvent>)>,
) -> PlayRecord {
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let (end, finalized, (start, events)) = match played {
        Some(winner) => (EndStatus::EndedMidway, TimeUs(0), winner),
        None => (
            EndStatus::CancelledBeforePlayback,
            TimeUs(0),
            (InputStart::default(), Vec::new()),
        ),
    };
    PlayRecord {
        versions: Versions {
            evaluator: Implementation::STANDARD_VERSION,
            app: "0.0.0-test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(&preset),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: Kind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: dev.sources[0].clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
        start,
        events,
        arrivals: Vec::new(),
        status: Status {
            end,
            finalized,
            incomplete: Incomplete::default(),
        },
        history,
    }
}

#[test]
fn cancelled_openings_become_a_history_the_decoder_takes_back() {
    let mut set = set(64);
    let mut history = HistoryRecorder::new(8);
    // Three openings, the first two cancelled while key 1 was held.
    let mut origin = 1_000;
    let mut start = set.open_attempt(TimeUs(origin), TimeUs(origin)).clone();
    let mut readers = set.take_readers();
    for i in 0..2_i64 {
        press(&mut set, origin + 100, 1);
        press(&mut set, origin + 200 + i, 0);
        press(&mut set, origin + 300, 1);
        let closed = origin + 400;
        history
            .keep(
                &start,
                TimeUs(origin),
                TimeUs(closed - origin),
                &mut readers,
                set.arrivals(),
                CancelReason::Held,
            )
            .unwrap();
        set.return_readers(readers);
        origin = closed;
        start = set.reopen(TimeUs(closed), TimeUs(origin)).unwrap().clone();
        readers = set.take_readers();
    }
    assert_eq!(start.held, [(SourceId(0), control(lane(1)), 1)]);
    press(&mut set, origin + 50, 0);
    let winner: Vec<_> = std::iter::from_fn(|| readers[0].pop()).collect();
    assert_eq!(winner.len(), 1);

    let played = history.history(TimeUs(origin));
    let origins: Vec<_> = played.openings.iter().map(|o| o.origin.0).collect();
    assert_eq!(origins, [-800, -400]);
    let ranges: Vec<_> = played.openings.iter().map(|o| o.events.clone()).collect();
    assert_eq!(ranges, [0..3, 3..6]);
    assert!(
        played
            .events
            .iter()
            .all(|e| matches!(e.kind, InputKind::Control { .. }))
    );
    assert_eq!(played.openings[1].start.held, start.held);
    let rec = record(played, Some((start, winner)));
    let back = replay::decode(&replay::encode(&rec), &DecodeLimits::STANDARD).unwrap();
    assert_eq!(back, rec);

    // Saved as cancelled before playback, the last kept opening is at 0.
    let last = history.last_origin().unwrap();
    let cancelled = record(history.history(last), None);
    assert_eq!(cancelled.history.openings[1].origin, TimeUs(0));
    let back = replay::decode(&replay::encode(&cancelled), &DecodeLimits::STANDARD).unwrap();
    assert!(!back.played());

    history.clear();
    assert!(history.is_empty());
    assert_eq!(history.history(TimeUs(0)), ReadyHistory::default());
}

#[test]
fn a_full_arena_seals_the_history_and_leaves_the_rest_unread() {
    let per_round = 1_100;
    let mut set = set(per_round);
    let mut history = HistoryRecorder::new(8);
    let start = set.open_attempt(TimeUs(0), TimeUs(0)).clone();
    let mut readers = set.take_readers();
    let open = |history: &mut HistoryRecorder, readers: &mut [input::SourceReader]| {
        history.keep(
            &start,
            TimeUs(0),
            TimeUs(10),
            readers,
            &[],
            CancelReason::Held,
        )
    };
    for _ in 0..MAX_CANCELLED_OPENINGS {
        open(&mut history, &mut readers).unwrap();
    }
    assert_eq!(open(&mut history, &mut readers), Err(HistoryFull::Openings));
    assert!(history.exhausted());
    assert_eq!(history.len(), MAX_CANCELLED_OPENINGS);

    // The events arena: what does not fit is left in its queue.
    history.clear();
    assert!(!history.exhausted());
    let mut kept = 0;
    let mut at = 10;
    let err = loop {
        for _ in 0..per_round / 2 {
            press(&mut set, at, 1);
            press(&mut set, at + 2, 0);
            at += 4;
        }
        match open(&mut history, &mut readers) {
            Ok(()) => kept += per_round,
            Err(e) => break e,
        }
    };
    assert_eq!(err, HistoryFull::Events);
    assert!(history.exhausted());
    let full = history.history(TimeUs(0));
    assert_eq!(full.events.len(), MAX_CANCELLED_EVENTS);
    let last = full.openings.last().unwrap().events.clone();
    assert_eq!(last, u32::try_from(kept).unwrap()..65_536);
    assert!(!readers[0].is_empty(), "nothing is dropped");

    let held = MAX_CANCELLED_OPENINGS + 1;
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), control(lane(1)), 1); held],
    };
    let mut small = HistoryRecorder::new(held - 1);
    assert_eq!(
        small.keep(
            &start,
            TimeUs(0),
            TimeUs(0),
            &mut [],
            &[],
            CancelReason::Held
        ),
        Err(HistoryFull::Held)
    );
}

#[test]
fn the_reservation_charges_the_arenas_actual_capacity() {
    let history = HistoryRecorder::new(40);
    assert_eq!(history.reserved(), HistoryRecorder::bytes(40));
    let base = Reservation {
        fixed: 1_000,
        per_input: 10,
    };
    let charged = base.with_history(40);
    assert_eq!(charged.fixed, 1_000 + HistoryRecorder::bytes(40));
    assert_eq!(charged.per_input, 10);
    // Bounded by the held snapshot's limit, whatever is asked.
    assert_eq!(
        HistoryRecorder::bytes(usize::MAX),
        HistoryRecorder::bytes(replay::MAX_HELD)
    );
}
