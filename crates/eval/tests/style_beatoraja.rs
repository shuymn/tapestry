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
//! Window values are at `#RANK 3` (100 %) unless a test says otherwise.
//! Gauges are in 1/100000 %, in the preset's order: NORMAL, ASSIST EASY,
//! EASY, HARD, EX HARD, HAZARD.

mod common;

use bms_model::{Chart, ChartBuilder, JudgeRank, TimeUs};
use common::{MS, device, empty_poors, judgments, lane, press, release};
use eval::{EvalInput, Evaluation, InputStart, JudgeEventKind, PhysicalInputEvent, evaluate};
use rules::Judgment::{self, Bad, Good, Great, Perfect, Poor};

const BPM: i64 = 150_000;
/// The first note's time.
const T: i64 = 1_000 * MS;
/// One percent of a gauge.
const PCT: i64 = 100_000;

const NORMAL: usize = 0;
const ASSIST_EASY: usize = 1;
const EASY: usize = 2;
const HARD: usize = 3;
const EX_HARD: usize = 4;
const HAZARD: usize = 5;

fn with_rank(mut chart: Chart, rank: Option<JudgeRank>) -> Chart {
    chart.meta.rank = rank;
    chart
}

/// Taps on key 1 at `times` (µs), at `#RANK 3`.
fn taps(times: &[i64]) -> Chart {
    taps_ranked(times, Some(JudgeRank::Rank(3)))
}

fn taps_ranked(times: &[i64], rank: Option<JudgeRank>) -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for &t in times {
        b = b.tap(lane(1), TimeUs(t));
    }
    with_rank(b.build().unwrap(), rank)
}

/// One long note on key 1 from `T` lasting `length`, at `rank`.
fn long(length: i64, rank: Option<JudgeRank>) -> Chart {
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
    // The note is then missed at its late BAD edge.
    assert_eq!(judged_at(&e), vec![(T + 280 * MS, Poor)]);
}

/// Source: play/JudgeManager.java:594-598: a note is missed once the time
/// passes its late BAD edge; a press on the edge still counts
/// (play/JudgeProperty.java:187, inclusive).
#[test]
fn an_unpressed_note_is_a_poor_at_its_late_bad_edge() {
    let chart = taps(&[T]);
    let missed = play(&chart, Vec::new());
    assert_eq!(judged_at(&missed), vec![(T + 280 * MS, Poor)]);
    let on_edge = play(&chart, tap_at(T + 280 * MS));
    assert_eq!(judged_at(&on_edge), vec![(T + 280 * MS, Bad)]);
    // Just after it: missed, and the press is too far behind the note for
    // an empty POOR (150 ms).
    let after = play(&chart, tap_at(T + 280 * MS + 1));
    assert_eq!(judged_at(&after), vec![(T + 280 * MS, Poor)]);
    assert_eq!(empty_poors(&after.events), 0);
}

// ------------------------------------------------------------ rank

/// Source: play/JudgeProperty.java:210 (NORMAL rate 75 for #RANK 2),
/// play/BMSPlayerRule.java:62, play/JudgeProperty.java:234 (edge × rate /
/// 100).
#[test]
fn rank_2_scales_every_window_to_75_percent() {
    let chart = taps_ranked(&[T], Some(JudgeRank::Rank(2)));
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
    for rank in [None, Some(JudgeRank::Rank(7)), Some(JudgeRank::Rank(-1))] {
        let chart = taps_ranked(&[T], rank);
        assert_eq!(judge_tap(&chart, 15 * MS), Some(Perfect), "{rank:?}");
        assert_eq!(judge_tap(&chart, 15 * MS + 1), Some(Great), "{rank:?}");
    }
}

/// Source: play/JudgeProperty.java:210 (rates 25 and 50 for #RANK 0 and 1).
#[test]
fn ranks_0_and_1_scale_to_25_and_50_percent() {
    let very_hard = taps_ranked(&[T], Some(JudgeRank::Rank(0)));
    assert_eq!(judge_tap(&very_hard, 5 * MS), Some(Perfect));
    assert_eq!(judge_tap(&very_hard, 5 * MS + 1), Some(Great));
    assert_eq!(judge_tap(&very_hard, 70 * MS), Some(Bad));
    let hard = taps_ranked(&[T], Some(JudgeRank::Rank(1)));
    assert_eq!(judge_tap(&hard, -10 * MS), Some(Perfect));
    assert_eq!(judge_tap(&hard, -30 * MS - 1), Some(Good));
}

/// Source: play/JudgeProperty.java:210 (`fixjudge` holds MS at 100): the
/// empty POOR distances do not scale with the rank.
#[test]
fn empty_poor_distances_do_not_scale() {
    let chart = taps_ranked(&[T], Some(JudgeRank::Rank(0)));
    let e = play(&chart, tap_at(T - 500 * MS));
    assert_eq!(empty_poors(&e.events), 1);
}

// ------------------------------------------------------------ empty POOR

/// Source: play/JudgeProperty.java:23 (MS early 500000),
/// play/JudgeManager.java:401.
#[test]
fn empty_poor_reaches_500_ms_before_a_note() {
    let chart = taps(&[T]);
    assert_eq!(empty_poors(&play(&chart, tap_at(T - 500 * MS)).events), 1);
    assert_eq!(
        empty_poors(&play(&chart, tap_at(T - 500 * MS - 1)).events),
        0
    );
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
    let chart = long(length, Some(JudgeRank::Rank(3)));
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
    let chart = long(length, Some(JudgeRank::Rank(3)));
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
    let chart = long(length, Some(JudgeRank::Rank(3)));
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
    let chart = long(1_000 * MS, Some(JudgeRank::Rank(3)));
    let e = play(&chart, Vec::new());
    assert_eq!(judged_at(&e), vec![(T + 280 * MS, Poor)]);
    assert_eq!(e.tally.bp, 1);
}

// ------------------------------------------------------------ gauges

/// Three taps 1 s apart with `#TOTAL 90`: a PERFECT or GREAT adds 30 % to
/// the gauges decided at the end, a GOOD 15 %.
fn three_taps() -> Chart {
    let mut chart = taps(&[T, T + 1_000 * MS, T + 2_000 * MS]);
    chart.meta.total_milli = Some(90_000);
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
    chart.meta.total_milli = Some(300_000);
    // POOR, PERFECT, GREAT, GOOD.
    let e = play(
        &chart,
        taps_at(&[T + 1_000 * MS, T + 2_040 * MS, T + 3_100 * MS]),
    );
    assert_eq!(gauge(&e, HARD), 90 * PCT + 15_000 + 12_000 + 3_000);
    assert_eq!(gauge(&e, EX_HARD), 84 * PCT + 15_000 + 6_000);
    assert_eq!(e.gauges[HAZARD].failed_at, Some(TimeUs(T + 280 * MS)));
}

/// Source: play/GaugeProperty.java:90-91 (HARD POOR −10 %, EX HARD −16 %),
/// play/GrooveGauge.java:218-222 (a gauge at 0 never changes again),
/// :246-248 (a survival gauge clears above 0).
#[test]
fn hard_fails_at_the_tenth_poor_and_ex_hard_at_the_seventh() {
    let times: Vec<i64> = (0..10).map(|k| T + k * 1_000 * MS).collect();
    let chart = taps(&times);
    let miss = |k: i64| Some(TimeUs(T + k * 1_000 * MS + 280 * MS));
    let e = play(&chart, Vec::new());
    assert_eq!(e.gauges[HARD].failed_at, miss(9));
    assert_eq!(e.gauges[EX_HARD].failed_at, miss(6));
    assert_eq!(e.gauges[HAZARD].failed_at, miss(0));
    // Nine POORs, then a PERFECT: HARD survives at 10.15 %.
    let e = play(&chart, tap_at(T + 9_000 * MS));
    assert_eq!(gauge(&e, HARD), 10 * PCT + 15_000);
    assert!(e.gauges[HARD].cleared);
    assert!(!e.gauges[EX_HARD].cleared);
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
        Some(JudgeRank::Rank(3)),
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
