use bms_model::{ChartBuilder, Lane, LnMode, TimeUs};
use input::hid::axis_control;
use input::{TapEvent, TapObservation};

use super::{AnalogTurn, ScratchDevice, ScratchInput, ScratchNote, ScratchNoteKind, Turn};
use crate::adjust_keys::{AdjustmentConfig, AdjustmentPlan};
use crate::adjustment::AxisQuantum;
use crate::keys::KeyConfigs;
use crate::settings::{DeviceSettings, LaneKey};

const MS: i64 = 1_000;

/// A controller whose scratch is its turntable axis and two buttons,
/// button 1 its Positive and button 2 its Negative direction.
fn plan(reversed: bool, quantum: Option<AxisQuantum>) -> AdjustmentPlan {
    let mut k = KeyConfigs::from(&DeviceSettings::default());
    k.controller.lanes = [axis_control(0x01, 0x30).0, 1, 2]
        .map(|control| LaneKey { control, lane: 0 })
        .to_vec();
    k.controller.scratch_positive = Some(1);
    k.controller.scratch_negative = Some(2);
    k.controller_device = Some(input::hid::HidProduct {
        vendor_id: 7,
        product_id: 8,
        interface: None,
    });
    AdjustmentConfig {
        reversed,
        ..AdjustmentConfig::default()
    }
    .plan(&k, quantum, None)
    .unwrap()
}

fn number(plan: &AdjustmentPlan, button: u32) -> u8 {
    plan.bindings
        .controls
        .iter()
        .find(|(c, _)| {
            matches!(c, input::TapControl::Controller { control, .. } if control.0 == button)
        })
        .unwrap()
        .1
}

fn at(ms: i64, event: TapEvent) -> TapObservation {
    TapObservation {
        at: TimeUs(ms * MS),
        seq: 0,
        event,
    }
}

fn axis(plan: &AdjustmentPlan, ms: i64, delta: i64, revolution: u32) -> TapObservation {
    at(
        ms,
        TapEvent::Axis {
            control: plan.axis().unwrap(),
            delta,
            revolution,
        },
    )
}

#[test]
fn digital_flags_follow_holds_losses_and_verified_resyncs() {
    let plan = plan(false, None);
    let (pos, neg) = (number(&plan, 1), number(&plan, 2));
    let mut d = ScratchDevice::new(&plan);
    let flags = |d: &ScratchDevice| (d.input().positive, d.input().negative);
    assert_eq!(d.input(), ScratchInput::NONE);
    d.observe(&at(1, TapEvent::Press(pos)));
    assert_eq!(flags(&d), (true, false));
    d.observe(&at(2, TapEvent::Press(neg)));
    assert_eq!(flags(&d), (true, true));
    // Both are published; the frame applies the priority.
    assert_eq!(d.input().direction(TimeUs(0), [false; 2]), Turn::Negative);
    d.observe(&at(3, TapEvent::Release(neg)));
    assert_eq!(flags(&d), (true, false));
    d.observe(&at(4, TapEvent::Lost));
    assert_eq!(d.input(), ScratchInput::NONE);
    let held = 1u128 << neg;
    d.observe(&at(
        5,
        TapEvent::Resync {
            held,
            verified: false,
        },
    ));
    assert_eq!(d.input(), ScratchInput::NONE);
    d.observe(&at(
        6,
        TapEvent::Resync {
            held,
            verified: true,
        },
    ));
    assert_eq!(flags(&d), (false, true));
    d.observe(&at(7, TapEvent::Release(neg)));
    assert_eq!(flags(&d), (false, false));
}

/// Every axis turns the mark, with or without a cover-adjustment
/// quantum: the plan drops a relative or non-wrapping axis without one,
/// the device does not.
#[test]
fn the_turn_step_takes_every_axis_before_the_quantum() {
    for quantum in [None, AxisQuantum::per_step(5)] {
        for revolution in [0, 256] {
            let plan = plan(false, quantum);
            let mut d = ScratchDevice::new(&plan);
            let o = axis(&plan, 10, 3, revolution);
            assert_eq!(
                plan.observation(&o).is_some(),
                quantum.is_some() || revolution != 0
            );
            d.observe(&o);
            // The default press threshold is 2 and the release 100 ms.
            assert_eq!(
                d.input().analog,
                Some(AnalogTurn {
                    negative: false,
                    until: TimeUs(110 * MS),
                }),
                "{quantum:?} {revolution}"
            );
            // A reversal presses the other way.
            d.observe(&axis(&plan, 20, -2, revolution));
            assert_eq!(
                d.input().analog,
                Some(AnalogTurn {
                    negative: true,
                    until: TimeUs(120 * MS),
                })
            );
        }
    }
    // The adjustment's reversal turns it the other way.
    let plan = plan(true, None);
    let mut d = ScratchDevice::new(&plan);
    d.observe(&axis(&plan, 10, 3, 0));
    assert!(d.input().analog.unwrap().negative);
}

#[test]
fn a_frame_expires_the_analog_press_then_takes_negative_first() {
    let analog = |negative| {
        Some(AnalogTurn {
            negative,
            until: TimeUs(100),
        })
    };
    let s = |positive, negative, analog| ScratchInput {
        positive,
        negative,
        analog,
    };
    let none = [false; 2];
    // Analog Negative over digital Positive until its deadline.
    let a = s(true, false, analog(true));
    assert_eq!(a.direction(TimeUs(99), none), Turn::Negative);
    assert_eq!(a.direction(TimeUs(100), none), Turn::Positive);
    // Digital Negative throughout.
    let b = s(false, true, analog(false));
    assert_eq!(b.direction(TimeUs(99), none), Turn::Negative);
    assert_eq!(b.direction(TimeUs(100), none), Turn::Negative);
    // Nothing digital: idle after it.
    let c = s(false, false, analog(true));
    assert_eq!(c.direction(TimeUs(99), none), Turn::Negative);
    assert_eq!(c.direction(TimeUs(100), none), Turn::Idle);
    // Autoplay's presses join: a Negative of either wins over a Positive
    // of the other.
    let d = s(false, true, None);
    assert_eq!(d.direction(TimeUs(0), [true, false]), Turn::Negative);
    let e = s(true, false, None);
    assert_eq!(e.direction(TimeUs(0), [false, true]), Turn::Negative);
    assert_eq!(e.direction(TimeUs(0), [true, false]), Turn::Positive);
}

#[test]
fn scratch_notes_mark_charge_ends_by_the_charts_mode() {
    let mut chart = ChartBuilder::new(120_000)
        .tap(Lane::SCRATCH, TimeUs(1_000 * MS))
        .long(Lane::SCRATCH, TimeUs(2_000 * MS), TimeUs(3_000 * MS))
        .mine(Lane::SCRATCH, TimeUs(3_500 * MS), 10)
        .tap(Lane::new(1).unwrap(), TimeUs(1_500 * MS))
        .build()
        .unwrap();
    for (mode, charge) in [
        (None, false),
        (Some(LnMode::Ln), false),
        (Some(LnMode::Cn), true),
        (Some(LnMode::Hcn), true),
    ] {
        chart.meta.ln_mode = mode;
        let end = TimeUs(3_000 * MS);
        assert_eq!(
            super::scratch_notes(&chart, mode),
            [
                ScratchNote {
                    time: TimeUs(1_000 * MS),
                    kind: ScratchNoteKind::Tap,
                },
                ScratchNote {
                    time: TimeUs(2_000 * MS),
                    kind: ScratchNoteKind::LongStart { end, charge },
                },
                ScratchNote {
                    time: end,
                    kind: ScratchNoteKind::LongEnd { charge },
                },
            ],
            "{mode:?}"
        );
    }
}
