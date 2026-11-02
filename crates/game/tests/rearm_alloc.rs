//! Repeated READY rearms allocate nothing and hold memory flat (design.md
//! "Scope of No Allocation", "Play Lifecycle", the start gate): each
//! candidate judges early input, a START hold cancels it through the
//! input side's latch, its popped and queued events go into the history,
//! the readers are lent back to the input side, which seals the opening and
//! opens the next candidate once START is released, and the logic step,
//! its effects and its audio sink are rearmed in place
//! (`Logic::rearm`, `SourceSet::reopen_candidate`,
//! `HistoryRecorder::{begin, take, drain, seal}`). This file holds a single
//! test, so the harness's other threads are not tracked.

mod common;
mod counting;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, lane};
use eval::ControlId;
use game::{DeviceSettings, HistoryRecorder, Logic, LogicConfig, audio_link_at};
use input::{
    AdjustmentBindings, DeviceId, DeviceKind, GatePhase, Packet, SourceConfig, SourceSet,
    TapControl, adjustment_queue,
};
use replay::CancelReason;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const Q: ControlId = ControlId(0x10);

#[test]
fn repeated_rearms_allocate_nothing_and_hold_memory_flat() {
    let keys = DeviceSettings::default().keyboard;
    let key1 = ControlId(keys.lanes.iter().find(|k| k.lane == 1).unwrap().control);
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(0))
        .tap(lane(1), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 2,
        max_held: 8,
        queue_capacity: 4_096,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(1), keys.recorded());
    set.end_batch(TimeUs(1));
    let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![(TapControl::Key(Q), 0)],
        latched: 1,
        wrap: None,
    })
    .unwrap();
    tap.set_active(true);
    let gate = tap.start_gate();
    set.set_adjustment(tap);

    let lead_in = 15 * MS;
    let mut at = 10;
    let mut origin = at + lead_in;
    let mut generation = 1;
    let start = set
        .open_candidate(TimeUs(at), TimeUs(origin), generation)
        .unwrap()
        .clone();
    let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[Vec::new()]);
    let (mut logic, mut side) = Logic::new(
        &chart,
        &rule,
        &device,
        &start,
        set.take_readers(),
        LogicConfig {
            inputs: 4_096,
            effects: 1_024,
        },
        Vec::new(),
    )
    .unwrap();
    let (sink, mut sounds) = audio_link_at(logic.max_events(), TimeUs(origin));
    logic.add_sink(Box::new(sink));
    let held = set.snapshot_capacity();
    logic.reserve_start(held);
    let mut next = start.clone();
    next.held.reserve(held);
    let mut history = HistoryRecorder::new(held);
    let reserved = (history.reserved(), set.snapshot_capacity());
    let batch = |set: &mut SourceSet, at: i64, control: ControlId, value: i32| {
        set.begin_batch(TimeUs(at));
        set.packet(&Packet {
            device: DeviceId(1),
            control,
            value,
            background: false,
        });
        set.end_batch(TimeUs(at + 1));
    };

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut effects = 0;
    for round in 0..60 {
        // The candidate judges a press early in its READY; every other
        // candidate's effects are left queued for the screen when it is
        // cancelled.
        for k in 0..3 {
            at += 5 * MS;
            batch(&mut set, at, key1, i32::from(k % 2 == 0));
            logic.advance_to(TimeUs(at - origin));
        }
        if round % 2 == 0 {
            side.drain_effects(|_| effects += 1);
        }
        // START held: the input side cancels the candidate.
        at += MS;
        batch(&mut set, at, Q, 1);
        assert_eq!(gate.state().phase, GatePhase::Cancelled);
        assert!(!gate.commit(generation));
        history.begin(logic.start(), TimeUs(origin)).unwrap();
        history.take(logic.popped()).unwrap();
        history.drain(logic.readers_mut()).unwrap();
        // A press while waiting is kept, and the release rearms.
        at += MS;
        batch(&mut set, at, key1, 0);
        at += MS;
        batch(&mut set, at, Q, 0);
        history.drain(logic.readers_mut()).unwrap();
        history
            .seal(TimeUs(at - origin), set.arrivals(), CancelReason::Held)
            .unwrap();
        set.return_readers(logic.lend_readers());
        origin = at + lead_in;
        generation += 1;
        let s = set
            .reopen_candidate(TimeUs(at), TimeUs(origin), generation)
            .unwrap();
        next.opened_at = s.opened_at;
        next.held.clear();
        next.held.extend_from_slice(&s.held);
        logic.restore_readers(set.take_readers());
        logic.rearm(&next, TimeUs(origin));
        while sounds.commands().pop().is_ok() {}
        // The cancelled candidate's effects never reach the screen.
        side.drain_effects(|_| panic!("a cancelled candidate's effect"));
        if history.len() == replay::MAX_CANCELLED_OPENINGS {
            history.clear();
        }
    }
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0);
    assert!(effects > 0, "each candidate judged");
    assert_eq!(side.rearms(), 60);
    assert_eq!((history.reserved(), set.snapshot_capacity()), reserved);
}
