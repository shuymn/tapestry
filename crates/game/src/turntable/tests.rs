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
        ingress: input::Ingress::INITIAL,
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

// The probes of the pinned source (`KeyInputProccessor.input` at chosen
// times): the clock reproduces their printed values.

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

fn every(from: i64, to: i64, step: i64) -> Vec<i64> {
    (from..=to)
        .step_by(usize::try_from(step).unwrap())
        .map(|ms| ms * MS)
        .collect()
}

/// One update of `s` at chart µs `t` over `notes`, and the presses it
/// gives.
fn update(s: &mut super::AutoplayScratch, notes: &[ScratchNote], t: i64) -> [bool; 2] {
    s.update(notes, TimeUs(t));
    s.pressed()
}

/// The direction autoplay alone gives at each chart µs of `times`,
/// updated in that order, and at each from a fresh state.
fn turns(notes: &[ScratchNote], times: &[i64]) -> Vec<Turn> {
    let direction = |pressed| ScratchInput::NONE.direction(TimeUs(0), pressed);
    let mut s = super::AutoplayScratch::NEW;
    times
        .iter()
        .map(|&t| {
            let run = direction(update(&mut s, notes, t));
            let fresh = direction(update(&mut super::AutoplayScratch::default(), notes, t));
            assert_eq!(run, fresh, "at {t}");
            run
        })
        .collect()
}

#[test]
fn a_tap_turns_for_80_ms() {
    use Turn::{Idle, Positive};
    let times = [999_999, 1_000_000, 1_079_999, 1_080_000];
    assert_eq!(
        turns(&[tap(1_000)], &times),
        [Idle, Positive, Positive, Idle]
    );
}

#[test]
fn a_note_at_the_start_turns() {
    use Turn::{Idle, Positive};
    assert_eq!(
        turns(&[tap(0)], &[0, 79_999, 80_000]),
        [Positive, Positive, Idle]
    );
    assert_eq!(
        turns(&ln(0, 500, false), &[0, 499_999, 500_000]),
        [Positive, Positive, Idle]
    );
}

#[test]
fn a_long_note_turns_for_its_length_at_least_80_ms() {
    use Turn::{Idle, Positive};
    let times = [999_999, 1_000_000, 1_999_999, 2_000_000];
    assert_eq!(
        turns(&ln(1_000, 2_000, false), &times),
        [Idle, Positive, Positive, Idle]
    );
    let times = [1_000_000, 1_029_999, 1_030_000, 1_079_999, 1_080_000];
    assert_eq!(
        turns(&ln(1_000, 1_030, false), &times),
        [Positive, Positive, Positive, Positive, Idle]
    );
}

/// A charge note's end turns the turntable back, the reverse turn that
/// end is played with.
// Source: SF-010
#[test]
fn a_charge_end_turns_back_for_80_ms() {
    use Turn::{Idle, Negative, Positive};
    let times = [1_999_999, 2_000_000, 2_079_999, 2_080_000];
    assert_eq!(
        turns(&ln(1_000, 2_000, true), &times),
        [Positive, Negative, Negative, Idle]
    );
}

/// Presses closer than 80 ms join, Negative first.
#[test]
fn presses_overlap_as_a_union() {
    use Turn::{Idle, Negative, Positive};
    // Two taps: Positive over [1 000, 1 120) ms.
    let times = [
        999_999, 1_000_000, 1_079_999, 1_080_000, 1_119_999, 1_120_000,
    ];
    assert_eq!(
        turns(&[tap(1_000), tap(1_040)], &times),
        [Idle, Positive, Positive, Positive, Positive, Idle]
    );
    // A charge end, then a tap: Negative over [1 000, 1 080), then
    // Positive to 1 120.
    let [s, e] = ln(500, 1_000, true);
    assert_eq!(
        turns(&[s, e, tap(1_040)], &times),
        [Positive, Negative, Negative, Positive, Positive, Idle]
    );
    // A charge end, then a long note: Negative over [1 000, 1 080), then
    // Positive to 1 500.
    let [c, d] = ln(1_040, 1_500, false);
    let times = [1_000_000, 1_079_999, 1_080_000, 1_499_999, 1_500_000];
    assert_eq!(
        turns(&[s, e, c, d], &times),
        [Negative, Negative, Positive, Positive, Idle]
    );
}

/// Plain long notes 1 ms apart, a frame every ms: both turn, idle for the
/// millisecond between.
#[test]
fn adjacent_long_notes_both_turn() {
    let [a, b] = ln(500, 1_000, false);
    let [c, d] = ln(1_001, 1_500, false);
    let times = every(490, 1_600, 1);
    let want: Vec<Turn> = times
        .iter()
        .map(|&t| {
            if (500_000..1_000_000).contains(&t) || (1_001_000..1_500_000).contains(&t) {
                Turn::Positive
            } else {
                Turn::Idle
            }
        })
        .collect();
    assert_eq!(turns(&[a, b, c, d], &times), want);
}

/// The presses at chart µs `t` by definition: the notes' intervals that
/// hold `t`, over every note.
fn defined(notes: &[ScratchNote], t: i64) -> [bool; 2] {
    let hold = 80 * MS;
    let mut out = [false; 2];
    for n in notes {
        let s = n.time.0;
        let (negative, end) = match n.kind {
            ScratchNoteKind::Tap => (false, s + hold),
            ScratchNoteKind::LongStart { end, .. } => (false, end.0.max(s + hold)),
            ScratchNoteKind::LongEnd { charge: true } => (true, s + hold),
            ScratchNoteKind::LongEnd { charge: false } => continue,
        };
        if s <= t && t < end {
            out[usize::from(negative)] = true;
        }
    }
    out
}

/// Whatever the calls before, the presses at a chart time are the
/// definition's: frames every ms, at 60 Hz, irregular with repeated
/// times and steps back, and first calls late in the chart.
#[test]
fn the_press_depends_on_chart_time_alone() {
    let mut notes = vec![tap(0), tap(1_000), tap(1_040)];
    notes.extend(ln(1_100, 1_120, false));
    notes.extend(ln(2_000, 3_000, true));
    notes.push(tap(3_020));
    notes.extend(ln(3_050, 4_000, false));
    notes.extend(ln(4_001, 4_500, false));
    let [s, e] = ln(5_500, 6_000, true);
    notes.extend([s, e, tap(6_000)]);
    let sixty_hz: Vec<i64> = (0..450).map(|i| i * 16_667).collect();
    let irregular = [
        0, 0, 999_999, 1_000_000, 1_000_000, 1_079_999, 1_080_000, 3_500_000, 1_050_000, 1_100_000,
        2_000_000, 2_000_000, 1_999_999, 6_000_000, 5_999_999, 6_079_999, 6_080_000, 7_000_000,
    ];
    let histories: [(&str, &[i64]); 5] = [
        ("every ms", &every(-10, 7_000, 1)),
        ("60 Hz", &sixty_hz),
        ("irregular", &irregular),
        ("first at 5 s", &[5_000_000, 5_500_000, 6_050_000]),
        (
            "first inside a long note",
            &[3_500_000, 3_999_999, 4_000_000],
        ),
    ];
    for (name, times) in histories {
        let mut st = super::AutoplayScratch::NEW;
        for &t in times {
            assert_eq!(
                update(&mut st, &notes, t),
                defined(&notes, t),
                "{name} at {t}"
            );
        }
    }
    // A first call at 5 s, its tap at 1 s long past: idle.
    let mut st = super::AutoplayScratch::NEW;
    assert_eq!(update(&mut st, &[tap(1_000)], 5_000 * MS), [false; 2]);
}

/// The Positive intervals equal the generated input's presses of the
/// scratch key: a tap held 80 ms, a long note held to its end.
#[test]
fn a_tap_turns_as_long_as_autoplay_holds_it() {
    let chart = ChartBuilder::new(120_000)
        .tap(Lane::SCRATCH, TimeUs(1_000 * MS))
        .long(Lane::SCRATCH, TimeUs(2_000 * MS), TimeUs(2_500 * MS))
        .build()
        .unwrap();
    let control = eval::ControlId(0x100);
    let profile = eval::SourceProfile {
        input_offset: TimeUs(0),
        bindings: vec![eval::Binding {
            control,
            lane: Lane::SCRATCH,
        }],
        axes: Vec::new(),
    };
    let mut held = Vec::new();
    let mut pressed_at = None;
    for e in input::autoplay(&chart, &profile).unwrap() {
        match e.kind {
            eval::InputKind::Control { value: 0, .. } => {
                held.push((pressed_at.take().unwrap(), e.received.0));
            }
            eval::InputKind::Control { .. } => pressed_at = Some(e.received.0),
            eval::InputKind::Disconnected => unreachable!(),
        }
    }
    assert_eq!(held.len(), 2);
    let notes = super::scratch_notes(&chart, None);
    let mut st = super::AutoplayScratch::NEW;
    let mut turned = Vec::new();
    let mut from = None;
    for t in 0..=3_000 * MS {
        let [positive, negative] = update(&mut st, &notes, t);
        assert!(!negative);
        match (positive, from) {
            (true, None) => from = Some(t),
            (false, Some(f)) => {
                turned.push((f, t));
                from = None;
            }
            _ => {}
        }
    }
    assert_eq!(turned, held);
}

/// A press whose deadline passed before a movement is released first, as
/// the evaluator releases it: a movement below the press threshold then
/// presses nothing. At the deadline itself the movement comes first and
/// holds the press.
#[test]
fn an_expired_press_is_released_before_a_later_movement() {
    for (late_ms, held) in [(101, false), (100, true)] {
        let plan = plan(false, None);
        let mut d = ScratchDevice::new(&plan);
        d.observe(&axis(&plan, 0, 2, 0));
        assert!(d.input().analog.is_some());
        // The hold threshold is 1, the press threshold 2.
        d.observe(&axis(&plan, late_ms, 1, 0));
        assert_eq!(d.input().analog.is_some(), held, "{late_ms} ms");
    }
}
