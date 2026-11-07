//! The adjustment bindings: the defaults in the default key configs,
//! every role collision refused, the scratch's directions and reversal,
//! and the tap's observations reaching the reducer through a plan.

use bms_model::TimeUs;
use eval::ControlId as Code;
use game::adjustment::{
    Adjustment, AxisQuantum, ControlId, Event, HeldSet, NoTempo, Observation, Role,
};
use game::{
    AdjustBinding, AdjustKeyError, AdjustmentConfig, AdjustmentKeys, Covers, DeviceSettings,
    HiSpeedSettings, KeyConfig, KeyConfigs, KeyError, LaneKey, PlayOptions, Role as KeyRole,
};
use input::hid::{HidProduct, axis_control};
use input::{
    DeviceId, DeviceKind, Packet, SourceConfig, SourceSet, TapControl, TapEvent, TapObservation,
    adjustment_queue,
};

const PRODUCT: HidProduct = HidProduct {
    vendor_id: 7,
    product_id: 8,
    interface: None,
};

fn keys() -> KeyConfigs {
    KeyConfigs::from(&DeviceSettings::default())
}

/// A controller binding the scratch to its turntable axis and two
/// buttons, and key 1 to a third.
fn with_controller() -> KeyConfigs {
    let mut k = keys();
    k.controller.lanes = vec![
        LaneKey {
            control: axis_control(0x01, 0x30).0,
            lane: 0,
        },
        LaneKey {
            control: 1,
            lane: 0,
        },
        LaneKey {
            control: 2,
            lane: 0,
        },
        LaneKey {
            control: 3,
            lane: 1,
        },
    ];
    k.controller_device = Some(PRODUCT);
    k
}

#[test]
fn the_defaults_bind_e_w_up_down_and_the_scratch_key() {
    let k = keys();
    let roles = |keys: &KeyConfig| AdjustmentKeys::of(keys).roles().collect::<Vec<_>>();
    assert_eq!(
        roles(&k.keyboard),
        [
            (0x12, Role::Start),
            (0x11, Role::Select),
            (0xE048, Role::AuxNegative),
            (0xE050, Role::AuxPositive),
            (0x2a, Role::ScratchPositive),
        ]
    );
    assert_eq!(roles(&k.controller), []);
    let config = AdjustmentConfig::default();
    assert_eq!(config, AdjustmentConfig::of(&DeviceSettings::default()));
    let plan = config.plan(&k, None, None).unwrap();
    let number = |code: u32| {
        plan.bindings
            .controls
            .iter()
            .find(|(c, _)| *c == TapControl::Key(Code(code)))
            .map(|&(_, n)| ControlId(n))
            .unwrap()
    };
    let role = |code| plan.roles.get(number(code));
    assert_eq!(role(0x12), Some(Role::Start));
    assert_eq!(role(0x11), Some(Role::Select));
    assert_eq!(role(0xE048), Some(Role::AuxNegative));
    assert_eq!(role(0xE050), Some(Role::AuxPositive));
    assert_eq!(role(0x2a), Some(Role::ScratchPositive));
    // Z is key 1, S key 2; F1 and F2 step the speed.
    assert_eq!(role(0x2c), Some(Role::White));
    assert_eq!(role(0x1f), Some(Role::Black));
    assert_eq!(role(0x3b), Some(Role::Slower));
    assert_eq!(role(0x3c), Some(Role::Faster));
    assert_eq!(
        plan.bindings.latched,
        (1 << number(0x12).0) | (1 << number(0x11).0)
    );
    // The panels tell START from SELECT.
    assert_eq!(
        plan.panel_roles(),
        [1 << number(0x12).0, 1 << number(0x11).0]
    );
    assert_eq!(plan.bindings.controls.len(), 14);
    assert_eq!(plan.axis(), None, "the keyboard has no axis");
}

#[test]
fn every_role_collision_is_refused() {
    let check = |f: fn(&mut KeyConfig)| {
        let mut k = keys().keyboard;
        f(&mut k);
        let e = AdjustmentKeys::of(&k).check(DeviceKind::Keyboard).err();
        // The key config's own check refuses the same.
        assert_eq!(
            k.check(DeviceKind::Keyboard).err(),
            e.map(KeyError::Adjustment)
        );
        e
    };
    let overlap = |control, first, second| {
        Some(AdjustKeyError::Overlap {
            control,
            first,
            second: AdjustBinding::Adjustment(second),
        })
    };
    // A lane key, retry, quit, and each other.
    assert_eq!(
        check(|k| k.start = Some(0x2c)),
        overlap(0x2c, AdjustBinding::Key(KeyRole::Lane(1)), Role::Start)
    );
    assert_eq!(
        check(|k| k.aux_positive = Some(0x0e)),
        overlap(0x0e, AdjustBinding::Key(KeyRole::Retry), Role::AuxPositive)
    );
    assert_eq!(
        check(|k| k.select = Some(0x01)),
        overlap(0x01, AdjustBinding::Key(KeyRole::Quit), Role::Select)
    );
    assert_eq!(
        check(|k| k.select = Some(0x12)),
        overlap(0x12, AdjustBinding::Adjustment(Role::Start), Role::Select)
    );
    // The scratch's own key, which only its directions take.
    assert_eq!(
        check(|k| k.aux_negative = Some(0x2a)),
        overlap(
            0x2a,
            AdjustBinding::Key(KeyRole::Lane(0)),
            Role::AuxNegative
        )
    );
    assert_eq!(
        check(|k| k.scratch_negative = Some(0x2a)),
        overlap(
            0x2a,
            AdjustBinding::Adjustment(Role::ScratchPositive),
            Role::ScratchNegative
        )
    );
    assert_eq!(
        check(|k| k.scratch_negative = Some(0x2c)),
        Some(AdjustKeyError::NotScratch {
            control: 0x2c,
            role: Role::ScratchNegative,
        })
    );
    // Reserved keys: F1 to F8, the overlay and the presets.
    for code in [0x3b, 0x3c, 0x42, 0x43, 0x58] {
        let mut k = keys().keyboard;
        k.start = Some(code);
        assert!(
            matches!(
                AdjustmentKeys::of(&k).check(DeviceKind::Keyboard),
                Err(AdjustKeyError::Reserved { control, .. }) if control == code
            ),
            "{code:#x}"
        );
    }
    // Two names of one key are one key.
    let mut k = keys().keyboard;
    k.start = Some(0x45);
    k.select = Some(0xE045);
    assert!(matches!(
        AdjustmentKeys::of(&k).check(DeviceKind::Keyboard),
        Err(AdjustKeyError::Overlap {
            control: 0xE045,
            ..
        })
    ));
}

#[test]
fn the_controller_binds_buttons_and_the_scratch_directions_among_its_own() {
    let mut k = with_controller().controller;
    let check = |k: &KeyConfig| AdjustmentKeys::of(k).check(DeviceKind::Controller);
    k.start = Some(9);
    k.scratch_positive = Some(1);
    k.scratch_negative = Some(2);
    assert_eq!(check(&k), Ok(()));
    k.select = Some(axis_control(0x01, 0x31).0);
    assert_eq!(check(&k), Err(AdjustKeyError::Axis(Role::Select)));
    k.select = Some(3);
    assert!(matches!(
        check(&k),
        Err(AdjustKeyError::Overlap { control: 3, .. })
    ));
    k.select = None;
    k.scratch_negative = Some(axis_control(0x01, 0x30).0);
    assert_eq!(check(&k), Err(AdjustKeyError::Axis(Role::ScratchNegative)));
}

#[test]
fn the_scratch_axis_signs_its_movement_and_reversal_flips_it() {
    let wrapping = AxisQuantum::wrapping(256).unwrap();
    let configured = AxisQuantum::per_step(5).unwrap();
    for reversed in [false, true] {
        let config = AdjustmentConfig {
            reversed,
            ..AdjustmentConfig::default()
        };
        let plan = config
            .plan(&with_controller(), Some(configured), Some(512))
            .unwrap();
        assert_eq!(plan.bindings.wrap, Some(512));
        let axis = plan.axis().unwrap();
        assert_eq!(plan.roles.get(ControlId(axis)), None);
        let o = TapObservation {
            at: TimeUs(5),
            seq: 3,
            event: TapEvent::Axis {
                control: axis,
                delta: 7,
                revolution: 256,
            },
        };
        let stepped = |quantum| {
            Some(Observation {
                time: TimeUs(5),
                seq: 3,
                event: Event::Axis {
                    control: ControlId(axis),
                    delta: if reversed { -7 } else { 7 },
                    quantum,
                },
            })
        };
        // A wrapping axis steps by the revolution the packet was counted
        // on, any other by the configured quantum.
        assert_eq!(plan.observation(&o), stepped(wrapping));
        let relative = TapObservation {
            event: TapEvent::Axis {
                control: axis,
                delta: 7,
                revolution: 0,
            },
            ..o
        };
        assert_eq!(plan.observation(&relative), stepped(configured));
        let other = TapObservation {
            event: TapEvent::Axis {
                control: axis + 1,
                delta: 7,
                revolution: 256,
            },
            ..o
        };
        assert_eq!(plan.observation(&other), None);
    }
    // Without a configured quantum a wrapping axis still steps; any other
    // does not.
    let plan = AdjustmentConfig::default()
        .plan(&with_controller(), None, None)
        .unwrap();
    let axis = plan.axis().unwrap();
    let o = |revolution| TapObservation {
        at: TimeUs(5),
        seq: 3,
        event: TapEvent::Axis {
            control: axis,
            delta: 7,
            revolution,
        },
    };
    assert!(plan.observation(&o(256)).is_some());
    assert_eq!(plan.observation(&o(0)), None);
}

/// The driver's tap publishes START and SELECT for the select screen's
/// panels by the plan's roles: SELECT (W) held is SELECT, not START.
#[test]
fn the_drivers_tap_tells_start_from_select() {
    let plan = AdjustmentConfig::default()
        .plan(&keys(), None, None)
        .unwrap();
    let options = PlayOptions::new(HiSpeedSettings::default(), Covers::default()).unwrap();
    let (_driver, tap, _reader) = game::AdjustDriver::new(&plan, options, 0).unwrap();
    let level = tap.panel_level();
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 1,
        max_sources: 1,
        max_held: 8,
        queue_capacity: 16,
        max_arrivals: 1,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.end_batch(TimeUs(1));
    set.set_adjustment(tap);
    for (code, start, select) in [(0x11, false, true), (0x12, true, true)] {
        set.begin_batch(TimeUs(10));
        set.packet(&Packet {
            device: DeviceId(1),
            control: Code(code),
            value: 1,
            background: false,
        });
        set.end_batch(TimeUs(11));
        assert!(set.adjustment_mut().unwrap().publish_panels(true));
        let p = level.read();
        assert_eq!((p.start, p.select), (start, select), "{code:#x}");
    }
}

#[test]
fn the_tap_reaches_the_reducer_through_the_plan() {
    let plan = AdjustmentConfig::default()
        .plan(&keys(), None, None)
        .unwrap();
    let (mut tap, mut rx) = adjustment_queue(plan.bindings.clone()).unwrap();
    let latch = tap.latch();
    tap.set_active(true);
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 2,
        max_held: 8,
        queue_capacity: 16,
        max_arrivals: 2,
    })
    .unwrap();
    set.begin_batch(TimeUs(0));
    set.connect(DeviceId(1), DeviceKind::Keyboard);
    set.end_batch(TimeUs(1));
    set.set_adjustment(tap);
    let options = PlayOptions::new(HiSpeedSettings::default(), Covers::default()).unwrap();
    let mut reducer = Adjustment::new(options, plan.roles, TimeUs(0), 0);
    let mut step = |at: i64, value: i32| {
        set.begin_batch(TimeUs(at));
        set.packet(&Packet {
            device: DeviceId(1),
            control: Code(0x12),
            value,
            background: false,
        });
        set.end_batch(TimeUs(at + 1));
        while let Some(o) = rx.pop() {
            reducer.observe(plan.observation(&o).unwrap(), &NoTempo);
        }
        reducer.modifiers_held()
    };
    assert!(step(10, 1), "START is held");
    assert!(latch.read().held);
    assert!(!step(20, 0));
    assert_eq!(latch.read().presses, 1);
    assert_eq!(
        plan.observation(&TapObservation {
            at: TimeUs(0),
            seq: 0,
            event: TapEvent::Resync {
                held: 0b101,
                verified: false,
            },
        })
        .map(|o| o.event),
        Some(Event::Resync {
            held: HeldSet(0b101),
            verified: false,
        })
    );
}

#[test]
fn more_controls_than_the_tap_observes_are_refused() {
    let mut k = keys();
    k.keyboard.lanes.extend((0..130).map(|i| LaneKey {
        control: 0x1200 + i,
        lane: 1,
    }));
    assert_eq!(
        AdjustmentConfig::default().plan(&k, None, None).err(),
        Some(AdjustKeyError::TooMany)
    );
}
