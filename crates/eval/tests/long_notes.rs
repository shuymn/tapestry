//! One judgment per long note (design.md "LN Judgment Modes"): the worse
//! of the head's judgment and the release's by the LN-end edges, scored
//! once; the held head and the visual hold as feedback that scores
//! nothing. Custom preset unless a test says otherwise: note windows
//! PERFECT ±20, GREAT ±45, GOOD ±100, BAD ±180 ms; LN-end edges PERFECT
//! 100, GREAT 150, GOOD 200 ms before the end.
//!
//! "Regression" tests fail under the earlier rule (the head judged at the
//! press, a release before end − 100 ms an LN break); "preservation" tests
//! hold under both.

mod common;

use bms_model::{Chart, ChartBuilder, JudgeRank, TimeUs};
use common::{MS, device, disconnect, lane, press, release, run, run_with};
use eval::{
    EvalError, EvalInput, Evaluator, InputStart, JudgeEvent, JudgeEventKind, PhysicalInputEvent,
    Tally, evaluate,
};
use rules::Judgment::{self, Bad, Good, Great, Perfect};
use rules::{GaugeValue, RulePreset, Window};

const BPM: i64 = 150_000;
/// Every long note's head, on key 1.
const T: i64 = 1_000 * MS;
/// Far past every deadline of these charts.
const AFTER: i64 = 10_000 * MS;

fn ln(length: i64) -> Chart {
    ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + length))
        .build()
        .unwrap()
}

/// What one judgment says: (time, judgment, deviation, release, origin).
type Row = (i64, Judgment, Option<i64>, Option<i64>, Option<i64>);

fn judged(events: &[JudgeEvent]) -> Vec<Row> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                release,
                ..
            } => Some((
                e.time.0,
                judgment,
                deviation.map(|d| d.0),
                release.map(|r| r.0),
                e.origin.map(|o| o.0),
            )),
            _ => None,
        })
        .collect()
}

/// Held heads: (time, judgment, deviation).
fn held(events: &[JudgeEvent]) -> Vec<(i64, Judgment, i64)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::LnHeld {
                judgment,
                deviation,
                ..
            } => Some((e.time.0, judgment, deviation.0)),
            _ => None,
        })
        .collect()
}

/// Visual holds ended early: (time, note, origin).
fn released(events: &[JudgeEvent]) -> Vec<(i64, usize, Option<i64>)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::LnReleased { note, .. } => {
                Some((e.time.0, note, e.origin.map(|o| o.0)))
            }
            _ => None,
        })
        .collect()
}

/// `inputs` on one source at offset 0, to the end.
fn play(chart: &Chart, inputs: &[PhysicalInputEvent]) -> (Vec<JudgeEvent>, Tally) {
    run(chart, &device(&[0]), &InputStart::default(), inputs, AFTER)
}

fn play_with(
    chart: &Chart,
    preset: &RulePreset,
    inputs: &[PhysicalInputEvent],
) -> (Vec<JudgeEvent>, Tally) {
    run_with(
        chart,
        preset,
        &device(&[0]),
        &InputStart::default(),
        inputs,
        AFTER,
    )
}

#[test]
fn t2_a_50_ms_note_held_50_ms_keeps_its_head_judgment() {
    // Preservation of the totals (one judgment, combo 1, BP 0); the times
    // and release distances are regressions: a head pressed before its end
    // is judged at the release or the end, not at the press.
    let chart = ln(50 * MS);
    for (h, judgment, at, distance, origin) in [
        // Released 50 ms after an early press: h before the end.
        (
            -20 * MS,
            Perfect,
            T + 30 * MS,
            Some(-20 * MS),
            Some(T + 30 * MS),
        ),
        (
            -45 * MS,
            Great,
            T + 5 * MS,
            Some(-45 * MS),
            Some(T + 5 * MS),
        ),
        (
            -100 * MS,
            Good,
            T - 50 * MS,
            Some(-100 * MS),
            Some(T - 50 * MS),
        ),
        // A late press before the end: the end deadline comes first.
        (20 * MS, Perfect, T + 50 * MS, None, None),
        (45 * MS, Great, T + 50 * MS, None, None),
        // A press after the end: decided at the press.
        (100 * MS, Good, T + 100 * MS, None, Some(T + 100 * MS)),
    ] {
        let (events, tally) = play(
            &chart,
            &[press(T + h, 0, 1), release(T + h + 50 * MS, 0, 1)],
        );
        assert_eq!(
            judged(&events),
            [(at, judgment, Some(h), distance, origin)],
            "head {h}"
        );
        assert_eq!((tally.combo, tally.bp), (1, 0), "head {h}");
    }
}

#[test]
fn t3_a_bad_head_decides_its_long_note_at_the_press() {
    // Regression: BP 1 (the earlier rule added an LN break, BP 2).
    for length in [0, 50 * MS] {
        let chart = ln(length);
        let (events, tally) = play(
            &chart,
            &[press(T - 180 * MS, 0, 1), release(T - 130 * MS, 0, 1)],
        );
        assert_eq!(
            judged(&events),
            [(T - 180 * MS, Bad, Some(-180 * MS), None, Some(T - 180 * MS))],
            "length {length}"
        );
        assert_eq!(held(&events), [], "length {length}");
        assert_eq!((tally.counts.bad, tally.bp, tally.combo), (1, 1, 0));
    }
}

#[test]
fn t4_the_perfect_ln_end_edge_is_inclusive() {
    // Pressed 20 ms early and released at once: 80 ms long, the release is
    // 100 ms before the end (PERFECT); 81 ms long, 101 ms (GREAT).
    // Regression for the 81 ms case.
    for (length, judgment) in [(80 * MS, Perfect), (81 * MS, Great)] {
        let chart = ln(length);
        let (events, tally) = play(
            &chart,
            &[press(T - 20 * MS, 0, 1), release(T - 20 * MS, 0, 1)],
        );
        let r = -20 * MS - length;
        assert_eq!(
            judged(&events),
            [(
                T - 20 * MS,
                judgment,
                Some(-20 * MS),
                Some(r),
                Some(T - 20 * MS)
            )]
        );
        assert_eq!((tally.combo, tally.bp), (1, 0));
    }
}

#[test]
fn t5_an_early_release_is_one_judgment_and_keeps_the_combo() {
    // Regression: a 150 ms note on time, released 120 ms early: one GREAT,
    // the combo goes on, no BP (the earlier rule broke the combo).
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 150 * MS))
        .tap(lane(2), TimeUs(T + 500 * MS))
        .build()
        .unwrap();
    let (events, tally) = play(
        &chart,
        &[
            press(T, 0, 1),
            release(T + 30 * MS, 0, 1),
            press(T + 500 * MS, 0, 2),
        ],
    );
    assert_eq!(
        judged(&events)[0],
        (
            T + 30 * MS,
            Great,
            Some(0),
            Some(-120 * MS),
            Some(T + 30 * MS)
        )
    );
    assert_eq!((tally.combo, tally.max_combo, tally.bp), (2, 2, 0));
    assert_eq!(tally.counts.great, 1);
}

#[test]
fn t6_each_ln_end_edge_of_a_1_s_note() {
    // Regression: a 1 s note pressed on time, released x before its end.
    let chart = ln(1_000 * MS);
    let end = T + 1_000 * MS;
    for (x, judgment) in [
        (100_000, Perfect),
        (100_001, Great),
        (150_000, Great),
        (150_001, Good),
        (200_000, Good),
        (200_001, Bad),
        (1, Perfect),
    ] {
        let (events, _) = play(&chart, &[press(T, 0, 1), release(end - x, 0, 1)]);
        assert_eq!(
            judged(&events),
            [(end - x, judgment, Some(0), Some(-x), Some(end - x))],
            "released {x} µs early"
        );
    }
}

#[test]
fn t7_bad_heads_and_a_great_head_released_within_great() {
    // A 150 ms note pressed 150 ms early: BAD at the press (preservation of
    // the judgment, regression of BP 1).
    let (events, tally) = play(
        &ln(150 * MS),
        &[press(T - 150 * MS, 0, 1), release(T + 150 * MS, 0, 1)],
    );
    assert_eq!(
        judged(&events),
        [(T - 150 * MS, Bad, Some(-150 * MS), None, Some(T - 150 * MS))]
    );
    assert_eq!(tally.bp, 1);
    // Regression: a 100 ms note pressed 40 ms early (GREAT), released 10 ms
    // before the head, 110 ms before the end (GREAT): one GREAT, no BP.
    let (events, tally) = play(
        &ln(100 * MS),
        &[press(T - 40 * MS, 0, 1), release(T - 10 * MS, 0, 1)],
    );
    assert_eq!(
        judged(&events),
        [(
            T - 10 * MS,
            Great,
            Some(-40 * MS),
            Some(-110 * MS),
            Some(T - 10 * MS)
        )]
    );
    assert_eq!((tally.combo, tally.bp), (1, 0));
}

/// The judgment of a 1 s note pressed on time and released `x` before its
/// end, under `rank`.
fn ranked(preset: &RulePreset, rank: JudgeRank, length: i64, x: i64) -> Judgment {
    let mut chart = ln(length);
    chart.meta.rank = Some(rank);
    let (events, _) = play_with(
        &chart,
        preset,
        &[press(T, 0, 1), release(T + length - x, 0, 1)],
    );
    let rows = judged(&events);
    assert_eq!(rows.len(), 1);
    rows[0].1
}

#[test]
fn t8_ln_end_edges_scale_with_the_rank() {
    // Regression: each case needs the LN-end edges scaled by the factor of
    // the note windows. Edges by hand: floor(edge × scale / 100 %).
    let custom = rules::custom();
    let s = 1_000 * MS;
    // #RANK 3, 125 %: 125, 187.5 and 250 ms.
    let r3 = JudgeRank::Rank(3);
    assert_eq!(ranked(&custom, r3, s, 125_000), Perfect);
    assert_eq!(ranked(&custom, r3, s, 125_001), Great);
    assert_eq!(ranked(&custom, r3, s, 187_500), Great);
    assert_eq!(ranked(&custom, r3, s, 187_501), Good);
    assert_eq!(ranked(&custom, r3, s, 250_000), Good);
    assert_eq!(ranked(&custom, r3, s, 250_001), Bad);
    // #DEFEXRANK 200: 200, 300 and 400 ms.
    let d200 = JudgeRank::DefExRankMilli(200_000);
    assert_eq!(ranked(&custom, d200, s, 200_000), Perfect);
    assert_eq!(ranked(&custom, d200, s, 300_000), Great);
    assert_eq!(ranked(&custom, d200, s, 400_000), Good);
    assert_eq!(ranked(&custom, d200, s, 400_001), Bad);
    // #RANK 0, 50 %: 50, 75 and 100 ms.
    let r0 = JudgeRank::Rank(0);
    assert_eq!(ranked(&custom, r0, s, 50_000), Perfect);
    assert_eq!(ranked(&custom, r0, s, 50_001), Great);
    assert_eq!(ranked(&custom, r0, s, 100_000), Good);
    assert_eq!(ranked(&custom, r0, s, 100_001), Bad);
    // #DEFEXRANK 33.333: GREAT is 150 ms × 0.33333 = 49_999.5 µs, floored
    // to 49_999, so 50_000 µs early is GOOD.
    let third = JudgeRank::DefExRankMilli(33_333);
    assert_eq!(ranked(&custom, third, s, 33_333), Perfect);
    assert_eq!(ranked(&custom, third, s, 33_334), Great);
    assert_eq!(ranked(&custom, third, s, 49_999), Great);
    assert_eq!(ranked(&custom, third, s, 50_000), Good);
    // Edges of 1, 2 and 3 µs (note early edges 0, 1, 2 and 3 µs) at 50 %
    // collapse to 0, 1 and 1: GOOD can no longer be reached, and 1 µs early
    // is GREAT where it is PERFECT unscaled.
    let mut tiny = rules::custom();
    for (w, us) in [
        (&mut tiny.windows.perfect, 0),
        (&mut tiny.windows.great, 1),
        (&mut tiny.windows.good, 2),
        (&mut tiny.windows.bad, 3),
    ] {
        w.early = TimeUs(us);
    }
    tiny.ln_end.perfect = TimeUs(1);
    tiny.ln_end.great = TimeUs(2);
    tiny.ln_end.good = TimeUs(3);
    assert_eq!(tiny.validate(), Ok(()));
    let r2 = JudgeRank::Rank(2);
    assert_eq!(ranked(&tiny, r2, s, 1), Perfect);
    assert_eq!(ranked(&tiny, r2, s, 3), Good);
    assert_eq!(ranked(&tiny, r0, s, 0), Perfect);
    assert_eq!(ranked(&tiny, r0, s, 1), Great);
    assert_eq!(ranked(&tiny, r0, s, 2), Bad);
}

#[test]
fn t9_a_release_at_the_end_and_a_press_at_the_end() {
    let chart = ln(50 * MS);
    let end = T + 50 * MS;
    // Regression: released exactly at the end, the release judges it (its
    // origin, a release distance of 0) and the end deadline adds nothing.
    let (events, _) = play(&chart, &[press(T, 0, 1), release(end, 0, 1)]);
    assert_eq!(
        judged(&events),
        [(end, Perfect, Some(0), Some(0), Some(end))]
    );
    assert_eq!(released(&events), [], "a release at the end is not early");
    // Pressed at the end: decided at the press. 1 µs before: held.
    let (events, _) = play(&chart, &[press(end, 0, 1), release(end + 10 * MS, 0, 1)]);
    assert_eq!(held(&events), []);
    assert_eq!(
        judged(&events),
        [(end, Good, Some(50 * MS), None, Some(end))]
    );
    let (events, _) = play(&chart, &[press(end - 1, 0, 1)]);
    assert_eq!(held(&events), [(end - 1, Good, 50 * MS - 1)]);
    assert_eq!(
        judged(&events),
        [(end, Good, Some(50 * MS - 1), None, None)]
    );
}

#[test]
fn t12_a_mine_at_the_ln_end() {
    // Held through the end: the long note is judged first, then the mine
    // hits. Released at that µs: the release judges it and the mine misses.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 100 * MS))
        .mine(lane(1), TimeUs(T + 100 * MS), 10)
        .build()
        .unwrap();
    let kinds = |events: &[JudgeEvent]| -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|e| match e.kind {
                JudgeEventKind::Judged { .. } => Some("judged"),
                JudgeEventKind::MineHit { .. } => Some("mine"),
                _ => None,
            })
            .collect()
    };
    let (events, _) = play(&chart, &[press(T, 0, 1), release(T + 200 * MS, 0, 1)]);
    assert_eq!(kinds(&events), ["judged", "mine"]);
    let (events, _) = play(&chart, &[press(T, 0, 1), release(T + 100 * MS, 0, 1)]);
    assert_eq!(kinds(&events), ["judged"]);
    assert_eq!(judged(&events)[0].3, Some(0));
}

/// The custom preset with the survival gauge at 50 %, away from its
/// bounds, so every change shows.
fn mid_gauges() -> RulePreset {
    let mut p = rules::custom();
    p.gauges[1].start = GaugeValue(50 * GaugeValue::PERCENT);
    p
}

#[test]
fn l1_4_a_held_head_scores_nothing_until_its_judgment() {
    // Regression: nothing is scored at the held head; each gauge changes
    // once, at the long note's judgment.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 500 * MS))
        .tap(lane(2), TimeUs(T + 2_000 * MS))
        .build()
        .unwrap();
    let preset = mid_gauges();
    let dev = device(&[0]);
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 8).unwrap();
    let start_tally = *ev.tally();
    let start_gauges = ev.gauges().to_vec();
    // A GREAT head, released 100 ms before the end (PERFECT): GREAT.
    ev.push_input(press(T - 30 * MS, 0, 1)).unwrap();
    ev.push_input(release(T + 400 * MS, 0, 1)).unwrap();
    let mut out = Vec::new();
    ev.advance_to(TimeUs(T + 400 * MS - 1), &mut out).unwrap();
    assert_eq!(held(&out), [(T - 30 * MS, Great, -30 * MS)]);
    assert_eq!(*ev.tally(), start_tally);
    assert_eq!(ev.gauges(), start_gauges.as_slice());
    assert!(
        !out.iter()
            .any(|e| matches!(e.kind, JudgeEventKind::Gauge { .. }))
    );
    out.clear();
    ev.advance_to(TimeUs(T + 400 * MS), &mut out).unwrap();
    // 2 notes: increment 8_000_000. GREAT: the normal gauge 20 % → 100 %
    // (capped at 10_000_000 exactly), survival 50 % + 0.16 %.
    let gauges: Vec<_> = out
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Gauge { gauge, value, .. } => Some((gauge, value.0)),
            _ => None,
        })
        .collect();
    assert_eq!(gauges, [(0, 10_000_000), (1, 5_016_000)]);
    let t = ev.tally();
    assert_eq!((t.counts.great, t.ex_score, t.combo, t.bp), (1, 1, 1, 0));
}

#[test]
fn l1_4_a_play_aborted_while_held_matches_live() {
    // A 500 ms note pressed on time, its release after the abort. Finalized
    // at end − 1, the long note is held and not judged; at the end, its end
    // deadline judges it. Live evaluation stepped to the same watermark
    // gives the same events and totals.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 500 * MS))
        .tap(lane(2), TimeUs(T + 2_000 * MS))
        .build()
        .unwrap();
    let preset = mid_gauges();
    let dev = device(&[0]);
    let inputs = [press(T, 0, 1), release(T + 700 * MS, 0, 1)];
    for (abort, judgments) in [(T + 500 * MS - 1, 0), (T + 500 * MS, 1)] {
        let input = EvalInput {
            events: inputs.to_vec(),
            device: dev.clone(),
            start: InputStart::default(),
            finalized: Some(TimeUs(abort)),
        };
        let batch = evaluate(&chart, &input, &preset).unwrap();
        let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 8).unwrap();
        for e in inputs {
            ev.push_input(e).unwrap();
        }
        let mut out = Vec::new();
        for step in [T, abort] {
            ev.advance_to(TimeUs(step), &mut out).unwrap();
        }
        assert_eq!(out, batch.events, "aborted at {abort}");
        assert_eq!(*ev.tally(), batch.tally, "aborted at {abort}");
        assert_eq!(judged(&out).len(), judgments, "aborted at {abort}");
        assert_eq!(held(&out).len(), 1);
        let counted = batch.tally.counts.perfect;
        assert_eq!(counted, u32::try_from(judgments).unwrap());
        // The survival gauge changed only at the judgment.
        assert_eq!(batch.gauges[1].history.len(), judgments);
    }
}

#[test]
fn l2_1_note_times_beyond_2_pow_32_are_refused() {
    let cap: i64 = 1 << 32;
    let preset = rules::custom();
    let dev = device(&[0]);
    let new =
        |chart: &Chart| Evaluator::new(chart, &preset, &dev, &InputStart::default(), 0).map(|_| ());
    // A long note from 0 to the cap is accepted; one ending at i64::MAX,
    // or a tap 1 µs past the cap, is refused.
    let mut chart = ln(1);
    chart.lanes[1][0].time = TimeUs(0);
    let end = |chart: &mut Chart, at: i64| {
        if let bms_model::NoteKind::Long { end, .. } = &mut chart.lanes[1][0].kind {
            *end = TimeUs(at);
        }
    };
    end(&mut chart, cap);
    assert_eq!(new(&chart), Ok(()));
    end(&mut chart, i64::MAX);
    assert_eq!(new(&chart), Err(EvalError::TimeOverflow));
    end(&mut chart, cap + 1);
    assert_eq!(new(&chart), Err(EvalError::TimeOverflow));
    for (at, ok) in [
        (cap, true),
        (cap + 1, false),
        (-cap, true),
        (-cap - 1, false),
    ] {
        let mut chart = ChartBuilder::new(BPM)
            .tap(lane(1), TimeUs(0))
            .build()
            .unwrap();
        chart.lanes[1][0].time = TimeUs(at);
        let expected = if ok {
            Ok(())
        } else {
            Err(EvalError::TimeOverflow)
        };
        assert_eq!(new(&chart), expected, "tap at {at}");
    }
}

#[test]
fn l1_2_scaled_edges_beyond_2_pow_32_refuse_the_rank() {
    // #DEFEXRANK 200 doubles every edge: 2^31 scales to the cap, 2^31 + 1
    // past it, for a note window and an LN-end edge alike.
    let cap: i64 = 1 << 32;
    let half = cap / 2;
    let dev = device(&[0]);
    let mut chart = ln(10 * MS);
    chart.meta.rank = Some(JudgeRank::DefExRankMilli(200_000));
    let new = |preset: &RulePreset| {
        Evaluator::new(&chart, preset, &dev, &InputStart::default(), 0).map(|_| ())
    };
    for edge in [half, half + 1] {
        let mut wide_bad = rules::custom();
        wide_bad.windows.bad = Window::symmetric(edge);
        let mut wide_end = rules::custom();
        wide_end.ln_end.good = TimeUs(edge);
        for preset in [&wide_bad, &wide_end] {
            assert_eq!(preset.validate(), Ok(()));
            let expected = if edge == half {
                Ok(())
            } else {
                Err(EvalError::InvalidRank)
            };
            assert_eq!(new(preset), expected, "edge {edge}");
        }
    }
}

#[test]
fn l2_2_a_released_lane_ends_the_visual_hold_early() {
    // A 500 ms note. Released early after a held head: the visual hold
    // ends at the release, then the release judges the long note.
    let chart = ln(500 * MS);
    let (events, _) = play(&chart, &[press(T, 0, 1), release(T + 300 * MS, 0, 1)]);
    let kinds: Vec<_> = events
        .iter()
        .filter(|e| e.time == TimeUs(T + 300 * MS))
        .map(|e| e.kind)
        .collect();
    assert!(matches!(
        kinds[0],
        JudgeEventKind::LnReleased { note: 0, .. }
    ));
    assert!(matches!(kinds[1], JudgeEventKind::Judged { .. }));
    assert_eq!(released(&events), [(T + 300 * MS, 0, Some(T + 300 * MS))]);
    // A BAD head is decided at its press but stays visually held until the
    // lane is released: that release ends it and scores nothing.
    let (events, tally) = play(
        &chart,
        &[press(T - 150 * MS, 0, 1), release(T + 100 * MS, 0, 1)],
    );
    assert_eq!(released(&events), [(T + 100 * MS, 0, Some(T + 100 * MS))]);
    let at_release: Vec<_> = events
        .iter()
        .filter(|e| e.time == TimeUs(T + 100 * MS))
        .collect();
    assert_eq!(at_release.len(), 1, "no judgment or gauge at the release");
    assert_eq!((tally.counts.bad, tally.bp), (1, 1));
    // Held through the end, or released after it: no early release.
    for until in [T + 500 * MS, T + 600 * MS] {
        let (events, _) = play(&chart, &[press(T, 0, 1), release(until, 0, 1)]);
        assert_eq!(released(&events), [], "released at {until}");
        let (events, _) = play(&chart, &[press(T - 150 * MS, 0, 1), release(until, 0, 1)]);
        assert_eq!(released(&events), [], "BAD head released at {until}");
    }
}

#[test]
fn l2_2_another_binding_keeps_the_lane_held_until_a_disconnect() {
    // Two sources bind key 1. Source 1 presses after source 0, which then
    // releases: the lane stays held, so neither hold ends. Source 1's
    // disconnect releases the lane before the end.
    let chart = ln(500 * MS);
    let dev = device(&[0, 0]);
    let inputs = [
        press(T, 0, 1),
        press(T + 10 * MS, 1, 1),
        release(T + 100 * MS, 0, 1),
        disconnect(T + 350 * MS, 1),
    ];
    let (events, tally) = run(&chart, &dev, &InputStart::default(), &inputs, AFTER);
    assert_eq!(released(&events), [(T + 350 * MS, 0, Some(T + 350 * MS))]);
    // 150 ms before the end: GREAT.
    assert_eq!(
        judged(&events),
        [(
            T + 350 * MS,
            Great,
            Some(0),
            Some(-150 * MS),
            Some(T + 350 * MS)
        )]
    );
    assert_eq!((tally.counts.great, tally.bp, tally.combo), (1, 0, 1));
    // Without the disconnect, source 1 holds it through the end.
    let (events, _) = run(&chart, &dev, &InputStart::default(), &inputs[..3], AFTER);
    assert_eq!(released(&events), []);
    assert_eq!(
        judged(&events),
        [(T + 500 * MS, Perfect, Some(0), None, None)]
    );
}
