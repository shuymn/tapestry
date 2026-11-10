//! The activity record's input section as the adjustment tap writes it
//! (design.md "Play Lifecycle", the loading state: the tuning controls;
//! S10 item 9.4a plan §2.10 R9-R14): each press, key repeat and release of
//! a tuning control at its batch, a change of what is held at the batch
//! that made it, and nothing for another control.

use std::sync::Arc;

use bms_model::TimeUs;
use eval::ControlId;
use input::{
    ActivityRecord, AdjustmentBindings, DeviceId, DeviceKind, Packet, PresentationFocus,
    SectionValue, SourceConfig, SourceSet, TapControl, WindowKeyEvent, WindowKeys,
    adjustment_queue, window_keys,
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

/// The window's presentation focus lost at `lost` and back at `back`,
/// each sampled at the input thread's wake then.
fn lose_focus(set: &mut SourceSet, focus: &PresentationFocus, lost: i64, back: i64) {
    focus.set(false);
    set.tick_presentation(TimeUs(lost));
    focus.set(true);
    set.tick_presentation(TimeUs(back));
}

/// A tuning control held across a focus loss, or come to be held while
/// focus was away, is disarmed until its last holder releases it
/// (design.md "Play Lifecycle", the window has focus): F2 held across a
/// loss and its return, and the window's F1 the same, leave the record
/// held at the loss; the window's F1 queued before a loss and taken by
/// its recovery, the window's made-up release and press around a loss,
/// and F2 pressed while focus is away never hold it. Their repeats and
/// release move nothing; a fresh press while focused counts.
#[test]
fn a_tuning_control_held_across_a_focus_loss_is_disarmed() {
    let (mut set, record, mut window) = rig(8);
    let focus = PresentationFocus::new();
    set.set_presentation_focus(focus.clone());
    // F2 from a device.
    key(&mut set, 10 * MS, F2, 1);
    let pressed = input(&record);
    assert!(pressed.held);
    lose_focus(&mut set, &focus, 20 * MS, 30 * MS);
    let lost = input(&record);
    assert!(!lost.held, "dropped at the loss");
    assert_eq!(lost.revision, pressed.revision + 1);
    key(&mut set, 40 * MS, F2, 1);
    assert_eq!(input(&record), lost, "a disarmed repeat");
    key(&mut set, 50 * MS, F2, 0);
    assert_eq!(input(&record), lost, "a disarmed release");
    key(&mut set, 60 * MS, F2, 1);
    let fresh = input(&record);
    assert_eq!(
        fresh,
        SectionValue {
            revision: lost.revision + 1,
            last: Some(TimeUs(60 * MS)),
            held: true,
            ..SectionValue::default()
        }
    );
    key(&mut set, 70 * MS, F2, 0);
    let released = input(&record);
    assert!(!released.held);
    assert_eq!(released.revision, fresh.revision + 1);
    // The window's F1.
    window.key(0, WindowKeyEvent::Press, false);
    set.tick_presentation(TimeUs(80 * MS));
    let window_pressed = input(&record);
    assert!(window_pressed.held);
    lose_focus(&mut set, &focus, 90 * MS, 100 * MS);
    let window_lost = input(&record);
    assert!(!window_lost.held, "dropped at the loss");
    assert_eq!(window_lost.revision, window_pressed.revision + 1);
    window.key(0, WindowKeyEvent::Repeat, false);
    set.tick_presentation(TimeUs(110 * MS));
    assert_eq!(input(&record), window_lost, "a disarmed repeat");
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(120 * MS));
    assert_eq!(input(&record), window_lost, "a disarmed release");
    window.key(0, WindowKeyEvent::Press, false);
    set.tick_presentation(TimeUs(130 * MS));
    let window_fresh = input(&record);
    assert!(window_fresh.held);
    assert_eq!(window_fresh.revision, window_lost.revision + 1);
    assert_eq!(window_fresh.last, Some(TimeUs(130 * MS)));
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(140 * MS));
    let idle = input(&record);
    assert!(!idle.held);
    // F1 queued, not yet taken, when focus goes: the loss's recovery
    // takes it from the window's held keys, its queued press dropped.
    window.key(0, WindowKeyEvent::Press, false);
    focus.set(false);
    set.tick_presentation(TimeUs(150 * MS));
    assert_eq!(input(&record), idle, "a hold taken by the recovery");
    focus.set(true);
    set.tick_presentation(TimeUs(160 * MS));
    assert_eq!(input(&record), idle, "and by the return's");
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(170 * MS));
    assert_eq!(input(&record), idle, "its release");
    window.key(0, WindowKeyEvent::Press, false);
    set.tick_presentation(TimeUs(180 * MS));
    let queued_fresh = input(&record);
    assert_eq!(
        queued_fresh,
        SectionValue {
            revision: idle.revision + 1,
            last: Some(TimeUs(180 * MS)),
            held: true,
            ..SectionValue::default()
        }
    );
    // The window's made-up release as focus goes and press as it comes
    // back: the return's recovery takes F1 held again.
    window.key(0, WindowKeyEvent::Release, true);
    focus.set(false);
    set.tick_presentation(TimeUs(190 * MS));
    let made_up_lost = input(&record);
    assert!(!made_up_lost.held, "dropped at the loss");
    assert_eq!(made_up_lost.revision, queued_fresh.revision + 1);
    window.key(0, WindowKeyEvent::Press, true);
    focus.set(true);
    set.tick_presentation(TimeUs(200 * MS));
    assert_eq!(input(&record), made_up_lost, "a made-up press");
    window.key(0, WindowKeyEvent::Repeat, false);
    set.tick_presentation(TimeUs(210 * MS));
    assert_eq!(input(&record), made_up_lost, "its repeat");
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(220 * MS));
    assert_eq!(input(&record), made_up_lost, "its release");
    window.key(0, WindowKeyEvent::Press, false);
    set.tick_presentation(TimeUs(230 * MS));
    let made_up_fresh = input(&record);
    assert_eq!(
        made_up_fresh,
        SectionValue {
            revision: made_up_lost.revision + 1,
            last: Some(TimeUs(230 * MS)),
            held: true,
            ..SectionValue::default()
        }
    );
    window.key(0, WindowKeyEvent::Release, false);
    set.tick_presentation(TimeUs(240 * MS));
    let idle = input(&record);
    assert!(!idle.held);
    // F2 first pressed while focus is away.
    focus.set(false);
    set.tick_presentation(TimeUs(250 * MS));
    key(&mut set, 260 * MS, F2, 1);
    assert_eq!(input(&record), idle, "a press while focus is away");
    focus.set(true);
    set.tick_presentation(TimeUs(270 * MS));
    assert_eq!(input(&record), idle, "held into the return");
    key(&mut set, 280 * MS, F2, 1);
    assert_eq!(input(&record), idle, "its repeat");
    key(&mut set, 290 * MS, F2, 0);
    assert_eq!(input(&record), idle, "its release");
    key(&mut set, 300 * MS, F2, 1);
    assert_eq!(
        input(&record),
        SectionValue {
            revision: idle.revision + 1,
            last: Some(TimeUs(300 * MS)),
            held: true,
            ..SectionValue::default()
        }
    );
}
