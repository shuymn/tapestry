//! The start gate on the input side (design.md "Play Lifecycle", the
//! start gate): a READY candidate opens only where START and SELECT are
//! verifiably released, the input thread cancels the armed candidate as it
//! accepts a hold, and exactly one of the commit and the cancellation wins
//! by generation.

use bms_model::TimeUs;
use eval::ControlId;
use input::{
    AdjustmentBindings, DeviceId, DeviceKind, GatePhase, GateState, Packet, ReopenError,
    SourceConfig, SourceSet, StartGate, TapControl, adjustment_queue,
};

const Q: ControlId = ControlId(0x10);
const W: ControlId = ControlId(0x11);
const A: ControlId = ControlId(0x1e);

fn packet(control: ControlId, value: i32) -> Packet {
    Packet {
        device: DeviceId(1),
        control,
        value,
        background: false,
    }
}

/// A keyboard recording `A`, a tap with START on Q and SELECT on W, and
/// the tap's start gate.
fn rig(max_held: usize) -> (SourceSet, StartGate) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(1), vec![A]);
    set.end_batch(TimeUs(1));
    let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![(TapControl::Key(Q), 0), (TapControl::Key(W), 1)],
        latched: 0b11,
        wrap: None,
    })
    .unwrap();
    tap.set_active(true);
    let gate = tap.start_gate();
    set.set_adjustment(tap);
    (set, gate)
}

fn batch(set: &mut SourceSet, at: i64, packets: &[Packet]) {
    set.begin_batch(TimeUs(at));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(at + 1));
}

const fn state(generation: u32, phase: GatePhase) -> GateState {
    GateState { generation, phase }
}

#[test]
fn one_of_commit_and_cancel_wins_by_generation() {
    let gate = StartGate::new();
    assert_eq!(gate.state(), state(0, GatePhase::Idle));
    assert!(!gate.commit(0), "an idle gate commits nothing");
    assert_eq!(gate.cancel(), None);
    gate.arm(3);
    assert!(!gate.commit(2), "a stale generation commits nothing");
    assert!(!gate.commit(4));
    assert!(gate.commit(3));
    assert_eq!(
        gate.cancel(),
        None,
        "a hold after the commit cancels nothing"
    );
    assert_eq!(gate.state(), state(3, GatePhase::Committed));
    gate.arm(4);
    assert_eq!(gate.cancel(), Some(4));
    assert!(!gate.commit(4), "the cancellation won");
    assert_eq!(gate.state(), state(4, GatePhase::Cancelled));
    gate.disarm();
    assert_eq!(gate.state(), state(0, GatePhase::Idle));
}

#[test]
fn a_held_role_defers_the_opening_and_a_release_opens_and_arms() {
    let (mut set, gate) = rig(4);
    batch(&mut set, 10, &[packet(Q, 1)]);
    assert!(set.holding());
    assert!(set.open_candidate(TimeUs(20), TimeUs(1_020), 1).is_none());
    assert!(!set.is_recording(), "a deferred opening opens nothing");
    assert_eq!(gate.state().phase, GatePhase::Idle);
    batch(&mut set, 30, &[packet(Q, 0)]);
    assert!(!set.holding());
    let start = set.open_candidate(TimeUs(40), TimeUs(1_040), 1).unwrap();
    assert_eq!(start.opened_at, TimeUs(-1_000));
    assert_eq!(gate.state(), state(1, GatePhase::Armed));
}

#[test]
fn the_input_thread_cancels_the_armed_candidate_as_it_accepts_a_hold() {
    let (mut set, gate) = rig(4);
    set.open_candidate(TimeUs(10), TimeUs(1_010), 1).unwrap();
    // A recorded lane key and a release cancel nothing.
    batch(&mut set, 20, &[packet(A, 1), packet(A, 0)]);
    assert_eq!(gate.state(), state(1, GatePhase::Armed));
    // SELECT pressed: the candidate is cancelled within that batch.
    set.begin_batch(TimeUs(30));
    set.packet(&packet(W, 1));
    assert_eq!(gate.state(), state(1, GatePhase::Cancelled));
    set.end_batch(TimeUs(31));
    assert!(!gate.commit(1), "the transport finds it cancelled");
}

#[test]
fn a_hold_after_the_commit_is_an_ordinary_press() {
    let (mut set, gate) = rig(4);
    set.open_candidate(TimeUs(10), TimeUs(1_010), 1).unwrap();
    assert!(gate.commit(1));
    batch(&mut set, 1_020, &[packet(Q, 1)]);
    assert_eq!(gate.state(), state(1, GatePhase::Committed));
}

#[test]
fn an_unverified_latch_defers_and_cancels() {
    // One held entry: a second held key does not fit.
    let (mut set, gate) = rig(1);
    set.open_candidate(TimeUs(10), TimeUs(1_010), 1).unwrap();
    batch(&mut set, 20, &[packet(A, 1), packet(ControlId(0x1f), 1)]);
    assert_eq!(gate.state(), state(1, GatePhase::Cancelled));
    assert!(set.holding());
    assert_eq!(
        set.reopen_candidate(TimeUs(30), TimeUs(1_030), 2).err(),
        Some(ReopenError::Held)
    );
}

#[test]
fn a_rearm_waits_for_the_release_and_arms_a_fresh_generation() {
    let (mut set, gate) = rig(4);
    set.open_candidate(TimeUs(10), TimeUs(1_010), 1).unwrap();
    let mut readers = set.take_readers();
    batch(&mut set, 20, &[packet(A, 1), packet(Q, 1)]);
    assert_eq!(gate.state(), state(1, GatePhase::Cancelled));
    for r in &mut readers {
        while r.pop().is_some() {}
    }
    set.return_readers(readers);
    assert_eq!(
        set.reopen_candidate(TimeUs(30), TimeUs(1_030), 2).err(),
        Some(ReopenError::Held)
    );
    assert_eq!(
        gate.state(),
        state(1, GatePhase::Cancelled),
        "nothing changed"
    );
    batch(&mut set, 40, &[packet(Q, 0)]);
    let start = set.reopen_candidate(TimeUs(50), TimeUs(1_050), 2).unwrap();
    // A fresh snapshot on the new timeline: A is still held.
    assert_eq!(start.opened_at, TimeUs(-1_000));
    assert_eq!(start.held, vec![(eval::SourceId(0), A, 1)]);
    assert_eq!(gate.state(), state(2, GatePhase::Armed));
    assert!(
        !gate.commit(1),
        "the cancelled generation's authorization is stale"
    );
    assert!(gate.commit(2));
    set.close_recording();
    assert_eq!(gate.state(), state(0, GatePhase::Idle));
}
