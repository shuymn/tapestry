//! An axis whose mode `device.toml` sets apart from its declaration is
//! filtered in that mode by the controller's report reader, so what is
//! recorded and what the menu sees are the reports its conversion uses.

use bms_model::TimeUs;
use eval::{ControlId, InputKind};
use input::hid::{AxisCap, HidDevice, HidLayout, HidProduct, HidReport, axis_cap, axis_control};
use input::{
    Capture, DeviceId, DeviceKind, MenuAction, MenuBindings, MenuEvent, SourceConfig, SourceSet,
    menu_queue,
};

const PAD: HidProduct = HidProduct {
    vendor_id: 0x1A2B,
    product_id: 0x3C4D,
    interface: None,
};
const X: ControlId = axis_control(0x01, 0x30);
const DEVICE: DeviceId = DeviceId(10);

/// What the values `xs`, reported one by one through the reader of a
/// controller declaring X as `cap`, give when `device.toml` sets X
/// `relative` (or absolute): the menu's events at a step of 2 while
/// recording is closed, then the values recorded in a play.
fn run(cap: AxisCap, relative: bool, xs: &[i32]) -> (Vec<MenuEvent>, Vec<i32>) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 4,
        queue_capacity: 64,
        max_arrivals: 1,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DEVICE, DeviceKind::Controller);
    set.set_product(DEVICE, PAD);
    set.add_axes(DEVICE, &[cap]);
    set.end_batch(TimeUs(0));
    set.set_axis_mode(PAD, vec![X], relative);
    let (tap, mut menu) = menu_queue(MenuBindings {
        product: PAD,
        controls: vec![(
            X,
            MenuAction::Turn {
                forward: MenuEvent::Down,
                backward: MenuEvent::Up,
            },
        )],
        step: 2,
        relative: Some(relative),
        wrap: None,
    });
    set.set_menu(tap);
    set.menu_mut().unwrap().set_active(true);
    let layout = HidLayout {
        collection: 0,
        axes: &[cap],
        buttons: &[],
    };
    let capture = Capture::new();
    let feed = |set: &mut SourceSet| {
        let mut pad = HidDevice::new(DEVICE, &layout);
        for &x in xs {
            let axes = [Some(x)];
            let report = HidReport {
                id: 0,
                buttons: None,
                axes: &axes,
            };
            set.begin_batch(TimeUs(1));
            pad.report(&report, false, &capture, set);
            set.end_batch(TimeUs(2));
        }
    };
    feed(&mut set);
    let events = std::iter::from_fn(|| menu.pop()).collect();
    set.set_recorded(DEVICE, vec![X]);
    set.open_recording(TimeUs(1));
    let mut readers = set.take_readers();
    feed(&mut set);
    let mut recorded = Vec::new();
    while let Some(e) = readers[0].pop() {
        if let InputKind::Control { value, .. } = e.kind {
            recorded.push(value);
        }
    }
    (events, recorded)
}

/// Declared absolute, set relative: two reports of the same movement are
/// two movements, both recorded, and two steps for the menu.
#[test]
fn an_axis_set_relative_keeps_repeated_deltas() {
    let declared = axis_cap(X, true, 0, 255, 8).unwrap();
    let (events, recorded) = run(declared, true, &[2, 2]);
    assert_eq!(recorded, [2, 2]);
    assert_eq!(events, [MenuEvent::Down; 2]);
    // As declared, the second is no change and is dropped.
    let (_, recorded) = run(declared, false, &[2, 2]);
    assert_eq!(recorded, [2]);
}

/// Declared relative, set absolute: a report at position 0 is a position,
/// recorded and turned back from 2 to 0 and on to 2 again for the menu.
#[test]
fn an_axis_set_absolute_keeps_position_zero() {
    let declared = axis_cap(X, false, -127, 127, 8).unwrap();
    let (events, recorded) = run(declared, false, &[2, 0, 2]);
    assert_eq!(recorded, [2, 0, 2]);
    assert_eq!(events, [MenuEvent::Up, MenuEvent::Down]);
    // As declared, 0 is no movement and is dropped.
    let (_, recorded) = run(declared, true, &[2, 0, 2]);
    assert_eq!(recorded, [2, 2]);
}

/// The start snapshot of a play after X reported 50 through the reader
/// while recording was closed, with X declared as `cap` and converted as
/// `relative` or absolute.
fn snapshot(cap: AxisCap, relative: bool) -> Vec<(eval::SourceId, ControlId, i32)> {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 4,
        queue_capacity: 8,
        max_arrivals: 1,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DEVICE, DeviceKind::Controller);
    set.set_product(DEVICE, PAD);
    set.add_axes(DEVICE, &[cap]);
    set.set_axis_mode(PAD, vec![X], relative);
    let layout = HidLayout {
        collection: 0,
        axes: &[cap],
        buttons: &[],
    };
    let mut pad = HidDevice::new(DEVICE, &layout);
    let axes = [Some(50)];
    let report = HidReport {
        id: 0,
        buttons: None,
        axes: &axes,
    };
    pad.report(&report, false, &Capture::new(), &mut set);
    set.end_batch(TimeUs(1));
    set.set_recorded(DEVICE, vec![X]);
    set.open_recording(TimeUs(2)).held
}

/// The start snapshot holds the position of an axis converted as
/// absolute, whatever it declares, so play's conversion starts from it
/// and counts the first movement; an axis converted as relative has no
/// position to hold.
#[test]
fn the_start_snapshot_follows_the_conversion_mode() {
    let source = eval::SourceId(0);
    let declared_relative = axis_cap(X, false, -127, 127, 8).unwrap();
    assert_eq!(snapshot(declared_relative, false), [(source, X, 50)]);
    assert_eq!(snapshot(declared_relative, true), []);
    let declared_absolute = axis_cap(X, true, 0, 255, 8).unwrap();
    assert_eq!(snapshot(declared_absolute, true), []);
    assert_eq!(snapshot(declared_absolute, false), [(source, X, 50)]);
}
