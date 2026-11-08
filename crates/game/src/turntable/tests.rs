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

// The probes of the pinned source (`KeyInputProccessor.input` and
// `JudgeManager.update` at chosen times): the clock and autoplay's state
// reproduce their printed values.

fn clock_at(ticks: impl IntoIterator<Item = (i64, Turn)>) -> super::TurntableClock {
    let mut c = super::TurntableClock::default();
    c.adopt(1);
    for (ms, turn) in ticks {
        c.tick(TimeUs(ms * MS), turn);
    }
    c
}

/// Idle for 600 ms: the source's `scratch` is 1 560 (−100°), and the
/// follower adds +100°.
#[test]
fn idle_600_ms_turns_minus_100_degrees() {
    let c = clock_at((0..=60).map(|i| (i * 10, Turn::Idle)));
    assert_eq!(c.source_sixths(), 1_560);
    assert_eq!(
        c.angle().rem_euclid(360 * 6_000),
        100 * 6_000,
        "the follower adds +100°"
    );
}

/// A first tick 500 ms after the play state began adds 0: the angle
/// counts from it.
#[test]
fn a_late_first_tick_adds_nothing() {
    let mut c = super::TurntableClock::default();
    c.adopt(1);
    c.tick(TimeUs(500 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), 0);
    c.tick(TimeUs(510 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), 2_150);
    c.tick(TimeUs(1_100 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), 1_560);
}

/// Positive for 300 ms turns −150°, then Negative +50°, then both
/// (Negative wins) +50° more.
#[test]
fn positive_then_negative_then_both() {
    let both = ScratchInput {
        positive: true,
        negative: true,
        analog: None,
    }
    .direction(TimeUs(0), [false; 2]);
    let ticks = (0..=30)
        .map(|i| (i * 10, Turn::Positive))
        .chain((31..=60).map(|i| (i * 10, Turn::Negative)))
        .chain((61..=90).map(|i| (i * 10, both)));
    let mut c = super::TurntableClock::default();
    c.adopt(1);
    let mut seen = Vec::new();
    for (ms, turn) in ticks {
        c.tick(TimeUs(ms * MS), turn);
        if ms % 300 == 0 && ms > 0 {
            seen.push(c.source_sixths());
        }
    }
    assert_eq!(seen, [1_260, 1_560, 1_860]);
}

#[test]
fn pause_resume_and_another_play_state() {
    let mut c = clock_at([(0, Turn::Idle), (100, Turn::Idle)]);
    let held = c.source_sixths();
    // Another number's pause is ignored.
    c.pause(2);
    c.tick(TimeUs(200 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held - 100);
    let held = c.source_sixths();
    c.pause(1);
    c.tick(TimeUs(5_000 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held);
    // Another number's resume is ignored; its own sets a fresh baseline.
    c.resume(2);
    c.tick(TimeUs(6_000 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held);
    c.resume(1);
    c.tick(TimeUs(7_000 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held);
    c.tick(TimeUs(7_010 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held - 10);
    // A resume of a running clock keeps its baseline.
    c.resume(1);
    c.tick(TimeUs(7_020 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), held - 20);
    // Another play state starts it over.
    c.adopt(2);
    assert_eq!(c.source_sixths(), 0);
    c.tick(TimeUs(8_000 * MS), Turn::Idle);
    assert_eq!(c.source_sixths(), 0);
}

/// A note for the state-machine fixtures.
fn note(ms: i64, kind: ScratchNoteKind) -> ScratchNote {
    ScratchNote {
        time: TimeUs(ms * MS),
        kind,
    }
}

fn tap(ms: i64) -> ScratchNote {
    note(ms, ScratchNoteKind::Tap)
}

/// A long note from `from` to `to` ms: its start and end events.
fn ln(from: i64, to: i64, charge: bool) -> [ScratchNote; 2] {
    [
        note(
            from,
            ScratchNoteKind::LongStart {
                end: TimeUs(to * MS),
                charge,
            },
        ),
        note(to, ScratchNoteKind::LongEnd { charge }),
    ]
}

/// The direction after each update at chart µs `t`, its state time
/// floored from `t + delta` µs.
fn run(notes: &[ScratchNote], delta: i64, times: &[i64]) -> Vec<(i64, i64, Turn)> {
    let mut s = super::AutoplayScratch::NEW;
    times
        .iter()
        .map(|&t| {
            let ms = super::state_ms(TimeUs(t + delta), TimeUs(0));
            s.update(notes, TimeUs(t), ms);
            (t, ms, ScratchInput::NONE.direction(TimeUs(0), s.pressed()))
        })
        .collect()
}

fn every(from: i64, to: i64, step: i64) -> Vec<i64> {
    (from..=to)
        .step_by(usize::try_from(step).unwrap())
        .map(|ms| ms * MS)
        .collect()
}

fn turn_at(out: &[(i64, i64, Turn)], t: i64) -> Turn {
    out.iter().find(|o| o.0 == t).unwrap().2
}

#[test]
fn a_tap_expires_after_more_than_80_ms() {
    let out = run(
        &[tap(1_000)],
        0,
        &[1_000_000, 1_040_000, 1_080_000, 1_081_000, 1_090_000],
    );
    let turns: Vec<Turn> = out.iter().map(|o| o.2).collect();
    use Turn::{Idle, Positive};
    assert_eq!(turns, [Positive, Positive, Positive, Idle, Idle]);
}

#[test]
fn a_plain_ln_holds_positive_one_update_past_its_end() {
    let out = run(&ln(1_000, 2_000, false), 0, &every(990, 2_100, 10));
    assert_eq!(turn_at(&out, 990_000), Turn::Idle);
    assert_eq!(turn_at(&out, 1_000_000), Turn::Positive);
    assert_eq!(turn_at(&out, 2_000_000), Turn::Positive);
    assert_eq!(turn_at(&out, 2_010_000), Turn::Positive);
    assert_eq!(turn_at(&out, 2_020_000), Turn::Idle);
}

#[test]
fn a_charge_note_end_turns_negative() {
    let out = run(&ln(1_000, 2_000, true), 0, &every(990, 2_100, 10));
    assert_eq!(turn_at(&out, 1_500_000), Turn::Positive);
    assert_eq!(turn_at(&out, 2_000_000), Turn::Negative);
    assert_eq!(turn_at(&out, 2_080_000), Turn::Negative);
    assert_eq!(turn_at(&out, 2_090_000), Turn::Idle);
}

#[test]
fn a_tap_after_a_charge_end_stays_negative_through_its_80_ms() {
    let [s, e] = ln(500, 1_000, true);
    let out = run(&[s, e, tap(1_040)], 0, &every(990, 1_200, 10));
    assert_eq!(turn_at(&out, 1_000_000), Turn::Negative);
    assert_eq!(turn_at(&out, 1_040_000), Turn::Negative);
    assert_eq!(turn_at(&out, 1_080_000), Turn::Negative);
    assert_eq!(turn_at(&out, 1_090_000), Turn::Positive);
    assert_eq!(turn_at(&out, 1_130_000), Turn::Idle);
}

#[test]
fn an_ln_after_a_charge_end_keeps_negative_through_it() {
    let [a, b] = ln(500, 1_000, true);
    let [c, d] = ln(1_040, 1_500, false);
    let out = run(&[a, b, c, d], 0, &every(990, 1_600, 10));
    for t in [1_000, 1_040, 1_500, 1_510] {
        assert_eq!(turn_at(&out, t * MS), Turn::Negative, "{t}");
    }
    assert_eq!(turn_at(&out, 1_520_000), Turn::Idle);
}

#[test]
fn two_taps_refresh_positive() {
    let out = run(&[tap(1_000), tap(1_040)], 0, &every(990, 1_200, 10));
    assert_eq!(turn_at(&out, 1_120_000), Turn::Positive);
    assert_eq!(turn_at(&out, 1_130_000), Turn::Idle);
}

#[test]
fn a_plain_ln_end_with_an_old_press() {
    let out = run(&ln(500, 1_000, false), 0, &every(490, 1_200, 10));
    assert_eq!(turn_at(&out, 1_010_000), Turn::Positive);
    assert_eq!(turn_at(&out, 1_020_000), Turn::Idle);
}

/// A frame crossing an expiry and a head: the head first, so the old
/// Negative survives the whole LN; a frame at 1 081 ms expires it first;
/// one at 1 080.5 ms floors to state 1 080 and does not.
#[test]
fn a_frame_crossing_an_expiry_and_a_head() {
    let [a, b] = ln(500, 1_000, true);
    let [c, d] = ln(1_082, 5_000, false);
    let notes = [a, b, c, d];
    let tail = every(1_090, 5_100, 10);
    let with = |extra: &[i64]| {
        let mut times = extra.to_vec();
        times.extend(&tail);
        run(&notes, 0, &times)
    };
    let a = with(&[1_000_000, 1_080_000]);
    assert_eq!(turn_at(&a, 1_090_000), Turn::Negative);
    assert_eq!(turn_at(&a, 5_010_000), Turn::Negative);
    assert_eq!(turn_at(&a, 5_020_000), Turn::Idle);
    let b = with(&[1_000_000, 1_080_000, 1_081_000]);
    assert_eq!(turn_at(&b, 1_081_000), Turn::Idle);
    assert_eq!(turn_at(&b, 1_090_000), Turn::Positive);
    assert_eq!(turn_at(&b, 5_010_000), Turn::Positive);
    let c = with(&[1_000_000, 1_080_500]);
    assert_eq!(turn_at(&c, 1_080_500), Turn::Negative);
    assert_eq!(turn_at(&c, 2_000_000), Turn::Negative);
}

/// Adjacent plain LNs, a frame every ms: the first's press lives to
/// 1 001, clears at 1 002, and the second is never pressed.
#[test]
fn adjacent_plain_lns() {
    let [a, b] = ln(500, 1_000, false);
    let [c, d] = ln(1_001, 1_500, false);
    let out = run(&[a, b, c, d], 0, &every(490, 1_600, 1));
    assert_eq!(turn_at(&out, 1_001_000), Turn::Positive);
    assert!(
        out.iter()
            .filter(|o| o.0 >= 1_002_000)
            .all(|o| o.2 == Turn::Idle)
    );
}

/// Fractional origins: the chart and state clocks differ by `delta`.
#[test]
fn fractional_origins_move_the_expiry() {
    let [a, b] = ln(500, 1_000, true);
    let [c, d] = ln(1_082, 5_000, false);
    let notes = [a, b, c, d];
    let mut times = vec![1_000_000, 1_080_500];
    times.extend(every(1_090, 5_100, 10));
    for (delta, stamp, at_1080_5, through) in [
        (0, 1_000, 1_080, Turn::Negative),
        (1_000_400, 2_000, 2_080, Turn::Negative),
        (999_600, 1_999, 2_080, Turn::Positive),
    ] {
        let out = run(&notes, delta, &times);
        assert_eq!(out[0].1, stamp, "{delta}");
        assert_eq!(out[1].1, at_1080_5, "{delta}");
        assert_eq!(turn_at(&out, 2_000_000), through, "{delta}");
    }
}
