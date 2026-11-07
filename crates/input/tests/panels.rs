//! The select screen's panels' level (design.md "Input", Menu input):
//! START and SELECT published as held, as the devices that reach the
//! adjustment tap hold them, only while the publication is open, and the
//! panel open sent with each controller menu event in packet order.

use bms_model::TimeUs;
use eval::ControlId;
use input::hid::HidProduct;
use input::{
    AdjustmentBindings, DeviceId, DeviceKind, MenuAction, MenuBindings, MenuEvent, MenuReceiver,
    Packet, PanelLevel, Panels, SourceConfig, SourceSet, TapControl, adjustment_queue, menu_queue,
};

const E: ControlId = ControlId(0x12);
const W: ControlId = ControlId(0x11);
const KEYBOARD: DeviceId = DeviceId(1);
const PAD: DeviceId = DeviceId(7);
const PRODUCT: HidProduct = HidProduct {
    vendor_id: 1,
    product_id: 2,
    interface: None,
};
/// The controller's START button, its white key and its scratch button.
const PAD_START: ControlId = ControlId(9);
const WHITE: ControlId = ControlId(1);
const SCRATCH: ControlId = ControlId(2);

fn packet(device: DeviceId, control: ControlId, value: i32) -> Packet {
    Packet {
        device,
        control,
        value,
        background: false,
    }
}

/// A keyboard and a controller; START on E and the controller's button 9,
/// SELECT on W; the controller's white key confirms and its scratch
/// button scrolls down.
fn rig() -> (SourceSet, PanelLevel, MenuReceiver) {
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 8,
        queue_capacity: 16,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(KEYBOARD, DeviceKind::Keyboard);
    set.connect(PAD, DeviceKind::Controller);
    set.set_product(PAD, PRODUCT);
    set.end_batch(TimeUs(1));
    let (mut tap, _rx) = adjustment_queue(AdjustmentBindings {
        controls: vec![
            (TapControl::Key(E), 0),
            (TapControl::Key(W), 1),
            (
                TapControl::Controller {
                    product: PRODUCT,
                    control: PAD_START,
                },
                2,
            ),
        ],
        latched: 0b111,
        ..AdjustmentBindings::default()
    })
    .unwrap();
    tap.set_panel_roles(0b101, 0b010);
    let level = tap.panel_level();
    set.set_adjustment(tap);
    let (menu, receiver) = menu_queue(MenuBindings {
        product: PRODUCT,
        controls: vec![
            (WHITE, MenuAction::Press(MenuEvent::Confirm)),
            (SCRATCH, MenuAction::Press(MenuEvent::Down)),
        ],
        step: 1,
        relative: None,
        wrap: None,
    });
    set.set_menu(menu);
    set.menu_mut().unwrap().set_active(true);
    (set, level, receiver)
}

/// One batch of `packets` at `at`, then the boundary's publication, open
/// or not.
fn batch(set: &mut SourceSet, at: i64, packets: &[Packet], open: bool) -> bool {
    set.begin_batch(TimeUs(at));
    for p in packets {
        set.packet(p);
    }
    set.end_batch(TimeUs(at + 1));
    set.adjustment_mut().unwrap().publish_panels(open, 0)
}

fn held(level: &PanelLevel) -> (bool, bool, Option<usize>) {
    let p = level.read();
    (p.start, p.select, p.open())
}

/// START → panel 1, both → panel 3, SELECT alone → panel 2, neither →
/// none, from either START control; each change counted, a boundary that
/// changes nothing not.
#[test]
fn the_level_follows_start_and_select_held() {
    let (mut set, level, _) = rig();
    assert_eq!(level.read(), Panels::default());
    assert!(batch(&mut set, 10, &[packet(KEYBOARD, E, 1)], true));
    assert_eq!(held(&level), (true, false, Some(0)));
    assert!(batch(&mut set, 20, &[packet(KEYBOARD, W, 1)], true));
    assert_eq!(held(&level), (true, true, Some(2)));
    assert!(batch(&mut set, 30, &[packet(KEYBOARD, E, 0)], true));
    assert_eq!(held(&level), (false, true, Some(1)));
    assert!(!batch(&mut set, 35, &[], true), "nothing changed");
    assert!(batch(&mut set, 40, &[packet(KEYBOARD, W, 0)], true));
    assert_eq!(held(&level), (false, false, None));
    assert_eq!(level.read().generation, 4);
    // The controller's START is START too.
    assert!(batch(&mut set, 50, &[packet(PAD, PAD_START, 1)], true));
    assert_eq!(held(&level), (true, false, Some(0)));
    // A press and its release within one batch publish nothing.
    let before = level.read().generation;
    assert!(!batch(
        &mut set,
        60,
        &[packet(KEYBOARD, W, 1), packet(KEYBOARD, W, 0)],
        true
    ));
    assert_eq!(level.read().generation, before);
}

/// A boundary that is not open (focus away, the thread not Idle)
/// publishes neither; open again, what is held then is published, so a
/// release lost meanwhile never keeps a panel. A device's removal drops
/// what it held.
#[test]
fn a_closed_boundary_or_a_removal_clears_the_level() {
    let (mut set, level, _) = rig();
    batch(&mut set, 10, &[packet(KEYBOARD, E, 1)], true);
    assert!(batch(&mut set, 20, &[], false));
    assert_eq!(held(&level), (false, false, None));
    // Released while closed, then open: nothing held.
    assert!(!batch(&mut set, 30, &[packet(KEYBOARD, E, 0)], false));
    assert!(!batch(&mut set, 40, &[], true));
    assert_eq!(held(&level), (false, false, None));
    // Held through a closed boundary: published again once open.
    batch(&mut set, 50, &[packet(KEYBOARD, E, 1)], false);
    assert!(batch(&mut set, 60, &[], true));
    assert_eq!(held(&level), (true, false, Some(0)));
    // The keyboard removed while it holds START.
    set.begin_batch(TimeUs(70));
    set.disconnect(KEYBOARD);
    set.end_batch(TimeUs(71));
    assert!(set.adjustment_mut().unwrap().publish_panels(true, 0));
    assert_eq!(held(&level), (false, false, None));
}

/// Each controller menu event carries the panel open as its packet came,
/// in packet order within a batch: START pressed before the scratch, then
/// SELECT, then both released before the white key.
#[test]
fn a_menu_event_carries_the_panel_of_its_packet() {
    let (mut set, _, mut menu) = rig();
    batch(
        &mut set,
        10,
        &[
            packet(PAD, WHITE, 1),
            packet(PAD, WHITE, 0),
            packet(KEYBOARD, E, 1),
            packet(PAD, SCRATCH, 1),
            packet(PAD, SCRATCH, 0),
            packet(KEYBOARD, W, 1),
            packet(PAD, WHITE, 1),
            packet(PAD, WHITE, 0),
            packet(KEYBOARD, E, 0),
            packet(KEYBOARD, W, 0),
            packet(PAD, WHITE, 1),
            packet(PAD, WHITE, 0),
        ],
        true,
    );
    let events: Vec<_> = std::iter::from_fn(|| menu.pop_panel()).collect();
    assert_eq!(
        events,
        [
            (MenuEvent::Confirm, None),
            (MenuEvent::Down, Some(0)),
            (MenuEvent::Confirm, Some(2)),
            (MenuEvent::Confirm, None),
        ]
    );
}

/// Menu events overflowing their queue while START is held leave no panel
/// open: the level is the held state, not the queue's.
#[test]
fn an_overflowing_menu_queue_keeps_no_panel_open() {
    let (mut set, level, menu) = rig();
    let mut packets = vec![packet(KEYBOARD, E, 1)];
    for _ in 0..=input::MENU_QUEUE {
        packets.extend([packet(PAD, SCRATCH, 1), packet(PAD, SCRATCH, 0)]);
    }
    batch(&mut set, 10, &packets, true);
    assert!(menu.dropped() > 0, "overflowed");
    assert_eq!(held(&level), (true, false, Some(0)));
    batch(&mut set, 20, &[packet(KEYBOARD, E, 0)], true);
    assert_eq!(held(&level), (false, false, None));
}
