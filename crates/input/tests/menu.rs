//! Controller menu input on the source set: which packets become menu
//! events, and the bounded queue they go through.

use bms_model::TimeUs;
use eval::ControlId;
use input::hid::{AxisCap, HidProduct, axis_cap, axis_control};
use input::{
    DeviceId, DeviceKind, MENU_QUEUE, MenuAction, MenuBindings, MenuEvent, MenuReceiver, Packet,
    SourceConfig, SourceSet, menu_queue,
};

const PAD: HidProduct = HidProduct {
    vendor_id: 0x1A2B,
    product_id: 0x3C4D,
    interface: None,
};
const OTHER: HidProduct = HidProduct {
    product_id: 0x0001,
    ..PAD
};
const CONFIRM: ControlId = ControlId(1);
const UP: ControlId = ControlId(8);
const X: ControlId = axis_control(0x01, 0x30);
const WHEEL: ControlId = axis_control(0x01, 0x38);
const STEP: u32 = 4;

fn bindings() -> MenuBindings {
    let turn = MenuAction::Turn {
        forward: MenuEvent::Down,
        backward: MenuEvent::Up,
    };
    MenuBindings {
        product: PAD,
        controls: vec![
            (CONFIRM, MenuAction::Press(MenuEvent::Confirm)),
            (UP, MenuAction::Press(MenuEvent::Up)),
            (X, turn),
            (WHEEL, turn),
        ],
        step: STEP,
        relative: None,
        wrap: None,
    }
}

fn caps() -> [AxisCap; 2] {
    [
        axis_cap(X, true, 0, 255, 8).unwrap(),
        axis_cap(WHEEL, false, -127, 127, 8).unwrap(),
    ]
}

/// A set with controllers `pads` (device, product) in the order found,
/// each with X (absolute, 256 values) and a relative wheel, and the menu
/// active.
fn set(pads: &[(u64, HidProduct)]) -> (SourceSet, MenuReceiver) {
    set_with(pads, &caps(), bindings())
}

/// As [`set`], each controller with axes `caps`, the menu with
/// `bindings`.
fn set_with(
    pads: &[(u64, HidProduct)],
    caps: &[AxisCap],
    bindings: MenuBindings,
) -> (SourceSet, MenuReceiver) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    for &(d, product) in pads {
        plug(&mut set, d, product, caps);
    }
    set.end_batch(TimeUs(0));
    let (tap, receiver) = menu_queue(bindings);
    set.set_menu(tap);
    set.menu_mut().unwrap().set_active(true);
    (set, receiver)
}

fn plug(set: &mut SourceSet, d: u64, product: HidProduct, caps: &[AxisCap]) {
    set.connect(DeviceId(d), DeviceKind::Controller);
    set.set_product(DeviceId(d), product);
    set.add_axes(DeviceId(d), caps);
}

/// Bindings of X alone, turning at `step`, with the revolution `wrap`
/// when set.
fn x_only(step: u32, wrap: Option<u32>) -> MenuBindings {
    MenuBindings {
        controls: vec![(
            X,
            MenuAction::Turn {
                forward: MenuEvent::Down,
                backward: MenuEvent::Up,
            },
        )],
        step,
        wrap,
        ..bindings()
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

fn send(set: &mut SourceSet, packets: &[Packet]) {
    set.begin_batch(TimeUs(1));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(2));
}

fn events(receiver: &mut MenuReceiver) -> Vec<MenuEvent> {
    std::iter::from_fn(|| receiver.pop()).collect()
}

/// A button press sends its event once; its release sends nothing.
#[test]
fn a_bound_button_press_sends_its_event() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    send(&mut set, &[packet(10, CONFIRM, 1), packet(10, CONFIRM, 0)]);
    send(&mut set, &[packet(10, UP, 1), packet(10, ControlId(2), 1)]);
    assert_eq!(events(&mut rx), [MenuEvent::Confirm, MenuEvent::Up]);
    assert!(set.menu_mut().unwrap().take_sent());
    assert!(!set.menu_mut().unwrap().take_sent());
}

/// An absolute axis sends an event per step of movement (4) taken the
/// short way round its revolution, higher values one way and lower the
/// other; its first report only says where it is, and a change of
/// direction starts the count again.
#[test]
fn an_axis_steps_the_short_way_round() {
    use MenuEvent::{Down, Up};
    let (mut set, mut rx) = set(&[(10, PAD)]);
    // First seen at 250, then 3 forward: nothing yet.
    send(&mut set, &[packet(10, X, 250), packet(10, X, 253)]);
    assert_eq!(events(&mut rx), []);
    // 253 to 1 is 4 forward across the boundary: 7 in all, one step.
    send(&mut set, &[packet(10, X, 1)]);
    assert_eq!(events(&mut rx), [Down]);
    // 3 more forward makes a step of the 3 left; then 3 back restarts
    // the count, and 1 more back makes 4.
    send(
        &mut set,
        &[packet(10, X, 4), packet(10, X, 1), packet(10, X, 0)],
    );
    assert_eq!(events(&mut rx), [Down, Up]);
    // Eight back in one report: two steps.
    send(&mut set, &[packet(10, X, 248)]);
    assert_eq!(events(&mut rx), [Up, Up]);
}

/// The movement past a step counts toward the next: at a step of 2,
/// 0 → 3 → 4 is two steps, and one report of 7 more is three, its last
/// one counting on.
#[test]
fn the_remainder_counts_toward_the_next_step() {
    let (mut set, mut rx) = set_with(&[(10, PAD)], &caps(), x_only(2, None));
    send(
        &mut set,
        &[packet(10, X, 0), packet(10, X, 3), packet(10, X, 4)],
    );
    assert_eq!(events(&mut rx), [MenuEvent::Down; 2]);
    send(&mut set, &[packet(10, X, 11), packet(10, X, 12)]);
    assert_eq!(events(&mut rx), [MenuEvent::Down; 4]);
}

/// A controller removed and connected again starts from nothing: the
/// movement counted before and its last position are forgotten, so its
/// first report after only says where it is.
#[test]
fn removal_forgets_the_movement_counted() {
    let (mut set, mut rx) = set_with(&[(10, PAD)], &caps(), x_only(2, None));
    send(&mut set, &[packet(10, X, 0), packet(10, X, 1)]);
    set.begin_batch(TimeUs(3));
    set.disconnect(DeviceId(10));
    plug(&mut set, 10, PAD, &caps());
    set.end_batch(TimeUs(3));
    send(&mut set, &[packet(10, X, 100), packet(10, X, 101)]);
    assert_eq!(events(&mut rx), []);
    send(&mut set, &[packet(10, X, 102)]);
    assert_eq!(events(&mut rx), [MenuEvent::Down]);
}

/// The axis turns in the revolution play converts it with: a device
/// declaring −127..127 (255 values) whose revolution `device.toml` sets to
/// 256 moves 126 → 127 → −128 by two, one step of 2; under the declared
/// revolution the last move would be none.
#[test]
fn the_configured_revolution_is_used() {
    let declared = [axis_cap(X, true, -127, 127, 8).unwrap()];
    let (mut set, mut rx) = set_with(&[(10, PAD)], &declared, x_only(2, Some(256)));
    send(
        &mut set,
        &[packet(10, X, 126), packet(10, X, 127), packet(10, X, -128)],
    );
    assert_eq!(events(&mut rx), [MenuEvent::Down]);
}

/// A relative axis counts its reported movement.
#[test]
fn a_relative_axis_counts_its_movement() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    send(&mut set, &[packet(10, WHEEL, 2), packet(10, WHEEL, 2)]);
    send(&mut set, &[packet(10, WHEEL, -5)]);
    assert_eq!(events(&mut rx), [MenuEvent::Down, MenuEvent::Up]);
}

/// Only the first connected unit of the configured model sends: another
/// model and a second unit of the model send nothing, and when the first
/// is removed the second takes over.
#[test]
fn only_the_first_unit_of_the_model_sends() {
    let (mut set, mut rx) = set(&[(9, OTHER), (10, PAD), (11, PAD)]);
    send(
        &mut set,
        &[
            packet(9, CONFIRM, 1),
            packet(11, CONFIRM, 1),
            packet(10, UP, 1),
        ],
    );
    assert_eq!(events(&mut rx), [MenuEvent::Up]);
    set.begin_batch(TimeUs(3));
    set.disconnect(DeviceId(10));
    set.end_batch(TimeUs(3));
    send(&mut set, &[packet(11, CONFIRM, 0), packet(11, CONFIRM, 1)]);
    assert_eq!(events(&mut rx), [MenuEvent::Confirm]);
}

/// Inactive, the menu sends nothing and forgets the movement counted, so
/// movement before it went inactive never completes a step after.
#[test]
fn an_inactive_menu_sends_nothing_and_forgets_movement() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    send(&mut set, &[packet(10, X, 0), packet(10, X, 3)]);
    set.menu_mut().unwrap().set_active(false);
    send(&mut set, &[packet(10, CONFIRM, 1), packet(10, X, 6)]);
    set.menu_mut().unwrap().set_active(true);
    send(&mut set, &[packet(10, X, 7)]);
    assert_eq!(events(&mut rx), []);
    assert!(!set.menu_mut().unwrap().take_sent());
}

/// A packet received in the background sends nothing, and a step it would
/// have completed is not completed by it.
#[test]
fn a_background_packet_sends_nothing() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    let background = |p: Packet| Packet {
        background: true,
        ..p
    };
    send(&mut set, &[packet(10, X, 0), packet(10, X, 3)]);
    send(
        &mut set,
        &[
            background(packet(10, CONFIRM, 1)),
            background(packet(10, X, 4)),
        ],
    );
    assert_eq!(events(&mut rx), []);
}

/// While recording is open nothing is sent, even with the menu active;
/// the controls are recorded as any play's.
#[test]
fn open_recording_sends_nothing() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    set.set_recorded(DeviceId(10), vec![CONFIRM]);
    set.open_recording(TimeUs(1));
    let mut readers = set.take_readers();
    send(&mut set, &[packet(10, CONFIRM, 1)]);
    assert_eq!(events(&mut rx), []);
    assert!(readers[0].pop().is_some());
    set.close_recording();
    send(&mut set, &[packet(10, CONFIRM, 0), packet(10, CONFIRM, 1)]);
    assert_eq!(events(&mut rx), [MenuEvent::Confirm]);
}

/// A full queue drops and counts the events past it, and takes events
/// again once read.
#[test]
fn a_full_queue_drops_and_counts() {
    let (mut set, mut rx) = set(&[(10, PAD)]);
    let n = MENU_QUEUE + 3;
    for _ in 0..n {
        send(&mut set, &[packet(10, CONFIRM, 1), packet(10, CONFIRM, 0)]);
    }
    assert_eq!(rx.dropped(), 3);
    assert_eq!(events(&mut rx).len(), MENU_QUEUE);
    send(&mut set, &[packet(10, CONFIRM, 1)]);
    assert_eq!(events(&mut rx), [MenuEvent::Confirm]);
    assert_eq!(rx.dropped(), 3);
}
