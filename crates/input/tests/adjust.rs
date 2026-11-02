//! The adjustment tap: presses and releases held by any device, key
//! repeats, signed axis movement, the hold latch, ticks, and a loss or a
//! focus change answered by a resynchronization.

use bms_model::TimeUs;
use eval::ControlId;
use input::hid::{AxisCap, HidProduct, axis_control};
use input::{
    ADJUST_QUEUE, AdjustmentBindings, AdjustmentReceiver, DeviceId, DeviceKind, Hold, HoldLatch,
    MAX_TAPPED, Packet, SourceConfig, SourceSet, TapControl, TapError, TapEvent, adjustment_queue,
};

const Q: ControlId = ControlId(0x10);
const W: ControlId = ControlId(0x11);
const UP: ControlId = ControlId(0xE048);
const SHIFT: ControlId = ControlId(0x2a);
const START: u8 = 0;
const SELECT: u8 = 1;
const AUX: u8 = 2;
const SCRATCH: u8 = 3;
const AXIS: u8 = 4;

const PRODUCT: HidProduct = HidProduct {
    vendor_id: 0x1234,
    product_id: 0x5678,
    interface: None,
};
const TURN: ControlId = axis_control(0x01, 0x30);

fn cap(relative: bool) -> AxisCap {
    AxisCap {
        control: TURN,
        relative,
        wrap: if relative { 0 } else { 256 },
        min: 0,
        max: if relative { 0 } else { 255 },
        bits: 8,
        signed: false,
    }
}

fn packet(device: u64, control: ControlId, value: i32) -> Packet {
    Packet {
        device: DeviceId(device),
        control,
        value,
        background: false,
    }
}

/// Two keyboards and a controller of `PRODUCT` with the turntable axis
/// (absolute, or relative), and an active tap.
fn rig(relative: bool) -> (SourceSet, AdjustmentReceiver, HoldLatch) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.connect(DeviceId(2), DeviceKind::Keyboard);
    set.connect(DeviceId(3), DeviceKind::Controller);
    set.set_product(DeviceId(3), PRODUCT);
    set.add_axes(DeviceId(3), &[cap(relative)]);
    set.end_batch(TimeUs(1));
    let (mut tap, rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![
            (TapControl::Key(Q), START),
            (TapControl::Key(W), SELECT),
            (TapControl::Key(UP), AUX),
            (TapControl::Key(SHIFT), SCRATCH),
            (
                TapControl::Controller {
                    product: PRODUCT,
                    control: TURN,
                },
                AXIS,
            ),
        ],
        latched: (1 << START) | (1 << SELECT),
        wrap: None,
    })
    .unwrap();
    let latch = tap.latch();
    tap.set_active(true);
    set.set_adjustment(tap);
    (set, rx, latch)
}

/// Runs one batch at `at` with `packets`.
fn batch(set: &mut SourceSet, at: i64, packets: &[Packet]) {
    set.begin_batch(TimeUs(at));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(at + 1));
}

fn drain(rx: &mut AdjustmentReceiver) -> Vec<(i64, TapEvent)> {
    std::iter::from_fn(|| rx.pop())
        .map(|o| (o.at.0, o.event))
        .collect()
}

#[test]
fn a_control_held_on_two_keyboards_is_pressed_once_and_released_by_the_last() {
    let (mut set, mut rx, _) = rig(false);
    batch(&mut set, 10, &[]);
    assert_eq!(drain(&mut rx), [(10, TapEvent::Resync(0))], "the baseline");
    batch(&mut set, 20, &[packet(1, Q, 1), packet(2, Q, 1)]);
    batch(&mut set, 30, &[packet(1, Q, 1), packet(1, Q, 0)]);
    batch(
        &mut set,
        40,
        &[packet(2, Q, 0), packet(1, W, 1), packet(1, W, 0)],
    );
    assert_eq!(
        drain(&mut rx),
        [
            (20, TapEvent::Press(START)),
            (20, TapEvent::Tick),
            // Keyboard 1's auto-repeat, then its release: keyboard 2 still
            // holds START.
            (30, TapEvent::Repeat(START)),
            (30, TapEvent::Tick),
            (40, TapEvent::Release(START)),
            (40, TapEvent::Press(SELECT)),
            (40, TapEvent::Release(SELECT)),
        ]
    );
}

#[test]
fn observations_are_numbered_in_order_while_recording_is_open() {
    let (mut set, mut rx, _) = rig(false);
    set.set_recorded(DeviceId(1), vec![SHIFT]);
    set.open_recording(TimeUs(5));
    let mut readers = set.take_readers();
    batch(&mut set, 8, &[]);
    batch(&mut set, 10, &[packet(1, SHIFT, 1), packet(1, UP, 1)]);
    let seen: Vec<_> = std::iter::from_fn(|| rx.pop()).collect();
    assert_eq!(
        seen.iter().map(|o| (o.seq, o.event)).collect::<Vec<_>>(),
        [
            (0, TapEvent::Resync(0)),
            (1, TapEvent::Press(SCRATCH)),
            (2, TapEvent::Press(AUX)),
            (3, TapEvent::Tick),
        ]
    );
    // The scratch key is recorded as before; the tap takes nothing from
    // the recording.
    let recorded = readers.iter_mut().find_map(|r| r.pop()).unwrap();
    assert_eq!(recorded.received, TimeUs(10));
}

#[test]
fn an_absolute_axis_moves_the_short_way_round_and_a_relative_one_as_reported() {
    let (mut set, mut rx, _) = rig(false);
    batch(&mut set, 10, &[packet(3, TURN, 250)]);
    batch(&mut set, 20, &[packet(3, TURN, 5)]);
    batch(&mut set, 30, &[packet(3, TURN, 250)]);
    assert_eq!(
        drain(&mut rx),
        [
            // The first report is only a baseline.
            (10, TapEvent::Resync(0)),
            (
                20,
                TapEvent::Axis {
                    control: AXIS,
                    delta: 11
                }
            ),
            (
                30,
                TapEvent::Axis {
                    control: AXIS,
                    delta: -11
                }
            ),
        ]
    );
    let (mut set, mut rx, _) = rig(true);
    batch(&mut set, 5, &[]);
    batch(&mut set, 10, &[packet(3, TURN, -3), packet(3, TURN, 0)]);
    assert_eq!(
        drain(&mut rx),
        [
            (5, TapEvent::Resync(0)),
            (
                10,
                TapEvent::Axis {
                    control: AXIS,
                    delta: -3
                }
            ),
        ]
    );
}

#[test]
fn the_latch_reports_a_held_modifier_and_counts_its_presses() {
    let (mut set, mut rx, latch) = rig(false);
    assert_eq!(latch.read(), Hold::default());
    batch(&mut set, 10, &[packet(1, UP, 1)]);
    assert!(!latch.read().held, "the auxiliary key is not latched");
    batch(&mut set, 20, &[packet(1, Q, 1), packet(2, Q, 1)]);
    assert_eq!(
        latch.read(),
        Hold {
            held: true,
            presses: 1,
            unverified: false,
        }
    );
    // Removing a device releases what it held: keyboard 2 still holds Q.
    set.begin_batch(TimeUs(30));
    set.disconnect(DeviceId(1));
    set.end_batch(TimeUs(31));
    assert!(latch.read().held);
    set.begin_batch(TimeUs(40));
    set.disconnect(DeviceId(2));
    set.end_batch(TimeUs(41));
    assert!(!latch.read().held);
    batch(&mut set, 50, &[packet(3, TURN, 1)]);
    let events = drain(&mut rx);
    assert!(events.contains(&(30, TapEvent::Release(AUX))));
    assert!(events.contains(&(40, TapEvent::Release(START))));
    // A press beyond the state table leaves the latch unverified until that
    // device goes.
    set.connect(DeviceId(5), DeviceKind::Keyboard);
    let keys: Vec<_> = (0x40..0x49).map(|c| packet(5, ControlId(c), 1)).collect();
    batch(&mut set, 60, &keys);
    assert!(latch.read().unverified);
    set.disconnect(DeviceId(5));
    assert!(!latch.read().unverified);
}

#[test]
fn an_overflow_sends_a_loss_then_the_held_controls_and_nothing_between() {
    let (mut set, mut rx, _) = rig(true);
    batch(&mut set, 10, &[packet(1, Q, 1)]);
    // Each batch sends an axis movement: more than the queue holds.
    for t in 0..ADJUST_QUEUE as i64 {
        batch(&mut set, 100 + t * 10, &[packet(3, TURN, 1)]);
    }
    // Released and pressed again while lost: no release is made up.
    batch(&mut set, 20_000, &[packet(1, Q, 0), packet(1, W, 1)]);
    let events = drain(&mut rx);
    assert!(rx.dropped() > 0);
    assert!(!events.iter().any(|(_, e)| *e == TapEvent::Lost));
    batch(&mut set, 30_000, &[packet(1, SHIFT, 1)]);
    assert_eq!(
        drain(&mut rx),
        [
            (30_000, TapEvent::Lost),
            (30_000, TapEvent::Resync((1 << SELECT) | (1 << SCRATCH))),
        ]
    );
    batch(&mut set, 30_010, &[packet(1, SHIFT, 0)]);
    assert_eq!(
        drain(&mut rx),
        [
            (30_010, TapEvent::Release(SCRATCH)),
            (30_010, TapEvent::Tick),
        ]
    );
}

#[test]
fn input_while_focus_is_away_is_answered_by_a_resynchronization() {
    let (mut set, mut rx, latch) = rig(false);
    batch(&mut set, 10, &[]);
    drain(&mut rx);
    let away = Packet {
        background: true,
        ..packet(1, Q, 1)
    };
    batch(&mut set, 20, &[away]);
    batch(&mut set, 30, &[packet(1, W, 1)]);
    assert_eq!(drain(&mut rx), [], "nothing while the gate is tripped");
    assert!(latch.read().held, "the latch follows the hand all the same");
    set.gate_mut().reset();
    batch(&mut set, 40, &[]);
    assert_eq!(
        drain(&mut rx),
        [
            (40, TapEvent::Lost),
            (40, TapEvent::Resync((1 << START) | (1 << SELECT))),
        ]
    );
}

#[test]
fn an_inactive_tap_counts_holders_and_sends_them_on_activation() {
    let (mut set, mut rx, _) = rig(false);
    set.adjustment_mut().unwrap().set_active(false);
    batch(&mut set, 10, &[packet(1, Q, 1)]);
    assert_eq!(drain(&mut rx), []);
    set.adjustment_mut().unwrap().set_active(true);
    batch(&mut set, 20, &[]);
    batch(&mut set, 20, &[]);
    assert_eq!(
        drain(&mut rx),
        [(20, TapEvent::Resync(1 << START)), (20, TapEvent::Tick)],
        "one tick per batch time"
    );
}

#[test]
fn an_axis_set_absolute_moves_the_short_way_round_its_configured_revolution() {
    // Declared relative, set absolute: no declared revolution, so the
    // configured one applies, as play's conversion takes it.
    let (mut set, _, _) = rig(true);
    set.set_axis_mode(PRODUCT, vec![TURN], false);
    let (mut tap, mut rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![(
            TapControl::Controller {
                product: PRODUCT,
                control: TURN,
            },
            AXIS,
        )],
        latched: 0,
        wrap: Some(256),
    })
    .unwrap();
    tap.set_active(true);
    set.set_adjustment(tap);
    batch(&mut set, 10, &[packet(3, TURN, 250)]);
    batch(&mut set, 20, &[packet(3, TURN, 5)]);
    assert_eq!(
        drain(&mut rx),
        [
            (10, TapEvent::Resync(0)),
            (
                20,
                TapEvent::Axis {
                    control: AXIS,
                    delta: 11
                }
            ),
        ]
    );
}

#[test]
fn bindings_beyond_the_limits_are_refused() {
    let key = |c| TapControl::Key(ControlId(c));
    let refused = |controls: Vec<(TapControl, u8)>| {
        adjustment_queue(AdjustmentBindings {
            controls,
            ..AdjustmentBindings::default()
        })
        .err()
    };
    assert_eq!(
        refused(vec![(key(1), MAX_TAPPED as u8)]),
        Some(TapError::Number(128))
    );
    assert_eq!(
        refused(vec![(key(1), 0), (key(1), 1)]),
        Some(TapError::Duplicate)
    );
    let many = (0..=MAX_TAPPED as u32).map(|c| (key(c), 0)).collect();
    assert_eq!(refused(many), Some(TapError::TooMany(129)));
    assert_eq!(refused(vec![(key(1), 0), (key(2), 0)]), None);
}
