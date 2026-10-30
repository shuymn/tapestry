//! Hand examples for section evaluation (design.md "Section Evaluation"):
//! the half-open targets, the lead-in, conversion state carried across the
//! start, the final deadline, the initial gauges and the gauge denominator.
//! The preset is the tapestry preset (windows ±20/45/100/180 ms, empty POOR up
//! to 500 ms before and 150 ms after a note).

mod common;

use bms_model::{Chart, ChartBuilder, NoteKind, SoundId, TimeUs};
use common::{MS, device, judgments, lane, press, release};
use eval::{
    AxisBinding, AxisMode, ControlId, DeviceProfile, EvalError, EvalInput, Evaluation, Evaluator,
    InputKind, InputStart, JudgeEvent, JudgeEventKind, PhysicalInputEvent, ScratchSettings,
    Section, SourceId, evaluate,
};
use rules::Judgment::{Bad, Good, Perfect, Poor};
use rules::{GaugeValue, RulePreset};

const BPM: i64 = 150_000;
const S: i64 = 1_000 * MS;
const PCT: i64 = GaugeValue::PERCENT;

/// `[start, end)` with a lead-in, every gauge at its preset start.
fn section(start: i64, end: i64, lead_in: i64, preset: &RulePreset) -> Section {
    Section {
        start: TimeUs(start),
        end: TimeUs(end),
        lead_in: TimeUs(lead_in),
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
    }
}

/// The 10 s to 20 s section with a 2 s lead-in (lead-in start 8 s).
fn ten_to_twenty(preset: &RulePreset) -> Section {
    section(10 * S, 20 * S, 2 * S, preset)
}

fn eval_with(
    chart: &Chart,
    preset: &RulePreset,
    device: &DeviceProfile,
    start: InputStart,
    events: Vec<PhysicalInputEvent>,
    section: Option<Section>,
) -> Evaluation {
    let input = EvalInput {
        events,
        device: device.clone(),
        start,
        finalized: None,
        section,
    };
    evaluate(chart, &input, preset).unwrap()
}

fn count(events: &[JudgeEvent], f: impl Fn(&JudgeEventKind) -> bool) -> usize {
    events.iter().filter(|e| f(&e.kind)).count()
}

fn sounds(events: &[JudgeEvent]) -> usize {
    count(events, |k| matches!(k, JudgeEventKind::PlaySound { .. }))
}

fn empty_poors(events: &[JudgeEvent]) -> usize {
    count(events, |k| matches!(k, JudgeEventKind::EmptyPoor { .. }))
}

/// Gives every judgeable note sound 0.
fn with_sounds(mut chart: Chart) -> Chart {
    for n in chart.lanes.iter_mut().flatten() {
        if !matches!(n.kind, NoteKind::Mine { .. }) {
            n.sound = Some(SoundId(0));
        }
    }
    chart
}

#[test]
fn section_boundaries_follow_the_half_open_rule() {
    // [10 s, 20 s): a tap at 10 s counts and one at 20 s does not; a long
    // note ending at 20 s does not and one ending 1 µs earlier does; a mine
    // at 10 s counts and one at 20 s does not. The tap at 19.9 s keeps
    // the final deadline (its miss at 20.08 s) after the 20 s press.
    let chart = with_sounds(
        ChartBuilder::new(BPM)
            .sound("k.wav")
            .tap(lane(1), TimeUs(10 * S))
            .tap(lane(2), TimeUs(20 * S))
            .long(lane(3), TimeUs(15 * S), TimeUs(20 * S))
            .long(lane(4), TimeUs(15 * S), TimeUs(20 * S - 1))
            .tap(lane(5), TimeUs(19_900 * MS))
            .mine(lane(6), TimeUs(10 * S), 2)
            .mine(lane(6), TimeUs(20 * S), 2)
            .build()
            .unwrap(),
    );
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let mut events = vec![
        press(9 * S, 0, 6),
        press(10 * S, 0, 1),
        release(10 * S + 20 * MS, 0, 1),
        press(15 * S, 0, 3),
        press(15 * S, 0, 4),
        release(20 * S - 1, 0, 4),
        press(20 * S, 0, 2),
        release(20 * S, 0, 3),
        release(20 * S + 20 * MS, 0, 2),
        release(20 * S + 50 * MS, 0, 6),
    ];
    events.sort_by_key(|e| e.received);
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(ten_to_twenty(&preset)),
    );
    assert!(e.completed);
    assert_eq!(
        judgments(&e.events),
        [
            (1, 0, Perfect, Some(0)),
            (4, 0, Perfect, Some(0)),
            (5, 0, Poor, None)
        ]
    );
    // The presses on lanes 2 and 3 have no target: no sound, no empty POOR.
    assert_eq!(sounds(&e.events), 2);
    assert_eq!(empty_poors(&e.events), 0);
    let mines: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::MineHit { lane, note } => Some((lane.index(), note, ev.time)),
            _ => None,
        })
        .collect();
    assert_eq!(mines, [(6, 0, TimeUs(10 * S))]);
}

#[test]
fn ln_crossing_a_section_edge_is_excluded() {
    // Each long note held for its whole length; only the one inside
    // [10 s, 20 s) counts.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(9_900 * MS), TimeUs(15 * S))
        .long(lane(2), TimeUs(15 * S), TimeUs(20 * S))
        .long(lane(3), TimeUs(19 * S), TimeUs(20_500 * MS))
        .long(lane(4), TimeUs(10 * S), TimeUs(20 * S - 1))
        .long(lane(5), TimeUs(10 * S - 1), TimeUs(12 * S))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let held = [
        (1, 9_900 * MS, 15 * S),
        (2, 15 * S, 20 * S),
        (3, 19 * S, 20_500 * MS),
        (4, 10 * S, 20 * S - 1),
        (5, 10 * S - 1, 12 * S),
    ];
    let mut events: Vec<_> = held
        .iter()
        .flat_map(|&(l, from, to)| [press(from, 0, l), release(to, 0, l)])
        .collect();
    events.sort_by_key(|e| e.received);
    let s = ten_to_twenty(&preset);
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(s.clone()),
    );
    assert_eq!(judgments(&e.events), [(4, 0, Perfect, Some(0))]);
    assert_eq!(
        count(&e.events, |k| matches!(k, JudgeEventKind::LnHeld { .. })),
        1
    );
    // The final deadline is the one long note's end.
    let ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), Some(&s), 0).unwrap();
    assert_eq!(ev.final_deadline(), TimeUs(20 * S - 1));
}

/// A tap with a sound on lane 1 at 10 s and one on lane 2 at 12 s, in the
/// 10 s to 20 s section, with lane-1 presses at `presses`.
fn lead_in(presses: &[i64], section: bool) -> Evaluation {
    let chart = with_sounds(
        ChartBuilder::new(BPM)
            .sound("k.wav")
            .tap(lane(1), TimeUs(10 * S))
            .tap(lane(2), TimeUs(12 * S))
            .build()
            .unwrap(),
    );
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let mut events: Vec<_> = presses
        .iter()
        .flat_map(|&t| [press(t, 0, 1), release(t + 1, 0, 1)])
        .collect();
    // Lane 2's first target is 2.1 s after this press.
    events.extend([press(9_900 * MS, 0, 2), release(9_910 * MS, 0, 2)]);
    events.sort_by_key(|e| e.received);
    let s = section.then(|| ten_to_twenty(&preset));
    eval_with(&chart, &preset, &dev, InputStart::default(), events, s)
}

#[test]
fn lead_in_press_outside_first_early_window_is_ignored() {
    // Before the lead-in (7.9 s), and in it 500 ms early, inside the empty
    // POOR distance, and 1 µs before the early BAD edge at 9.82 s.
    let presses = [7_900 * MS, 9_500 * MS, 9_820 * MS - 1];
    let e = lead_in(&presses, true);
    assert_eq!(empty_poors(&e.events), 0);
    assert_eq!(sounds(&e.events), 0);
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Poor, None), (2, 0, Poor, None)]
    );
    // The same presses in a whole-chart play: an empty POOR and sounds.
    let whole = lead_in(&presses, false);
    assert!(empty_poors(&whole.events) > 0);
    assert!(sounds(&whole.events) > 0);
}

#[test]
fn lead_in_press_on_the_first_early_edge_is_judged() {
    let e = lead_in(&[9_820 * MS], true);
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Bad, Some(-180 * MS)), (2, 0, Poor, None)]
    );
    assert_eq!(sounds(&e.events), 1);
}

#[test]
fn lead_in_release_judges_a_long_note_taken_in_the_lead_in() {
    // The first target is a long note from 10 s to 10.18 s. Pressed at
    // 9.97 s (30 ms early, a GREAT head) and released at 9.99 s, still in
    // the lead-in and before the head: 190 ms before its end, which the
    // LN-end edges (100/150/200 ms) grade GOOD, the worse of the two.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(10 * S), TimeUs(10_180 * MS))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let events = vec![press(9_970 * MS, 0, 1), release(9_990 * MS, 0, 1)];
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(ten_to_twenty(&preset)),
    );
    let judged: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                release,
                ..
            } => Some((ev.time, judgment, deviation, release)),
            _ => None,
        })
        .collect();
    assert_eq!(
        judged,
        [(
            TimeUs(9_990 * MS),
            Good,
            Some(TimeUs(-30 * MS)),
            Some(TimeUs(-190 * MS))
        )]
    );
}

#[test]
fn held_across_section_start_is_not_a_press() {
    // Lane 1 pressed before the lead-in, lane 2 held in the snapshot, lane
    // 3 pressed in the lead-in, all held across the start: neither the tap
    // nor the long note's head is pressed, and the mine on lane 3 hits.
    let chart = with_sounds(
        ChartBuilder::new(BPM)
            .sound("k.wav")
            .tap(lane(1), TimeUs(10 * S))
            .long(lane(2), TimeUs(10 * S), TimeUs(11 * S))
            .mine(lane(3), TimeUs(10_500 * MS), 2)
            .build()
            .unwrap(),
    );
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), common::control(2), 1)],
    };
    let events = vec![
        press(5 * S, 0, 1),
        press(9 * S, 0, 3),
        release(10_500 * MS + 1, 0, 1),
        release(10_900 * MS, 0, 2),
        release(11 * S, 0, 3),
    ];
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        start,
        events,
        Some(ten_to_twenty(&preset)),
    );
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Poor, None), (2, 0, Poor, None)]
    );
    assert_eq!(sounds(&e.events), 0);
    assert_eq!(
        count(&e.events, |k| matches!(k, JudgeEventKind::LnHeld { .. })),
        0
    );
    assert_eq!(
        count(&e.events, |k| matches!(k, JudgeEventKind::MineHit { .. })),
        1
    );
}

const AXIS: ControlId = ControlId(200);

fn turn(at: i64, by: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: AXIS,
            value: by,
        },
    }
}

/// A scratch tap at 10 s, a relative axis pressing after 2 and released
/// 100 ms after it stops, and the axis turned at `turns`.
fn scratch(turns: &[(i64, i32)]) -> Evaluation {
    let chart = with_sounds(
        ChartBuilder::new(BPM)
            .sound("k.wav")
            .tap(lane(0), TimeUs(10 * S))
            .build()
            .unwrap(),
    );
    let preset = rules::tapestry();
    let mut dev = device(&[0]);
    dev.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: lane(0),
        scratch: ScratchSettings {
            mode: AxisMode::Relative,
            press_threshold: 2,
            hold_threshold: 1,
            release_after_us: 100_000,
        },
    });
    let events = turns.iter().map(|&(t, by)| turn(t, by)).collect();
    eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(ten_to_twenty(&preset)),
    )
}

#[test]
fn pending_scratch_release_at_lead_in_start_carries_over() {
    // Pressed 50 ms before the lead-in start, released by its deadline 50
    // ms into the lead-in, silently; the turn at 9.99 s is a new press.
    let e = scratch(&[(7_950 * MS, 2), (9_990 * MS, 2)]);
    assert_eq!(judgments(&e.events), [(0, 0, Perfect, Some(-10 * MS))]);
    assert_eq!(e.events.first().map(|ev| ev.time), Some(TimeUs(9_990 * MS)));
    // Kept turning from before the lead-in through the note: the lane
    // stays pressed, with no press edge, and the tap is missed.
    let held: Vec<_> = (0..45).map(|k| (7_950 * MS + k * 50 * MS, 2)).collect();
    let e = scratch(&held);
    assert_eq!(judgments(&e.events), [(0, 0, Poor, None)]);
    assert_eq!(sounds(&e.events), 0);
}

#[test]
fn section_finalizes_through_its_final_deadline() {
    // The tap at 19.5 s misses at 19.68 s; the long note held through its
    // end at 19.9 s is the final deadline.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(19_500 * MS))
        .long(lane(2), TimeUs(18 * S), TimeUs(19_900 * MS))
        .tap(lane(1), TimeUs(25 * S))
        .build()
        .unwrap();
    let mut preset = rules::tapestry();
    // Wide enough that a press after the final deadline would be an empty
    // POOR if it were judged.
    preset.tables.keys.empty_poor.behind = TimeUs(S);
    let dev = device(&[0]);
    let s = ten_to_twenty(&preset);
    let held = [press(18 * S, 0, 2), release(20_500 * MS, 0, 2)];
    let mut ev =
        Evaluator::new(&chart, &preset, &dev, &InputStart::default(), Some(&s), 8).unwrap();
    assert_eq!(ev.final_deadline(), TimeUs(19_900 * MS));
    for e in held {
        ev.push_input(e).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(19_900 * MS - 1), &mut out).unwrap();
    assert!(!ev.is_complete());
    assert_eq!(judgments(&out), [(1, 0, Poor, None)]);
    ev.advance_to(TimeUs(19_900 * MS), &mut out).unwrap();
    assert!(ev.is_complete());
    assert_eq!(
        judgments(&out),
        [(1, 0, Poor, None), (2, 0, Perfect, Some(0))]
    );
    assert_eq!(
        out.last().map(|e| (e.time, e.kind)),
        Some((TimeUs(19_900 * MS), JudgeEventKind::SongEnd))
    );
    // A press 1 µs after the final deadline is not judged.
    let late = [press(19_900 * MS + 1, 0, 1), release(19_950 * MS, 0, 1)];
    let without = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        held.to_vec(),
        Some(s.clone()),
    );
    let with = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        vec![held[0], late[0], late[1], held[1]],
        Some(s),
    );
    assert!(with.completed);
    assert_eq!(with, without);
}

#[test]
fn section_without_targets_completes_at_its_start() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(5 * S))
        .mine(lane(2), TimeUs(25 * S), 2)
        .build()
        .unwrap();
    let mut preset = rules::tapestry();
    preset.tables.keys.empty_poor.behind = TimeUs(100 * S);
    let dev = device(&[0]);
    let s = section(30 * S, 40 * S, 2 * S, &preset);
    let events = vec![
        press(29 * S, 0, 1),
        press(30 * S, 0, 2),
        release(31 * S, 0, 1),
        release(31 * S, 0, 2),
    ];
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(s),
    );
    assert!(e.completed);
    let kinds: Vec<_> = e.events.iter().map(|ev| (ev.time, ev.kind)).collect();
    assert_eq!(kinds, [(TimeUs(30 * S), JudgeEventKind::SongEnd)]);
}

/// The tapestry gauges (normal, survival) from `initial` after the section's
/// one tap at 10 s, pressed on time or missed.
fn gauges_after(press_it: bool, initial: [i64; 2]) -> Result<Vec<GaugeValue>, EvalError> {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(10 * S))
        .tap(lane(1), TimeUs(30 * S))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let s = Section {
        initial_gauges: initial.map(GaugeValue).to_vec(),
        ..ten_to_twenty(&preset)
    };
    let ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), Some(&s), 0)?;
    let before: Vec<_> = ev.gauges().iter().map(|g| g.value).collect();
    assert_eq!(before, initial.map(GaugeValue));
    let events = if press_it {
        vec![press(10 * S, 0, 1), release(10 * S + 1, 0, 1)]
    } else {
        Vec::new()
    };
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(s),
    );
    Ok(e.gauges.iter().map(|g| g.value).collect())
}

#[test]
fn initial_gauge_is_applied() {
    // Two notes over the chart: a PERFECT adds floor(160,000 × 100 / 2)
    // = 8,000,000 (80 %) to the normal gauge, 0.16 % to the survival one.
    assert_eq!(
        gauges_after(true, [10 * PCT, 50 * PCT]),
        Ok(vec![GaugeValue(90 * PCT), GaugeValue(50 * PCT + 16_000)])
    );
    // At the limits: the normal gauge at its 100 % maximum stays there; at
    // its 2 % minimum a POOR (−6 %) keeps it there. The survival gauge at
    // 100 % loses 9 %.
    assert_eq!(
        gauges_after(true, [100 * PCT, 100 * PCT]),
        Ok(vec![GaugeValue(100 * PCT), GaugeValue(100 * PCT)])
    );
    assert_eq!(
        gauges_after(false, [2 * PCT, 100 * PCT]),
        Ok(vec![GaugeValue(2 * PCT), GaugeValue(91 * PCT)])
    );
    // The smallest survival start, 1 unit, fails at the POOR.
    assert_eq!(
        gauges_after(false, [20 * PCT, 1]),
        Ok(vec![GaugeValue(14 * PCT), GaugeValue::EMPTY])
    );
}

#[test]
fn initial_gauges_outside_the_bounds_are_refused() {
    let refused = Err(EvalError::InvalidSection {
        field: "initial_gauges",
    });
    // Below the normal gauge's 2 % minimum, above 100 %, and a survival
    // gauge starting failed.
    assert_eq!(gauges_after(true, [2 * PCT - 1, 50 * PCT]), refused);
    assert_eq!(gauges_after(true, [100 * PCT + 1, 50 * PCT]), refused);
    assert_eq!(gauges_after(true, [20 * PCT, 0]), refused);
    // One value for two gauges.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(10 * S))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let s = Section {
        initial_gauges: vec![GaugeValue(20 * PCT)],
        ..ten_to_twenty(&preset)
    };
    let new = |s: &Section| {
        Evaluator::new(&chart, &preset, &dev, &InputStart::default(), Some(s), 0).map(|_| ())
    };
    assert_eq!(
        new(&s),
        Err(EvalError::InvalidSection {
            field: "initial_gauges"
        })
    );
    // An end before the start, a negative lead-in, and a lead-in start
    // outside i64.
    let range = Section {
        end: TimeUs(10 * S - 1),
        ..ten_to_twenty(&preset)
    };
    assert_eq!(
        new(&range),
        Err(EvalError::InvalidSection { field: "range" })
    );
    let lead = Err(EvalError::InvalidSection { field: "lead_in" });
    let negative = Section {
        lead_in: TimeUs(-1),
        ..ten_to_twenty(&preset)
    };
    assert_eq!(new(&negative), lead);
    let overflow = Section {
        start: TimeUs(i64::MIN + 1),
        lead_in: TimeUs(2),
        ..ten_to_twenty(&preset)
    };
    assert_eq!(new(&overflow), lead);
    // An empty range is a section.
    let empty = Section {
        end: TimeUs(10 * S),
        ..ten_to_twenty(&preset)
    };
    assert_eq!(new(&empty), Ok(()));
}

#[test]
fn section_gauge_uses_the_chart_denominator() {
    // #TOTAL 160 over 1,000 taps, one every 100 ms from 1 s: the increment
    // is floor(160,000 × 100 / 1,000) = 16,000 (0.16 %), and the normal
    // gauge's PERFECT adds all of it. The 100 taps from 10 s to 19.9 s,
    // each pressed on time (and no other), all PERFECT from 20 %, end at
    // 20 % + 100 × 0.16 % = 36 %. The
    // section's own 100 notes would give 1.6 % each and end at 100 %.
    let mut b = ChartBuilder::new(BPM);
    let mut events = Vec::new();
    for i in 0..1_000 {
        let t = S + i * 100 * MS;
        b = b.tap(lane(1), TimeUs(t));
        if (10 * S..20 * S).contains(&t) {
            events.extend([press(t, 0, 1), release(t + 20 * MS, 0, 1)]);
        }
    }
    let mut chart = b.build().unwrap();
    chart.meta.judgment_headers = vec![common::total(160_000)];
    let preset = rules::tapestry();
    let dev = device(&[0]);
    let s = Section {
        initial_gauges: vec![GaugeValue(20 * PCT), GaugeValue::FULL],
        ..ten_to_twenty(&preset)
    };
    let e = eval_with(
        &chart,
        &preset,
        &dev,
        InputStart::default(),
        events,
        Some(s),
    );
    assert_eq!(e.tally.counts.perfect, 100);
    assert_eq!(judgments(&e.events).len(), 100);
    assert_eq!(e.gauges[0].value, GaugeValue(36 * PCT));
}
