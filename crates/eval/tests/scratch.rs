//! Analog scratch: axis reports converted into presses and releases of a
//! lane, with the release after the axis stops as a deadline in the
//! evaluator's scan (design.md "Analog Scratch").
//!
//! The axis is control `AXIS` of source 0, bound to the scratch as
//! `TURNTABLE` unless a test says otherwise: absolute, 256 positions per
//! revolution, press after 2, hold by 1, release 100 ms after the last
//! press or hold. Evaluation times equal receive times (offset 0).

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::{MS, device, disconnect, lane, press, release};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, ConversionState, DeviceProfile, EvalError,
    EvalInput, Evaluator, InputKind, InputStart, JudgeEvent, JudgeEventKind, LaneEdge,
    PhysicalInputEvent, PushError, ScratchSettings, SourceId, evaluate, window_inputs,
};
use rules::Judgment::{self, Bad, Perfect};

const BPM: i64 = 150_000;
const AXIS: ControlId = ControlId(0x1_0030);
const SCRATCH: Lane = Lane::SCRATCH;

/// An absolute axis of 256 values a revolution with the default
/// thresholds.
const TURNTABLE: ScratchSettings = ScratchSettings {
    mode: AxisMode::Absolute { wrap: 256 },
    ..ScratchSettings::DEFAULT
};

/// Source 0 with the axis on the scratch and keys 1-7 on controls 1-7.
fn axis_device(scratch: ScratchSettings) -> DeviceProfile {
    let mut d = device(&[0]);
    d.sources[0].bindings.retain(|b| b.lane != SCRATCH);
    d.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: SCRATCH,
        scratch,
    });
    d
}

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

/// Conversion alone: applies each input at its receive time.
struct Conv(ConversionState);

impl Conv {
    fn new(scratch: ScratchSettings) -> Self {
        Self::with_start(scratch, &InputStart::default())
    }

    fn with_start(scratch: ScratchSettings, start: &InputStart) -> Self {
        Self(ConversionState::init(&axis_device(scratch), start).unwrap())
    }

    fn step(&mut self, ev: PhysicalInputEvent) -> Vec<LaneEdge> {
        self.0.apply(&ev, ev.received).as_slice().to_vec()
    }

    /// Edges of reports `(time µs, value)` in order, one list per report.
    fn reports(&mut self, rs: &[(i64, i32)]) -> Vec<Vec<LaneEdge>> {
        rs.iter().map(|&(t, v)| self.step(report(t, v))).collect()
    }

    fn deadline(&self) -> Option<i64> {
        self.0.next_release().map(|(t, _, _)| t.0)
    }
}

const P: LaneEdge = LaneEdge::Press(SCRATCH);
const R: LaneEdge = LaneEdge::Release(SCRATCH);

/// Evaluates in one step up to `watermark`.
fn run(
    chart: &Chart,
    dev: &DeviceProfile,
    inputs: &[PhysicalInputEvent],
    to: i64,
) -> Vec<JudgeEvent> {
    let rule = rules::custom();
    let mut ev = Evaluator::new(chart, &rule, dev, &InputStart::default(), 256).unwrap();
    for &i in inputs {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(to), &mut out).unwrap();
    out
}

/// `(time, judgment, release, origin)` of each long note judged after its
/// head was held.
fn ln_ends(events: &[JudgeEvent]) -> Vec<(i64, Judgment, Option<i64>, Option<i64>)> {
    let mut held = Vec::new();
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::LnHeld { lane, note, .. } => {
                held.push((lane, note));
                None
            }
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                release,
                ..
            } if held.contains(&(lane, note)) => Some((
                e.time.0,
                judgment,
                release.map(|r| r.0),
                e.origin.map(|o| o.0),
            )),
            _ => None,
        })
        .collect()
}

/// `(lane, note, judgment, deviation)` of each judgment.
fn judged(events: &[JudgeEvent]) -> Vec<(usize, usize, rules::Judgment, Option<i64>)> {
    common::judgments(events)
}

/// An LN on the scratch from 1000 to 2000 ms.
fn scratch_ln() -> Chart {
    ChartBuilder::new(BPM)
        .long(SCRATCH, TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .build()
        .unwrap()
}

#[test]
fn axis_wraparound_is_continuous() {
    // Forward through 255 → 0: one movement of 1 each step. The press comes
    // at 2 positions (2 ms) and nothing releases on the way round.
    let mut c = Conv::new(TURNTABLE);
    let fwd = c.reports(&[(0, 254), (1, 255), (2, 0), (3, 1), (4, 2)]);
    assert_eq!(fwd, [vec![], vec![], vec![P], vec![], vec![]]);
    assert_eq!(c.deadline(), Some(4 + 100 * MS), "each step held it");
    // Backward through 0 → 255: a reversal at 2 positions back (12 ms),
    // then holds on the way round.
    let back = c.reports(&[(11, 1), (12, 0), (13, 255), (14, 254)]);
    assert_eq!(back, [vec![], vec![R, P], vec![], vec![]]);
    assert_eq!(c.deadline(), Some(14 + 100 * MS));

    // Without wraparound, 255 → 0 is 255 positions back: a reversal.
    let no_wrap = ScratchSettings {
        mode: AxisMode::Absolute { wrap: 0 },
        ..TURNTABLE
    };
    let mut c = Conv::new(no_wrap);
    let edges = c.reports(&[(0, 254), (1, 255), (2, 256), (3, 0)]);
    assert_eq!(edges, [vec![], vec![], vec![P], vec![R, P]]);

    // Half a revolution counts forward; one more is backward.
    let mut c = Conv::new(TURNTABLE);
    // 0 → 128 is +128 (press forward); 128 → 130 holds.
    assert_eq!(
        c.reports(&[(0, 0), (1, 128), (2, 130)]),
        [vec![], vec![P], vec![]]
    );
    let mut c = Conv::new(TURNTABLE);
    // 0 → 129 is −127 (press backward); 129 → 131 reverses.
    assert_eq!(
        c.reports(&[(0, 0), (1, 129), (2, 131)]),
        [vec![], vec![P], vec![R, P]]
    );
}

#[test]
fn reversal_is_a_new_press() {
    // Two taps on the scratch 60 ms apart, hit by turning one way and then
    // the other.
    let chart = ChartBuilder::new(BPM)
        .tap(SCRATCH, TimeUs(1_000 * MS))
        .tap(SCRATCH, TimeUs(1_060 * MS))
        .build()
        .unwrap();
    let dev = axis_device(TURNTABLE);
    let inputs = [
        report(900 * MS, 100),   // where the axis is
        report(1_000 * MS, 102), // +2: press, the first tap
        report(1_030 * MS, 101), // −1: below the threshold, still pressed
        report(1_060 * MS, 100), // −2: a new press, the second tap
    ];
    let events = run(&chart, &dev, &inputs, 2_000 * MS);
    assert_eq!(
        judged(&events),
        [(0, 0, Perfect, Some(0)), (0, 1, Perfect, Some(0))]
    );

    let mut c = Conv::new(TURNTABLE);
    let edges: Vec<_> = inputs.iter().map(|&e| c.step(e)).collect();
    assert_eq!(edges, [vec![], vec![P], vec![], vec![R, P]]);
}

#[test]
fn release_after_stop_is_a_deadline_not_a_frame() {
    let chart = scratch_ln();
    let dev = axis_device(TURNTABLE);
    let rule = rules::custom();
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), 8).unwrap();
    ev.push_input(report(900 * MS, 10)).unwrap();
    ev.push_input(report(1_000 * MS, 12)).unwrap(); // press: head PERFECT
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_100 * MS - 1), &mut out).unwrap();
    assert!(
        out.iter()
            .any(|e| matches!(e.kind, JudgeEventKind::LnHeld { .. }))
    );
    assert_eq!(judged(&out), [], "the release waits for the watermark");
    // One jump far past the deadline: the release happens at the
    // deadline, 100 ms after the last movement, not at the step. It is
    // 900 ms before the end, past every LN-end edge: one BAD, with no
    // input as its origin.
    ev.advance_to(TimeUs(1_900 * MS), &mut out).unwrap();
    assert_eq!(ln_ends(&out), [(1_100 * MS, Bad, Some(-900 * MS), None)]);
    assert_eq!(judged(&out), [(0, 0, Bad, Some(0))]);
    assert_eq!((ev.tally().counts.bad, ev.tally().bp), (1, 1));
    // The timeout ends the visual hold too, before the end.
    let ended: Vec<_> = out
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::LnHoldEnded { .. }))
        .map(|e| (e.time.0, e.origin))
        .collect();
    assert_eq!(ended, [(1_100 * MS, None)]);
}

#[test]
fn release_deadline_is_on_the_evaluation_timeline() {
    // The source is 5 ms late: received at 1005 ms is 1000 ms for
    // judgment, and the release is due 100 ms after that, at 1100 ms.
    let chart = scratch_ln();
    let mut dev = axis_device(TURNTABLE);
    dev.sources[0].input_offset = TimeUs(5 * MS);
    let events = run(
        &chart,
        &dev,
        &[report(905 * MS, 10), report(1_005 * MS, 12)],
        1_900 * MS,
    );
    assert_eq!(judged(&events), [(0, 0, Bad, Some(0))]);
    assert_eq!(ln_ends(&events), [(1_100 * MS, Bad, Some(-900 * MS), None)]);
}

#[test]
fn movement_moves_pending_release_deadline() {
    let chart = scratch_ln();
    let dev = axis_device(TURNTABLE);
    let rule = rules::custom();
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), 8).unwrap();
    for e in [
        report(900 * MS, 10),
        report(1_000 * MS, 12), // press; release due at 1100 ms
        report(1_050 * MS, 13), // hold; due at 1150 ms
        report(1_140 * MS, 14), // hold; due at 1240 ms
        report(1_240 * MS, 15), // at the deadline: inputs first, due at 1340 ms
    ] {
        ev.push_input(e).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_340 * MS - 1), &mut out).unwrap();
    assert_eq!(judged(&out), [], "no release at any earlier deadline");
    ev.advance_to(TimeUs(1_340 * MS), &mut out).unwrap();
    assert_eq!(ln_ends(&out), [(1_340 * MS, Bad, Some(-660 * MS), None)]);
}

#[test]
fn hid_disconnect_releases_scratch_and_buttons() {
    // The scratch LN is held by the axis and the key 3 LN by a button of
    // the same device, which is removed at 1500 ms. The axis releases 1 s
    // after it stops.
    let chart = ChartBuilder::new(BPM)
        .long(SCRATCH, TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .long(lane(3), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let slow = ScratchSettings {
        release_after_us: 1_000_000,
        ..TURNTABLE
    };
    let dev = axis_device(slow);
    let inputs = [
        report(900 * MS, 10),
        report(1_000 * MS, 12),
        press(1_000 * MS, 0, 3),
        report(1_450 * MS, 13), // release due at 2450 ms
        disconnect(1_500 * MS, 0),
    ];
    let events = run(&chart, &dev, &inputs, 3_000 * MS);
    assert_eq!(judged(&events).len(), 2);
    // Both are released at the removal, as inputs, 500 ms before their
    // end: BAD. The pending release is gone.
    assert_eq!(
        ln_ends(&events),
        [
            (1_500 * MS, Bad, Some(-500 * MS), Some(1_500 * MS)),
            (1_500 * MS, Bad, Some(-500 * MS), Some(1_500 * MS)),
        ]
    );

    let mut c = Conv::new(slow);
    let edges: Vec<_> = inputs.iter().map(|&e| c.step(e)).collect();
    assert_eq!(
        edges,
        [
            vec![],
            vec![P],
            vec![LaneEdge::Press(lane(3))],
            vec![],
            vec![R, LaneEdge::Release(lane(3))]
        ]
    );
    assert_eq!(c.deadline(), None);
    // Where the axis was is forgotten: the next report only sets it.
    assert_eq!(c.reports(&[(1_600 * MS, 200)]), [vec![]]);
}

#[test]
fn axis_capacity_holds_a_song_of_1khz_reports() {
    // A three-minute chart: taps on key 1 every second to 180 s, so the
    // final deadline is 180 s + 180 ms. With a 1 s lead-in the window is
    // 181.18 s, 182 whole seconds; 9 buttons (7 keys, retry, quit) and one
    // axis get 182 × (min(9 × 60, 240) + 1000) = 225_680 inputs.
    let mut b = ChartBuilder::new(BPM).tap(SCRATCH, TimeUs(500 * MS));
    for s in 1..=180 {
        b = b.tap(lane(1), TimeUs(s * 1_000 * MS));
    }
    let chart = b.build().unwrap();
    let dev = axis_device(TURNTABLE);
    let rule = rules::custom();
    let probe = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), 0).unwrap();
    assert_eq!(probe.final_deadline(), TimeUs(180_180 * MS));
    let lead_in = 1_000 * MS;
    let span = TimeUs(lead_in + probe.final_deadline().0);
    let capacity = window_inputs(span, &[(9, 1)]);
    assert_eq!(capacity, 225_680);

    // The axis turns forward one position every millisecond from the start
    // of the countdown to the final deadline: 181_181 reports.
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), capacity).unwrap();
    let mut pushed = 0;
    let mut t = -lead_in;
    let mut value = 0;
    while t <= probe.final_deadline().0 {
        ev.push_input(report(t, value)).unwrap();
        pushed += 1;
        t += MS;
        value = (value + 1) % 256;
    }
    assert_eq!(pushed, 181_181);
    // Key 1 pressed and released, one edge a microsecond after the final
    // deadline, fills the rest of the ring exactly.
    let mut t = probe.final_deadline().0 + 1;
    while pushed < capacity {
        let e = if pushed % 2 == 0 {
            press(t, 0, 1)
        } else {
            release(t, 0, 1)
        };
        ev.push_input(e).unwrap();
        pushed += 1;
        t += 1;
    }
    assert_eq!(ev.push_input(report(t, 0)), Err(PushError::Full));
    let mut out = Vec::with_capacity(ev.max_events(capacity));
    ev.advance_to(TimeUs(t), &mut out).unwrap();
    // Every input was evaluated: the spinning axis pressed once, 2 ms into
    // the countdown, and held to the end, so the scratch tap at 500 ms was
    // missed; the 180 key taps were missed before the key presses.
    let j = judged(&out);
    assert_eq!(
        j.iter().filter(|j| j.0 == 0).collect::<Vec<_>>(),
        [&(0, 0, rules::Judgment::Poor, None)]
    );
    assert_eq!(j.len(), 181);
    assert!(ev.is_complete());
}

#[test]
fn window_inputs_rounds_up_and_saturates() {
    // 1 µs past a whole second is another second.
    assert_eq!(window_inputs(TimeUs(2_000_001), &[(1, 0)]), 3 * 60);
    assert_eq!(window_inputs(TimeUs(2_000_000), &[(0, 1)]), 2 * 1000);
    // A source's buttons together get at most 240 edges a second: 4 get
    // 240, 5 get 240 too; each source has its own.
    assert_eq!(window_inputs(TimeUs(1_000_000), &[(4, 0)]), 240);
    assert_eq!(window_inputs(TimeUs(1_000_000), &[(5, 0)]), 240);
    assert_eq!(
        window_inputs(TimeUs(1_000_000), &[(5, 0), (3, 2)]),
        240 + 180 + 2000
    );
    assert_eq!(window_inputs(TimeUs(0), &[(5, 5)]), 0);
    assert_eq!(window_inputs(TimeUs(-1), &[(5, 5)]), 0);
    assert_eq!(
        window_inputs(TimeUs(i64::MAX), &[(1, usize::MAX)]),
        usize::MAX
    );
    assert_eq!(
        window_inputs(TimeUs(1), &[(0, usize::MAX), (0, 1)]),
        usize::MAX
    );
}

#[test]
fn small_movement_and_pauses_do_not_press() {
    let mut c = Conv::new(TURNTABLE);
    // A position's flicker at rest never adds up.
    let edges = c.reports(&[(0, 50), (1, 51), (2, 50), (3, 51), (4, 50)]);
    assert!(edges.iter().all(Vec::is_empty), "{edges:?}");
    // One position, then one more after a pause longer than 100 ms: the
    // pause starts the count again. After a pause of exactly 100 ms it
    // adds up and presses.
    let edges = c.reports(&[(10 * MS, 51), (110 * MS + 1, 52), (210 * MS + 1, 53)]);
    assert_eq!(edges, [vec![], vec![], vec![P]]);
    // A change of direction starts the count again: +1 then -2 presses
    // backward at the -2, where adding up would leave -1.
    let mut c = Conv::new(TURNTABLE);
    let edges = c.reports(&[(0, 50), (1, 51), (2, 49)]);
    assert_eq!(edges, [vec![], vec![], vec![P]]);
}

#[test]
fn hold_threshold_and_reversal_threshold() {
    let s = ScratchSettings {
        press_threshold: 4,
        hold_threshold: 2,
        ..TURNTABLE
    };
    let mut c = Conv::new(s);
    assert_eq!(c.reports(&[(0, 0), (10, 4)]), [vec![], vec![P]]);
    assert_eq!(c.deadline(), Some(10 + 100 * MS));
    // +1 does not hold; +1 more does.
    c.reports(&[(20, 5)]);
    assert_eq!(c.deadline(), Some(10 + 100 * MS));
    c.reports(&[(30, 6)]);
    assert_eq!(c.deadline(), Some(30 + 100 * MS));
    // −3 back is below the press threshold: neither a reversal nor a hold.
    assert_eq!(c.reports(&[(40, 3)]), [vec![]]);
    assert_eq!(c.deadline(), Some(30 + 100 * MS));
    // The deadline releases it.
    let (_, l, axis) = c.0.next_release().unwrap();
    assert_eq!(l, SCRATCH);
    assert_eq!(c.0.release_due(axis), Some(R));
    assert_eq!(c.deadline(), None);
    // Releasing again, or an axis that does not exist, changes nothing.
    assert_eq!(c.0.release_due(axis), None);
    assert_eq!(c.0.release_due(axis + 1), None);
    assert!(!c.0.is_held(SCRATCH));

    // Thresholds of 0 count as 1.
    let zero = ScratchSettings {
        press_threshold: 0,
        hold_threshold: 0,
        ..TURNTABLE
    };
    let mut c = Conv::new(zero);
    assert_eq!(
        c.reports(&[(0, 0), (1, 0), (2, 1)]),
        [vec![], vec![], vec![P]]
    );
}

#[test]
fn relative_axis_uses_reports_as_movement() {
    let s = ScratchSettings {
        mode: AxisMode::Relative,
        ..TURNTABLE
    };
    // A relative axis ignores a value in the snapshot.
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), AXIS, 90)],
    };
    let mut c = Conv::with_start(s, &start);
    // -1 -1 is -2: a press; 0 is no movement; +2 is a reversal.
    let edges = c.reports(&[(1, -1), (2, -1), (3, 0), (4, 2)]);
    assert_eq!(edges, [vec![], vec![P], vec![], vec![R, P]]);
}

#[test]
fn snapshot_sets_where_an_absolute_axis_starts() {
    // Held at 200 when recording opened (still, or turning across a
    // retry): the first report moves from there. Released at the start, so
    // a turn that continues presses at once.
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), AXIS, 200)],
    };
    let mut c = Conv::with_start(TURNTABLE, &start);
    assert!(!c.0.is_held(SCRATCH));
    assert_eq!(c.reports(&[(5, 202)]), [vec![P]]);
    // Without the snapshot, the first report only sets where it is.
    let mut c = Conv::new(TURNTABLE);
    assert_eq!(c.reports(&[(5, 202), (6, 203)]), [vec![], vec![]]);
}

#[test]
fn button_and_axis_on_one_lane_combine() {
    let mut dev = axis_device(TURNTABLE);
    dev.sources[0].bindings.push(Binding {
        control: ControlId(100),
        lane: SCRATCH,
    });
    let button = |t, value| PhysicalInputEvent {
        received: TimeUs(t),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(100),
            value,
        },
    };
    let mut c = ConversionState::init(&dev, &InputStart::default()).unwrap();
    let mut step = |e: PhysicalInputEvent| c.apply(&e, e.received).as_slice().to_vec();
    assert_eq!(step(report(0, 0)), []);
    assert_eq!(step(report(1, 2)), [P]);
    assert_eq!(step(button(2, 1)), [], "already held by the axis");
    // A reversal while the button holds is no edge of the lane.
    assert_eq!(step(report(3, 0)), []);
    let (_, _, axis) = c.next_release().unwrap();
    assert_eq!(c.release_due(axis), None, "the button still holds it");
    assert!(c.is_held(SCRATCH));
    assert_eq!(c.apply(&button(4, 0), TimeUs(4)).as_slice(), [R]);
}

#[test]
fn one_control_bound_twice_is_refused() {
    let mut dev = axis_device(TURNTABLE);
    dev.sources[0].bindings.push(Binding {
        control: AXIS,
        lane: lane(4),
    });
    let refused = Err(EvalError::DuplicateBinding {
        source: SourceId(0),
        control: AXIS,
    });
    assert_eq!(
        ConversionState::init(&dev, &InputStart::default()).map(|_| ()),
        refused
    );
    let mut dev = axis_device(TURNTABLE);
    let again = dev.sources[0].axes[0];
    dev.sources[0].axes.push(again);
    assert_eq!(
        ConversionState::init(&dev, &InputStart::default()).map(|_| ()),
        refused
    );
}

#[test]
fn scratch_release_comes_before_a_mine_at_the_same_time() {
    // Released at 1100 ms by its deadline, as a release input at that time
    // would be: the mine at 1100 ms finds the lane released.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(2_000 * MS))
        .mine(SCRATCH, TimeUs(1_100 * MS), 10)
        .mine(SCRATCH, TimeUs(1_099 * MS), 10)
        .build()
        .unwrap();
    let dev = axis_device(TURNTABLE);
    let events = run(
        &chart,
        &dev,
        &[report(900 * MS, 0), report(1_000 * MS, 2)],
        3_000 * MS,
    );
    let hits: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::MineHit { .. }))
        .map(|e| e.time.0)
        .collect();
    assert_eq!(hits, [1_099 * MS]);
}

#[test]
fn song_end_does_not_wait_for_a_pending_release() {
    // The final deadline is the miss of the tap at 1000 ms: 1180 ms. The
    // axis presses at 1150 ms, releasing at 1250 ms.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let dev = axis_device(TURNTABLE);
    let events = run(
        &chart,
        &dev,
        &[report(1_100 * MS, 0), report(1_150 * MS, 2)],
        1_180 * MS,
    );
    let ends: Vec<_> = events
        .iter()
        .filter(|e| e.kind == JudgeEventKind::SongEnd)
        .map(|e| e.time.0)
        .collect();
    assert_eq!(ends, [1_180 * MS]);
}

#[test]
fn deadline_saturates_at_the_end_of_time() {
    let mut c = Conv::new(TURNTABLE);
    c.reports(&[(i64::MAX - 10, 0), (i64::MAX - 5, 2)]);
    assert_eq!(c.deadline(), Some(i64::MAX));
}

#[test]
fn aborted_and_incremental_scratch_match_batch() {
    // A scratch LN, a scratch tap and a key tap, played with the axis and
    // a key, then evaluated live in steps and in one batch.
    let chart = ChartBuilder::new(BPM)
        .long(SCRATCH, TimeUs(1_000 * MS), TimeUs(1_500 * MS))
        .tap(SCRATCH, TimeUs(2_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let dev = axis_device(TURNTABLE);
    let mut events = vec![report(800 * MS, 30)];
    // Turn forward 2 at 1000 ms (the head), then 1 every 40 ms to 1480 ms
    // (held; released at 1580 ms), then back 3 at 2000 ms (a press at the
    // tap) with the key.
    for (i, t) in (1_000..=1_480).step_by(40).enumerate() {
        events.push(report(t * MS, 32 + i32::try_from(i).unwrap()));
    }
    events.push(report(2_000 * MS, 41));
    events.push(press(2_000 * MS, 0, 2));
    events.push(report(2_001 * MS, 40));
    events.push(release(2_050 * MS, 0, 2));
    let input = EvalInput {
        events: events.clone(),
        device: dev.clone(),
        start: InputStart::default(),
        finalized: None,
    };
    let rule = rules::custom();
    let batch = evaluate(&chart, &input, &rule).unwrap();
    // Head PERFECT, held, judged at its end by a deadline that comes before
    // the release (1580 ms), and both taps PERFECT.
    assert_eq!(
        judged(&batch.events),
        [
            (0, 0, Perfect, Some(0)),
            (0, 1, Perfect, Some(0)),
            (2, 0, Perfect, Some(0))
        ]
    );
    assert_eq!(ln_ends(&batch.events), [(1_500 * MS, Perfect, None, None)]);
    for step in [MS, 7 * MS] {
        let rule = rules::custom();
        let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), 64).unwrap();
        let mut out = Vec::new();
        let mut next = 0;
        let mut t = 0;
        while t < 3_000 * MS {
            while next < events.len() && events[next].received.0 <= t {
                ev.push_input(events[next]).unwrap();
                next += 1;
            }
            ev.advance_to(TimeUs(t), &mut out).unwrap();
            t += step;
        }
        ev.advance_to(TimeUs(3_000 * MS), &mut out).unwrap();
        assert_eq!(out, batch.events, "step {step}");
    }

    // Aborted at 1050 ms, with the release pending at 1140 ms: the head is
    // held and its long note is not judged or counted.
    let aborted = evaluate(
        &chart,
        &EvalInput {
            finalized: Some(TimeUs(1_050 * MS)),
            ..input
        },
        &rule,
    )
    .unwrap();
    assert_eq!(judged(&aborted.events), []);
    assert_eq!(aborted.tally.counts.perfect, 0);
    assert_eq!(ln_ends(&aborted.events), []);
    assert_eq!(&batch.events[..aborted.events.len()], aborted.events);
}

#[test]
fn axes_are_found_by_source_and_control() {
    // Source 0 binds two axes, source 1 one with the same control as
    // source 0's second: each report presses its own lane.
    let mut dev = device(&[0, 0]);
    for s in &mut dev.sources {
        s.bindings.clear();
    }
    let bind = |control, l| AxisBinding {
        control: ControlId(control),
        lane: lane(l),
        scratch: TURNTABLE,
    };
    dev.sources[0].axes = vec![bind(0x1_0035, 2), bind(0x1_0030, 5)];
    dev.sources[1].axes = vec![bind(0x1_0030, 6)];
    let mut c = ConversionState::init(&dev, &InputStart::default()).unwrap();
    let mut turn = |source, control, t, value| {
        let e = PhysicalInputEvent {
            received: TimeUs(t),
            source: SourceId(source),
            kind: InputKind::Control {
                control: ControlId(control),
                value,
            },
        };
        c.apply(&e, e.received).as_slice().to_vec()
    };
    for (source, control) in [(1, 0x1_0030), (0, 0x1_0035), (0, 0x1_0030)] {
        assert_eq!(turn(source, control, 1, 0), []);
    }
    assert_eq!(turn(1, 0x1_0030, 2, 2), [LaneEdge::Press(lane(6))]);
    assert_eq!(turn(0, 0x1_0030, 3, 2), [LaneEdge::Press(lane(5))]);
    assert_eq!(turn(0, 0x1_0035, 4, 2), [LaneEdge::Press(lane(2))]);
    // An unbound control of a source with axes, and a source with no
    // profile, change nothing.
    assert_eq!(turn(0, 0x1_0031, 5, 50), []);
    assert_eq!(turn(7, 0x1_0030, 6, 50), []);
    // A disconnect releases only its own source's axes.
    let gone = PhysicalInputEvent {
        received: TimeUs(7),
        source: SourceId(0),
        kind: InputKind::Disconnected,
    };
    assert_eq!(
        c.apply(&gone, gone.received).as_slice(),
        [LaneEdge::Release(lane(5)), LaneEdge::Release(lane(2))]
    );
    assert!(c.is_held(lane(6)));
}

#[test]
fn profiles_beyond_the_binding_limits_are_refused() {
    let axis = |i: usize| AxisBinding {
        control: ControlId(0x1_0000 + u32::try_from(i).unwrap()),
        lane: SCRATCH,
        scratch: TURNTABLE,
    };
    let key = |i: usize| Binding {
        control: ControlId(u32::try_from(i).unwrap()),
        lane: lane(1),
    };
    let profile = |sources: usize, keys: usize, axes: usize| {
        let mut d = device(&vec![0; sources]);
        for s in &mut d.sources {
            s.bindings = (0..keys).map(key).collect();
            s.axes = (0..axes).map(axis).collect();
        }
        ConversionState::init(&d, &InputStart::default()).map(|_| ())
    };
    let refused = Err(EvalError::TooManyBindings);
    // Per source: 8 axes and 256 keys fit, one more does not.
    assert_eq!(profile(1, 256, 8), Ok(()));
    assert_eq!(profile(1, 256, 9), refused);
    assert_eq!(profile(1, 257, 8), refused);
    // In all: 16 axes and 1024 keys fit, one more source's worth does not.
    assert_eq!(profile(2, 0, 8), Ok(()));
    assert_eq!(profile(3, 0, 6), refused);
    assert_eq!(profile(4, 256, 0), Ok(()));
    assert_eq!(profile(5, 205, 0), refused);
}

#[test]
fn signed_axis_jitter_at_its_wrap_is_one_position() {
    // A signed 8-bit turntable reports -128..=127: 256 positions a
    // revolution. Flicker between 127 and -128 is +1 then -1 the short
    // way round, never a press; a turn through it is continuous.
    let mut c = Conv::new(TURNTABLE);
    let flicker = c.reports(&[(0, 127), (1, -128), (2, 127), (3, -128), (4, 127)]);
    assert!(flicker.iter().all(Vec::is_empty), "{flicker:?}");
    let turn = c.reports(&[(10, -128), (11, -127)]);
    assert_eq!(turn, [vec![], vec![P]]);
    // With the 255 positions a declared -127..=127 suggests, 127 to -128
    // is no movement at all, so the same two-position turn is lost: the
    // wrap must be the field's 256.
    let mut c = Conv::new(TURNTABLE);
    assert_eq!(
        c.reports(&[(0, 127), (1, -128), (2, -127)]),
        [vec![], vec![], vec![P]]
    );
    let short = ScratchSettings {
        mode: AxisMode::Absolute { wrap: 255 },
        ..TURNTABLE
    };
    let mut c = Conv::new(short);
    let edges = c.reports(&[(0, 127), (1, -128), (2, -127)]);
    assert!(edges.iter().all(Vec::is_empty), "{edges:?}");
}
