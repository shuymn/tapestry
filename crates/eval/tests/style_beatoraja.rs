//! Hand-written examples of the beatoraja-style preset
//! (`rules::beatoraja_style()`), each naming the public source it follows.
//!
//! Sources are files of beatoraja (<https://github.com/exch-bms2/beatoraja>
//! at `83574048ba489082eb46df7fe9806b464bf1f526`, under
//! `src/bms/player/beatoraja/`) and of jbms-parser
//! (<https://github.com/exch-bms2/jbms-parser> at
//! `8108cd54a19275810e1f5d2c9586f008eb6db95e`, under `src/bms/model/`),
//! cited as `file:line`. The behaviors the preset model cannot express yet
//! are listed in the preset's module, with the examples that would differ;
//! none of them is asserted here.
//!
//! The time of a miss is this evaluator's convention, [`MISSED`] after the
//! note (difference H); the POOR itself follows the source.
//!
//! Window values are at `#RANK 3` (100 %) unless a test says otherwise.
//! Gauges are in 1/100000 %, in the preset's order: NORMAL, ASSIST EASY,
//! EASY, HARD, EX HARD, HAZARD.

mod common;

use bms_model::{Chart, ChartBuilder, JudgmentHeader, Lane, TimeUs};
use common::{MS, device, empty_poors, judgments, lane, press, release};
use eval::{
    EvalError, EvalInput, Evaluation, Evaluator, InputStart, JudgeEventKind, PhysicalInputEvent,
    evaluate,
};
use rules::Judgment::{self, Bad, Good, Great, Perfect, Poor};
use rules::{JudgeWindows, LaneClass, LnEndWindows, RuleError, Side, Window};

const BPM: i64 = 150_000;
/// The first note's time.
const T: i64 = 1_000 * MS;
/// One percent of a gauge.
const PCT: i64 = 100_000;
/// When this evaluator decides a miss: at the late BAD edge (280 ms at
/// `#RANK 3`). beatoraja decides it at the first frame after the edge
/// (difference H in the preset module), so this time is this evaluator's
/// convention, not the source's.
const MISSED: i64 = 280 * MS;

const NORMAL: usize = 0;
const ASSIST_EASY: usize = 1;
const EASY: usize = 2;
const HARD: usize = 3;
const EX_HARD: usize = 4;
const HAZARD: usize = 5;

fn with_rank(mut chart: Chart, rank: Option<JudgmentHeader>) -> Chart {
    chart.meta.judgment_headers = rank.into_iter().collect();
    chart
}

/// Taps on key 1 at `times` (µs), at `#RANK 3`.
fn taps(times: &[i64]) -> Chart {
    taps_ranked(times, Some(common::rank(3)))
}

fn taps_ranked(times: &[i64], rank: Option<JudgmentHeader>) -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for &t in times {
        b = b.tap(lane(1), TimeUs(t));
    }
    with_rank(b.build().unwrap(), rank)
}

/// One long note on key 1 from `T` lasting `length`, at `rank`.
fn long(length: i64, rank: Option<JudgmentHeader>) -> Chart {
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + length))
        .build()
        .unwrap();
    with_rank(chart, rank)
}

fn play(chart: &Chart, events: Vec<PhysicalInputEvent>) -> Evaluation {
    let input = EvalInput {
        events,
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: None,
    };
    let e = evaluate(chart, &input, &rules::beatoraja_style()).unwrap();
    assert!(e.completed);
    e
}

/// A press at `at` released 10 µs later, on key 1.
fn tap_at(at: i64) -> Vec<PhysicalInputEvent> {
    vec![press(at, 0, 1), release(at + 10, 0, 1)]
}

/// Presses at each of `at`, each released 10 µs later.
fn taps_at(at: &[i64]) -> Vec<PhysicalInputEvent> {
    at.iter().flat_map(|&t| tap_at(t)).collect()
}

/// The judgment of the one note pressed at deviation `d` (press − note).
fn judge_tap(chart: &Chart, d: i64) -> Option<Judgment> {
    let e = play(chart, tap_at(T + d));
    judgments(&e.events)
        .into_iter()
        .find(|j| j.3.is_some())
        .map(|j| j.2)
}

/// Each judgment: (time, judgment).
fn judged_at(e: &Evaluation) -> Vec<(i64, Judgment)> {
    e.events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged { judgment, .. } => Some((ev.time.0, judgment)),
            _ => None,
        })
        .collect()
}

fn gauge(e: &Evaluation, g: usize) -> i64 {
    e.gauges[g].value.0
}

// ------------------------------------------------------------ windows

/// Source: play/JudgeProperty.java:23 (PG {-20000, 20000}); both edges
/// inclusive, play/JudgeProperty.java:187.
#[test]
fn perfect_is_20_ms_each_side() {
    let chart = taps(&[T]);
    for (d, j) in [
        (-20 * MS, Perfect),
        (20 * MS, Perfect),
        (-20 * MS - 1, Great),
        (20 * MS + 1, Great),
    ] {
        assert_eq!(judge_tap(&chart, d), Some(j), "at {d}");
    }
}

/// Source: play/JudgeProperty.java:23 (GR {-60000, 60000}).
#[test]
fn great_is_60_ms_each_side() {
    let chart = taps(&[T]);
    for (d, j) in [
        (-60 * MS, Great),
        (60 * MS, Great),
        (-60 * MS - 1, Good),
        (60 * MS + 1, Good),
    ] {
        assert_eq!(judge_tap(&chart, d), Some(j), "at {d}");
    }
}

/// Source: play/JudgeProperty.java:23 (GD {-150000, 150000}).
#[test]
fn good_is_150_ms_each_side() {
    let chart = taps(&[T]);
    for (d, j) in [
        (-150 * MS, Good),
        (150 * MS, Good),
        (-150 * MS - 1, Bad),
        (150 * MS + 1, Bad),
    ] {
        assert_eq!(judge_tap(&chart, d), Some(j), "at {d}");
    }
}

/// Source: play/JudgeProperty.java:23 (BD {-280000, 220000}, as {late,
/// early} per :58-59, with note − press as the deviation,
/// play/JudgeManager.java:386).
#[test]
fn bad_is_220_ms_early_and_280_ms_late() {
    let chart = taps(&[T]);
    assert_eq!(judge_tap(&chart, -220 * MS), Some(Bad));
    assert_eq!(judge_tap(&chart, 280 * MS), Some(Bad));
    assert_eq!(judge_tap(&chart, 280 * MS + 1), None);
}

/// Source: play/JudgeManager.java:401 and play/JudgeProperty.java:23 (MS
/// {-150000, 500000}): a press just before the early BAD edge judges no
/// note and is an empty POOR, since the note is within 500 ms after it.
#[test]
fn a_press_past_the_early_bad_edge_is_an_empty_poor() {
    let chart = taps(&[T]);
    let e = play(&chart, tap_at(T - 220 * MS - 1));
    assert_eq!(empty_poors(&e.events), 1);
    // The note is then missed (at this evaluator's miss time).
    assert_eq!(judged_at(&e), vec![(T + MISSED, Poor)]);
}

/// Source: play/JudgeManager.java:594-598: a note is missed once its late
/// BAD edge has passed; a press on the edge still counts
/// (play/JudgeProperty.java:187, inclusive). The POOR's time is
/// [`MISSED`], this evaluator's convention.
#[test]
fn an_unpressed_note_is_a_poor_at_its_late_bad_edge() {
    let chart = taps(&[T]);
    let missed = play(&chart, Vec::new());
    assert_eq!(judged_at(&missed), vec![(T + MISSED, Poor)]);
    let on_edge = play(&chart, tap_at(T + 280 * MS));
    assert_eq!(judged_at(&on_edge), vec![(T + 280 * MS, Bad)]);
    // Just after it: missed, and the press is too far behind the note for
    // an empty POOR (150 ms).
    let after = play(&chart, tap_at(T + 280 * MS + 1));
    assert_eq!(judged_at(&after), vec![(T + MISSED, Poor)]);
    assert_eq!(empty_poors(&after.events), 0);
}

// ------------------------------------------------------------ rank

/// Source: play/JudgeProperty.java:210 (NORMAL rate 75 for #RANK 2),
/// play/BMSPlayerRule.java:62, play/JudgeProperty.java:234 (edge × rate /
/// 100).
#[test]
fn rank_2_scales_every_window_to_75_percent() {
    let chart = taps_ranked(&[T], Some(common::rank(2)));
    for (d, j) in [
        (15 * MS, Perfect),
        (15 * MS + 1, Great),
        (45 * MS, Great),
        (45 * MS + 1, Good),
        (-112_500, Good),
        (-112_501, Bad),
        (-165 * MS, Bad),
        (210 * MS, Bad),
    ] {
        assert_eq!(judge_tap(&chart, d), Some(j), "at {d}");
    }
    assert_eq!(
        judged_at(&play(&chart, Vec::new())),
        vec![(T + 210 * MS, Poor)]
    );
}

/// Source: jbms-parser BMSModel.java:67,71 (rank 2 when none is declared)
/// and BMSDecoder.java:702-713 (an out-of-range #RANK is ignored), then
/// play/BMSPlayerRule.java:62: both scale by 75 %.
#[test]
fn absent_and_unknown_ranks_scale_to_75_percent() {
    for rank in [None, Some(common::rank(7)), Some(common::rank(-1))] {
        let chart = taps_ranked(&[T], rank);
        assert_eq!(judge_tap(&chart, 15 * MS), Some(Perfect), "{rank:?}");
        assert_eq!(judge_tap(&chart, 15 * MS + 1), Some(Great), "{rank:?}");
    }
}

/// Source: jbms-parser BMSDecoder.java:716-727 (`#DEFEXRANK` read with
/// `Integer.parseInt`, so `120.5` throws and the earlier #RANK stays), then
/// play/JudgeProperty.java:210 (#RANK 3, rate 100).
#[test]
fn rejected_defexrank_keeps_the_earlier_rank() {
    let mut chart = taps(&[T]);
    chart
        .meta
        .judgment_headers
        .push(common::defexrank_decimal(120_500));
    assert_eq!(judge_tap(&chart, 20 * MS), Some(Perfect));
    assert_eq!(judge_tap(&chart, 20 * MS + 1), Some(Great));
    assert_eq!(judge_tap(&chart, 280 * MS), Some(Bad));
    assert_eq!(
        judged_at(&play(&chart, Vec::new())),
        vec![(T + MISSED, Poor)]
    );
}

/// Source: play/JudgeProperty.java:210 (rates 25 and 50 for #RANK 0 and 1).
#[test]
fn ranks_0_and_1_scale_to_25_and_50_percent() {
    let very_hard = taps_ranked(&[T], Some(common::rank(0)));
    assert_eq!(judge_tap(&very_hard, 5 * MS), Some(Perfect));
    assert_eq!(judge_tap(&very_hard, 5 * MS + 1), Some(Great));
    assert_eq!(judge_tap(&very_hard, 70 * MS), Some(Bad));
    let hard = taps_ranked(&[T], Some(common::rank(1)));
    assert_eq!(judge_tap(&hard, -10 * MS), Some(Perfect));
    assert_eq!(judge_tap(&hard, -30 * MS - 1), Some(Good));
}

/// Source: play/JudgeProperty.java:210 (`fixjudge` holds MS at 100): the
/// empty POOR distances do not scale with the rank.
#[test]
fn empty_poor_distances_do_not_scale() {
    let chart = taps_ranked(&[T], Some(common::rank(0)));
    let e = play(&chart, tap_at(T - 500 * MS + 1));
    assert_eq!(empty_poors(&e.events), 1);
}

// ------------------------------------------------------------ empty POOR

/// Source: play/JudgeProperty.java:23 (MS early 500000) and
/// play/JudgeManager.java:387-389: the note scan stops at notes 500 ms or
/// more ahead, so a note exactly 500 ms after the press is not reached.
#[test]
fn empty_poor_reaches_up_to_but_not_500_ms_before_a_note() {
    let chart = taps(&[T]);
    assert_eq!(
        empty_poors(&play(&chart, tap_at(T - 500 * MS + 1)).events),
        1
    );
    assert_eq!(empty_poors(&play(&chart, tap_at(T - 500 * MS)).events), 0);
}

/// Source: play/JudgeProperty.java:23 (MS late -150000) and
/// play/JudgeManager.java:400-401 (a judged note still makes an empty
/// POOR).
#[test]
fn empty_poor_reaches_150_ms_after_a_judged_note() {
    let chart = taps(&[T]);
    let at = |after: i64| empty_poors(&play(&chart, taps_at(&[T, T + after])).events);
    assert_eq!(at(150 * MS), 1);
    assert_eq!(at(150 * MS + 1), 0);
}

/// Source: play/JudgeProperty.java:29 (MS keeps the combo),
/// play/JudgeManager.java:660-668, BMSPlayer.java:915 (MS adds to BP),
/// ScoreData.java:278 (MS has its own count), play/GaugeProperty.java:89
/// (NORMAL MS −2 %).
#[test]
fn empty_poor_keeps_the_combo_adds_bp_and_costs_two_percent() {
    let chart = taps(&[T, T + 1_000 * MS]);
    let e = play(&chart, taps_at(&[T, T + 600 * MS, T + 1_000 * MS]));
    let t = &e.tally;
    assert_eq!((t.empty_poor, t.bp, t.counts.poor), (1, 1, 0));
    assert_eq!((t.combo, t.max_combo), (2, 2));
    // Two PERFECTs at TOTAL 260 / 2 notes = +130 % each, capped at 100 %,
    // with −2 % in between.
    let history: Vec<i64> = e.gauges[NORMAL].history.iter().map(|p| p.value.0).collect();
    assert_eq!(history, vec![100 * PCT, 98 * PCT, 100 * PCT]);
}

// ------------------------------------------------------------ score

/// Source: ScoreData.java:421-422 (EX = PG × 2 + GR), play/JudgeProperty.java:29
/// (BAD and POOR break the combo), BMSPlayer.java:915 (BAD and POOR add to
/// BP).
#[test]
fn ex_score_combo_and_bp() {
    let chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS, T + 3_000 * MS]);
    let e = play(&chart, taps_at(&[T, T + 1_040 * MS, T + 2_200 * MS]));
    assert_eq!(
        judgments(&e.events)
            .into_iter()
            .map(|j| j.2)
            .collect::<Vec<_>>(),
        vec![Perfect, Great, Bad, Poor]
    );
    let t = &e.tally;
    assert_eq!((t.ex_score, t.bp, t.combo, t.max_combo), (3, 2, 0, 2));
}

// ------------------------------------------------------------ long notes

/// Source: play/JudgeProperty.java:25 (LN end PG {-120000, 120000}, GR
/// 160000, GD 200000), play/JudgeManager.java:534 (the worse of head and
/// release), :538-541 (an early release past every edge is BAD).
#[test]
fn ln_release_edges_are_120_160_and_200_ms_before_the_end() {
    let length = 1_000 * MS;
    let chart = long(length, Some(common::rank(3)));
    let end = T + length;
    for (before, j) in [
        (120 * MS, Perfect),
        (120 * MS + 1, Great),
        (160 * MS, Great),
        (160 * MS + 1, Good),
        (200 * MS, Good),
        (200 * MS + 1, Bad),
        (900 * MS, Bad),
    ] {
        let e = play(&chart, vec![press(T, 0, 1), release(end - before, 0, 1)]);
        assert_eq!(judged_at(&e), vec![(end - before, j)], "{before} before");
    }
}

/// Source: play/JudgeProperty.java:234 applied to the LN-end table
/// (play/JudgeProperty.java:112, :171): at the absent rank's 75 % the edges
/// are 90, 120 and 150 ms.
#[test]
fn ln_release_edges_scale_with_the_rank() {
    let length = 1_000 * MS;
    let chart = long(length, None);
    let end = T + length;
    for (before, j) in [
        (90 * MS, Perfect),
        (90 * MS + 1, Great),
        (120 * MS + 1, Good),
        (150 * MS + 1, Bad),
    ] {
        let e = play(&chart, vec![press(T, 0, 1), release(end - before, 0, 1)]);
        assert_eq!(judged_at(&e), vec![(end - before, j)], "{before} before");
    }
}

/// Source: play/JudgeManager.java:534: the long note takes the worse of its
/// head's and its release's judgments.
#[test]
fn ln_takes_the_worse_of_head_and_release() {
    let length = 1_000 * MS;
    let chart = long(length, Some(common::rank(3)));
    let end = T + length;
    for (head, before, j) in [
        // A GREAT head, a PERFECT release.
        (40 * MS, 50 * MS, Great),
        // A PERFECT head, a GOOD release.
        (0, 180 * MS, Good),
        // A GOOD head, a GREAT release.
        (-100 * MS, 150 * MS, Good),
    ] {
        let e = play(
            &chart,
            vec![press(T + head, 0, 1), release(end - before, 0, 1)],
        );
        assert_eq!(judged_at(&e), vec![(end - before, j)], "head {head}");
    }
}

/// Source: play/JudgeManager.java:572-573: held past its end, a long note
/// takes its head's judgment at the end.
#[test]
fn ln_held_through_its_end_keeps_the_head_judgment() {
    let length = 500 * MS;
    let chart = long(length, Some(common::rank(3)));
    let e = play(
        &chart,
        vec![press(T + 50 * MS, 0, 1), release(T + 900 * MS, 0, 1)],
    );
    assert_eq!(judged_at(&e), vec![(T + length, Great)]);
    assert_eq!(e.tally.combo, 1);
}

/// Source: play/JudgeManager.java:608-611: a missed head is one POOR, and
/// the long note counts once toward the gauge (jbms-parser
/// TimeLine.java:143-148).
#[test]
fn a_missed_ln_head_is_one_poor() {
    let chart = long(1_000 * MS, Some(common::rank(3)));
    let e = play(&chart, Vec::new());
    assert_eq!(judged_at(&e), vec![(T + MISSED, Poor)]);
    assert_eq!(e.tally.bp, 1);
}

// ------------------------------------------------------------ gauges

/// Three taps 1 s apart with `#TOTAL 90`: a PERFECT or GREAT adds 30 % to
/// the gauges decided at the end, a GOOD 15 %.
fn three_taps() -> Chart {
    let mut chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS]);
    chart.meta.judgment_headers.push(common::total(90_000));
    chart
}

/// Every gauge's value, in the preset's order.
fn all_gauges(e: &Evaluation) -> [i64; 6] {
    [NORMAL, ASSIST_EASY, EASY, HARD, EX_HARD, HAZARD].map(|g| gauge(e, g))
}

/// Source: play/GaugeProperty.java:89 (NORMAL: start 20, border 80; PG and
/// GR 1.0, GD 0.5), play/GrooveGauge.java:260 (increases × TOTAL / notes),
/// :246-248 (cleared at or above the border).
#[test]
fn normal_clears_at_exactly_80_percent() {
    let chart = three_taps();
    // GOOD, GOOD, PERFECT: 20 + 15 + 15 + 30 = 80 %.
    let e = play(
        &chart,
        taps_at(&[T + 100 * MS, T + 1_100 * MS, T + 2_000 * MS]),
    );
    assert_eq!(gauge(&e, NORMAL), 80 * PCT);
    assert!(e.gauges[NORMAL].cleared);
    // GOOD, GOOD, BAD: 20 + 15 + 15 − 3 = 47 %.
    let e = play(
        &chart,
        taps_at(&[T + 100 * MS, T + 1_100 * MS, T + 2_200 * MS]),
    );
    assert_eq!(gauge(&e, NORMAL), 47 * PCT);
    assert!(!e.gauges[NORMAL].cleared);
}

/// Source: play/GaugeProperty.java:87-89 (BAD −1.5 % on ASSIST EASY and
/// EASY, −3 % on NORMAL; borders 60, 80, 80), :90-92 (HARD BAD −5 %, EX
/// HARD −8 %, HAZARD −100 %), play/GrooveGauge.java:218-222 (clamped to
/// max 100 %).
#[test]
fn a_bad_after_two_perfects_on_every_gauge() {
    let e = play(&three_taps(), taps_at(&[T, T + 1_000 * MS, T + 2_200 * MS]));
    assert_eq!(
        all_gauges(&e),
        [77 * PCT, 7_850_000, 7_850_000, 95 * PCT, 92 * PCT, 0]
    );
    let cleared: Vec<bool> = e.gauges.iter().map(|g| g.cleared).collect();
    assert_eq!(cleared, [false, true, false, true, true, false]);
    assert_eq!(e.gauges[HAZARD].failed_at, Some(TimeUs(T + 2_200 * MS)));
}

/// Source: play/GaugeProperty.java:87-92 (POOR: NORMAL −6, ASSIST EASY −3,
/// EASY −4.5, HARD −10, EX HARD −16, HAZARD −100 %).
#[test]
fn a_poor_after_two_perfects_on_every_gauge() {
    let e = play(&three_taps(), taps_at(&[T, T + 1_000 * MS]));
    assert_eq!(
        all_gauges(&e),
        [74 * PCT, 77 * PCT, 7_550_000, 90 * PCT, 84 * PCT, 0]
    );
}

/// Source: play/GaugeProperty.java:89 (NORMAL min 2 %),
/// play/GrooveGauge.java:218-222 (clamped to min).
#[test]
fn normal_floors_at_2_percent() {
    let chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS, T + 3_000 * MS]);
    let e = play(&chart, Vec::new());
    let history: Vec<i64> = e.gauges[NORMAL].history.iter().map(|p| p.value.0).collect();
    assert_eq!(history, vec![14 * PCT, 8 * PCT, 2 * PCT, 2 * PCT]);
}

/// Source: play/GaugeProperty.java:90-92 (PG, GR, GD: HARD +0.15, +0.12,
/// +0.03 %; EX HARD and HAZARD +0.15, +0.06, 0).
#[test]
fn survival_increases_per_judgment() {
    let mut chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS, T + 3_000 * MS]);
    chart.meta.judgment_headers.push(common::total(300_000));
    // POOR, PERFECT, GREAT, GOOD.
    let e = play(
        &chart,
        taps_at(&[T + 1_000 * MS, T + 2_040 * MS, T + 3_100 * MS]),
    );
    assert_eq!(gauge(&e, HARD), 90 * PCT + 15_000 + 12_000 + 3_000);
    assert_eq!(gauge(&e, EX_HARD), 84 * PCT + 15_000 + 6_000);
    assert_eq!(e.gauges[HAZARD].failed_at, Some(TimeUs(T + MISSED)));
}

/// Source: play/GaugeProperty.java:91-92 (EX HARD POOR −16 %, HAZARD
/// POOR −100 %, neither reduced while low), play/GrooveGauge.java:218-222
/// (a gauge at 0 never changes again), :246-248 (a survival gauge clears
/// above 0). HARD's misses are not asserted: below 50 % its damage is
/// reduced there (difference C). The failure times are [`MISSED`] after the
/// note, this evaluator's convention.
#[test]
fn ex_hard_fails_at_the_seventh_poor_and_hazard_at_the_first() {
    let times: Vec<i64> = (0..10).map(|k| T + k * 1_000 * MS).collect();
    let chart = taps(&times);
    let miss = |k: i64| Some(TimeUs(T + k * 1_000 * MS + MISSED));
    let e = play(&chart, Vec::new());
    assert_eq!(e.gauges[EX_HARD].failed_at, miss(6));
    assert_eq!(e.gauges[HAZARD].failed_at, miss(0));
    // A PERFECT on the last note does not bring them back.
    let e = play(&chart, tap_at(T + 9_000 * MS));
    for g in [EX_HARD, HAZARD] {
        assert_eq!(gauge(&e, g), 0, "{g}");
        assert!(!e.gauges[g].cleared, "{g}");
    }
}

/// Source: play/JudgeManager.java:244-246 and play/GrooveGauge.java:63-66
/// (a held mine subtracts its value in percent, not reduced while low),
/// :218-222 (clamped; a gauge at 0 never changes again), :246-248.
#[test]
fn a_lethal_mine_fails_every_survival_gauge_for_good() {
    let chart = with_rank(
        ChartBuilder::new(BPM)
            .mine(lane(1), TimeUs(T), 100)
            .tap(lane(1), TimeUs(T + 1_000 * MS))
            .tap(lane(1), TimeUs(T + 2_000 * MS))
            .build()
            .unwrap(),
        Some(common::rank(3)),
    );
    let mut events = vec![press(T - 100 * MS, 0, 1), release(T + 100 * MS, 0, 1)];
    events.extend(taps_at(&[T + 1_000 * MS, T + 2_000 * MS]));
    let e = play(&chart, events);
    for g in [HARD, EX_HARD, HAZARD] {
        assert_eq!(gauge(&e, g), 0, "{g}");
        assert!(e.gauges[g].failed_at.is_some(), "{g}");
        assert!(!e.gauges[g].cleared, "{g}");
    }
    // NORMAL floors at 2 % and recovers: two PERFECTs of 260 / 2 notes.
    assert_eq!(gauge(&e, NORMAL), 100 * PCT);
}

/// Source: play/GaugeProperty.java:87-92 (MS: NORMAL −2, ASSIST EASY −0.5,
/// EASY −1, HARD −5, EX HARD −8, HAZARD −10 %), play/JudgeManager.java:401.
#[test]
fn empty_poor_damage_on_every_gauge() {
    let chart = taps(&[T]);
    let e = play(&chart, tap_at(T - 400 * MS));
    assert_eq!(empty_poors(&e.events), 1);
    let first: Vec<i64> = e.gauges.iter().map(|g| g.history[0].value.0).collect();
    assert_eq!(
        first,
        [18 * PCT, 1_950_000, 19 * PCT, 95 * PCT, 92 * PCT, 90 * PCT]
    );
}

/// Source: play/JudgeManager.java:244-246 (a mine damages while its lane is
/// held as it passes), play/GrooveGauge.java:63-66 (by its value in
/// percent, not reduced like a judgment), jbms-parser Section.java:513 and
/// MineNote.java (the value is the chart's number).
#[test]
fn a_held_mine_damages_one_percent_per_unit() {
    let chart = with_rank(
        ChartBuilder::new(BPM)
            .mine(lane(1), TimeUs(T), 10)
            .tap(lane(1), TimeUs(T + 1_000 * MS))
            .build()
            .unwrap(),
        Some(common::rank(3)),
    );
    let mut events = vec![press(T - 100 * MS, 0, 1), release(T + 100 * MS, 0, 1)];
    events.extend(tap_at(T + 1_000 * MS));
    let e = play(&chart, events);
    let first: Vec<i64> = e.gauges.iter().map(|g| g.history[0].value.0).collect();
    assert_eq!(
        first,
        [10 * PCT, 10 * PCT, 10 * PCT, 90 * PCT, 90 * PCT, 90 * PCT]
    );
    // Not held as it passes: no damage.
    let e = play(&chart, tap_at(T + 1_000 * MS));
    assert_eq!(gauge(&e, HARD), 100 * PCT);
}

/// Source: play/BMSPlayerRule.java:75-78, :85 and jbms-parser
/// BMSModel.java:75,79: without `#TOTAL` it is at least 260, the value used
/// here (the preset module's E).
#[test]
fn absent_total_is_260() {
    // 260 % over 4 notes: a GOOD adds 32.5 %.
    let chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS, T + 3_000 * MS]);
    let e = play(&chart, tap_at(T + 100 * MS));
    assert_eq!(e.gauges[NORMAL].history[0].value.0, 5_250_000);
}

/// Source: jbms-parser BMSDecoder.java:730-743 (`if (total > 0)`, else a
/// warning and the earlier `#TOTAL` stays). The custom preset reads the same
/// declarations as loader version 6 did: the later `#TOTAL 0` counts, and a
/// `#TOTAL` that is not positive leaves its default, 160.
#[test]
fn zero_total_after_a_total() {
    let mut chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS, T + 3_000 * MS]);
    chart
        .meta
        .judgment_headers
        .extend([common::total(200_000), common::total(0)]);
    // 200 % over 4 notes: a PERFECT adds 50 %.
    let e = play(&chart, tap_at(T));
    assert_eq!(e.gauges[NORMAL].history[0].value.0, 70 * PCT);
    // Custom: 160 % over 4 notes adds 40 %.
    let input = EvalInput {
        events: tap_at(T),
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: None,
    };
    let custom = evaluate(&chart, &input, &rules::custom()).unwrap();
    assert_eq!(custom.gauges[0].history[0].value.0, 60 * PCT);
}

// ------------------------------------------------------------ lane classes

/// A tap on the scratch and one on key 1, both at `T`, at `rank`.
fn scratch_and_key(rank: Option<JudgmentHeader>) -> Chart {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(0), TimeUs(T))
        .tap(lane(1), TimeUs(T))
        .build()
        .unwrap();
    with_rank(chart, rank)
}

/// Presses on the scratch and key 1 at `at`, each released 10 µs later.
fn both_at(at: i64) -> Vec<PhysicalInputEvent> {
    vec![
        press(at, 0, 0),
        press(at, 0, 1),
        release(at + 10, 0, 0),
        release(at + 10, 0, 1),
    ]
}

/// The judgments on lane `l`, in order.
fn judged_on(e: &Evaluation, l: usize) -> Vec<Judgment> {
    judgments(&e.events)
        .into_iter()
        .filter(|j| j.0 == l)
        .map(|j| j.2)
        .collect()
}

/// Source: play/JudgeProperty.java:24 (scratch notes: PG {-30000, 30000},
/// BD {-290000, 230000}, MS {-160000, 500000}), :27 (scratch LN ends: PG
/// 130 ms), :191-197 (the table per note type), and
/// play/JudgeManager.java:380, :494, :595 (a scratch press, release and
/// miss read the scratch's). At the absent rank's 75 %, the scratch's
/// PERFECT is ±22.5 ms, its late BAD edge 217.5 ms and its LN-end PERFECT
/// 97.5 ms, against the keys' 15, 210 and 90 ms; its empty POOR reaches
/// 160 ms behind (not scaled), the keys' 150 ms.
#[test]
fn scratch_uses_its_own_windows() {
    let chart = scratch_and_key(None);
    // A note pressed 20 ms late.
    let e = play(&chart, both_at(T + 20 * MS));
    assert_eq!(judged_on(&e, 0), [Perfect]);
    assert_eq!(judged_on(&e, 1), [Great]);
    // Its miss: 215 ms late is inside the scratch's late BAD edge and past
    // the keys'.
    let e = play(&chart, both_at(T + 215 * MS));
    assert_eq!(judged_on(&e, 0), [Bad]);
    assert_eq!(judged_on(&e, 1), [Poor]);
    // An empty POOR 155 ms after a judged note.
    let mut presses = both_at(T);
    presses.extend(both_at(T + 155 * MS));
    let e = play(&chart, presses);
    let empty: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::EmptyPoor { lane } => Some(lane.index()),
            _ => None,
        })
        .collect();
    assert_eq!(empty, [0]);
    // A long note's end, released 95 ms before it.
    let end = T + 1_000 * MS;
    let chart = ChartBuilder::new(BPM)
        .long(lane(0), TimeUs(T), TimeUs(end))
        .long(lane(1), TimeUs(T), TimeUs(end))
        .build()
        .unwrap();
    let held = vec![
        press(T, 0, 0),
        press(T, 0, 1),
        release(end - 95 * MS, 0, 0),
        release(end - 95 * MS, 0, 1),
    ];
    let e = play(&with_rank(chart, None), held);
    assert_eq!(judged_on(&e, 0), [Perfect]);
    assert_eq!(judged_on(&e, 1), [Great]);
}

/// Source: play/BMSPlayerRule.java:62 (`#RANK 4` takes NORMAL's fifth rate,
/// 125, play/JudgeProperty.java:210) and :63 (`#DEFEXRANK n` is
/// `n × 75 / 100` in `int` arithmetic, 75 being `#RANK 2`'s rate), then
/// each edge `edge × rate / 100` (play/JudgeProperty.java:234).
#[test]
fn defexrank_and_rank_4_scale_like_the_source() {
    // The keys' PERFECT edge (20 ms) at 125, 75, 75 (75.75 floored), 76 and
    // 150 %.
    for (rank, edge) in [
        (common::rank(4), 25_000),
        (common::defexrank(100), 15_000),
        (common::defexrank(101), 15_000),
        (common::defexrank(102), 15_200),
        (common::defexrank(200), 30_000),
    ] {
        let chart = taps_ranked(&[T], Some(rank));
        assert_eq!(judge_tap(&chart, edge), Some(Perfect), "{rank:?}");
        assert_eq!(judge_tap(&chart, edge + 1), Some(Great), "{rank:?}");
    }
    // The scratch's PERFECT (30 ms) at #RANK 4: 37.5 ms, where the keys'
    // is 25 ms.
    let chart = scratch_and_key(Some(common::rank(4)));
    let e = play(&chart, both_at(T + 37_500));
    assert_eq!(
        (judged_on(&e, 0), judged_on(&e, 1)),
        (vec![Perfect], vec![Great])
    );
    let e = play(&chart, both_at(T + 37_501));
    assert_eq!(judged_on(&e, 0), [Great]);
}

/// Source: play/BMSPlayerRule.java:63 (`1 × 75 / 100` is 0 in `int`
/// arithmetic) and play/JudgeProperty.java:234 (every edge `× 0 / 100`):
/// zero-width windows, and the chart plays. The empty POOR range is not
/// scaled (:210).
#[test]
fn defexrank_1_gives_zero_width_windows() {
    let chart = scratch_and_key(Some(common::defexrank(1)));
    let e = play(&chart, both_at(T));
    assert_eq!(
        (judged_on(&e, 0), judged_on(&e, 1)),
        (vec![Perfect], vec![Perfect])
    );
    // 1 µs off either way judges no note: each note is missed at its own
    // time, and each press is an empty POOR.
    for d in [-1, 1] {
        let e = play(&chart, both_at(T + d));
        assert_eq!(judged_at(&e), [(T, Poor), (T, Poor)], "at {d}");
        assert_eq!(empty_poors(&e.events), 2, "at {d}");
    }
}

/// Source: play/JudgeProperty.java:234: each edge scales on its own, so
/// flooring can tie two of them. Only a preset's own table must ascend
/// (design.md "Evaluation Stages").
#[test]
fn derived_edges_may_tie() {
    let style = rules::beatoraja_style();
    let dev = device(&[0]);
    // #DEFEXRANK 1: every edge 0.
    let chart = scratch_and_key(Some(common::defexrank(1)));
    let ev = Evaluator::new(&chart, &style, &dev, &InputStart::default(), None, 0).unwrap();
    for l in [lane(0), lane(1)] {
        let t = ev.table(l).table;
        assert_eq!(t.windows.perfect, Window::symmetric(0));
        assert_eq!(t.windows.bad, Window::symmetric(0));
        assert_eq!(t.ln_end.good, TimeUs(0));
    }
    // Edges of 1 to 4 µs at #RANK 0 (25 %): PERFECT, GREAT and GOOD floor
    // to 0, BAD to 1.
    let mut tiny = style.clone();
    tiny.tables.keys.windows = JudgeWindows {
        perfect: Window::symmetric(1),
        great: Window::symmetric(2),
        good: Window::symmetric(3),
        bad: Window::symmetric(4),
    };
    tiny.tables.keys.ln_end = LnEndWindows {
        perfect: TimeUs(1),
        great: TimeUs(2),
        good: TimeUs(3),
    };
    assert_eq!(tiny.validate(), Ok(()));
    let chart = taps_ranked(&[T], Some(common::rank(0)));
    let judge = |at: i64| {
        let input = EvalInput {
            events: tap_at(at),
            device: device(&[0]),
            start: InputStart::default(),
            finalized: None,
            section: None,
        };
        let e = evaluate(&chart, &input, &tiny).unwrap();
        judgments(&e.events).first().map(|j| j.2)
    };
    assert_eq!(judge(T), Some(Perfect));
    assert_eq!(judge(T + 1), Some(Bad));
    // A preset's own table that does not ascend is refused.
    let mut tied = style.clone();
    tied.tables.scratch.windows.great = tied.tables.scratch.windows.perfect;
    let refused = RuleError::WindowsNotAscending {
        class: LaneClass::Scratch,
        side: Side::Early,
        judgment: Great,
    };
    assert_eq!(tied.validate(), Err(refused.clone()));
    assert_eq!(
        Evaluator::new(&chart, &tied, &dev, &InputStart::default(), None, 0).map(|_| ()),
        Err(EvalError::InvalidPreset(refused))
    );
}

/// Source: play/JudgeManager.java:189-196 (one envelope, from the farthest
/// late edge to the farthest early edge of the scaled note and scratch
/// tables with their MS pairs) and :387-392 (the scan stops at a note that
/// far ahead, excluded), with the MS pairs' 500 ms early edge
/// (play/JudgeProperty.java:23-24, inclusive at play/JudgeManager.java:401).
/// At `#DEFEXRANK 291` (218 %) the scratch's early BAD edge is 501.4 ms,
/// past 500 ms; at `#DEFEXRANK 290` (217 %) it is 499.1 ms.
#[test]
fn scan_envelope_follows_the_scaled_tables() {
    // A judged scratch note exactly 500 ms ahead: the first press judges it
    // BAD 501 ms early, the second is 500 ms before it.
    let scratch = |n| {
        let chart = ChartBuilder::new(BPM)
            .tap(lane(0), TimeUs(T))
            .build()
            .unwrap();
        with_rank(chart, Some(common::defexrank(n)))
    };
    let presses = vec![
        press(T - 501 * MS, 0, 0),
        release(T - 501 * MS + 10, 0, 0),
        press(T - 500 * MS, 0, 0),
        release(T - 500 * MS + 10, 0, 0),
    ];
    let e = play(&scratch(291), presses.clone());
    assert_eq!(judged_at(&e), [(T - 501 * MS, Bad)]);
    assert_eq!(empty_poors(&e.events), 1);
    // Below that scale no press 500 ms early or more judges a note, and the
    // scan stops short of a note 500 ms ahead.
    let e = play(&scratch(290), presses);
    assert!(judgments(&e.events).iter().all(|j| j.3.is_none()));
    assert_eq!(empty_poors(&e.events), 0);
    // One envelope spans both tables: an unjudged key note exactly 500 ms
    // ahead, past the keys' early BAD edge (479.6 ms at 218 %), is an empty
    // POOR once the scratch's edge widens the envelope.
    let key = |n| taps_ranked(&[T], Some(common::defexrank(n)));
    assert_eq!(
        empty_poors(&play(&key(291), tap_at(T - 500 * MS)).events),
        1
    );
    assert_eq!(
        empty_poors(&play(&key(290), tap_at(T - 500 * MS)).events),
        0
    );
}

/// Source: play/JudgeManager.java:387-389 (`dmtime >= mjudgeend` ends the
/// scan) with the envelope of :189-196. At `#DEFEXRANK 291` (218 %) the
/// scratch's early BAD edge, 501.4 ms, is the envelope's far end, so a
/// scratch note exactly that far ahead is not reached and 1 µs nearer is
/// BAD; it is past the 500 ms empty POOR range too. The keys' early BAD
/// edge, 479.6 ms, lies inside the envelope and stays inclusive.
#[test]
fn an_early_bad_edge_at_the_envelope_end_is_not_reached() {
    let chart = scratch_and_key(Some(common::defexrank(291)));
    let first = |presses: Vec<PhysicalInputEvent>| {
        let e = play(&chart, presses);
        let pressed: Vec<_> = judgments(&e.events)
            .into_iter()
            .filter(|j| j.3.is_some())
            .map(|j| (j.0, j.2, j.3))
            .collect();
        (pressed, empty_poors(&e.events))
    };
    let scratch_at = |d: i64| vec![press(T + d, 0, 0), release(T + d + 10, 0, 0)];
    assert_eq!(first(scratch_at(-501_400)), (Vec::new(), 0));
    assert_eq!(
        first(scratch_at(-501_399)),
        (vec![(0, Bad, Some(-501_399))], 0)
    );
    let key_at = vec![press(T - 479_600, 0, 1), release(T - 479_590, 0, 1)];
    assert_eq!(first(key_at), (vec![(1, Bad, Some(-479_600))], 0));
}

/// Known difference (the preset module): the source plays any scale its
/// `int` arithmetic gives (play/BMSPlayerRule.java:63), while every edge
/// here stays within 2^32 µs. `#DEFEXRANK 1974698` scales by 1 481 023 %,
/// taking the scratch's 290 ms late edge to 4 294 966 700 µs; `1974699`
/// scales by 1 481 024 % and passes 2^32 µs.
#[test]
fn a_defexrank_past_2_pow_32_is_refused() {
    let style = rules::beatoraja_style();
    let dev = device(&[0]);
    let late = |n| {
        let chart = taps_ranked(&[T], Some(common::defexrank(n)));
        Evaluator::new(&chart, &style, &dev, &InputStart::default(), None, 0)
            .map(|ev| ev.table(Lane::SCRATCH).table.windows.bad.late)
    };
    assert_eq!(late(1_974_698), Ok(TimeUs(4_294_966_700)));
    assert_eq!(late(1_974_699), Err(EvalError::InvalidRank));
}
