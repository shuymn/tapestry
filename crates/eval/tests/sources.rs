//! Sources, the watermark over them, and the merge of their events.

mod common;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, disconnect, judgments, lane, press, release};
use eval::{
    Binding, ControlId, ConversionState, Evaluator, InputKind, InputStart, LaneEdge,
    PhysicalInputEvent, Published, SourceId, SourceState, StageError, Staged, merge, watermark,
};
use rules::Judgment::{Great, Perfect, Poor};

fn staged(source: u16, events: &[PhysicalInputEvent]) -> Staged {
    let mut s = Staged::with_capacity(SourceId(source), 16);
    for &e in events {
        s.push(e).unwrap();
    }
    s
}

#[test]
fn merge_tie_breaks_by_source_then_seq() {
    // Source 0 is 5 ms late, source 1 is not; their events cross in
    // evaluation time.
    let dev = device(&[5 * MS, 0]);
    let a = [
        press(10 * MS, 0, 1),   // evaluation 5 ms
        press(15 * MS, 0, 2),   // evaluation 10 ms
        release(15 * MS, 0, 1), // evaluation 10 ms, same batch
        press(40 * MS, 0, 3),   // evaluation 35 ms, beyond the merge bound
    ];
    let b = [
        press(5 * MS, 1, 4),  // evaluation 5 ms
        press(7 * MS, 1, 5),  // evaluation 7 ms
        press(10 * MS, 1, 6), // evaluation 10 ms
        press(30 * MS, 1, 7), // evaluation 30 ms, the bound itself
    ];
    // Staging order in the slice does not matter: source 1 comes first.
    let mut stages = [staged(1, &b), staged(0, &a)];
    let mut out = Vec::new();
    merge(&mut stages, &dev, TimeUs(30 * MS), &mut out);
    assert_eq!(
        out,
        [a[0], b[0], b[1], a[1], a[2], b[2], b[3]],
        "order is (evaluation time, source, order within source)"
    );
    assert_eq!((stages[0].len(), stages[1].len()), (0, 1));
    merge(&mut stages, &dev, TimeUs(35 * MS), &mut out);
    assert_eq!(out.last(), Some(&a[3]));
    assert!(stages.iter().all(Staged::is_empty));
}

#[test]
fn staging_refuses_rather_than_grows_or_mixes() {
    let mut s = Staged::with_capacity(SourceId(0), 1);
    assert_eq!(s.push(press(0, 1, 1)), Err(StageError::OtherSource));
    assert_eq!(s.push(press(0, 0, 1)), Ok(()));
    assert_eq!(s.push(press(1, 0, 1)), Err(StageError::Full));
}

#[test]
fn watermark_with_every_source_retired_is_none() {
    let dev = device(&[3 * MS, 0]);
    let mut published = [
        Published::open(TimeUs(20 * MS)),
        Published::closed(TimeUs(10 * MS)),
    ];
    // Offsets apply: min(20 − 3, 10 − 0).
    assert_eq!(watermark(&published, &dev), Some(TimeUs(10 * MS)));
    published[1].state = SourceState::Retired;
    assert_eq!(watermark(&published, &dev), Some(TimeUs(17 * MS)));
    published[0].state = SourceState::Retired;
    assert_eq!(watermark(&published, &dev), None);
}

#[test]
fn removed_source_is_drained_before_retiring() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let preset = rules::custom();
    let dev = device(&[0, 0]);
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), None, 8).unwrap();
    let mut out = Vec::new();
    let mut merged = Vec::with_capacity(8);

    // Source 1 pressed, then was removed at 1050 ms; source 0 has published
    // far ahead.
    let mut stages = [
        staged(0, &[]),
        staged(1, &[press(1_000 * MS, 1, 1), disconnect(1_050 * MS, 1)]),
    ];
    let mut published = [
        Published::open(TimeUs(3_000 * MS)),
        Published::closed(TimeUs(1_050 * MS)),
    ];

    // Not drained yet: the closed source still bounds the watermark.
    published[1].retire_if_drained(true, &stages[1]);
    assert_eq!(published[1].state, SourceState::Closed);
    let w = watermark(&published, &dev).unwrap();
    assert_eq!(w, TimeUs(1_050 * MS));
    merge(&mut stages, &dev, w, &mut merged);
    assert_eq!(merged.len(), 2);
    for e in merged.drain(..) {
        ev.push_input(e).unwrap();
    }
    ev.advance_to(w, &mut out).unwrap();
    assert_eq!(judgments(&out), [(1, 0, Perfect, Some(0))]);

    // Staging is empty, but the queue is not: still closed.
    published[1].retire_if_drained(false, &stages[1]);
    assert_eq!(published[1].state, SourceState::Closed);
    // An open source is never retired, drained or not.
    published[0].retire_if_drained(true, &stages[0]);
    assert_eq!(published[0].state, SourceState::Open);
    // Drained: retired, and the watermark follows the remaining source.
    published[1].retire_if_drained(true, &stages[1]);
    assert_eq!(published[1].state, SourceState::Retired);
    let w = watermark(&published, &dev).unwrap();
    assert_eq!(w, TimeUs(3_000 * MS));
    ev.advance_to(w, &mut out).unwrap();
    assert_eq!(
        judgments(&out),
        [(1, 0, Perfect, Some(0)), (2, 0, Poor, None)]
    );
}

#[test]
fn crossing_offsets_judge_in_evaluation_order() {
    // Source 1 is 30 ms late. Its press, received after source 0's, counts
    // earlier and takes the nearer note first.
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_040 * MS))
        .build()
        .unwrap();
    let preset = rules::custom();
    let dev = device(&[0, 30 * MS]);
    let mut stages = [
        staged(0, &[press(1_019 * MS, 0, 1), release(1_025 * MS, 0, 1)]),
        staged(1, &[press(1_030 * MS, 1, 1), release(1_031 * MS, 1, 1)]),
    ];
    let mut merged = Vec::new();
    merge(&mut stages, &dev, TimeUs(2_000 * MS), &mut merged);
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), None, 8).unwrap();
    for e in merged {
        ev.push_input(e).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(2_000 * MS), &mut out).unwrap();
    assert_eq!(
        judgments(&out),
        [(1, 0, Perfect, Some(0)), (1, 1, Great, Some(-21 * MS))]
    );
}

#[test]
fn lane_stays_held_while_any_control_holds_it() {
    // Source 0 binds a second control to lane 1; source 1 binds lane 1 too.
    let mut dev = device(&[0, 0]);
    dev.sources[0].bindings.push(Binding {
        control: ControlId(100),
        lane: lane(1),
    });
    let extra = |received, value| {
        let mut e = press(received, 0, 1);
        e.kind = InputKind::Control {
            control: ControlId(100),
            value,
        };
        e
    };
    let mut c = ConversionState::init(&dev, &InputStart::default()).unwrap();
    let l1 = lane(1);
    let mut step = |e: PhysicalInputEvent| c.apply(&e, e.received).as_slice().to_vec();
    assert_eq!(step(press(1, 0, 1)), [LaneEdge::Press(l1)]);
    assert_eq!(step(extra(2, 1)), []);
    // Releasing one of two held controls keeps the lane held.
    assert_eq!(step(release(3, 0, 1)), []);
    assert_eq!(step(extra(4, 0)), [LaneEdge::Release(l1)]);
    // A disconnect releases only its own source's controls.
    assert_eq!(step(press(5, 1, 1)), [LaneEdge::Press(l1)]);
    assert_eq!(step(press(6, 0, 1)), []);
    assert_eq!(step(disconnect(7, 1)), []);
    assert_eq!(step(press(8, 0, 1)), []);
    assert_eq!(step(release(9, 0, 1)), [LaneEdge::Release(l1)]);
    assert!(!c.is_held(l1));
}

#[test]
fn two_scratch_controls_held_across_a_retry_combine() {
    // The scratch's two directions on two controls of one source, both
    // held when the next play's recording opened: the lane starts held
    // with no press edge, stays held while either is, and releases with
    // the second.
    let mut dev = device(&[0]);
    dev.sources[0].bindings.push(Binding {
        control: ControlId(100),
        lane: lane(0),
    });
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![
            (SourceId(0), ControlId(0), 1),
            (SourceId(0), ControlId(100), 1),
        ],
    };
    let mut c = ConversionState::init(&dev, &start).unwrap();
    let s = lane(0);
    assert!(c.is_held(s));
    let set = |control, value| PhysicalInputEvent {
        received: TimeUs(1),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(control),
            value,
        },
    };
    let mut step = |e: PhysicalInputEvent| c.apply(&e, e.received).as_slice().to_vec();
    assert_eq!(step(set(100, 0)), [], "the other direction still holds it");
    assert_eq!(step(set(0, 0)), [LaneEdge::Release(s)]);
    assert_eq!(step(set(100, 1)), [LaneEdge::Press(s)]);
}
