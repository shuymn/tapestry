//! Keeping cancelled READY openings allocates nothing once the arenas and
//! the attempt's source table are reserved (design.md "Scope of No
//! Allocation"): every opening up to the history's limit, each with its
//! held snapshot, events and arrivals, the input side reopening between
//! them, the arena running out, and the next attempt reusing all of it.
//! This file holds a single test, so the harness's other threads are not
//! tracked.

mod counting;

use bms_model::TimeUs;
use eval::ControlId;
use game::HistoryRecorder;
use input::{DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};
use replay::{CancelReason, MAX_CANCELLED_OPENINGS};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn keeping_openings_allocates_nothing() {
    let key = ControlId(0x1e);
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 2,
        max_held: 4,
        queue_capacity: 2_048,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.set_recorded(DeviceId(1), vec![key]);
    set.end_batch(TimeUs(1));
    let mut history = HistoryRecorder::new(8);
    let mut origin = 10;
    set.open_attempt(TimeUs(origin), TimeUs(origin));
    let mut readers = set.take_readers();

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    let mut at = origin;
    for attempt in 0..3 {
        for opening in 0..=MAX_CANCELLED_OPENINGS {
            // Enough events in the last attempt to run the arena out.
            let presses = if attempt == 2 { 601 } else { 5 };
            for i in 0..presses {
                at += 2;
                set.begin_batch(TimeUs(at));
                set.packet(&Packet {
                    device: DeviceId(1),
                    control: key,
                    value: i32::from(i % 2 == 0),
                    background: false,
                });
                if opening == 3 && i == 0 {
                    set.connect(DeviceId(2), DeviceKind::Keyboard);
                }
                set.end_batch(TimeUs(at + 1));
            }
            at += 2;
            let kept = history.keep(
                set.opening().unwrap(),
                TimeUs(origin),
                TimeUs(at - origin),
                &mut readers,
                set.arrivals(),
                CancelReason::Held,
            );
            if kept.is_err() {
                // Drained here to reopen; a sealed capture would end.
                for r in &mut readers {
                    while r.pop().is_some() {}
                }
            }
            set.return_readers(readers);
            origin = at;
            set.reopen(TimeUs(at), TimeUs(origin)).unwrap();
            readers = set.take_readers();
        }
        assert!(history.exhausted());
        assert!(set.opening().unwrap().held.len() == 1);
        history.clear();
    }
    let calls = counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0);
}
