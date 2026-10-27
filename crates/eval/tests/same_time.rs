//! Notes of one lane at one µs, a long note whose head and end share a µs,
//! and a mine at a note's µs, under the existing rules (design.md "What is
//! rounded", "Algorithms"): one assigned hit per press, nearest first and
//! earlier on a tie, earlier notes passed at a judgment.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{
    AxisBinding, AxisMode, ControlId, EvalInput, Evaluation, InputKind, JudgeEvent, JudgeEventKind,
    PhysicalInputEvent, ScratchSettings, SourceId, evaluate,
};
use rules::{GaugeValue, Judgment, RulePreset};

const BPM: i64 = 150_000;
const T: i64 = 1_000 * MS;

/// What happened to each note: (lane, note, judgment, deviation, time).
type Judged = (usize, usize, Judgment, Option<i64>, i64);

fn judged(events: &[JudgeEvent]) -> Vec<Judged> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                ..
            } => Some((
                lane.index(),
                note,
                judgment,
                deviation.map(|d| d.0),
                e.time.0,
            )),
            _ => None,
        })
        .collect()
}

fn mine_hits(events: &[JudgeEvent]) -> Vec<(usize, i64)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::MineHit { note, .. } => Some((note, e.time.0)),
            _ => None,
        })
        .collect()
}

fn eval_with(chart: &Chart, preset: &RulePreset, inputs: Vec<PhysicalInputEvent>) -> Evaluation {
    let input = EvalInput {
        events: inputs,
        device: device(&[0]),
        ..EvalInput::default()
    };
    let e = evaluate(chart, &input, preset).unwrap();
    // Evaluation always runs past every remaining deadline.
    assert!(e.completed);
    e
}

fn eval(chart: &Chart, inputs: Vec<PhysicalInputEvent>) -> Evaluation {
    eval_with(chart, &rules::custom(), inputs)
}

/// The final value of gauge `g` (0 normal, 1 survival).
fn gauge(e: &Evaluation, g: usize) -> i64 {
    e.gauges[g].value.0
}

fn pct(n: i64) -> i64 {
    n * GaugeValue::PERCENT
}

#[test]
fn one_press_at_the_later_note_passes_the_stacked_earlier_ones() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(100))
        .tap(lane(1), TimeUs(100))
        .tap(lane(1), TimeUs(101))
        .build()
        .unwrap();
    let e = eval(&chart, vec![press(101, 0, 1), release(200, 0, 1)]);
    assert_eq!(
        judged(&e.events),
        vec![
            (1, 0, Judgment::Poor, None, 101),
            (1, 1, Judgment::Poor, None, 101),
            (1, 2, Judgment::Perfect, Some(0), 101),
        ]
    );
}

#[test]
fn presses_at_one_microsecond_take_its_notes_in_order() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(100))
        .tap(lane(1), TimeUs(100))
        .tap(lane(1), TimeUs(101))
        .build()
        .unwrap();
    let inputs = vec![
        press(100, 0, 1),
        release(100, 0, 1),
        press(100, 0, 1),
        release(150, 0, 1),
    ];
    let e = eval(&chart, inputs);
    let j = judged(&e.events);
    assert_eq!(
        &j[..2],
        [
            (1, 0, Judgment::Perfect, Some(0), 100),
            (1, 1, Judgment::Perfect, Some(0), 100),
        ]
    );
    // The third note is left to its miss deadline at the end of BAD, 180 ms
    // late.
    assert_eq!(j[2], (1, 2, Judgment::Poor, None, 101 + 180 * MS));
    assert_eq!(j.len(), 3);
}

/// A long note whose head and end are both at `T`, on key 1.
/// A long note whose head and end are both at `T`, on key 1.
fn zero_length_ln() -> Chart {
    ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T))
        .build()
        .unwrap()
}

/// The long-note heads held: (note, judgment, deviation, time).
fn held(events: &[JudgeEvent]) -> Vec<(usize, Judgment, i64, i64)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::LnHeld {
                note,
                judgment,
                deviation,
                ..
            } => Some((note, judgment, deviation.0, e.time.0)),
            _ => None,
        })
        .collect()
}

/// Each judgment's release distance from its end, and its origin.
fn releases(events: &[JudgeEvent]) -> Vec<(Option<i64>, Option<i64>)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged { release, .. } => {
                Some((release.map(|r| r.0), e.origin.map(|o| o.0)))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_zero_length_ln_pressed_early_is_held_to_its_end() {
    // Regression: an early head of a zero-length long note is held, and
    // judged at its end (a deadline) or at the release that ends it.
    let chart = zero_length_ln();
    // Held through the end: the head's judgment at the end deadline.
    for (h, judgment) in [(-20 * MS, Judgment::Perfect), (-45 * MS, Judgment::Great)] {
        let e = eval(&chart, vec![press(T + h, 0, 1)]);
        assert_eq!(held(&e.events), vec![(0, judgment, h, T + h)], "{h}");
        assert_eq!(judged(&e.events), vec![(1, 0, judgment, Some(h), T)], "{h}");
        assert_eq!(releases(&e.events), vec![(None, None)], "{h}");
    }
    // Pressed 1 µs early, released at the end: judged at the release, which
    // is exactly at the end.
    let e = eval(&chart, vec![press(T - 1, 0, 1), release(T, 0, 1)]);
    assert_eq!(
        judged(&e.events),
        vec![(1, 0, Judgment::Perfect, Some(-1), T)]
    );
    assert_eq!(releases(&e.events), vec![(Some(0), Some(T))]);
    // Pressed and released 1 µs early: the release is 1 µs before the
    // end, inside the PERFECT edge (100 ms).
    let e = eval(&chart, vec![press(T - 1, 0, 1), release(T - 1, 0, 1)]);
    assert_eq!(
        judged(&e.events),
        vec![(1, 0, Judgment::Perfect, Some(-1), T - 1)]
    );
    assert_eq!(releases(&e.events), vec![(Some(-1), Some(T - 1))]);
    // A GOOD head (−100 ms) released 50 ms early (a PERFECT release): the
    // worse, GOOD, at the release.
    let e = eval(
        &chart,
        vec![press(T - 100 * MS, 0, 1), release(T - 50 * MS, 0, 1)],
    );
    assert_eq!(
        judged(&e.events),
        vec![(1, 0, Judgment::Good, Some(-100 * MS), T - 50 * MS)]
    );
    assert_eq!(
        releases(&e.events),
        vec![(Some(-50 * MS), Some(T - 50 * MS))]
    );
    assert_eq!((e.tally.combo, e.tally.bp), (1, 0));
}

#[test]
fn a_zero_length_ln_pressed_on_time_or_late_is_decided_at_the_press() {
    let chart = zero_length_ln();
    for (at, judgment) in [(T, Judgment::Perfect), (T + 30 * MS, Judgment::Great)] {
        let e = eval(&chart, vec![press(at, 0, 1), release(at + 200 * MS, 0, 1)]);
        assert_eq!(
            judged(&e.events),
            vec![(1, 0, judgment, Some(at - T), at)],
            "{at}"
        );
        // Decided at the press, never held; the later release adds nothing.
        assert_eq!(held(&e.events), vec![], "{at}");
        assert_eq!(releases(&e.events), vec![(None, Some(at))], "{at}");
        assert_eq!(e.tally.bp, 0);
    }
}

#[test]
fn a_missed_zero_length_ln_is_one_poor() {
    let chart = zero_length_ln();
    let e = eval(&chart, vec![]);
    assert_eq!(
        judged(&e.events),
        vec![(1, 0, Judgment::Poor, None, T + 180 * MS)]
    );
    assert_eq!(held(&e.events), vec![]);
    assert_eq!(e.tally.bp, 1);
}

#[test]
fn a_head_at_the_microsecond_its_previous_ln_ends() {
    // Regression for the judgment time: LN A from T to T + 200 ms and a tap
    // B at its end. Held through the end, released and pressed again at
    // that µs: A is judged by the release, exactly at its end, then B.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 200 * MS))
        .tap(lane(1), TimeUs(T + 200 * MS))
        .build()
        .unwrap();
    let inputs = vec![
        press(T, 0, 1),
        release(T + 200 * MS, 0, 1),
        press(T + 200 * MS, 0, 1),
        release(T + 250 * MS, 0, 1),
    ];
    let e = eval(&chart, inputs);
    assert_eq!(held(&e.events), vec![(0, Judgment::Perfect, 0, T)]);
    assert_eq!(
        judged(&e.events),
        vec![
            (1, 0, Judgment::Perfect, Some(0), T + 200 * MS),
            (1, 1, Judgment::Perfect, Some(0), T + 200 * MS),
        ]
    );
    assert_eq!(
        releases(&e.events),
        vec![(Some(0), Some(T + 200 * MS)), (None, Some(T + 200 * MS))]
    );
    assert_eq!((e.tally.bp, e.tally.empty_poor, e.tally.combo), (0, 0, 2));
}

/// Source 0 with an absolute axis (256 a revolution, default thresholds) on
/// the scratch instead of a button.
const AXIS: ControlId = ControlId(0x1_0030);

fn report(received: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(0),
        kind: InputKind::Control {
            control: AXIS,
            value,
        },
    }
}

#[test]
fn a_scratch_reversal_takes_the_second_note_of_its_microsecond() {
    let chart = ChartBuilder::new(BPM)
        .tap(Lane::SCRATCH, TimeUs(T))
        .tap(Lane::SCRATCH, TimeUs(T))
        .build()
        .unwrap();
    let mut device = device(&[0]);
    device.sources[0]
        .bindings
        .retain(|b| b.lane != Lane::SCRATCH);
    device.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: Lane::SCRATCH,
        scratch: ScratchSettings {
            mode: AxisMode::Absolute { wrap: 256 },
            ..ScratchSettings::DEFAULT
        },
    });
    // Where the axis is, then +2 (a press) and -2 (a reversal: a release
    // and a new press) in one µs.
    let input = EvalInput {
        events: vec![report(T - 100 * MS, 100), report(T, 102), report(T, 100)],
        device,
        ..EvalInput::default()
    };
    let e = evaluate(&chart, &input, &rules::custom()).unwrap();
    assert_eq!(
        judged(&e.events),
        vec![
            (0, 0, Judgment::Perfect, Some(0), T),
            (0, 1, Judgment::Perfect, Some(0), T),
        ]
    );
    assert_eq!((e.tally.bp, e.tally.empty_poor), (0, 0));
}

#[test]
fn a_mine_at_a_notes_microsecond_hits_only_while_held() {
    // A tap and a mine of damage 10 at T on key 1, in either order.
    let tap_first = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .mine(lane(1), TimeUs(T), 10)
        .build()
        .unwrap();
    let mine_first = ChartBuilder::new(BPM)
        .mine(lane(1), TimeUs(T), 10)
        .tap(lane(1), TimeUs(T))
        .build()
        .unwrap();
    let alone = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .build()
        .unwrap();
    let held = || vec![press(T, 0, 1), release(T + 50 * MS, 0, 1)];
    let tapped = || vec![press(T, 0, 1), release(T, 0, 1)];
    let base = eval(&alone, held());
    for (chart, tap) in [(&tap_first, 0), (&mine_first, 1)] {
        // Held through T: the tap is judged once and the mine hits at T.
        let e = eval(chart, held());
        assert_eq!(
            judged(&e.events),
            vec![(1, tap, Judgment::Perfect, Some(0), T)]
        );
        assert_eq!(mine_hits(&e.events), vec![(1 - tap, T)]);
        // Half a percent per damage unit: 5 % off the normal gauge, and
        // off the survival gauge's capped 100 %.
        assert_eq!(gauge(&base, 0) - gauge(&e, 0), pct(5));
        assert_eq!(gauge(&e, 1), pct(95));
        // Released in the same µs: the tap is judged, the mine misses.
        let e = eval(chart, tapped());
        assert_eq!(
            judged(&e.events),
            vec![(1, tap, Judgment::Perfect, Some(0), T)]
        );
        assert_eq!(mine_hits(&e.events), vec![]);
        assert_eq!(gauge(&e, 0), gauge(&base, 0));
    }
}
