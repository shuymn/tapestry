//! The activity record's input section as the adjustment tap writes it
//! (design.md "Play Lifecycle", the loading state: the tuning controls;
//! S10 item 9.4a plan §2.10 R9-R14): each press, key repeat and release of
//! a tuning control at its batch, a change of what is held at the batch
//! that made it, and nothing for another control.

use std::sync::Arc;

use bms_model::TimeUs;
use eval::ControlId;
use input::{
    ActivityRecord, AdjustmentBindings, DeviceId, DeviceKind, Packet, SectionValue, SourceConfig,
    SourceSet, TapControl, WindowKeyEvent, WindowKeys, adjustment_queue, window_keys,
};

/// F2 on the keyboard: a tuning control, number 0.
const F2: ControlId = ControlId(0x3C);
/// A lane key: a play adjustment only under START or SELECT, number 1.
const LANE: ControlId = ControlId(0x2C);
const KEYBOARD: DeviceId = DeviceId(1);
const MS: i64 = 1_000;

/// A keyboard reaching a tap whose tuning control is F2 (number 0), the
/// lane key number 1, and the window's F1 number 2; the record it writes.
fn rig(max_held: usize) -> (SourceSet, Arc<ActivityRecord>, WindowKeys) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held,
        queue_capacity: 16,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(KEYBOARD, DeviceKind::Keyboard);
    set.end_batch(TimeUs(1));
    let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![(TapControl::Key(F2), 0), (TapControl::Key(LANE), 1)],
        ..AdjustmentBindings::default()
    })
    .unwrap();
    tap.set_active(true);
    tap.set_tuning_controls(0b101);
    let (window, receiver) = window_keys();
    tap.set_window_keys(receiver, [Some(2), None]);
    let record = Arc::new(ActivityRecord::default());
    tap.set_activity(Arc::clone(&record));
    set.set_adjustment(tap);
    (set, record, window)
}

fn key(set: &mut SourceSet, at: i64, control: ControlId, value: i32) {
    set.begin_batch(TimeUs(at));
    set.packet(&Packet {
        device: KEYBOARD,
        control,
        value,
        background: false,
    });
    set.end_batch(TimeUs(at + 1));
}

fn input(record: &ActivityRecord) -> SectionValue {
    record.read().unwrap().input
}

/// R9-R11: F2's press, key repeat and release each move the revision and
/// stamp the last activity at their batch; held from the press to the
/// release. The window's F1 (an autoplay session's) the same, at the
/// wake that takes it.
#[test]
fn a_tuning_press_repeat_and_release_publish_at_the_event() {
    let (mut set, record, mut window) = rig(8);
    let idle = input(&record);
    assert!(!idle.held);
    assert_eq!(idle.last, None);
    key(&mut set, 10 * MS, F2, 1);
    let pressed = input(&record);
    assert_eq!(
        pressed,
        SectionValue {
            revision: idle.revision + 1,
            last: Some(TimeUs(10 * MS)),
            held: true,
            ..SectionValue::default()
        }
    );
    key(&mut set, 20 * MS, F2, 1);
    let repeated = input(&record);
    assert_eq!(repeated.revision, pressed.revision + 1);
    assert_eq!(repeated.last, Some(TimeUs(20 * MS)));
    assert!(repeated.held);
    key(&mut set, 30 * MS, F2, 0);
    let released = input(&record);
    assert_eq!(released.revision, repeated.revision + 1);
    assert_eq!(released.last, Some(TimeUs(30 * MS)));
    assert!(!released.held);
    // The window's F1.
    window.key(0, WindowKeyEvent::Press, false);
    set.tick_presentation(TimeUs(40 * MS));
    let window_pressed = input(&record);
    assert_eq!(window_pressed.revision, released.revision + 1);
    assert_eq!(window_pressed.last, Some(TimeUs(40 * MS)));
    assert!(window_pressed.held);
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(50 * MS));
    let window_released = input(&record);
    assert_eq!(window_released.revision, window_pressed.revision + 1);
    assert_eq!(window_released.last, Some(TimeUs(50 * MS)));
    assert!(!window_released.held);
}

/// R12: the lane key, a play adjustment only under START or SELECT,
/// pressed, repeated and released moves nothing in the record.
#[test]
fn another_controls_hold_is_not_activity() {
    let (mut set, record, _window) = rig(8);
    let idle = input(&record);
    for (at, value) in [(10, 1), (20, 1), (30, 0)] {
        key(&mut set, at * MS, LANE, value);
        assert_eq!(input(&record), idle, "{at} ms");
    }
}

/// R14: a device whose held state no longer fits its table makes the
/// tap's held state unverified, which holds the record until the device
/// is removed, each change moving the revision at its batch.
#[test]
fn an_unverified_hold_holds_the_record() {
    let (mut set, record, _window) = rig(1);
    key(&mut set, 10 * MS, LANE, 1);
    let one = input(&record);
    assert!(!one.held, "the lane key alone");
    key(&mut set, 20 * MS, F2, 1);
    let lost = input(&record);
    assert!(lost.held, "the table is full: unverified");
    assert_eq!(lost.revision, one.revision + 1);
    assert_eq!(lost.last, Some(TimeUs(20 * MS)));
    set.begin_batch(TimeUs(30 * MS));
    set.disconnect(KEYBOARD);
    set.end_batch(TimeUs(30 * MS + 1));
    let removed = input(&record);
    assert!(!removed.held);
    assert_eq!(removed.revision, lost.revision + 1);
    assert_eq!(removed.last, Some(TimeUs(30 * MS)));
}
