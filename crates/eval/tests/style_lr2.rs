//! Hand-written examples of the LR2-style preset (`rules::lr2_style()`),
//! each naming the public source it follows: LR2oraja Endless Dream
//! (<https://github.com/seraxis/lr2oraja-endlessdream> at `d4995153`, under
//! `core/src/bms/player/beatoraja/`, cited as `file:line`). The preset's
//! module (crates/rules/src/lr2_style.rs) lists every value with its source
//! and the known differences.
//!
//! | Value | Source |
//! | --- | --- |
//! | Note windows at 100 %: PERFECT ±21, GREAT ±60, GOOD ±120, BAD ±200 ms, keys and scratch alike | `play/JudgeProperty.java:54-55` |
//! | LN-end edges PERFECT, GREAT and GOOD all 120 ms, keys and scratch alike; earlier is BAD | `play/JudgeProperty.java:56`, `:58`; `play/JudgeManager.java:580-594` |
//! | `#RANK` 0-4: 25, 50, 75, 100, 75 %; none: 75 %; `#DEFEXRANK n`: `n × 75 / 100` % | `play/JudgeProperty.java:169`, `play/BMSPlayerRule.java:62-65` |
//! | PERFECT, GREAT and GOOD interpolated below 100 %, linear from it, then clamped to the next wider; BAD not scaled | `play/JudgeProperty.java:185-249` |
//! | Empty POOR: a note 0 to 1000 ms after the press, judged or not, none behind it; not scaled | `play/JudgeProperty.java:54-55`, `play/JudgeManager.java:414-442` |
//! | Empty POOR keeps the combo without adding to it, adds to BP, has its own count | `play/JudgeProperty.java:60`, `play/JudgeManager.java:716-725` |
//! | Note assignment: combo priority ([`Assignment::ComboPriority`]) | `PlayConfig.java:103`, the candidate loop `play/JudgeManager.java:414-455` |
//! | Missed once its late BAD edge has passed, taps and LN heads alike | `play/JudgeManager.java:640-660` |
//! | Gauges `{min, max, start, clear, death}` and changes `{PG, GR, GD, BD, PR, MS}` | `play/GaugeProperty.java:117-122`, field order `:161` |
//! | ASSIST EASY, EASY and NORMAL increases × `TOTAL / notes` | `play/GrooveGauge.java:263` (`TOTAL` modifier), `play/GaugeProperty.java:117-119` |
//! | HARD and EX HARD damage × `max(fix1, fix2)` ([`TotalCorrection::ScaleDamage`]), MS included | `play/GrooveGauge.java:279-309` (`MODIFY_DAMAGE`), `play/GaugeProperty.java:120-121`; once per chart, `play/GrooveGauge.java:207-211` |
//! | HARD damage × 0.6 below 32 % | `play/GaugeProperty.java:120`, `play/GrooveGauge.java:232-243` |
//! | A survival gauge a change leaves strictly below 2 % drops to 0, a mine's change included | `play/GrooveGauge.java:218-225`, `play/GaugeProperty.java:120-122` (death 2); a mine changes the gauge by `setValue`, `play/GrooveGauge.java:63-66`, `play/JudgeManager.java:266-269` |
//! | `#TOTAL` absent ([`DefaultTotal::NotesLinear`]): `160 + (N + clamp(N − 400, 0, 200)) × 0.16` over the whole chart's notes | `play/BMSPlayerRule.java:73-79`, `:88-89` |
//!
//! Gauges are in 1/100000 %, in the preset's order: NORMAL, ASSIST EASY,
//! EASY, HARD, EX HARD, HAZARD. The gauge arithmetic is this evaluator's:
//! each change exact until one rounding down to 1/100000 %, where the source
//! computes in `f32` (a known difference every target shares, design.md
//! "What a -style Preset Reproduces").
//!
//! [`Assignment::ComboPriority`]: rules::Assignment::ComboPriority
//! [`TotalCorrection::ScaleDamage`]: rules::TotalCorrection::ScaleDamage
//! [`DefaultTotal::NotesLinear`]: rules::DefaultTotal::NotesLinear

mod common;

use bms_model::{Chart, ChartBuilder, JudgmentHeader, Lane, TimeUs};
use common::{MS, both_at, device, empty_poors, judged_on, lane, press, release};
use eval::{
    EvalError, EvalInput, Evaluation, Evaluator, InputStart, JudgeEventKind, PhysicalInputEvent,
    Section, evaluate,
};
use rules::Judgment::{Bad, Good, Great, Perfect, Poor};
use rules::{Assignment, GaugeValue, Judgment, RulePreset};

const BPM: i64 = 150_000;
/// One percent of a gauge.
const PCT: i64 = GaugeValue::PERCENT;
/// The one target of [`from_values`]'s section.
const TARGET: i64 = 11_000 * MS;
/// A BAD: 200 ms late at `#RANK 3`.
const BAD_AT: i64 = TARGET + 200 * MS;

/// The first note of the press examples.
const T: i64 = 1_000 * MS;
/// When this evaluator decides a miss: at the late BAD edge, 200 ms.
const MISSED: i64 = 200 * MS;

const NORMAL: usize = 0;
const HARD: usize = 3;
const EX_HARD: usize = 4;
const HAZARD: usize = 5;

fn preset() -> RulePreset {
    rules::lr2_style()
}

/// A chart of `n` judgeable notes, at `#RANK 3`: a tap on key 1 at
/// [`TARGET`], the one note of [`from_values`]'s section, and `n − 1` taps on
/// key 3 from 30 s, 10 ms apart, which the section does not reach; with
/// `#TOTAL` in thousandths when given, and a mine of `mine` units on key 2
/// at [`TARGET`] when given.
fn one_target(n: i64, total: Option<i64>, mine: Option<i32>) -> Chart {
    let mut b = ChartBuilder::new(BPM).tap(lane(1), TimeUs(TARGET));
    if let Some(damage) = mine {
        b = b.mine(lane(2), TimeUs(TARGET), damage);
    }
    for k in 0..n - 1 {
        b = b.tap(lane(3), TimeUs(30_000 * MS + k * 10 * MS));
    }
    let mut chart = b.build().unwrap();
    chart.meta.judgment_headers = vec![common::rank(3)];
    chart.meta.judgment_headers.extend(total.map(common::total));
    chart
}

/// The section [10 s, 20 s) of `chart` under `preset`, from its start
/// values with each `(gauge, value)` of `set` replacing one, with `events`
/// in time order.
fn from_values(
    preset: &RulePreset,
    chart: &Chart,
    set: &[(usize, i64)],
    mut events: Vec<PhysicalInputEvent>,
) -> Evaluation {
    events.sort_by_key(|e| e.received);
    let mut initial: Vec<GaugeValue> = preset.gauges.iter().map(|g| g.start).collect();
    for &(g, v) in set {
        initial[g] = GaugeValue(v);
    }
    let input = EvalInput {
        events,
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: Some(Section {
            start: TimeUs(10_000 * MS),
            end: TimeUs(20_000 * MS),
            lead_in: TimeUs(2_000 * MS),
            initial_gauges: initial,
        }),
    };
    let e = evaluate(chart, &input, preset).unwrap();
    assert!(e.completed);
    e
}

/// A press on key 1 at `at`, released 10 µs later.
fn tap_at(at: i64) -> Vec<PhysicalInputEvent> {
    vec![press(at, 0, 1), release(at + 10, 0, 1)]
}

/// Key 2 held across the mine at [`TARGET`].
fn hold_mine() -> Vec<PhysicalInputEvent> {
    vec![
        press(TARGET - 100 * MS, 0, 2),
        release(TARGET + 100 * MS, 0, 2),
    ]
}

fn gauge(e: &Evaluation, g: usize) -> i64 {
    e.gauges[g].value.0
}

/// The one judgment of the section's note.
fn judged(e: &Evaluation) -> Vec<Judgment> {
    common::judgments(&e.events)
        .into_iter()
        .map(|j| j.2)
        .collect()
}

/// Source: play/BMSPlayerRule.java:73-79 (a `#TOTAL` not above 0 takes the
/// default), :88-89 (`160 + (N + clamp(N − 400, 0, 200)) × 0.16`),
/// play/GrooveGauge.java:263 (NORMAL's PERFECT × `TOTAL / notes`). The
/// default is the whole chart's, here read through NORMAL's PERFECT from
/// 2 %: 0.16 per note, twice that from the 401st to the 600th.
#[test]
fn lr2_default_total_matches_the_source() {
    let preset = preset();
    for (n, increase) in [
        // 160.16 % for one note: clamped to 100 %.
        (1, 98 * PCT),
        // 223.84, 224, 224.32 %.
        (399, 56_100),
        (400, 56_000),
        (401, 55_940),
        // 287.68, 288, 288.16 %.
        (599, 48_026),
        (600, 48_000),
        (601, 47_946),
        (1_000, 35_200),
    ] {
        let e = from_values(
            &preset,
            &one_target(n, None, None),
            &[(NORMAL, 2 * PCT)],
            tap_at(TARGET),
        );
        assert_eq!(judged(&e), [Perfect]);
        assert_eq!(gauge(&e, NORMAL) - 2 * PCT, increase, "{n} notes");
    }
    // A declared #TOTAL counts instead: 300 over 1000 notes.
    let e = from_values(
        &preset,
        &one_target(1_000, Some(300_000), None),
        &[(NORMAL, 2 * PCT)],
        tap_at(TARGET),
    );
    assert_eq!(gauge(&e, NORMAL), 2 * PCT + 30_000);
    // No notes: the chart is refused before any default is taken.
    let mines = ChartBuilder::new(BPM)
        .mine(lane(2), TimeUs(TARGET), 1)
        .build()
        .unwrap();
    let dev = device(&[0]);
    assert_eq!(
        Evaluator::new(&mines, &preset, &dev, &InputStart::default(), None, 0).map(|_| ()),
        Err(EvalError::NoJudgeableNotes)
    );
}

/// Source: play/GrooveGauge.java:279-309 (`MODIFY_DAMAGE`: a negative value
/// × `max(fix1, fix2)`, `fix1 = 10 / min(10, max(1, floor(TOTAL / 16) −
/// 5))`, `fix2` by note count), :207-211 (on each of the six values once
/// per chart), play/GaugeProperty.java:121 (EX HARD BAD −12 %, MS −2 %).
#[test]
fn lr2_damage_correction_matches_the_source() {
    let preset = preset();
    let bad = |n, total| {
        let e = from_values(
            &preset,
            &one_target(n, Some(total), None),
            &[],
            tap_at(BAD_AT),
        );
        assert_eq!(judged(&e), [Bad]);
        gauge(&e, EX_HARD)
    };
    // EX HARD's BAD from 100 %, × the factor.
    for (n, total, after) in [
        // fix1 alone (fix2 is 1 from 1000 notes): 1 from #TOTAL 240, 10/9
        // just below, 5 from 112, 10 just below 112.
        (1_000, 240_000, 88 * PCT),
        (1_000, 239_999, 8_666_666),
        (1_000, 112_000, 40 * PCT),
        (1_000, 111_999, 0),
        // fix2 alone (fix1 is 1 from #TOTAL 240): 10 up to 20 notes, 5 at
        // 60, 4 at 125, 3 at 250, 2.4 at 400, 2 at 500, 1.002 at 999.
        (20, 300_000, 0),
        (60, 300_000, 40 * PCT),
        (125, 300_000, 52 * PCT),
        (250, 300_000, 64 * PCT),
        (400, 300_000, 7_120_000),
        (500, 300_000, 76 * PCT),
        (999, 300_000, 8_797_600),
        // The larger: 2.4 over 10/9, and 10/9 over 1.002.
        (400, 224_000, 7_120_000),
        (999, 224_000, 8_666_666),
    ] {
        assert_eq!(bad(n, total), after, "{n} notes, #TOTAL {total}");
    }
    // The empty POOR's −2 % too, × 10 at 20 notes; HAZARD has no
    // correction, so its −10 % stays.
    let chart = one_target(20, Some(300_000), None);
    let e = from_values(&preset, &chart, &[], tap_at(TARGET - 400 * MS));
    let first = |g: usize| e.gauges[g].history[0].value.0;
    assert_eq!([first(EX_HARD), first(HAZARD)], [80 * PCT, 90 * PCT]);
    // An increase is not corrected: +0.1 % from 50 %.
    let e = from_values(&preset, &chart, &[(EX_HARD, 50 * PCT)], tap_at(TARGET));
    assert_eq!(gauge(&e, EX_HARD), 50 * PCT + 10_000);
}

/// Source: play/GrooveGauge.java:218-225 (`setValue`: clamped, then 0 when
/// strictly below `death`), play/GaugeProperty.java:120-122 (death 2 on
/// HARD, EX HARD and HAZARD); a mine reaches it through `addValue`
/// (:63-66, play/JudgeManager.java:266-269). With 1000 notes and `#TOTAL
/// 240` the correction is 1.
#[test]
fn survival_below_two_percent_fails_including_mines() {
    let preset = preset();
    let chart = one_target(1_000, Some(240_000), None);
    // EX HARD's BAD (−12 %) from 14 % leaves exactly 2 % and survives; from
    // 13.99999 % it leaves 1.99999 % and fails at 0.
    let e = from_values(&preset, &chart, &[(EX_HARD, 14 * PCT)], tap_at(BAD_AT));
    assert_eq!(gauge(&e, EX_HARD), 2 * PCT);
    assert_eq!(e.gauges[EX_HARD].failed_at, None);
    assert!(e.gauges[EX_HARD].cleared);
    let e = from_values(&preset, &chart, &[(EX_HARD, 1_399_999)], tap_at(BAD_AT));
    assert_eq!(gauge(&e, EX_HARD), 0);
    assert_eq!(e.gauges[EX_HARD].reference.0, 199_999);
    assert_eq!(e.gauges[EX_HARD].failed_at, Some(TimeUs(BAD_AT)));
    assert!(!e.gauges[EX_HARD].cleared);
    // HARD below 32 %: −6 % × 0.6 from 5.6 % leaves 2 %; from 5.59999 %,
    // 1.99999 %.
    let e = from_values(&preset, &chart, &[(HARD, 560_000)], tap_at(BAD_AT));
    assert_eq!(gauge(&e, HARD), 2 * PCT);
    let e = from_values(&preset, &chart, &[(HARD, 559_999)], tap_at(BAD_AT));
    assert_eq!(e.gauges[HARD].failed_at, Some(TimeUs(BAD_AT)));
    // A mine of 10 units (−10 %) from 12 % leaves 2 %; from 11.99999 % it
    // fails. The note is a PERFECT after the mine.
    let chart = one_target(1_000, Some(240_000), Some(10));
    let mut events = hold_mine();
    events.extend(tap_at(TARGET + 10 * MS));
    let set = [(HARD, 12 * PCT), (EX_HARD, 1_199_999), (HAZARD, 12 * PCT)];
    let e = from_values(&preset, &chart, &set, events);
    let at_mine: Vec<(i64, Option<TimeUs>)> = [HARD, EX_HARD, HAZARD]
        .map(|g| (e.gauges[g].history[0].value.0, e.gauges[g].failed_at))
        .to_vec();
    assert_eq!(
        at_mine,
        [(2 * PCT, None), (0, Some(TimeUs(TARGET))), (2 * PCT, None)]
    );
}

/// Source: play/GrooveGauge.java:229-240 (`update`: the guts reduce only
/// what a judgment changes), :63-66 (`addValue`: a mine's value is added as
/// it is), :207-211 (the modifiers change only the six judgment values),
/// :218-225 (the clamp and the death apply to both); play/JudgeManager.java:
/// 266-269 (Endless Dream), and beatoraja's HARD bands
/// (`play/GaugeProperty.java:90` at exch-bms2/beatoraja@83574048).
#[test]
fn mine_damage_skips_judgment_modifiers() {
    let preset = preset();
    // 20 notes at #TOTAL 300: judgment damage × 10.
    let chart = one_target(20, Some(300_000), Some(5));
    let mut events = hold_mine();
    events.extend(tap_at(TARGET + 10 * MS));
    // HARD at 30 % (below 32 %): the mine's 5 units take 5 %, neither × 10
    // nor × 0.6; EX HARD from 100 % loses 5 %.
    let e = from_values(&preset, &chart, &[(HARD, 30 * PCT)], events.clone());
    let at_mine = |e: &Evaluation, g: usize| e.gauges[g].history[0].value.0;
    assert_eq!(at_mine(&e, HARD), 25 * PCT);
    assert_eq!(at_mine(&e, EX_HARD), 95 * PCT);
    // A BAD there takes 6 % × 10 × 0.6.
    let e = from_values(&preset, &chart, &[(HARD, 30 * PCT)], tap_at(BAD_AT));
    assert_eq!(gauge(&e, HARD), 0);
    assert_eq!(e.gauges[HARD].reference.0, 0);
    // The death applies: 5 units from 6.5 % leave 1.5 %, a failure.
    let e = from_values(&preset, &chart, &[(HARD, 650_000)], events.clone());
    assert_eq!(e.gauges[HARD].failed_at, Some(TimeUs(TARGET)));
    assert_eq!(e.gauges[HARD].history[0].reference.0, 150_000);
    // The clamp applies: 200 units from 100 % stop at 0.
    let lethal = one_target(20, Some(300_000), Some(200));
    let e = from_values(&preset, &lethal, &[], events);
    assert_eq!(e.gauges[EX_HARD].history[0].reference.0, 0);
    assert_eq!(e.gauges[EX_HARD].failed_at, Some(TimeUs(TARGET)));
    // beatoraja-style's HARD at 15 % (× 0.5 for a judgment): 10 units take
    // 10 %.
    let style = rules::beatoraja_style();
    let chart = one_target(20, None, Some(10));
    let mut events = hold_mine();
    events.extend(tap_at(TARGET + 10 * MS));
    let e = from_values(&style, &chart, &[(HARD, 15 * PCT)], events);
    assert_eq!(e.gauges[HARD].history[0].value.0, 5 * PCT);
}

/// Source: play/GrooveGauge.java:207-211 (the correction is part of the
/// judgment's value), :229-240 (the band chosen from the value before the
/// change, then `setValue`), :218-225 (clamp, then death); design.md
/// "Evaluation Stages" (accounting) fixes the order and the one rounding.
/// HARD's POOR (−10 %) at 1000 notes and `#TOTAL 224` (× 10/9).
#[test]
fn gauge_effect_order_is_fixed() {
    let preset = preset();
    let chart = one_target(1_000, Some(224_000), None);
    let poor = |before: i64| {
        let e = from_values(&preset, &chart, &[(HARD, before)], Vec::new());
        assert_eq!(judged(&e), [Poor]);
        let g = &e.gauges[HARD];
        (g.value.0, g.reference.0, g.failed_at.is_some())
    };
    // At 32 % no band: −1 111 111.1 rounds down to −1 111 112. That the
    // value after it is below 32 % does not matter.
    assert_eq!(poor(3_200_000), (2_088_888, 2_088_888, false));
    // Just below, × 3/5 too: −666 666.6 rounds once, to −666 667 (rounding
    // after each factor would give −666 668).
    assert_eq!(poor(3_199_999), (2_533_332, 2_533_332, false));
    // Added, then the death: 133 333 is below 2 %.
    assert_eq!(poor(800_000), (0, 133_333, true));
    // Added, then the clamp: −416 667 stops at 0.
    assert_eq!(poor(250_000), (0, 0, true));
    // An increase takes neither: +0.1 % from 31.99999 %.
    let e = from_values(&preset, &chart, &[(HARD, 3_199_999)], tap_at(TARGET));
    assert_eq!(gauge(&e, HARD), 3_209_999);
}

/// design.md "Evaluation Stages" (counts by purpose): a section's gauges
/// read the whole chart's notes, for the default `#TOTAL` and the damage
/// correction alike, so a note moves a gauge as far as in a full play.
#[test]
fn a_practiced_section_reads_the_whole_charts_note_count() {
    let preset = preset();
    // 1000 notes, one of them in the section: #TOTAL 352 (not 160.16 for
    // one note) and no correction (not × 10).
    let chart = one_target(1_000, None, None);
    let e = from_values(&preset, &chart, &[(NORMAL, 2 * PCT)], tap_at(TARGET));
    assert_eq!(gauge(&e, NORMAL), 2 * PCT + 35_200);
    let e = from_values(&preset, &chart, &[], tap_at(BAD_AT));
    assert_eq!(gauge(&e, EX_HARD), 88 * PCT);
    assert_eq!(gauge(&e, HARD), 94 * PCT);
    // The same note as a whole chart of one: 160.16 % and × 10.
    let alone = one_target(1, None, None);
    let e = from_values(&preset, &alone, &[(NORMAL, 2 * PCT)], tap_at(TARGET));
    assert_eq!(gauge(&e, NORMAL), 100 * PCT);
    let e = from_values(&preset, &alone, &[], tap_at(BAD_AT));
    assert_eq!(gauge(&e, EX_HARD), 0);
}

/// Builds a chart on key 1 at `#RANK 3`.
fn chart(build: impl FnOnce(ChartBuilder) -> ChartBuilder) -> Chart {
    let mut chart = build(ChartBuilder::new(BPM)).build().unwrap();
    chart.meta.judgment_headers = vec![common::rank(3)];
    chart
}

fn play_with(chart: &Chart, preset: &RulePreset, events: Vec<PhysicalInputEvent>) -> Evaluation {
    let input = EvalInput {
        events,
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: None,
    };
    let e = evaluate(chart, &input, preset).unwrap();
    assert!(e.completed);
    e
}

/// A press at `at` on key 1, released `hold` later.
fn hold_at(at: i64, hold: i64) -> Vec<PhysicalInputEvent> {
    vec![press(at, 0, 1), release(at + hold, 0, 1)]
}

/// Each judgment: (time, note index, judgment, deviation).
fn judged_at(e: &Evaluation) -> Vec<(i64, usize, rules::Judgment, Option<i64>)> {
    let times = e.events.iter().filter_map(|ev| match ev.kind {
        JudgeEventKind::Judged { .. } => Some(ev.time.0),
        _ => None,
    });
    times
        .zip(common::judgments(&e.events))
        .map(|(t, (_, note, j, d))| (t, note, j, d))
        .collect()
}

/// Known difference (the preset's module): an LN head is judged like a tap.
/// A press inside its late BAD window takes it BAD, where Endless Dream
/// makes a late BAD head ineligible (play/JudgeManager.java:435-437) and
/// LR2 has already missed it at its late GOOD edge. At `#RANK 3` (GOOD
/// ±120 ms, BAD ±200 ms), a long note from 0 to 500 ms pressed 150 ms late
/// is BAD at the press under either assignment policy, up to the BAD edge
/// itself. Each chart below judges the same with the head a tap.
#[test]
fn late_bad_ln_head_is_taken_like_a_tap() {
    let alone = |head: bool| {
        chart(|b| {
            if head {
                b.long(lane(1), TimeUs(T), TimeUs(T + 500 * MS))
            } else {
                b.tap(lane(1), TimeUs(T))
            }
        })
    };
    for assignment in [Assignment::ComboPriority, Assignment::NearestTiesEarlier] {
        let mut p = preset();
        p.assignment = assignment;
        for late in [150 * MS, MISSED] {
            let at = T + late;
            for head in [true, false] {
                let e = play_with(&alone(head), &p, hold_at(at, 10));
                assert_eq!(
                    judged_at(&e),
                    [(at, 0, Bad, Some(late))],
                    "{assignment:?}, {late}, head {head}"
                );
                assert_eq!(empty_poors(&e.events), 0);
            }
        }
    }
    // A long note from 0 to 50 ms and a tap at 300 ms, pressed at 160 ms:
    // combo priority keeps the head (BAD), since the tap lies outside GOOD,
    // and the tap is missed at its own deadline. A tap at 0 ms and a long
    // note from 30 ms, pressed at 190 ms: the tap is past its late GOOD edge,
    // so the head (160 ms late, BAD) takes over and the tap is passed.
    let later_tap = |head: bool| {
        chart(|b| {
            let b = if head {
                b.long(lane(1), TimeUs(T), TimeUs(T + 50 * MS))
            } else {
                b.tap(lane(1), TimeUs(T))
            };
            b.tap(lane(1), TimeUs(T + 300 * MS))
        })
    };
    let earlier_tap = |head: bool| {
        chart(|b| {
            let b = b.tap(lane(1), TimeUs(T));
            if head {
                b.long(lane(1), TimeUs(T + 30 * MS), TimeUs(T + 80 * MS))
            } else {
                b.tap(lane(1), TimeUs(T + 30 * MS))
            }
        })
    };
    for head in [true, false] {
        let at = T + 160 * MS;
        let e = play_with(&later_tap(head), &preset(), hold_at(at, 10));
        assert_eq!(
            judged_at(&e),
            [
                (at, 0, Bad, Some(160 * MS)),
                (T + 300 * MS + MISSED, 1, Poor, None)
            ],
            "head {head}"
        );
        let at = T + 190 * MS;
        let e = play_with(&earlier_tap(head), &preset(), hold_at(at, 10));
        assert_eq!(
            judged_at(&e),
            [(at, 0, Poor, None), (at, 1, Bad, Some(160 * MS))],
            "head {head}"
        );
    }
}

/// Known difference (the preset's module): an unpressed LN head is missed
/// at its late BAD edge, like a tap, as in Endless Dream
/// (play/JudgeManager.java:640-660), where LR2 misses it at its late GOOD
/// edge. At `#RANK 3`, with a long note from 0 to 50 ms on key 1 left
/// unpressed and a tap at 150 ms on key 2 hit PERFECT, the head is POOR at
/// 200 ms, after the PERFECT; LR2 has it POOR at 120 ms, before.
#[test]
fn unpressed_ln_head_is_poor_at_the_late_bad_edge() {
    let two_lanes = chart(|b| {
        b.long(lane(1), TimeUs(T), TimeUs(T + 50 * MS))
            .tap(lane(2), TimeUs(T + 150 * MS))
    });
    let at = T + 150 * MS;
    let e = play_with(
        &two_lanes,
        &preset(),
        vec![press(at, 0, 2), release(at + 10, 0, 2)],
    );
    let order: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged { lane, judgment, .. } => {
                Some((ev.time.0, lane.index(), judgment))
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, [(at, 2, Perfect), (T + MISSED, 1, Poor)]);
    // Pressed 1 µs past the edge, the head is already POOR, and nothing
    // ahead makes the press an empty POOR.
    let alone = chart(|b| b.long(lane(1), TimeUs(T), TimeUs(T + 500 * MS)));
    let e = play_with(&alone, &preset(), hold_at(T + MISSED + 1, 10));
    assert_eq!(judged_at(&e), [(T + MISSED, 0, Poor, None)]);
    assert_eq!(empty_poors(&e.events), 0);
}

/// G. A BAD long-note head decides its long note at the press: an early
/// BAD head is taken like a tap, and the release that follows changes
/// nothing.
#[test]
fn bad_ln_head_is_scored_at_the_press() {
    let chart = chart(|b| b.long(lane(1), TimeUs(T), TimeUs(T + 500 * MS)));
    let at = T - 150 * MS;
    let e = play_with(&chart, &preset(), hold_at(at, 600 * MS));
    assert_eq!(judged_at(&e), [(at, 0, Bad, Some(-150 * MS))]);
    let ended: Vec<_> = e
        .events
        .iter()
        .filter(|ev| matches!(ev.kind, JudgeEventKind::LnHoldEnded { .. }))
        .map(|ev| ev.time.0)
        .collect();
    assert_eq!(ended, [at + 600 * MS], "released 50 ms before the end");
}

/// A chart on key 1 built by `build`, at `rank`.
fn ranked(rank: Option<JudgmentHeader>, build: impl FnOnce(ChartBuilder) -> ChartBuilder) -> Chart {
    let mut chart = build(ChartBuilder::new(BPM)).build().unwrap();
    chart.meta.judgment_headers = rank.into_iter().collect();
    chart
}

/// The judgment a press at deviation `d` (press − note) gives a tap at `T`
/// on key 1 at `rank`, if the press judges it.
fn judge_tap(rank: Option<JudgmentHeader>, d: i64) -> Option<Judgment> {
    let chart = ranked(rank, |b| b.tap(lane(1), TimeUs(T)));
    let e = play_with(&chart, &preset(), hold_at(T + d, 10));
    judged_at(&e)
        .into_iter()
        .find(|j| j.0 == T + d)
        .map(|j| j.2)
}

/// The judgments of a long note on key 1 from `T` to `T + 1 s` at `rank`,
/// pressed at deviation `head` and released `before_end` before its end.
fn judge_release(rank: Option<JudgmentHeader>, head: i64, before_end: i64) -> Vec<Judgment> {
    let end = T + 1_000 * MS;
    let chart = ranked(rank, |b| b.long(lane(1), TimeUs(T), TimeUs(end)));
    let events = vec![press(T + head, 0, 1), release(end - before_end, 0, 1)];
    judged(&play_with(&chart, &preset(), events))
}

/// Key 1's and the scratch's tables after the window policy, at `rank`.
fn prepared(rank: Option<JudgmentHeader>) -> [rules::LaneTable; 2] {
    let chart = ranked(rank, |b| b.tap(lane(1), TimeUs(T)));
    let preset = preset();
    let dev = device(&[0]);
    let ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), None, 0).unwrap();
    [*ev.table(lane(1)), *ev.table(Lane::SCRATCH)]
}

/// Checks symmetric `[PERFECT, GREAT, GOOD]` edges and a 200 ms BAD at
/// `rank` through presses on both sides of each edge, and the LN-end
/// edges, all at GOOD's, through releases.
fn assert_windows(rank: Option<JudgmentHeader>, [pg, gr, gd]: [i64; 3]) {
    let at = |d| judge_tap(rank, d);
    let msg = format!("{rank:?}");
    for side in [-1, 1] {
        assert_eq!(at(side * pg), Some(Perfect), "{msg}");
        assert_eq!(at(side * (pg + 1)), Some(Great), "{msg}");
        assert_eq!(at(side * gr), Some(Great), "{msg}");
        assert_eq!(at(side * (gr + 1)), Some(Good), "{msg}");
        assert_eq!(at(side * gd), Some(Good), "{msg}");
        assert_eq!(at(side * (gd + 1)), Some(Bad), "{msg}");
        assert_eq!(at(side * 200 * MS), Some(Bad), "{msg}");
    }
    assert_eq!(at(-200 * MS - 1), None, "{msg}");
    assert_eq!(judge_release(rank, 0, gd), [Perfect], "{msg}");
    assert_eq!(judge_release(rank, 0, gd + 1), [Bad], "{msg}");
}

/// Source: play/JudgeProperty.java:54-55 (LR2: PG {-21000, 21000}, GR
/// {-60000, 60000}, GD {-120000, 120000}, BD {-200000, 200000}), both
/// edges inclusive (play/JudgeManager.java:439-440); at `#RANK 3` the
/// window rule takes them as they are (play/JudgeProperty.java:169,
/// :198-199, `judgerank >= 100`). The keys and the scratch share them.
#[test]
fn windows_at_rank_3_match_the_source() {
    assert_windows(Some(common::rank(3)), [21 * MS, 60 * MS, 120 * MS]);
    for table in prepared(Some(common::rank(3))) {
        assert_eq!(table, preset().tables.keys);
    }
}

/// Source: play/JudgeProperty.java:169 (`#RANK` 0-4: 25, 50, 75, 100, 75),
/// play/BMSPlayerRule.java:62 (no rank: `#RANK 2`'s 75), :65 (`#DEFEXRANK
/// n`: `n × 75 / 100`), play/JudgeProperty.java:185-190 (the LR2 table),
/// :192-222 (below 100 %, interpolated between its columns at 0, 25, 50,
/// 75 and 100 %), :233-239 (PERFECT, GREAT and GOOD only: BAD keeps
/// 200 ms), and the LN-end table through the same rule (:120-122). Worked
/// out by hand in the rules crate (`interpolated_windows_at_each_rank`);
/// the empty POOR range does not scale either
/// (`empty_poor_ranges_match_the_source`).
#[test]
fn windows_interpolate_like_the_source() {
    let defex = |n| Some(common::defexrank(n));
    for (rank, edges) in [
        (Some(common::rank(0)), [8 * MS, 24 * MS, 40 * MS]),
        (Some(common::rank(1)), [15 * MS, 30 * MS, 60 * MS]),
        (Some(common::rank(2)), [18 * MS, 40 * MS, 100 * MS]),
        (Some(common::rank(4)), [18 * MS, 40 * MS, 100 * MS]),
        (None, [18 * MS, 40 * MS, 100 * MS]),
        (defex(100), [18 * MS, 40 * MS, 100 * MS]),
        // 37.5 %: half way from 25 to 50 %.
        (defex(50), [11_500, 27 * MS, 50 * MS]),
        // 60 %: 0.4 of the way from 50 to 75 %.
        (defex(80), [16_200, 34 * MS, 76 * MS]),
    ] {
        assert_windows(rank, edges);
    }
}

/// Source: play/BMSPlayerRule.java:65 stores `n × 75 / 100` as an `int`
/// before the interpolation (play/JudgeProperty.java:192-222). Here the
/// percentage is taken as it is (a known difference in the preset's
/// module): `#DEFEXRANK 1` is 0.75 %, 0.03 of the way to 25 % (PERFECT,
/// GREAT and GOOD 240, 720 and 1200 µs; 0 % and zero-width there), and
/// `#DEFEXRANK 133` is 99.75 %, 0.99 of the way from 75 to 100 % (20.97,
/// 59.8 and 119.8 ms; 99 % there, 20.88, 59.2 and 119.2 ms).
#[test]
fn defexrank_interpolates_at_its_exact_percentage() {
    assert_windows(Some(common::defexrank(1)), [240, 720, 1_200]);
    assert_windows(Some(common::defexrank(133)), [20_970, 59_800, 119_800]);
}

/// Source: play/JudgeProperty.java:198-199 (from 100 %, `edge ×
/// judgerank / 100`) and :241-249 (from GOOD down, each of PERFECT, GREAT
/// and GOOD no wider than the next, on each side), BAD unscaled (:233-239);
/// the LN-end edges likewise (:120-122), whose BAD row is 200 ms too
/// (:56, :58). At `#DEFEXRANK 400` (300 %), GOOD is BAD's 200 ms, not
/// 360 ms, and the LN-end edges with it: a release 200 ms before the end
/// keeps the head, 1 µs earlier is BAD. At `#DEFEXRANK 600` (450 %) GREAT
/// is clamped too.
#[test]
fn scaled_windows_clamp_to_the_next_wider() {
    let edges = |rank| {
        prepared(rank).map(|t| {
            let w = t.windows;
            let l = t.ln_end;
            (
                [w.perfect, w.great, w.good, w.bad].map(|w| (w.early.0, w.late.0)),
                [l.perfect.0, l.great.0, l.good.0],
            )
        })
    };
    let clamped = |pg: i64, gr: i64| {
        (
            [
                (pg, pg),
                (gr, gr),
                (200 * MS, 200 * MS),
                (200 * MS, 200 * MS),
            ],
            [200 * MS; 3],
        )
    };
    let at_300 = Some(common::defexrank(400));
    assert_eq!(edges(at_300), [clamped(63 * MS, 180 * MS); 2]);
    let at_450 = Some(common::defexrank(600));
    assert_eq!(edges(at_450), [clamped(94_500, 200 * MS); 2]);
    // Through presses and releases at 300 %.
    let at = |d| judge_tap(at_300, d);
    assert_eq!(at(-63 * MS), Some(Perfect));
    assert_eq!(at(180 * MS), Some(Great));
    assert_eq!(at(180 * MS + 1), Some(Good));
    assert_eq!(at(-200 * MS), Some(Good));
    assert_eq!(at(-200 * MS - 1), None);
    assert_eq!(judge_release(at_300, 0, 200 * MS), [Perfect]);
    assert_eq!(judge_release(at_300, 0, 200 * MS + 1), [Bad]);
}

/// Source: play/JudgeProperty.java:56 (LR2 LN end: PG, GR and GD all
/// {-120000, 120000}) and play/JudgeManager.java:580-596: the release's
/// judgment is the first row holding it, then the worse of head and
/// release (`:582`), so inside 120 ms before the end the head's judgment
/// stays. At `#RANK 3`, for a PERFECT, a GREAT (30 ms late) and a GOOD
/// (100 ms early) head.
#[test]
fn release_within_good_width_keeps_head() {
    let rank = Some(common::rank(3));
    for (head, judgment) in [(0, Perfect), (30 * MS, Great), (-100 * MS, Good)] {
        for before_end in [1, 60 * MS, 120 * MS] {
            assert_eq!(
                judge_release(rank, head, before_end),
                [judgment],
                "head at {head}, released {before_end} before the end"
            );
        }
    }
}

/// Source: play/JudgeManager.java:580-594: a release earlier than the GOOD
/// row is graded BAD (`:586-590`, `lnendJudge = 3`; the release margin is
/// 0, play/JudgeProperty.java:57), never POOR, whatever the head's
/// judgment. The LN-end edges are the same 120 ms for PERFECT, GREAT and
/// GOOD, so a release 120.001 ms before the end is BAD, not GREAT or GOOD
/// as under widening edges.
#[test]
fn earlier_release_is_bad() {
    let rank = Some(common::rank(3));
    for head in [0, 30 * MS, -100 * MS] {
        for before_end in [120 * MS + 1, 150 * MS, 200 * MS + 1, 900 * MS] {
            assert_eq!(
                judge_release(rank, head, before_end),
                [Bad],
                "head at {head}, released {before_end} before the end"
            );
        }
    }
}

/// The empty POORs of presses at `at`, each released 10 µs later, on key 1
/// of a chart with taps at `taps` at `rank`.
fn empty_poors_of(rank: Option<JudgmentHeader>, taps: &[i64], at: &[i64]) -> usize {
    let chart = ranked(rank, |mut b| {
        for &t in taps {
            b = b.tap(lane(1), TimeUs(t));
        }
        b
    });
    let events = at.iter().flat_map(|&a| hold_at(a, 10)).collect();
    empty_poors(&play_with(&chart, &preset(), events).events)
}

/// Source: play/JudgeProperty.java:54-55 (the MS pair {0, 1000000}: from
/// 0 to 1000 ms after the press, nothing before it) and
/// play/JudgeManager.java:414-442: a judged note inside the pair is an
/// empty POOR (`:433-434`), an unjudged one inside it but outside every
/// window too (`:438-442`), so an unjudged note makes one only past the
/// early BAD edge. The scan stops at a note 1000 ms ahead (`:416`); here
/// the range's far edge is inclusive like every edge (a known difference
/// in the preset's module). Not scaled: the same at `#RANK 0` (:233-239).
#[test]
fn empty_poor_ranges_match_the_source() {
    for rank in [Some(common::rank(3)), Some(common::rank(0))] {
        // An unjudged note at T: inside early BAD the press takes it.
        let unjudged = |at: i64| empty_poors_of(rank, &[T], &[at]);
        assert_eq!(unjudged(T - 200 * MS), 0);
        assert_eq!(unjudged(T - 200 * MS - 1), 1);
        assert_eq!(unjudged(T - 999 * MS), 1);
        assert_eq!(unjudged(T - 1_000 * MS), 1);
        assert_eq!(unjudged(T - 1_000 * MS - 1), 0);
        // A judged note: hit 100 ms early, then pressed again before it,
        // at it, and after it, where there is no late side.
        let judged = |at: i64| empty_poors_of(rank, &[T], &[T - 100 * MS, at]);
        assert_eq!(judged(T - 50 * MS), 1);
        assert_eq!(judged(T), 1);
        assert_eq!(judged(T + 1), 0);
        assert_eq!(judged(T + 150 * MS), 0);
        // A missed note behind the press makes none either.
        assert_eq!(empty_poors_of(rank, &[T], &[T + 300 * MS]), 0);
    }
}

/// Source: play/JudgeProperty.java:60 (MS keeps the combo) and
/// play/JudgeManager.java:716-725 (`judge < 5` adds to it); BP and the own
/// count as beatoraja's; play/GaugeProperty.java:119-122 (MS: NORMAL −2 %,
/// HARD and EX HARD −2 %, HAZARD −10 %), the damage correction on HARD and
/// EX HARD (play/GrooveGauge.java:279-309) and HARD's × 0.6 below 32 %
/// (:232-243) applying to it like to any judgment.
#[test]
fn empty_poor_keeps_the_combo_adds_bp_and_takes_its_gauge_damage() {
    let chart = chart(|b| {
        b.tap(lane(1), TimeUs(T))
            .tap(lane(1), TimeUs(T + 1_000 * MS))
    });
    let events = [T, T + 600 * MS, T + 1_000 * MS]
        .into_iter()
        .flat_map(|at| hold_at(at, 10))
        .collect();
    let e = play_with(&chart, &preset(), events);
    let t = &e.tally;
    assert_eq!((t.empty_poor, t.bp, t.counts.poor), (1, 1, 0));
    assert_eq!((t.combo, t.max_combo), (2, 2));
    // Two notes: #TOTAL 160.32, HARD and EX HARD damage × 10. After a
    // PERFECT, every gauge full, the empty POOR takes −2 %, −20 %, −20 %
    // and −10 %.
    let second = |g: usize| e.gauges[g].history[1].value.0;
    assert_eq!(
        [NORMAL, HARD, EX_HARD, HAZARD].map(second),
        [98 * PCT, 80 * PCT, 80 * PCT, 90 * PCT]
    );
    // HARD below 32 %, with no correction (1000 notes, #TOTAL 240): × 0.6.
    let chart = one_target(1_000, Some(240_000), None);
    let mut events = tap_at(TARGET - 400 * MS);
    events.extend(tap_at(TARGET));
    for (before, after) in [(32 * PCT, 30 * PCT), (3_199_999, 3_079_999)] {
        let e = from_values(&preset(), &chart, &[(HARD, before)], events.clone());
        assert_eq!(e.tally.empty_poor, 1);
        assert_eq!(e.gauges[HARD].history[0].value.0, after, "from {before}");
    }
}

/// Source: play/JudgeProperty.java:55 and :58 (the LR2 scratch tables equal
/// the keys': PG ±21, GR ±60, GD ±120, BD ±200 ms; LN end 120 ms; MS {0,
/// 1000000}); a scratch press and release read them
/// (play/JudgeManager.java:407, :538). A scratch press 25 ms early is GREAT
/// here, where beatoraja-style's ±30 ms scratch PERFECT holds it.
#[test]
fn scratch_uses_the_key_windows() {
    let taps = chart(|b| b.tap(Lane::SCRATCH, TimeUs(T)).tap(lane(1), TimeUs(T)));
    for (d, judgment) in [
        (-21 * MS, Perfect),
        (-25 * MS, Great),
        (60 * MS, Great),
        (120 * MS, Good),
        (120 * MS + 1, Bad),
        (-200 * MS, Bad),
    ] {
        let e = play_with(&taps, &preset(), both_at(T + d));
        assert_eq!(judged_on(&e, 0), [judgment], "{d}");
        assert_eq!(judged_on(&e, 1), [judgment], "{d}");
    }
    let style = rules::beatoraja_style();
    let e = play_with(&taps, &style, both_at(T - 25 * MS));
    assert_eq!(judged_on(&e, 0), [Perfect]);
    // A scratch long note released 120 ms and 120.001 ms before its end.
    let end = T + 1_000 * MS;
    let long = chart(|b| b.long(Lane::SCRATCH, TimeUs(T), TimeUs(end)));
    for (before_end, judgment) in [(120 * MS, Perfect), (120 * MS + 1, Bad)] {
        let events = vec![press(T, 0, 0), release(end - before_end, 0, 0)];
        let e = play_with(&long, &preset(), events);
        assert_eq!(judged_on(&e, 0), [judgment], "{before_end}");
    }
    // A scratch empty POOR 1000 ms before a scratch note; none behind one.
    let tap = chart(|b| b.tap(Lane::SCRATCH, TimeUs(T)));
    for (presses, empty) in [
        (vec![T - 1_000 * MS], 1),
        (vec![T - 1_000 * MS - 1], 0),
        (vec![T, T + 100 * MS], 0),
    ] {
        let events = presses
            .iter()
            .flat_map(|&at| [press(at, 0, 0), release(at + 10, 0, 0)])
            .collect();
        let e = play_with(&tap, &preset(), events);
        assert_eq!(empty_poors(&e.events), empty, "{presses:?}");
    }
}

/// Known differences (the preset's module): one press judges one note, and
/// the notes it passes are POOR at the press. At `#RANK 3`, with taps at 0
/// and 160 ms and a long note from 50 to 100 ms on one lane, a press at
/// 190 ms takes the 160 ms tap (GREAT, 30 ms late; by combo priority,
/// since the 0 ms tap and the head, 140 ms late, are past their late GOOD
/// edges, play/JudgeAlgorithm.java:15-20); the source's multi-BAD
/// (play/JudgeManager.java:993-1061) gives BAD on the 0 ms tap and the
/// head, here both are POOR at the press. With taps at 0 and 160 ms, a
/// press at 130 ms takes the later tap and passes the earlier one, which a
/// second press at 190 ms no longer finds (BAD there, where it stays
/// hittable until its separate miss pass, :640-660).
#[test]
fn one_press_judges_one_note() {
    let mixed = chart(|b| {
        b.tap(lane(1), TimeUs(T))
            .long(lane(1), TimeUs(T + 50 * MS), TimeUs(T + 100 * MS))
            .tap(lane(1), TimeUs(T + 160 * MS))
    });
    let at = T + 190 * MS;
    let e = play_with(&mixed, &preset(), hold_at(at, 10));
    assert_eq!(
        judged_at(&e),
        [
            (at, 0, Poor, None),
            (at, 1, Poor, None),
            (at, 2, Great, Some(30 * MS))
        ]
    );
    let chart = chart(|b| b.tap(lane(1), TimeUs(T)).tap(lane(1), TimeUs(T + 160 * MS)));
    let mut events = hold_at(T + 130 * MS, 10);
    events.extend(hold_at(T + 190 * MS, 10));
    let e = play_with(&chart, &preset(), events);
    assert_eq!(
        judged_at(&e),
        [
            (T + 130 * MS, 0, Poor, None),
            (T + 130 * MS, 1, Great, Some(-30 * MS))
        ]
    );
    assert_eq!(empty_poors(&e.events), 0);
}

/// Selection × empty POOR × LN heads: a press 160 ms after an LN head takes
/// it (BAD), so a tap 740 ms ahead makes no empty POOR; a press past the
/// head's late BAD edge finds it missed, and the tap 700 ms ahead makes the
/// press an empty POOR (the MS candidate, play/JudgeManager.java:438-442).
#[test]
fn missed_head_leaves_an_empty_poor_to_a_later_note() {
    let chart = chart(|b| {
        b.long(lane(1), TimeUs(T), TimeUs(T + 50 * MS))
            .tap(lane(1), TimeUs(T + 900 * MS))
    });
    let at = T + 160 * MS;
    let e = play_with(&chart, &preset(), hold_at(at, 10));
    assert_eq!(empty_poors(&e.events), 0);
    assert_eq!(
        judged_at(&e),
        [
            (at, 0, Bad, Some(160 * MS)),
            (T + 900 * MS + MISSED, 1, Poor, None)
        ]
    );
    let e = play_with(&chart, &preset(), hold_at(T + 200 * MS, 10));
    assert_eq!(empty_poors(&e.events), 0, "the edge itself takes the head");
    let e = play_with(&chart, &preset(), hold_at(T + 200 * MS + 1, 10));
    assert_eq!(empty_poors(&e.events), 1);
    assert_eq!(
        judged_at(&e),
        [
            (T + MISSED, 0, Poor, None),
            (T + 900 * MS + MISSED, 1, Poor, None)
        ]
    );
    assert_eq!((e.tally.empty_poor, e.tally.bp), (1, 3));
}

/// Known difference (the preset's module): a note already judged makes an
/// empty POOR when it lies up to 1000 ms after the press, as in Endless
/// Dream (play/JudgeManager.java:433-434), where LR2 considers only the
/// lane's current unjudged note. At `#RANK 3`, a note at 1000 ms hit early
/// BAD at 830 ms, then a press at 900 ms: an empty POOR here and in
/// Endless Dream, none in LR2.
#[test]
fn a_judged_note_ahead_makes_an_empty_poor() {
    let chart = chart(|b| b.tap(lane(1), TimeUs(T)));
    let mut events = hold_at(T - 170 * MS, 10);
    events.extend(hold_at(T - 100 * MS, 10));
    let e = play_with(&chart, &preset(), events);
    assert_eq!(judged_at(&e), [(T - 170 * MS, 0, Bad, Some(-170 * MS))]);
    assert_eq!(empty_poors(&e.events), 1);
}

/// Known difference (the preset's module): a long note's timing is its
/// head's. A head 30 ms late released 100 ms before the end is GREAT (the
/// worse of the head's GREAT and the release's PERFECT,
/// play/JudgeManager.java:582); the source keeps the deviation of larger
/// magnitude, the release's, and counts an early GREAT 100 ms off
/// (:583-585, :708). Here the event holds the head's +30 ms and the
/// release's −100 ms apart.
#[test]
fn ln_timing_is_the_heads() {
    let end = T + 1_000 * MS;
    let chart = chart(|b| b.long(lane(1), TimeUs(T), TimeUs(end)));
    let events = vec![press(T + 30 * MS, 0, 1), release(end - 100 * MS, 0, 1)];
    let e = play_with(&chart, &preset(), events);
    let judged: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                release,
                ..
            } => Some((judgment, deviation, release)),
            _ => None,
        })
        .collect();
    assert_eq!(
        judged,
        [(Great, Some(TimeUs(30 * MS)), Some(TimeUs(-100 * MS)))]
    );
}

/// Source: play/JudgeManager.java:640-660 (the head is missed past its late
/// BAD edge), :438-449 (the tap 600 ms ahead is the MS candidate), :500
/// (the candidate's sound plays). At `#RANK 3`, with a long note from 1000
/// to 1050 ms (sound A) and a tap at 1900 ms (sound B) on one lane, a press
/// at 1300 ms is an empty POOR against the tap and plays B, not the lane's
/// latest note before the press, as there (design.md "The Only Output Is
/// the Judgment Event Stream").
#[test]
fn an_empty_poor_plays_the_note_it_is_against() {
    let mut chart = chart(|b| {
        b.sound("a.wav")
            .sound("b.wav")
            .long(lane(1), TimeUs(T), TimeUs(T + 50 * MS))
            .tap(lane(1), TimeUs(T + 900 * MS))
    });
    for (k, note) in chart.lanes[1].iter_mut().enumerate() {
        note.sound = Some(bms_model::SoundId(u16::try_from(k).unwrap()));
    }
    let at = T + 300 * MS;
    let e = play_with(&chart, &preset(), hold_at(at, 10));
    assert_eq!(empty_poors(&e.events), 1);
    let played: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::PlaySound { sound, .. } => Some((ev.time.0, sound.0)),
            _ => None,
        })
        .collect();
    assert_eq!(played, [(at, 1)]);
}
