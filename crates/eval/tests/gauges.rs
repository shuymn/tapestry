//! Hand-computed whole results with the custom preset: counts, combo, EX
//! score, BP, LN breaks and both gauges.
//!
//! Gauges are in 1/100000 %: the normal gauge starts at 2_000_000 (20 %),
//! floors at 200_000 (2 %), and caps at 10_000_000; the survival gauge starts
//! at 10_000_000. With `#TOTAL` absent (160), the per-note increment is
//! 16_000_000 / notes.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{EvalError, EvalInput, Evaluation, InputStart, JudgeEventKind, LnOutcome, evaluate};
use rules::{GaugeDelta, GaugeValue, RulePreset};

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
    ln_breaks: u32,
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
        ln_breaks: t.ln_breaks,
        empty_poor: t.empty_poor,
        normal: e.gauges[0].value.0,
        survival: e.gauges[1].value.0,
    }
}

fn play(chart: &Chart, events: Vec<eval::PhysicalInputEvent>) -> Evaluation {
    play_with(chart, &rules::custom(), events)
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
    };
    let e = evaluate(chart, &input, rule).unwrap();
    assert!(e.completed);
    e
}

/// A gauge's values after each change.
fn history(e: &Evaluation, gauge: usize) -> Vec<i64> {
    e.gauges[gauge].history.iter().map(|p| p.value.0).collect()
}

fn ln_ends(e: &Evaluation) -> Vec<(i64, LnOutcome)> {
    e.events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::LnEnd { outcome, .. } => Some((ev.time.0, outcome)),
            _ => None,
        })
        .collect()
}

#[test]
fn ln_early_release_after_perfect_head() {
    // LN 1000-2000 ms and a tap at 2100 ms: 2 notes, increment 8_000_000.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .tap(lane(1), TimeUs(2_100 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(1_000 * MS, 0, 1),   // head PERFECT: 10_000_000 (capped), 10_000_000
            release(1_700 * MS, 0, 1), // 300 ms early: break, POOR damage: 9_400_000, 9_100_000
            press(1_850 * MS, 0, 1),   // 250 ms before the tap: empty POOR: 9_100_000, 8_650_000
            release(1_860 * MS, 0, 1),
            press(2_100 * MS, 0, 1), // tap PERFECT: 10_000_000 (capped), 8_666_000
        ],
    );
    assert_eq!(
        summary(&e),
        Summary {
            counts: [2, 0, 0, 0, 0],
            combo: 1,
            max_combo: 1,
            ex: 4,
            bp: 1,
            ln_breaks: 1,
            empty_poor: 1,
            normal: 10_000_000,
            survival: 8_666_000,
        }
    );
    assert_eq!(ln_ends(&e), [(1_700 * MS, LnOutcome::Broken)]);
}

#[test]
fn ln_release_margin_is_end_minus_100_ms() {
    // Released exactly at end − 100 ms completes; 1 µs earlier breaks.
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![
            press(1_000 * MS, 0, 1),
            press(1_000 * MS, 0, 2),
            release(1_900 * MS - 1, 0, 2),
            release(1_900 * MS, 0, 1),
        ],
    );
    assert_eq!(
        ln_ends(&e),
        [
            (1_900 * MS - 1, LnOutcome::Broken),
            (1_900 * MS, LnOutcome::Completed)
        ]
    );
    assert_eq!(e.tally.ln_breaks, 1);
}

#[test]
fn late_head_after_short_ln_end() {
    // LN 1000-1050 ms, head pressed at 1100 ms (+100 ms, GOOD), after its
    // end: completed at the press. One note: GOOD gives 8_000_000.
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
            ln_breaks: 0,
            empty_poor: 0,
            normal: 10_000_000,
            survival: 10_000_000,
        }
    );
    assert_eq!(ln_ends(&e), [(1_100 * MS, LnOutcome::Completed)]);

    // Pressed exactly at the end (+50 ms, GOOD): completed at the press.
    // The press itself ends it, right after its judgment and gauges, not
    // the LN end deadline at the same time.
    let e = play(&chart, vec![press(1_050 * MS, 0, 2)]);
    assert_eq!(ln_ends(&e), [(1_050 * MS, LnOutcome::Completed)]);
    assert_eq!(e.tally.counts.good, 1);
    let at_end: Vec<_> = e
        .events
        .iter()
        .filter(|ev| ev.time == TimeUs(1_050 * MS))
        .map(|ev| (ev.kind, ev.origin))
        .collect();
    assert!(matches!(at_end[0].0, JudgeEventKind::Judged { .. }));
    assert!(matches!(
        at_end[1].0,
        JudgeEventKind::Gauge { gauge: 0, .. }
    ));
    assert!(matches!(
        at_end[2].0,
        JudgeEventKind::Gauge { gauge: 1, .. }
    ));
    assert!(matches!(at_end[3].0, JudgeEventKind::LnEnd { .. }));
    assert_eq!(at_end[3].1, Some(TimeUs(1_050 * MS)));
    assert_eq!(at_end.len(), 4);
}

#[test]
fn ln_tail_deadline_survives_judged_head() {
    // Head PERFECT and never released: only the LN end deadline completes it.
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
            ln_breaks: 0,
            empty_poor: 0,
            normal: 10_000_000,
            survival: 10_000_000,
        }
    );
    assert_eq!(ln_ends(&e), [(2_000 * MS, LnOutcome::Completed)]);
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
            ln_breaks: 0,
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
    // The lane is held from 500 ms (too early to judge) to 1150 ms.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(6), TimeUs(1_000 * MS))
        .mine(lane(6), TimeUs(1_100 * MS), 10)
        .build()
        .unwrap();
    let e = play(
        &chart,
        vec![press(500 * MS, 0, 6), release(1_150 * MS, 0, 6)],
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
            ln_breaks: 0,
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
            press(1_700 * MS, 0, 7), // 9_700_000, 9_550_000
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
            bp: 0,
            ln_breaks: 0,
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
fn poor_ln_end_and_held_mine_at_one_time() {
    // At 2180 ms a tap misses (lane 3), a held LN ends (lane 2), and a held
    // 10-unit mine passes (lane 4). Survival is at 34 % before them: the
    // POOR takes full damage (34 → 25 %), then the mine its full 5 %
    // (→ 20 %). A halved mine would leave 22.5 %; the mine first, 24.5 %.
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
            press(300 * MS, 0, 2), // head PERFECT: 10_000_000 (capped) both
            press(400 * MS, 0, 1), // mine of 66 %: 3_400_000 both
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
            ln_breaks: 0,
            empty_poor: 0,
            normal: 2_300_000,
            survival: 2_000_000,
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
            JudgeEventKind::Judged {
                lane: lane(3),
                note: 0,
                judgment: rules::Judgment::Poor,
                deviation: None,
                combo: 0,
            },
            g(0, 2_800_000),
            g(1, 2_500_000),
            JudgeEventKind::LnEnd {
                lane: lane(2),
                note: 0,
                outcome: LnOutcome::Completed,
            },
            JudgeEventKind::MineHit {
                lane: lane(4),
                note: 0,
            },
            g(0, 2_300_000),
            g(1, 2_000_000),
            JudgeEventKind::SongEnd,
        ]
    );
}

#[test]
fn reduced_odd_damage_floors_with_its_sign() {
    // Survival POOR of −3 units, taken below 30 %: −3 / 2 = −1.5, floored
    // to −2.
    let mut rule = rules::custom();
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
    for total in [None, Some(0), Some(-1), Some(-160_000)] {
        chart.meta.total_milli = total;
        let e = play(&chart, vec![press(1_000 * MS, 0, 1)]);
        assert_eq!(history(&e, 0)[0], 5_200_000, "{total:?}");
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
        ln_breaks: 0,
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
    chart.meta.total_milli = Some(100_001);
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
            ln_breaks: 0,
            empty_poor: 0,
            normal: 6_400_049,
            survival: 9_100_000,
        }
    );

    // An extreme #TOTAL overflows i64 before clamping; it stays in i128 and
    // is clamped to the cap.
    chart.meta.total_milli = Some(i64::MAX);
    let e = play(&chart, vec![press(1_030 * MS, 0, 1)]);
    assert_eq!(e.gauges[0].history[0].value, GaugeValue::FULL);
}

#[test]
fn mines_and_ln_heads_in_denominator() {
    // 3 taps, 2 LNs and 4 mines: 5 judgeable notes, increment 3_200_000.
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
            ln_breaks: 0,
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
        evaluate(&chart, &input, &rules::custom()),
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
