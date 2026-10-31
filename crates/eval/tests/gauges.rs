//! Hand-computed whole results with the tapestry preset: counts, combo, EX
//! score, BP, empty POORs and both gauges.
//!
//! Gauges are in 1/100000 %: the normal gauge starts at 2_000_000 (20 %),
//! floors at 200_000 (2 %), and caps at 10_000_000; the survival gauge starts
//! at 10_000_000. With `#TOTAL` absent (160), the per-note increment is
//! 16_000_000 / notes.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, lane, press, release, total};
use eval::{EvalError, EvalInput, Evaluation, InputStart, JudgeEventKind, evaluate};
use rules::{GaugeDelta, GaugeValue, Judgment, RulePreset};

const BPM: i64 = 150_000;

/// Everything a whole result asserts.
#[derive(Debug, PartialEq, Eq)]
struct Summary {
    /// PERFECT, GREAT, GOOD, BAD, POOR.
    counts: [u32; 5],
    combo: u32,
    max_combo: u32,
    ex: u32,
    bp: u32,
    empty_poor: u32,
    normal: i64,
    survival: i64,
}

fn summary(e: &Evaluation) -> Summary {
    let t = &e.tally;
    let c = &t.counts;
    Summary {
        counts: [c.perfect, c.great, c.good, c.bad, c.poor],
        combo: t.combo,
        max_combo: t.max_combo,
        ex: t.ex_score,
        bp: t.bp,
        empty_poor: t.empty_poor,
        normal: e.gauges[0].value.0,
        survival: e.gauges[1].value.0,
    }
}

fn play(chart: &Chart, events: Vec<eval::PhysicalInputEvent>) -> Evaluation {
    play_with(chart, &rules::tapestry(), events)
}

fn play_with(
    chart: &Chart,
    rule: &RulePreset,
    events: Vec<eval::PhysicalInputEvent>,
) -> Evaluation {
    let input = EvalInput {
        events,
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: None,
    };
    let e = evaluate(chart, &input, rule).unwrap();
    assert!(e.completed);
    e
}

/// A gauge's values after each change.
fn history(e: &Evaluation, gauge: usize) -> Vec<i64> {
    e.gauges[gauge].history.iter().map(|p| p.value.0).collect()
}

/// The times a gauge changed at.
fn changed_at(e: &Evaluation, gauge: usize) -> Vec<i64> {
    e.gauges[gauge].history.iter().map(|p| p.time.0).collect()
}

/// Each judgment: (time, judgment, deviation, release, origin).
type Row = (i64, Judgment, Option<i64>, Option<i64>, Option<i64>);

fn judged(e: &Evaluation) -> Vec<Row> {
    e.events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                release,
                ..
            } => Some((
                ev.time.0,
                judgment,
                deviation.map(|d| d.0),
                release.map(|d| d.0),
                ev.origin.map(|o| o.0),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn ln_early_release_grades_the_long_note_bad() {
    // Regression: one judgment per long note. LN 1000-2000 ms and a tap at
    // 2100 ms: 2 notes, increment 8_000_000. The head is PERFECT, the
    // release 300 ms early is past every LN-end edge (200 ms): the long
    // note is one BAD, scored at the release.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(2_100 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(1_000 * MS, 0, 1),   // head PERFECT, held: no change
            release(1_700 * MS, 0, 1), // BAD: 1_800_000, 9_500_000
            press(1_850 * MS, 0, 1),   // 250 ms before the tap: empty POOR: 1_500_000, 9_050_000
            release(1_860 * MS, 0, 1),
            press(2_100 * MS, 0, 1), // tap PERFECT: 9_500_000, 9_066_000
        ],
    );
    assert_eq!(
        summary(&e),
        Summary {
            counts: [1, 0, 0, 1, 0],
            combo: 1,
            max_combo: 1,
            ex: 2,
            bp: 2,
            empty_poor: 1,
            normal: 9_500_000,
            survival: 9_066_000,
        }
    );
    assert_eq!(
        judged(&e),
        [
            (
                1_700 * MS,
                Judgment::Bad,
                Some(0),
                Some(-300 * MS),
                Some(1_700 * MS)
            ),
            (
                2_100 * MS,
                Judgment::Perfect,
                Some(0),
                None,
                Some(2_100 * MS)
            ),
        ]
    );
    assert_eq!(history(&e, 0), [1_800_000, 1_500_000, 9_500_000]);
    assert_eq!(changed_at(&e, 0), [1_700 * MS, 1_850 * MS, 2_100 * MS]);
}

#[test]
fn late_head_after_short_ln_end() {
    // LN 1000-1050 ms, head pressed at 1100 ms (+100 ms, GOOD), after its
    // end: decided at the press. One note: GOOD gives 8_000_000.
    let chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_050 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![press(1_100 * MS, 0, 2), release(1_120 * MS, 0, 2)],
    );
    assert_eq!(
        summary(&e),
        Summary {
            counts: [0, 0, 1, 0, 0],
            combo: 1,
            max_combo: 1,
            ex: 0,
            bp: 0,
            empty_poor: 0,
            normal: 10_000_000,
            survival: 10_000_000,
        }
    );
    assert_eq!(
        judged(&e),
        [(
            1_100 * MS,
            Judgment::Good,
            Some(100 * MS),
            None,
            Some(1_100 * MS)
        )]
    );

    // Pressed exactly at the end (+50 ms, GOOD): decided at the press, by
    // the press itself, not the LN end deadline at the same time.
    let e = play(&chart, vec![press(1_050 * MS, 0, 2)]);
    assert_eq!(e.tally.counts.good, 1);
    let at_end: Vec<_> = e
        .events
        .iter()
        .filter(|ev| ev.time == TimeUs(1_050 * MS))
        .map(|ev| (ev.kind, ev.origin))
        .collect();
    assert!(matches!(
        at_end[0].0,
        JudgeEventKind::Judged {
            judgment: Judgment::Good,
            release: None,
            ..
        }
    ));
    assert_eq!(at_end[0].1, Some(TimeUs(1_050 * MS)));
    assert!(matches!(
        at_end[1].0,
        JudgeEventKind::Gauge { gauge: 0, .. }
    ));
    assert!(matches!(
        at_end[2].0,
        JudgeEventKind::Gauge { gauge: 1, .. }
    ));
    assert_eq!(at_end.len(), 3);
}

#[test]
fn ln_held_through_its_end_is_scored_at_the_end() {
    // Regression for the scoring time: the head PERFECT is held and never
    // released, so the gauges change once, at the end deadline, not at
    // the press.
    let chart = ChartBuilder::new(BPM)
        .long(lane(3), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let e = play(&chart, vec![press(1_000 * MS, 0, 3)]);
    assert_eq!(
        summary(&e),
        Summary {
            counts: [1, 0, 0, 0, 0],
            combo: 1,
            max_combo: 1,
            ex: 2,
            bp: 0,
            empty_poor: 0,
            normal: 10_000_000,
            survival: 10_000_000,
        }
    );
    assert_eq!(
        judged(&e),
        [(2_000 * MS, Judgment::Perfect, Some(0), None, None)]
    );
    assert_eq!(changed_at(&e, 0), [2_000 * MS]);
    assert_eq!(changed_at(&e, 1), [2_000 * MS]);
}

#[test]
fn mine_damages_only_when_held() {
    // Mines of 10 units (5 %) on lane 4 at 1000, 2000 and 2500 ms; a tap on
    // lane 5 is left unhit.
    let chart = ChartBuilder::new(BPM)
        .mine(lane(4), TimeUs(1_000 * MS), 10)
        .mine(lane(4), TimeUs(2_000 * MS), 10)
        .mine(lane(4), TimeUs(2_500 * MS), 10)
        .tap(lane(5), TimeUs(3_000 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            // Pressed at the mine's time: inputs come first, so it is held.
            press(1_000 * MS, 0, 4),
            release(1_200 * MS, 0, 4),
            // Released at the mine's time: released first, so no damage.
            press(1_500 * MS, 0, 4),
            release(2_000 * MS, 0, 4),
        ],
    );
    // Mine: 1_500_000, 9_500_000. POOR: 900_000, 8_600_000.
    assert_eq!(
        summary(&e),
        Summary {
            counts: [0, 0, 0, 0, 1],
            combo: 0,
            max_combo: 0,
            ex: 0,
            bp: 1,
            empty_poor: 0,
            normal: 900_000,
            survival: 8_600_000,
        }
    );
    let hits: Vec<_> = e
        .events
        .iter()
        .filter(|ev| matches!(ev.kind, JudgeEventKind::MineHit { .. }))
        .map(|ev| ev.time.0)
        .collect();
    assert_eq!(hits, [1_000 * MS]);
}

#[test]
fn mine_after_unhit_tap_uses_chronological_deadlines() {
    // Tap at 1000 ms (misses at 1180 ms) and a mine at 1100 ms on one lane.
    // The lane is held from 450 ms (550 ms early: no judgment and no empty
    // POOR) to 1150 ms.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(6), TimeUs(1_000 * MS))
        .mine(lane(6), TimeUs(1_100 * MS), 10)
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![press(450 * MS, 0, 6), release(1_150 * MS, 0, 6)],
    );
    // Mine while held: 1_500_000, 9_500_000. POOR: 900_000, 8_600_000.
    assert_eq!(
        summary(&e),
        Summary {
            counts: [0, 0, 0, 0, 1],
            combo: 0,
            max_combo: 0,
            ex: 0,
            bp: 1,
            empty_poor: 0,
            normal: 900_000,
            survival: 8_600_000,
        }
    );
}

#[test]
fn empty_poor_full_result() {
    // Two taps, increment 8_000_000. The empty POOR takes half a POOR:
    // 3 % and 4.5 %.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(7), TimeUs(1_000 * MS))
        .tap(lane(7), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(1_000 * MS, 0, 7), // 10_000_000, 10_000_000
            release(1_010 * MS, 0, 7),
            press(1_700 * MS, 0, 7), // BP: 9_700_000, 9_550_000
            release(1_710 * MS, 0, 7),
            press(2_000 * MS, 0, 7), // 10_000_000, 9_566_000
        ],
    );
    assert_eq!(
        summary(&e),
        Summary {
            counts: [2, 0, 0, 0, 0],
            combo: 2,
            max_combo: 2,
            ex: 4,
            bp: 1,
            empty_poor: 1,
            normal: 10_000_000,
            survival: 9_566_000,
        }
    );
    // The cap hides the empty POOR in the final value; the history shows it.
    assert_eq!(history(&e, 0), [10_000_000, 9_700_000, 10_000_000]);
    assert_eq!(history(&e, 1), [10_000_000, 9_550_000, 9_566_000]);
}

#[test]
fn ln_end_tap_miss_and_held_mine_at_one_time() {
    // Regression for the deadline order: at 2180 ms a held LN ends (lane 2:
    // its visual hold ends, then it is judged),
    // a tap misses (lane 3), and a held 10-unit mine passes (lane 4). The
    // long note is judged first, then the miss, then the mine. 2 notes:
    // increment 8_000_000. The 66 % mine at 500 ms takes the normal gauge
    // to its 2 % floor and survival to 34 %; the LN's PERFECT then gives
    // 8_200_000 and 34.16 %. The POOR takes full survival damage (34.16 →
    // 25.16 %), then the mine its full 5 % (→ 20.16 %).
    let chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(300 * MS), TimeUs(2_180 * MS))
        .mine(lane(1), TimeUs(500 * MS), 132)
        .tap(lane(3), TimeUs(2_000 * MS))
        .mine(lane(4), TimeUs(2_180 * MS), 10)
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(300 * MS, 0, 2), // head PERFECT, held: no change
            press(400 * MS, 0, 1), // mine of 66 %: 200_000, 3_400_000
            release(600 * MS, 0, 1),
            press(2_100 * MS, 0, 4),
        ],
    );
    assert_eq!(
        summary(&e),
        Summary {
            counts: [1, 0, 0, 0, 1],
            combo: 0,
            max_combo: 1,
            ex: 2,
            bp: 1,
            empty_poor: 0,
            normal: 7_100_000,
            survival: 2_016_000,
        }
    );
    let at: Vec<_> = e
        .events
        .iter()
        .filter(|ev| ev.time == TimeUs(2_180 * MS))
        .map(|ev| ev.kind)
        .collect();
    let g = |gauge, value| JudgeEventKind::Gauge {
        gauge,
        value: GaugeValue(value),
        reference: GaugeValue(value),
    };
    assert_eq!(
        at,
        [
            JudgeEventKind::LnHoldEnded {
                lane: lane(2),
                note: 0,
            },
            JudgeEventKind::Judged {
                lane: lane(2),
                note: 0,
                judgment: Judgment::Perfect,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 1,
            },
            g(0, 8_200_000),
            g(1, 3_416_000),
            JudgeEventKind::Judged {
                lane: lane(3),
                note: 0,
                judgment: Judgment::Poor,
                deviation: None,
                release: None,
                combo: 0,
            },
            g(0, 7_600_000),
            g(1, 2_516_000),
            JudgeEventKind::MineHit {
                lane: lane(4),
                note: 0,
            },
            g(0, 7_100_000),
            g(1, 2_016_000),
            JudgeEventKind::SongEnd,
        ]
    );
}

#[test]
fn reduced_odd_damage_floors_with_its_sign() {
    // Survival POOR of −3 units, taken below 30 %: −3 / 2 = −1.5, floored
    // to −2.
    let mut rule = rules::tapestry();
    rule.gauges[1].change.poor = GaugeDelta::Fixed(GaugeValue(-3));
    let chart = ChartBuilder::new(BPM)
        .mine(lane(1), TimeUs(500 * MS), 141)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let e = play_with(
        &chart,
        &rule,
        vec![press(400 * MS, 0, 1), release(600 * MS, 0, 1)],
    );
    assert_eq!(history(&e, 1), [2_950_000, 2_949_998]);
}

#[test]
fn non_positive_total_counts_as_absent() {
    // Five taps: with #TOTAL 160 the increment is 3_200_000.
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .tap(lane(1), TimeUs(4_000 * MS))
        .tap(lane(1), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    for declared in [None, Some(0), Some(-1), Some(-160_000)] {
        chart.meta.judgment_headers = declared.map(total).into_iter().collect();
        let e = play(&chart, vec![press(1_000 * MS, 0, 1)]);
        assert_eq!(history(&e, 0)[0], 5_200_000, "{declared:?}");
    }
}

/// A mine of `units` (0.5 % each) held at 500 ms, then one unhit tap.
fn mine_then_poor(units: i32) -> Evaluation {
    let chart = ChartBuilder::new(BPM)
        .mine(lane(1), TimeUs(500 * MS), units)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    play(&chart, vec![press(400 * MS, 0, 1), release(600 * MS, 0, 1)])
}

#[test]
fn survival_damage_halves_below_30_percent() {
    let poor_only = |normal, survival| Summary {
        counts: [0, 0, 0, 0, 1],
        combo: 0,
        max_combo: 0,
        ex: 0,
        bp: 1,
        empty_poor: 0,
        normal,
        survival,
    };
    // 140 units: survival to exactly 30 %; the POOR takes full damage (9 %).
    // The normal gauge stays at its 2 % floor.
    assert_eq!(summary(&mine_then_poor(140)), poor_only(200_000, 2_100_000));
    // 141 units: 29.5 %, below 30 %: the POOR is halved (4.5 %).
    assert_eq!(summary(&mine_then_poor(141)), poor_only(200_000, 2_500_000));
}

#[test]
fn gauge_rounding_is_floor_in_i128() {
    // #TOTAL 100.001 over 3 notes: 10_000_100 / 3 = 3_333_366 rem 2, and a
    // GOOD's half is 1_666_683.
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(3_000 * MS))
        .build()
        .unwrap();
    chart.meta.judgment_headers = vec![total(100_001)];
    let e = play(
        &chart,
        vec![
            press(1_030 * MS, 0, 1), // GREAT: 5_333_366, 10_000_000
            release(1_040 * MS, 0, 1),
            press(2_060 * MS, 0, 1), // GOOD: 7_000_049, 10_000_000
        ],
    ); // POOR: 6_400_049, 9_100_000
    assert_eq!(
        summary(&e),
        Summary {
            counts: [0, 1, 1, 0, 1],
            combo: 0,
            max_combo: 2,
            ex: 1,
            bp: 1,
            empty_poor: 0,
            normal: 6_400_049,
            survival: 9_100_000,
        }
    );

    // The largest #TOTAL a chart can declare (12 integer digits) is far
    // past the cap; the increment stays in i128 and is clamped to it.
    chart.meta.judgment_headers = vec![total(999_999_999_999_999)];
    let e = play(&chart, vec![press(1_030 * MS, 0, 1)]);
    assert_eq!(e.gauges[0].history[0].value, GaugeValue::FULL);
}

#[test]
fn mines_left_out_and_long_notes_counted_once_in_denominator() {
    // Preservation: 3 taps, 2 LNs and 4 mines: 5 judgeable notes, one
    // judgment each, increment 3_200_000.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .tap(lane(3), TimeUs(3_000 * MS))
        .long(lane(4), TimeUs(4_000 * MS), TimeUs(4_500 * MS))
        .long(lane(5), TimeUs(5_000 * MS), TimeUs(5_500 * MS))
        .mine(lane(6), TimeUs(6_000 * MS), 10)
        .mine(lane(6), TimeUs(6_100 * MS), 10)
        .mine(lane(7), TimeUs(6_000 * MS), 10)
        .mine(lane(7), TimeUs(6_100 * MS), 10)
        .build()
        .unwrap();
    // One PERFECT (5_200_000, 10_000_000), then four POORs.
    let e = play(&chart, vec![press(1_000 * MS, 0, 1)]);
    assert_eq!(
        summary(&e),
        Summary {
            counts: [1, 0, 0, 0, 4],
            combo: 0,
            max_combo: 1,
            ex: 2,
            bp: 4,
            empty_poor: 0,
            normal: 2_800_000,
            survival: 6_400_000,
        }
    );
}

#[test]
fn zero_judgeable_notes_is_an_error() {
    let chart = ChartBuilder::new(BPM)
        .mine(lane(1), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    let input = EvalInput {
        device: device(&[0]),
        ..EvalInput::default()
    };
    assert_eq!(
        evaluate(&chart, &input, &rules::tapestry()),
        Err(EvalError::NoJudgeableNotes)
    );
}

#[test]
fn survival_gauge_never_recovers() {
    // A 200-unit mine (100 %) takes the survival gauge to exactly 0 at
    // 1000 ms; a PERFECT afterwards does not bring it back.
    let chart = ChartBuilder::new(BPM)
        .mine(lane(1), TimeUs(1_000 * MS), 200)
        .tap(lane(2), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(900 * MS, 0, 1),
            release(1_100 * MS, 0, 1),
            press(2_000 * MS, 0, 2),
        ],
    );
    let survival = &e.gauges[1];
    assert_eq!(survival.failed_at, Some(TimeUs(1_000 * MS)));
    assert_eq!(survival.value, GaugeValue::EMPTY);
    // The reference trajectory goes on: 0 + 0.16 %.
    assert_eq!(survival.reference, GaugeValue(16_000));
    assert!(!survival.cleared);
    let after: Vec<_> = survival.history.iter().map(|p| p.value.0).collect();
    assert_eq!(after, [0, 0]);
    // The normal gauge: floor 2 %, then +16_000_000, capped.
    assert_eq!(e.gauges[0].value, GaugeValue::FULL);
    assert!(e.gauges[0].cleared);
}

/// A whole chart's gauges start at their rules' start values, and the
/// evaluation's final deadline is its evaluator's, completed or stopped at
/// a finalized watermark before it.
#[test]
fn gauges_start_at_their_rule_and_final_deadline_is_the_evaluators() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let e = play(&chart, vec![press(1_000 * MS, 0, 1)]);
    let starts: Vec<_> = e.gauges.iter().map(|g| g.start).collect();
    assert_eq!(starts, [GaugeValue(2_000_000), GaugeValue(10_000_000)]);
    assert_eq!(
        starts,
        preset.gauges.iter().map(|g| g.start).collect::<Vec<_>>()
    );
    let deadline = eval::Evaluator::new(
        &chart,
        &preset,
        &device(&[0]),
        &InputStart::default(),
        None,
        0,
    )
    .unwrap()
    .final_deadline();
    // The second tap's miss, past its 2 s.
    assert!(deadline > TimeUs(2_000 * MS));
    assert_eq!(e.final_deadline, deadline);
    let input = EvalInput {
        events: vec![press(1_000 * MS, 0, 1)],
        device: device(&[0]),
        start: InputStart::default(),
        finalized: Some(TimeUs(1_500 * MS)),
        section: None,
    };
    let aborted = evaluate(&chart, &input, &preset).unwrap();
    assert!(!aborted.completed);
    assert_eq!(aborted.final_deadline, deadline);
}
