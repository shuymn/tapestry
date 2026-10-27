//! Hand examples for tap judgment: "this input gives this judgment".
//! The preset is the custom preset (windows ±20/45/100/180 ms at #RANK 2,
//! empty POOR up to 500 ms before and 150 ms after a note).

mod common;

use bms_model::{ChartBuilder, JudgeRank, SoundId, TimeUs};
use common::{MS, device, empty_poors, judgments, lane, press, release, run, run_with, set};
use eval::{AdvanceError, ControlId, Evaluator, InputStart, JudgeEventKind, PushError, SourceId};
use rules::Judgment::{Bad, Good, Great, Perfect, Poor};

const BPM: i64 = 150_000;

#[test]
fn press_on_late_edge_counts_before_miss() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let dev = device(&[0]);
    // The press sits exactly on the late BAD edge, which is also the miss
    // deadline, and the watermark stops exactly there.
    let edge = 1_000 * MS + 180 * MS;
    let (events, tally) = run(
        &chart,
        &dev,
        &InputStart::default(),
        &[press(edge, 0, 1)],
        edge,
    );
    assert_eq!(judgments(&events), [(1, 0, Bad, Some(180 * MS))]);
    assert_eq!((tally.counts.bad, tally.counts.poor), (1, 0));
}

#[test]
fn miss_waits_for_watermark() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let dev = device(&[0]);
    let preset = rules::custom();
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 8).unwrap();
    let mut out = Vec::new();
    // A press on the late edge (1180 ms) is queued, but the watermark is
    // 1 µs before it: neither the press nor the miss is processed.
    ev.push_input(press(1_180 * MS, 0, 1)).unwrap();
    ev.advance_to(TimeUs(1_180 * MS - 1), &mut out).unwrap();
    assert!(out.is_empty());
    // At the watermark, the press is judged and the miss never happens.
    ev.advance_to(TimeUs(1_180 * MS), &mut out).unwrap();
    assert_eq!(judgments(&out), [(1, 0, Bad, Some(180 * MS))]);

    // Without a press, the POOR appears only when the watermark reaches the
    // deadline.
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 8).unwrap();
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_180 * MS - 1), &mut out).unwrap();
    assert!(out.is_empty());
    ev.advance_to(TimeUs(1_180 * MS), &mut out).unwrap();
    assert_eq!(judgments(&out), [(1, 0, Poor, None)]);
    assert_eq!(out[0].time, TimeUs(1_180 * MS));
}

#[test]
fn finalized_or_unordered_input_and_backward_advance_are_refused() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        // A future note the queued press will judge.
        .tap(lane(1), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    let dev = device(&[0]);
    let preset = rules::custom();
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 2).unwrap();
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_180 * MS), &mut out).unwrap();
    assert_eq!(judgments(&out), [(1, 0, Poor, None)]);
    // At the finalized watermark itself: too late, the miss is final.
    assert_eq!(
        ev.push_input(press(1_180 * MS, 0, 1)),
        Err(PushError::Finalized {
            time: TimeUs(1_180 * MS),
            finalized: TimeUs(1_180 * MS),
        })
    );
    ev.push_input(press(1_190 * MS, 0, 1)).unwrap();
    assert_eq!(
        ev.push_input(release(1_185 * MS, 0, 1)),
        Err(PushError::OutOfOrder {
            time: TimeUs(1_185 * MS),
            last: TimeUs(1_190 * MS),
        })
    );
    ev.push_input(release(1_190 * MS, 0, 1)).unwrap();
    assert_eq!(ev.push_input(press(1_191 * MS, 0, 1)), Err(PushError::Full));
    assert_eq!(
        ev.advance_to(TimeUs(1_179 * MS), &mut out),
        Err(AdvanceError::Backward {
            watermark: TimeUs(1_179 * MS),
            finalized: TimeUs(1_180 * MS),
        })
    );
    // The refused calls changed nothing: the two queued inputs still run
    // (the press judges the future note), and an equal watermark is not
    // backward.
    ev.advance_to(TimeUs(1_180 * MS), &mut out).unwrap();
    ev.advance_to(TimeUs(2_000 * MS), &mut out).unwrap();
    assert_eq!(
        judgments(&out),
        [(1, 0, Poor, None), (1, 1, Bad, Some(-110 * MS))]
    );
    ev.push_input(press(2_001 * MS, 0, 1)).unwrap();
}

#[test]
fn finalization_compares_evaluation_time_not_receive_time() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let preset = rules::custom();
    let w = TimeUs(1_000 * MS);

    // +5 ms: received after the watermark, but evaluated at or before it.
    let late = device(&[5 * MS]);
    let mut ev = Evaluator::new(&chart, &preset, &late, &InputStart::default(), 4).unwrap();
    ev.advance_to(w, &mut Vec::new()).unwrap();
    assert_eq!(
        ev.push_input(press(1_005 * MS, 0, 1)),
        Err(PushError::Finalized {
            time: w,
            finalized: w,
        })
    );
    assert_eq!(ev.push_input(press(1_005 * MS + 1, 0, 1)), Ok(()));

    // −5 ms: received at or before the watermark, but evaluated after it.
    let early = device(&[-5 * MS]);
    let mut ev = Evaluator::new(&chart, &preset, &early, &InputStart::default(), 4).unwrap();
    ev.advance_to(w, &mut Vec::new()).unwrap();
    assert_eq!(
        ev.push_input(press(995 * MS, 0, 1)),
        Err(PushError::Finalized {
            time: w,
            finalized: w,
        })
    );
    assert_eq!(ev.push_input(press(995 * MS + 1, 0, 1)), Ok(()));
}

#[test]
fn nearest_note_wins_and_early_press_does_not_steal() {
    let chart = ChartBuilder::new(BPM)
        // A pair 100 ms apart: a press 10 ms before the later note.
        .tap(lane(2), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(1_100 * MS))
        // A pair 100 ms apart: a press exactly between them.
        .tap(lane(2), TimeUs(3_000 * MS))
        .tap(lane(2), TimeUs(3_100 * MS))
        // A press early for a note whose earlier neighbour is still open.
        .tap(lane(2), TimeUs(5_000 * MS))
        .tap(lane(2), TimeUs(5_150 * MS))
        .build()
        .unwrap();
    let dev = device(&[0]);
    let inputs = [
        press(1_090 * MS, 0, 2),
        release(1_095 * MS, 0, 2),
        press(3_050 * MS, 0, 2),
        release(3_060 * MS, 0, 2),
        press(4_990 * MS, 0, 2),
        release(4_995 * MS, 0, 2),
    ];
    let (events, _) = run(&chart, &dev, &InputStart::default(), &inputs, 6_000 * MS);
    assert_eq!(
        judgments(&events),
        [
            // Nearer (−10 ms) beats older (+90 ms), which is passed.
            (2, 0, Poor, None),
            (2, 1, Perfect, Some(-10 * MS)),
            // A tie (±50 ms) goes to the earlier note.
            (2, 2, Good, Some(50 * MS)),
            (2, 3, Poor, None),
            // Early for 5000 ms, and 5150 ms stays open.
            (2, 4, Perfect, Some(-10 * MS)),
            (2, 5, Poor, None),
        ]
    );
}

/// Version 1's range: only the next unjudged note, up to 300 ms ahead.
/// The time and note index of each judgment in `events`.
fn judged_at(events: &[eval::JudgeEvent]) -> Vec<(i64, usize)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged { note, .. } => Some((e.time.0, note)),
            _ => None,
        })
        .collect()
}

/// The time of each empty POOR in `events`.
fn empty_poor_times(events: &[eval::JudgeEvent]) -> Vec<i64> {
    events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
        .map(|e| e.time.0)
        .collect()
}

/// Notes at 1000 and 1060 ms; presses at 1040 and 1050 ms.
fn overtaking(preset: &rules::RulePreset) -> (Vec<eval::JudgeEvent>, eval::Tally) {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_060 * MS))
        .build()
        .unwrap();
    let inputs = [
        press(1_040 * MS, 0, 1),
        release(1_045 * MS, 0, 1),
        press(1_050 * MS, 0, 1),
        release(1_055 * MS, 0, 1),
    ];
    run_with(
        &chart,
        preset,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    )
}

#[test]
fn overtaking_press_misses_the_earlier_note() {
    // 1040 ms lies inside the first note's GOOD window (+40 ms), but the
    // second note is nearer (-20 ms): it is judged, and the first note is
    // passed, a POOR at the press, before the press's own judgment.
    let (events, tally) = overtaking(&rules::custom());
    assert_eq!(
        judgments(&events),
        [(1, 0, Poor, None), (1, 1, Perfect, Some(-20 * MS))]
    );
    assert_eq!(judged_at(&events), [(1_040 * MS, 0), (1_040 * MS, 1)]);
    // The press at 1050 ms has no unjudged note left, but lies 50 ms after
    // the first note: an empty POOR.
    assert_eq!(empty_poor_times(&events), [1_050 * MS]);
    // The POOR and the empty POOR add to BP; the POOR came first, so the
    // PERFECT starts a combo the empty POOR keeps.
    assert_eq!(
        (tally.counts.perfect, tally.counts.good, tally.counts.poor),
        (1, 0, 1)
    );
    assert_eq!((tally.bp, tally.combo, tally.max_combo), (2, 1, 1));
}

#[test]
fn overtaking_misses_every_earlier_note_in_order() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(1_050 * MS))
        .tap(lane(3), TimeUs(1_100 * MS))
        .build()
        .unwrap();
    let inputs = [press(1_100 * MS, 0, 3), release(1_110 * MS, 0, 3)];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    // Both POORs at the press, in note order, then the press's PERFECT;
    // their deadlines (1180 and 1230 ms) add nothing.
    assert_eq!(
        judgments(&events),
        [
            (3, 0, Poor, None),
            (3, 1, Poor, None),
            (3, 2, Perfect, Some(0))
        ]
    );
    assert_eq!(
        judged_at(&events),
        [(1_100 * MS, 0), (1_100 * MS, 1), (1_100 * MS, 2)]
    );
    assert_eq!((tally.counts.poor, tally.bp, tally.combo), (2, 2, 1));
}

#[test]
fn overtaken_ln_head_loses_its_ln() {
    // An LN 1000-1050 ms and a tap at 1100 ms; the press at 1095 ms is
    // nearer the tap (-5 ms) than the head (+95 ms).
    let chart = ChartBuilder::new(BPM)
        .long(lane(6), TimeUs(1_000 * MS), TimeUs(1_050 * MS))
        .tap(lane(6), TimeUs(1_100 * MS))
        .build()
        .unwrap();
    let inputs = [press(1_095 * MS, 0, 6), release(1_120 * MS, 0, 6)];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    assert_eq!(
        judgments(&events),
        [(6, 0, Poor, None), (6, 1, Perfect, Some(-5 * MS))]
    );
    assert_eq!(judged_at(&events), [(1_095 * MS, 0), (1_095 * MS, 1)]);
    // The missed head is its long note's one POOR, never held.
    assert!(!events.iter().any(|e| matches!(
        e.kind,
        JudgeEventKind::LnHeld { .. } | JudgeEventKind::LnReleased { .. }
    )));
    assert_eq!((tally.counts.poor, tally.bp, tally.combo), (1, 1, 1));
}

#[test]
fn overtaken_ln_head_skips_its_future_end() {
    // An LN 1000-1150 ms and a tap at 1200 ms; the press at 1140 ms is
    // nearer the tap (-60 ms, GOOD) than the head (+140 ms). The LN's end
    // deadline at 1150 ms comes after the press and must do nothing.
    let chart = ChartBuilder::new(BPM)
        .long(lane(7), TimeUs(1_000 * MS), TimeUs(1_150 * MS))
        .tap(lane(7), TimeUs(1_200 * MS))
        .build()
        .unwrap();
    let inputs = [press(1_140 * MS, 0, 7), release(1_160 * MS, 0, 7)];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    assert_eq!(
        judgments(&events),
        [(7, 0, Poor, None), (7, 1, Good, Some(-60 * MS))]
    );
    assert_eq!(judged_at(&events), [(1_140 * MS, 0), (1_140 * MS, 1)]);
    assert!(!events.iter().any(|e| matches!(
        e.kind,
        JudgeEventKind::LnHeld { .. } | JudgeEventKind::LnReleased { .. }
    )));
    assert_eq!((tally.counts.poor, tally.bp, tally.combo), (1, 1, 1));
}

#[test]
fn empty_poor_before_an_already_judged_note() {
    // The tap at 1000 ms is judged early (BAD, -150 ms) at 850 ms; a press
    // at 900 ms has no unjudged note left, but the judged note lies 100 ms
    // ahead: an empty POOR.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let inputs = [
        press(850 * MS, 0, 1),
        release(860 * MS, 0, 1),
        press(900 * MS, 0, 1),
        release(910 * MS, 0, 1),
    ];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    assert_eq!(judgments(&events), [(1, 0, Bad, Some(-150 * MS))]);
    assert_eq!(empty_poor_times(&events), [900 * MS]);
    // BAD and the empty POOR each add to BP; BAD broke the combo.
    assert_eq!((tally.empty_poor, tally.bp, tally.combo), (1, 2, 0));
}

#[test]
fn empty_poor_reaches_behind_judged_notes() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(2), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(3_000 * MS))
        .build()
        .unwrap();
    let inputs = [
        press(1_000 * MS, 0, 2),
        release(1_010 * MS, 0, 2),
        // 100 ms after the judged note: an empty POOR.
        press(1_100 * MS, 0, 2),
        release(1_110 * MS, 0, 2),
        // Exactly 150 ms after: an empty POOR.
        press(1_150 * MS, 0, 2),
        release(1_150 * MS, 0, 2),
        // 150 ms + 1 µs and 200 ms after: nothing.
        press(1_150 * MS + 1, 0, 2),
        release(1_150 * MS + 1, 0, 2),
        press(1_200 * MS, 0, 2),
        release(1_210 * MS, 0, 2),
        press(3_000 * MS, 0, 2),
        release(3_010 * MS, 0, 2),
        // After the last note, with no unjudged note left: an empty POOR.
        press(3_100 * MS, 0, 2),
        release(3_110 * MS, 0, 2),
    ];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        4_000 * MS,
    );
    assert_eq!(
        judgments(&events),
        [(2, 0, Perfect, Some(0)), (2, 1, Perfect, Some(0))]
    );
    assert_eq!(
        empty_poor_times(&events),
        [1_100 * MS, 1_150 * MS, 3_100 * MS]
    );
    assert_eq!((tally.empty_poor, tally.bp), (3, 3));
    // Not counted and not breaking the combo.
    assert_eq!((tally.counts.poor, tally.combo, tally.max_combo), (0, 2, 2));
}

#[test]
fn empty_poor_reaches_ahead_of_unjudged_notes() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(4), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let inputs = [
        // 500 ms + 1 µs before the note: nothing.
        press(1_500 * MS - 1, 0, 4),
        release(1_500 * MS - 1, 0, 4),
        // Exactly 500 ms and 400 ms before: empty POORs.
        press(1_500 * MS, 0, 4),
        release(1_510 * MS, 0, 4),
        press(1_600 * MS, 0, 4),
        release(1_610 * MS, 0, 4),
        press(2_000 * MS, 0, 4),
    ];
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        3_000 * MS,
    );
    assert_eq!(judgments(&events), [(4, 0, Perfect, Some(0))]);
    assert_eq!(empty_poor_times(&events), [1_500 * MS, 1_600 * MS]);
    assert_eq!((tally.empty_poor, tally.bp, tally.combo), (2, 2, 1));
}

#[test]
fn mashing_after_a_judged_note_counts_every_press() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(5), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut inputs = vec![press(1_000 * MS, 0, 5), release(1_010 * MS, 0, 5)];
    // Five presses 30 ms apart, the last exactly 150 ms after the note.
    for i in 1..=5 {
        inputs.push(press((1_000 + 30 * i) * MS, 0, 5));
        inputs.push(release((1_000 + 30 * i) * MS + 10 * MS, 0, 5));
    }
    let (events, tally) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    assert_eq!(judgments(&events), [(5, 0, Perfect, Some(0))]);
    assert_eq!(empty_poors(&events), 5);
    assert_eq!((tally.empty_poor, tally.bp, tally.combo), (5, 5, 1));
}

#[test]
fn input_offset_shifts_judgment() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(4), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let late = [press(1_024 * MS, 0, 4)];
    let (events, _) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &late,
        2_000 * MS,
    );
    assert_eq!(judgments(&events), [(4, 0, Great, Some(24 * MS))]);
    let (events, _) = run(
        &chart,
        &device(&[5 * MS]),
        &InputStart::default(),
        &late,
        2_000 * MS,
    );
    assert_eq!(judgments(&events), [(4, 0, Perfect, Some(19 * MS))]);
}

#[test]
fn held_at_start_is_pressed_without_judgment() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(5), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), ControlId(5), 1)],
    };
    let inputs = [
        // Auto-repeat of the held key: no edge, so no judgment.
        press(990 * MS, 0, 5),
        // Its release judges nothing.
        release(995 * MS, 0, 5),
        // The next press is judged.
        press(1_005 * MS, 0, 5),
    ];
    let (events, tally) = run(&chart, &device(&[0]), &start, &inputs, 2_000 * MS);
    assert_eq!(judgments(&events), [(5, 0, Perfect, Some(5 * MS))]);
    assert_eq!(tally.empty_poor, 0);
}

#[test]
fn disconnect_releases_held_controls() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(6), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    // Two keyboards share the binding of lane 6.
    let inputs = [
        press(500 * MS, 1, 6),
        common::disconnect(600 * MS, 1),
        // Lane 6 was released by the disconnect, so this is a press edge.
        press(1_000 * MS, 0, 6),
    ];
    let (events, _) = run(
        &chart,
        &device(&[0, 0]),
        &InputStart::default(),
        &inputs,
        2_000 * MS,
    );
    assert_eq!(judgments(&events), [(6, 0, Perfect, Some(0))]);
}

#[test]
fn origin_is_raw_receive_time() {
    let mut chart = ChartBuilder::new(BPM)
        .sound("k.wav")
        .tap(lane(7), TimeUs(1_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    chart.lanes[7][0].sound = Some(SoundId(0));
    let (events, _) = run(
        &chart,
        &device(&[5 * MS]),
        &InputStart::default(),
        &[press(1_005 * MS, 0, 7)],
        3_000 * MS,
    );
    // Gauge reports share their judgment's time and origin; leave them out.
    let summary: Vec<_> = events
        .iter()
        .filter(|e| !matches!(e.kind, JudgeEventKind::Gauge { .. }))
        .map(|e| (e.seq, e.time.0, e.origin.map(|o| o.0)))
        .collect();
    assert_eq!(
        summary,
        [
            (0, 1_000 * MS, Some(1_005 * MS)),
            (1, 1_000 * MS, Some(1_005 * MS)),
            (4, 2_180 * MS, None),
            (7, 2_180 * MS, None),
        ]
    );
    assert!(events.iter().all(|e| match e.kind {
        JudgeEventKind::Gauge { .. } => e.time == events[usize::try_from(e.seq).unwrap() - 1].time,
        _ => true,
    }));
    assert_eq!(events[7].kind, JudgeEventKind::SongEnd);
    assert!(matches!(
        events[0].kind,
        JudgeEventKind::PlaySound {
            sound: SoundId(0),
            ..
        }
    ));
    assert_eq!(
        judgments(&events),
        [(7, 0, Perfect, Some(0)), (7, 1, Poor, None)]
    );
}

/// Every window edge is inclusive after the rank scale: #RANK 0 halves the
/// windows to ±10/22.5/50/90 ms.
#[test]
fn rank_scaled_window_edges_are_inclusive() {
    let deviations = [
        (-10_000, Some(Perfect)),
        (10_000, Some(Perfect)),
        (-10_001, Some(Great)),
        (10_001, Some(Great)),
        (-22_500, Some(Great)),
        (22_500, Some(Great)),
        (-22_501, Some(Good)),
        (22_501, Some(Good)),
        (-50_000, Some(Good)),
        (50_000, Some(Good)),
        (-50_001, Some(Bad)),
        (50_001, Some(Bad)),
        (-90_000, Some(Bad)),
        (90_000, Some(Bad)),
        // Outside BAD: too early is an empty POOR and the note is missed;
        // too late comes after the miss, an empty POOR on the missed note.
        (-90_001, None),
        (90_001, None),
    ];
    let mut builder = ChartBuilder::new(BPM);
    let mut inputs = Vec::new();
    for (i, &(d, _)) in deviations.iter().enumerate() {
        let note = (i64::try_from(i).unwrap() + 1) * 1_000 * MS;
        builder = builder.tap(lane(1), TimeUs(note));
        inputs.push(set(note + d, 0, 1, 1));
        inputs.push(set(note + d + 1, 0, 1, 0));
    }
    let mut chart = builder.build().unwrap();
    chart.meta.rank = Some(JudgeRank::Rank(0));
    let (events, _) = run(
        &chart,
        &device(&[0]),
        &InputStart::default(),
        &inputs,
        30_000 * MS,
    );
    let expected: Vec<_> = deviations
        .iter()
        .enumerate()
        .map(|(i, &(d, j))| match j {
            Some(j) => (1, i, j, Some(d)),
            None => (1, i, Poor, None),
        })
        .collect();
    assert_eq!(judgments(&events), expected);
    assert_eq!(empty_poors(&events), 2);
}
