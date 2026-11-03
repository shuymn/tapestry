//! The adjustment tap's markers and recoveries (design.md "Play
//! Adjustments", order and ownership): an opening, a cancelling hold and
//! a fence in the order the input thread decides them, a loss recovered
//! once however long the consumer stalls, the FIFO bounded, openings
//! revalidated against the presentation focus, the owed wake, the repeat
//! deadlines, and the window's F1 and F2.

use bms_model::TimeUs;
use eval::ControlId;
use input::{
    ADJUST_QUEUE, AdjustmentBindings, AdjustmentReceiver, DeviceId, DeviceKind, Packet,
    PresentationFocus, RESERVED, ReopenError, SourceConfig, SourceSet, TapControl, TapEvent,
    WINDOW_KEYS, WindowKeyEvent, adjustment_queue, window_keys,
};

const Q: ControlId = ControlId(0x10);
const W: ControlId = ControlId(0x11);
const SHIFT: ControlId = ControlId(0x2a);
const A: ControlId = ControlId(0x1e);
const F1: ControlId = ControlId(0x3b);
const F2: ControlId = ControlId(0x3c);
const START: u8 = 0;
const SELECT: u8 = 1;
const SCRATCH: u8 = 2;
const SLOWER: u8 = 3;
const FASTER: u8 = 4;

fn packet(control: ControlId, value: i32) -> Packet {
    Packet {
        device: DeviceId(1),
        control,
        value,
        background: false,
    }
}

/// A keyboard recording `A`; a tap with START on Q, SELECT on W, a
/// repeating scratch direction on Left Shift every 50 ms, and F1 and F2;
/// the presentation focus.
fn rig() -> (SourceSet, AdjustmentReceiver, PresentationFocus) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    let focus = PresentationFocus::new();
    set.set_presentation_focus(focus.clone());
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(1), vec![A]);
    set.end_batch(TimeUs(1));
    let (mut tap, rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![
            (TapControl::Key(Q), START),
            (TapControl::Key(W), SELECT),
            (TapControl::Key(SHIFT), SCRATCH),
            (TapControl::Key(F1), SLOWER),
            (TapControl::Key(F2), FASTER),
        ],
        latched: (1 << START) | (1 << SELECT),
        wrap: None,
        repeating: 1 << SCRATCH,
        repeat_us: 50_000,
    })
    .unwrap();
    tap.set_active(true);
    set.set_adjustment(tap);
    (set, rx, focus)
}

fn batch(set: &mut SourceSet, at: i64, packets: &[Packet]) {
    set.begin_batch(TimeUs(at));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(at + 1));
}

fn drain(rx: &mut AdjustmentReceiver) -> Vec<TapEvent> {
    std::iter::from_fn(|| rx.pop()).map(|o| o.event).collect()
}

/// Fills the queue with ticks of a held START until only the reserved
/// slots are left; returns the time after.
fn fill(set: &mut SourceSet, from: i64) -> i64 {
    batch(set, from, &[packet(Q, 1)]);
    let mut at = from;
    for _ in 0..ADJUST_QUEUE {
        at += 1;
        set.tick_presentation(TimeUs(at));
    }
    at
}

const fn markers(e: &TapEvent) -> bool {
    !matches!(e, TapEvent::Tick)
}

#[test]
fn an_opening_and_a_cancelling_hold_are_marked_in_order() {
    let (mut set, mut rx, _) = rig();
    batch(&mut set, 10, &[]);
    set.begin_batch(TimeUs(20));
    assert!(set.open_candidate(TimeUs(20), TimeUs(2_020), 7).is_some());
    set.end_batch(TimeUs(21));
    batch(&mut set, 30, &[packet(Q, 1)]);
    let events: Vec<_> = drain(&mut rx).into_iter().filter(markers).collect();
    assert_eq!(
        events,
        [
            TapEvent::Resync {
                held: 0,
                verified: true
            },
            TapEvent::Opened {
                generation: 7,
                origin: TimeUs(2_020)
            },
            // The hold, then the cancellation it caused.
            TapEvent::Press(START),
            TapEvent::Cancelled { generation: 7 },
        ]
    );
}

#[test]
fn opened_carries_the_presentation_clock_under_an_offset() {
    let (mut set, mut rx, _) = rig();
    set.set_presentation_offset(TimeUs(1_000_000));
    batch(&mut set, 10, &[]);
    set.begin_batch(TimeUs(20));
    assert!(set.open_candidate(TimeUs(20), TimeUs(2_020), 1).is_some());
    set.end_batch(TimeUs(21));
    let opened: Vec<_> = std::iter::from_fn(|| rx.pop())
        .filter(|o| matches!(o.event, TapEvent::Opened { .. }))
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].at, TimeUs(1_000_020));
    assert_eq!(
        opened[0].event,
        TapEvent::Opened {
            generation: 1,
            origin: TimeUs(1_002_020)
        }
    );
}

#[test]
fn a_fence_follows_everything_observed_before_it() {
    let (mut set, mut rx, _) = rig();
    batch(&mut set, 10, &[packet(W, 1)]);
    set.adjustment_mut().unwrap().fence(3, TimeUs(11));
    batch(&mut set, 20, &[packet(W, 0)]);
    let events: Vec<_> = drain(&mut rx).into_iter().filter(markers).collect();
    assert_eq!(
        events,
        [
            TapEvent::Resync {
                held: 0,
                verified: true
            },
            TapEvent::Press(SELECT),
            TapEvent::Fence(3),
            TapEvent::Release(SELECT),
        ]
    );
}

#[test]
fn a_loss_between_sampling_and_the_boundary_defers_the_opening() {
    let (mut set, mut rx, focus) = rig();
    batch(&mut set, 10, &[]);
    set.begin_batch(TimeUs(20));
    // Away and back after the batch sampled the focus.
    focus.set(false);
    focus.set(true);
    assert!(set.open_candidate(TimeUs(20), TimeUs(2_020), 1).is_none());
    set.end_batch(TimeUs(21));
    assert!(
        !drain(&mut rx)
            .iter()
            .any(|e| matches!(e, TapEvent::Opened { .. })),
        "nothing opened"
    );
    // The next batch samples the loss; the opening goes ahead.
    set.begin_batch(TimeUs(30));
    assert!(set.open_candidate(TimeUs(30), TimeUs(2_030), 1).is_some());
    set.end_batch(TimeUs(31));
    let events = drain(&mut rx);
    assert_eq!(
        &events[..3],
        [
            TapEvent::Lost,
            TapEvent::Resync {
                held: 0,
                verified: true
            },
            TapEvent::Opened {
                generation: 1,
                origin: TimeUs(2_030)
            },
        ]
    );
}

#[test]
fn a_loss_between_sampling_and_the_boundary_holds_the_rearm() {
    let (mut set, _rx, focus) = rig();
    batch(&mut set, 10, &[]);
    set.begin_batch(TimeUs(20));
    assert!(set.open_candidate(TimeUs(20), TimeUs(2_020), 1).is_some());
    set.end_batch(TimeUs(21));
    set.begin_batch(TimeUs(30));
    focus.set(false);
    assert_eq!(
        set.reopen_candidate(TimeUs(30), TimeUs(2_030), 2).err(),
        Some(ReopenError::Held)
    );
    set.end_batch(TimeUs(31));
    focus.set(true);
    // Focus back, but a loss was counted since the sample: held again
    // until a batch samples it.
    set.begin_batch(TimeUs(40));
    assert!(set.reopen_candidate(TimeUs(40), TimeUs(2_040), 2).is_ok());
    set.end_batch(TimeUs(41));
}

#[test]
fn an_unverified_baseline_verifies_no_hold() {
    let (mut set, mut rx, focus) = rig();
    batch(&mut set, 10, &[packet(Q, 1)]);
    drain(&mut rx);
    focus.set(false);
    batch(&mut set, 20, &[]);
    assert_eq!(
        drain(&mut rx),
        [
            TapEvent::Lost,
            TapEvent::Resync {
                held: 1 << START,
                verified: false
            },
        ],
        "while focus is away nothing it holds is verified"
    );
}

#[test]
fn a_second_loss_before_flush_extends_the_region() {
    let (mut set, mut rx, focus) = rig();
    let at = fill(&mut set, 10);
    // The queue has only its reserved slots: a released SELECT is lost,
    // and the recovery takes two of them.
    batch(&mut set, at + 1, &[packet(W, 1)]);
    // Two more losses: the first makes a second recovery in the reserved
    // slots left, the other extends it while it waits for room.
    batch(&mut set, at + 2, &[packet(W, 0)]);
    focus.set(false);
    focus.set(true);
    batch(&mut set, at + 3, &[packet(W, 1)]);
    let events = drain(&mut rx);
    assert!(
        events.contains(&TapEvent::Lost),
        "the first region went at once"
    );
    assert!(rx.owed(), "the second waits, extended");
    // Room again: whatever waited goes, once, with the held set now.
    set.tick_presentation(TimeUs(at + 4));
    let after = drain(&mut rx);
    assert!(!rx.owed());
    let pairs = after.iter().filter(|e| **e == TapEvent::Lost).count();
    assert!(pairs <= 1, "one owed recovery at most: {after:?}");
    assert_eq!(
        after.iter().find(|e| matches!(e, TapEvent::Resync { .. })),
        Some(&TapEvent::Resync {
            held: (1 << START) | (1 << SELECT),
            verified: true
        })
    );
}

#[test]
fn a_second_loss_after_an_unconsumed_pair_queues_one_owed_recovery() {
    let (mut set, mut rx, _) = rig();
    let at = fill(&mut set, 10);
    // Loss 1 sends its pair into the reserved slots at once; losses 2 and
    // 3 after it was sent, the consumer still stalled, make one recovery
    // that waits for room.
    batch(&mut set, at + 1, &[packet(W, 1)]);
    batch(&mut set, at + 2, &[packet(W, 0)]);
    batch(&mut set, at + 3, &[packet(W, 1)]);
    batch(&mut set, at + 4, &[packet(W, 0)]);
    let stalled = drain(&mut rx);
    let tail: Vec<_> = stalled
        .iter()
        .filter(|e| markers(e))
        .rev()
        .take(4)
        .collect();
    // The last entries the queue took are recoveries, nothing between.
    assert!(
        tail.iter()
            .all(|e| matches!(e, TapEvent::Lost | TapEvent::Resync { .. })),
        "{tail:?}"
    );
    assert!(rx.owed(), "one recovery still owed");
    set.tick_presentation(TimeUs(at + 5));
    let after: Vec<_> = drain(&mut rx).into_iter().filter(markers).collect();
    assert_eq!(
        after,
        [
            TapEvent::Lost,
            TapEvent::Resync {
                held: 1 << START,
                verified: true
            },
        ]
    );
    assert!(!rx.owed());
}

#[test]
fn markers_never_exceed_five_under_a_stalled_consumer() {
    let (mut set, mut rx, focus) = rig();
    batch(&mut set, 10, &[]);
    set.begin_batch(TimeUs(20));
    assert!(set.open_candidate(TimeUs(20), TimeUs(2_020), 1).is_some());
    set.end_batch(TimeUs(21));
    drain(&mut rx);
    let at = fill(&mut set, 30);
    // With only the reserved slots left: a cancellation, a fence, a
    // rearm's opening and repeated focus losses.
    batch(&mut set, at + 1, &[packet(W, 1)]);
    set.adjustment_mut().unwrap().fence(1, TimeUs(at + 2));
    for k in 0..10 {
        focus.set(false);
        focus.set(true);
        batch(&mut set, at + 3 + k, &[packet(SHIFT, 1), packet(SHIFT, 0)]);
    }
    let taken = drain(&mut rx);
    let reserved_used = taken.len() - (ADJUST_QUEUE - RESERVED);
    assert!(reserved_used <= RESERVED, "{reserved_used}");
    // The rest flushes in order once there is room, the recovery last.
    set.tick_presentation(TimeUs(at + 100));
    let rest: Vec<_> = drain(&mut rx).into_iter().filter(markers).collect();
    assert!(rest.len() <= 5, "{rest:?}");
    assert!(!rx.owed());
}

#[test]
fn open_and_rearm_reply_without_waiting_for_the_consumer() {
    let (mut set, mut rx, _) = rig();
    batch(&mut set, 10, &[]);
    let at = fill(&mut set, 20);
    // Released while the queue is full, and more losses: the reserved
    // slots hold two recoveries, and a third waits for room.
    batch(&mut set, at + 1, &[packet(Q, 0)]);
    batch(&mut set, at + 2, &[packet(W, 1)]);
    batch(&mut set, at + 3, &[packet(W, 0)]);
    assert!(rx.owed());
    // The opening is decided at once and owed behind the recovery, not
    // waited for.
    set.begin_batch(TimeUs(at + 4));
    assert!(
        set.open_candidate(TimeUs(at + 4), TimeUs(at + 2_000), 1)
            .is_some()
    );
    set.end_batch(TimeUs(at + 5));
    drain(&mut rx);
    set.tick_presentation(TimeUs(at + 6));
    assert_eq!(
        drain(&mut rx)
            .into_iter()
            .filter(markers)
            .collect::<Vec<_>>(),
        [
            TapEvent::Lost,
            TapEvent::Resync {
                held: 0,
                verified: true
            },
            TapEvent::Opened {
                generation: 1,
                origin: TimeUs(at + 2_000)
            },
        ]
    );
    // Once its opening is consumed, a rearm answers at once as well.
    set.begin_batch(TimeUs(at + 7));
    assert!(
        set.reopen_candidate(TimeUs(at + 7), TimeUs(at + 3_000), 2)
            .is_ok()
    );
    set.end_batch(TimeUs(at + 8));
    assert!(drain(&mut rx).contains(&TapEvent::Opened {
        generation: 2,
        origin: TimeUs(at + 3_000)
    }));
}

#[test]
fn a_drain_before_the_owed_flag_is_published_is_seen_by_the_recheck() {
    let (mut set, mut rx, _) = rig();
    let at = fill(&mut set, 10);
    batch(&mut set, at + 1, &[packet(W, 1)]);
    batch(&mut set, at + 2, &[packet(W, 0)]);
    batch(&mut set, at + 3, &[packet(W, 1)]);
    assert!(rx.owed());
    // The consumer drains everything: the next wake flushes, and after
    // any drain that sees `owed`, the consumer wakes the producer.
    drain(&mut rx);
    assert!(rx.owed(), "still published until the producer runs");
    set.tick_presentation(TimeUs(at + 4));
    assert!(!rx.owed(), "the producer's flush clears it");
    assert!(drain(&mut rx).contains(&TapEvent::Lost));
}

#[test]
fn a_held_scratch_is_due_its_repeats_through_silent_idle() {
    let (mut set, mut rx, _) = rig();
    batch(&mut set, 0, &[]);
    batch(&mut set, 1_000, &[packet(Q, 1), packet(SHIFT, 1)]);
    drain(&mut rx);
    // No packets: the tap's deadlines alone wake the thread.
    let mut now = 1_000;
    let mut ticks = Vec::new();
    while now < 601_000 {
        let due = set.next_due().expect("a held scratch is due").0;
        assert!(due > now);
        now = due;
        set.tick_presentation(TimeUs(now));
        ticks.extend(
            std::iter::from_fn(|| rx.pop())
                .filter(|o| o.event == TapEvent::Tick)
                .map(|o| o.at.0),
        );
    }
    // The press's group closes 1 µs later; then a tick at every repeat.
    let expected: Vec<i64> = std::iter::once(1_001)
        .chain((1..=12).map(|k| 1_000 + k * 50_000))
        .collect();
    assert_eq!(ticks, expected);
}

#[test]
fn deadlines_are_suspended_while_a_recovery_waits() {
    let (mut set, mut rx, _) = rig();
    batch(&mut set, 0, &[packet(SHIFT, 1)]);
    let at = fill(&mut set, 10);
    for k in 1..=3 {
        batch(&mut set, at + k, &[packet(W, 1), packet(W, 0)]);
    }
    assert!(rx.owed());
    assert_eq!(set.next_due(), None, "no deadline while blocked");
    // Bounded wakes: however often the thread is woken, nothing loops.
    for k in 0..100 {
        set.tick_presentation(TimeUs(at + 10 + k));
        assert_eq!(set.next_due(), None);
    }
    drain(&mut rx);
    set.tick_presentation(TimeUs(at + 200));
    assert!(set.next_due().is_some(), "deadlines resume after the drain");
}

#[test]
fn window_keys_overflow_at_65_and_recover_with_a_suppressed_baseline() {
    let (mut set, mut rx, _) = rig();
    let (mut keys, receiver) = window_keys();
    set.adjustment_mut()
        .unwrap()
        .set_window_keys(receiver, [Some(SLOWER), Some(FASTER)]);
    batch(&mut set, 10, &[]);
    drain(&mut rx);
    assert!(keys.key(1, WindowKeyEvent::Press, false));
    for _ in 1..WINDOW_KEYS {
        assert!(keys.key(1, WindowKeyEvent::Repeat, false));
    }
    assert!(!keys.key(1, WindowKeyEvent::Repeat, false), "the 65th");
    // While the input thread has not caught up, events show only in the
    // held keys.
    assert!(!keys.key(1, WindowKeyEvent::Release, false));
    assert!(!keys.key(0, WindowKeyEvent::Press, false));
    set.tick_presentation(TimeUs(20));
    let events: Vec<_> = drain(&mut rx).into_iter().filter(markers).collect();
    assert_eq!(events.len(), 64 + 2, "{events:?}");
    assert_eq!(events[0], TapEvent::Press(FASTER));
    assert!(events[1..64].iter().all(|e| *e == TapEvent::Repeat(FASTER)));
    assert_eq!(
        events[64..],
        [
            TapEvent::Lost,
            TapEvent::Resync {
                held: 1 << SLOWER,
                verified: true
            },
        ]
    );
    // Acknowledged: the window queues again, and its next events follow.
    assert!(keys.key(0, WindowKeyEvent::Release, false));
    set.tick_presentation(TimeUs(30));
    assert_eq!(
        drain(&mut rx)
            .into_iter()
            .filter(markers)
            .collect::<Vec<_>>(),
        [TapEvent::Release(SLOWER)]
    );
}

#[test]
fn synthetic_window_keys_change_only_the_held_keys() {
    let (mut set, mut rx, _) = rig();
    let (mut keys, receiver) = window_keys();
    set.adjustment_mut()
        .unwrap()
        .set_window_keys(receiver, [Some(SLOWER), Some(FASTER)]);
    batch(&mut set, 10, &[]);
    drain(&mut rx);
    assert!(!keys.key(1, WindowKeyEvent::Press, true));
    set.tick_presentation(TimeUs(20));
    assert_eq!(
        drain(&mut rx)
            .into_iter()
            .filter(|e| !matches!(e, TapEvent::Tick))
            .count(),
        0
    );
}
