//! Hand-written examples of the LR2-style rules, each naming the public
//! source it follows. `rules::lr2_style()` does not exist yet (stage 2,
//! PR8): until it does, the examples run under [`preset`], a preset built
//! here: beatoraja-style with the values and policies of LR2oraja Endless
//! Dream (<https://github.com/seraxis/lr2oraja-endlessdream> at `d4995153`,
//! under `core/src/bms/player/beatoraja/`, cited as `file:line`) that the
//! examples read. Its other rules, LN-end edges, empty POOR ranges and the
//! window policy among them, are beatoraja-style's stand-ins and not under
//! test; `#RANK 3` is 100 % under both.
//!
//! | Value | Source |
//! | --- | --- |
//! | Note windows at 100 %: PERFECT ±21, GREAT ±60, GOOD ±120, BAD ±200 ms, keys and scratch alike | `play/JudgeProperty.java:54-55` |
//! | Note assignment: combo priority ([`Assignment::ComboPriority`]), beatoraja's default kept | the candidate loop `play/JudgeManager.java:414-455` |
//! | A late BAD LN head cannot be the primary ([`HeadEligibility::NotLateBad`]) | `play/JudgeManager.java:435-437` |
//! | Gauges `{min, max, start, clear, death}` and changes `{PG, GR, GD, BD, PR, MS}` | `play/GaugeProperty.java:117-122`, field order `:161` |
//! | ASSIST EASY, EASY and NORMAL increases × `TOTAL / notes` | `play/GrooveGauge.java:263` (`TOTAL` modifier), `:117-119` |
//! | HARD and EX HARD damage × `max(fix1, fix2)` ([`TotalCorrection::ScaleDamage`]), MS included | `play/GrooveGauge.java:279-309` (`MODIFY_DAMAGE`), `play/GaugeProperty.java:120-121`; once per chart, `play/GrooveGauge.java:207-211` |
//! | HARD damage × 0.6 below 32 % | `play/GaugeProperty.java:120`, `play/GrooveGauge.java:232-240` |
//! | A survival gauge a change leaves strictly below 2 % drops to 0, a mine's change included | `play/GrooveGauge.java:218-225`, `play/GaugeProperty.java:120-122` (death 2); a mine changes the gauge by `setValue`, `play/GrooveGauge.java:63-66`, `play/JudgeManager.java:266-269` |
//! | `#TOTAL` absent ([`DefaultTotal::NotesLinear`]): `160 + (N + clamp(N − 400, 0, 200)) × 0.16` over the whole chart's notes | `play/BMSPlayerRule.java:73-79`, `:88-89` |
//!
//! Gauges are in 1/100000 %, in the preset's order: NORMAL, ASSIST EASY,
//! EASY, HARD, EX HARD, HAZARD. The gauge arithmetic is this evaluator's:
//! each change exact until one rounding down to 1/100000 %, where the source
//! computes in `f32` (a known difference every target shares, design.md
//! "What a -style Preset Reproduces").
//!
//! # Known differences
//!
//! - **An ineligible head drops the earlier candidate there.** Endless
//!   Dream makes a late BAD LN head ineligible by giving it judge 6
//!   (`play/JudgeManager.java:435-437`), which reuses beatoraja's existing
//!   "no judgment here" branch, and that branch also resets the candidate
//!   the scan had already taken (`:445-452`). The rule only says a late BAD
//!   head cannot be taken, and LR2 judges the oldest note, so here the
//!   earlier candidate stays the primary. With a tap at 0 ms and a long
//!   note from 30 ms on one lane, a press at 190 ms is BAD on the tap here
//!   and the head is missed at 230 ms; there the press judges nothing, the
//!   tap is missed at 200 ms and the head at 230 ms
//!   (`late_bad_ln_head_is_not_selectable`).

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, empty_poors, lane, press, release};
use eval::{
    EvalError, EvalInput, Evaluation, Evaluator, InputStart, JudgeEventKind, PhysicalInputEvent,
    Section, evaluate,
};
use rules::Judgment::{Bad, Perfect, Poor};
use rules::{
    Assignment, DefaultTotal, GaugeDelta, GaugeKind, GaugeRule, GaugeValue, HeadEligibility,
    JudgeWindows, Judgment, LowDamage, PerJudgment, RulePreset, TotalCorrection, Window,
};

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

/// A change in hundredths of a percent, fixed or a `TOTAL / notes` share in
/// tenths.
fn centi(n: i64) -> GaugeDelta {
    GaugeDelta::Fixed(GaugeValue::centi_percent(n))
}

fn share(tenths: u32) -> GaugeDelta {
    GaugeDelta::TotalShare {
        num: tenths,
        den: 10,
    }
}

/// A gauge decided at the end: start 20 %, min 2 %, the `TOTAL` modifier on
/// its increases (`play/GaugeProperty.java:117-119`).
fn end_gauge(id: &str, clear: i64, up: [u32; 2], damage: [i64; 3]) -> GaugeRule {
    let [bad, poor, empty] = damage.map(|d| centi(-d));
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::EndThreshold {
            clear: GaugeValue(clear * PCT),
        },
        start: GaugeValue(20 * PCT),
        min: GaugeValue(2 * PCT),
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: share(up[0]),
            great: share(up[0]),
            good: share(up[1]),
            bad,
            poor,
        },
        empty_poor: empty,
        correction: TotalCorrection::None,
        low_damage: Vec::new(),
    }
}

/// A survival gauge from 100 %, dead below 2 % (`play/GaugeProperty.java:
/// 120-122`), with fixed changes in hundredths of a percent.
fn survival_gauge(
    id: &str,
    change: [i64; 6],
    correction: TotalCorrection,
    low_damage: Vec<LowDamage>,
) -> GaugeRule {
    let [perfect, great, good, bad, poor, empty] = change.map(centi);
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::Survival {
            fails_below: GaugeValue(2 * PCT),
        },
        start: GaugeValue::FULL,
        min: GaugeValue::EMPTY,
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect,
            great,
            good,
            bad,
            poor,
        },
        empty_poor: empty,
        correction,
        low_damage,
    }
}

/// beatoraja-style with Endless Dream's 100 % note windows on both lane
/// classes (`play/JudgeProperty.java:54-55`), combo priority, its LN-head
/// eligibility (`play/JudgeManager.java:435-437`), gauges
/// (`play/GaugeProperty.java:117-122`) and default `#TOTAL`
/// (`play/BMSPlayerRule.java:88-89`).
fn preset() -> RulePreset {
    let mut p = rules::beatoraja_style();
    p.id = "lr2-style-examples".to_owned();
    let windows = JudgeWindows {
        perfect: Window::symmetric(21 * MS),
        great: Window::symmetric(60 * MS),
        good: Window::symmetric(120 * MS),
        bad: Window::symmetric(200 * MS),
    };
    p.tables.keys.windows = windows;
    p.tables.scratch.windows = windows;
    p.assignment = Assignment::ComboPriority;
    p.ln_mode.head_eligibility = HeadEligibility::NotLateBad;
    p.default_total = DefaultTotal::NotesLinear;
    p.gauges = vec![
        end_gauge("normal", 80, [10, 5], [400, 600, 200]),
        end_gauge("assist_easy", 60, [12, 6], [320, 480, 160]),
        end_gauge("easy", 80, [12, 6], [320, 480, 160]),
        survival_gauge(
            "hard",
            [10, 10, 5, -600, -1_000, -200],
            TotalCorrection::ScaleDamage,
            vec![LowDamage {
                below: GaugeValue(32 * PCT),
                num: 3,
                den: 5,
            }],
        ),
        survival_gauge(
            "ex_hard",
            [10, 10, 5, -1_200, -2_000, -200],
            TotalCorrection::ScaleDamage,
            Vec::new(),
        ),
        survival_gauge(
            "hazard",
            [15, 6, 0, -10_000, -10_000, -1_000],
            TotalCorrection::None,
            Vec::new(),
        ),
    ];
    assert_eq!(p.validate(), Ok(()));
    p
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

/// K. A long note from 0 to 50 ms and a tap at 300 ms, pressed at 160 ms:
/// the head would be a late BAD, so it cannot be the primary, which combo
/// priority alone would keep since the tap lies outside GOOD. The press
/// takes the tap, 140 ms early (BAD), and the head it passes is POOR at the
/// press, before it (`play/JudgeManager.java:435-437`; the passed note is
/// POOR at the press, a known difference shared with beatoraja-style).
#[test]
fn ineligible_head_is_passed_and_poor() {
    let chart = chart(|b| {
        b.long(lane(1), TimeUs(T), TimeUs(T + 50 * MS))
            .tap(lane(1), TimeUs(T + 300 * MS))
    });
    let at = T + 160 * MS;
    let e = play_with(&chart, &preset(), hold_at(at, 10));
    assert_eq!(
        judged_at(&e),
        [(at, 0, Poor, None), (at, 1, Bad, Some(-140 * MS))]
    );
    // Without the eligibility rule, combo priority keeps the head.
    let mut inside = preset();
    inside.ln_mode.head_eligibility = HeadEligibility::InsideBad;
    let e = play_with(&chart, &inside, hold_at(at, 10));
    assert_eq!(judged_at(&e)[0], (at, 0, Bad, Some(160 * MS)));
}

/// K. A late BAD press that reaches an LN head no primary passes leaves the
/// head where it is: it is not taken, and it is missed at its own deadline
/// (`play/JudgeManager.java:435-437`). Pressed at 160 ms with the long note
/// alone, the press takes nothing, under either assignment policy.
#[test]
fn late_bad_ln_head_is_not_selectable() {
    let alone = chart(|b| b.long(lane(1), TimeUs(T), TimeUs(T + 50 * MS)));
    for assignment in [Assignment::ComboPriority, Assignment::NearestTiesEarlier] {
        let mut p = preset();
        p.assignment = assignment;
        let e = play_with(&alone, &p, hold_at(T + 160 * MS, 10));
        assert_eq!(
            judged_at(&e),
            [(T + MISSED, 0, Poor, None)],
            "{assignment:?}"
        );
        assert_eq!(empty_poors(&e.events), 0);
        assert!(
            !e.events
                .iter()
                .any(|ev| matches!(ev.kind, JudgeEventKind::LnHoldEnded { .. })),
            "the press took no head"
        );
    }
    // A tap at 0 ms and a long note from 30 ms, pressed at 190 ms: the head
    // (160 ms late) is not the primary, which stays the tap, BAD at the
    // press, and the head is missed at its own deadline, 230 ms. Endless
    // Dream drops the tap too (a known difference, in the module docs).
    let before = chart(|b| {
        b.tap(lane(1), TimeUs(T))
            .long(lane(1), TimeUs(T + 30 * MS), TimeUs(T + 80 * MS))
    });
    let at = T + 190 * MS;
    let e = play_with(&before, &preset(), hold_at(at, 10));
    assert_eq!(
        judged_at(&e),
        [
            (at, 0, Bad, Some(190 * MS)),
            (T + 30 * MS + MISSED, 1, Poor, None)
        ]
    );
}

/// G. A BAD long-note head decides its long note at the press, also under
/// the head eligibility rule: an early BAD head is eligible, and the
/// release that follows changes nothing.
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
