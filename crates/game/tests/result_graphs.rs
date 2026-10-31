//! The result screen's graphs (design.md "Play Lifecycle", "Gauges and
//! Session Policy"): the time axis and its columns, the condensed gauge
//! histories, the listed gauges and the press deviations, through
//! `ReEvaluate` and `ResultScreen` on recorded plays.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, control, device, lane, press};
use eval::{GaugePoint, GaugeResult, InputKind, PhysicalInputEvent, SourceId};
use game::{
    Axis, GRAPH_COLUMNS, GaugeColumn, GaugeIndex, ReEvaluate, ResultContext, ResultExtension,
    ResultScreen, SHOWN_GAUGES, Timing, ViewStatus, ViewSummary, condense, listed,
};
use proptest::prelude::*;
use replay::verify::Coverage;
use replay::{
    BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete, PlayRecord,
    PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions, WakePolicy,
};
use rules::{GaugeValue, Implementation, Judgment, PerJudgment, RulePreset};

const N: u16 = GRAPH_COLUMNS as u16;

fn release(received: i64, l: bms_model::Lane) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(0),
        kind: InputKind::Control {
            control: control(l),
            value: 0,
        },
    }
}

/// A record of `events` on `chart` under `preset`, finalized at `finalized`
/// with `end`.
fn record(
    preset: &RulePreset,
    events: Vec<PhysicalInputEvent>,
    end: EndStatus,
    finalized: TimeUs,
) -> PlayRecord {
    let dev = device(&[0]);
    PlayRecord {
        versions: Versions {
            evaluator: Implementation::STANDARD_VERSION,
            app: "0.0.0-test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(preset),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: dev.sources[0].clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
        start: eval::InputStart {
            opened_at: TimeUs(-1_000 * MS),
            held: Vec::new(),
        },
        events,
        arrivals: Vec::new(),
        status: Status {
            end,
            finalized,
            incomplete: Incomplete::default(),
        },
    }
}

fn deadline(chart: &Chart, preset: &RulePreset) -> TimeUs {
    eval::Evaluator::new(
        chart,
        preset,
        &device(&[0]),
        &eval::InputStart::default(),
        None,
        0,
    )
    .unwrap()
    .final_deadline()
}

/// `count` taps on lane 1 from 1 s, 500 ms apart.
fn taps(count: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..count {
        b = b.tap(lane(1), TimeUs(1_000 * MS + i * 500 * MS));
    }
    b.build().unwrap()
}

fn re(chart: &Chart, rec: &PlayRecord, preset: &RulePreset) -> replay::verify::ReEvaluation {
    let ctx = ResultContext {
        record: rec,
        chart,
        preset: &rules::tapestry(),
        live: &[],
    };
    ReEvaluate::under(&ctx, preset).unwrap()
}

fn summary(r: &replay::verify::ReEvaluation, preset: &RulePreset, axis: Axis) -> ViewSummary {
    ViewSummary::of(
        &r.evaluation,
        preset,
        axis,
        r.coverage,
        r.as_played,
        &r.started_at_default,
        None,
    )
}

#[test]
fn columns_at_the_axis_boundaries() {
    // 1 ms a column from 1 s.
    let a = Axis {
        origin: TimeUs(1_000_000),
        end: TimeUs(1_288_000),
    };
    assert_eq!(a.column(TimeUs(0)), 0);
    assert_eq!(a.column(TimeUs(1_000_999)), 0);
    assert_eq!(a.column(TimeUs(1_001_000)), 1);
    assert_eq!(a.column(TimeUs(1_287_999)), 287);
    // The end, and past it, in the last column.
    assert_eq!(a.column(TimeUs(1_288_000)), 287);
    assert_eq!(a.column(TimeUs(9_000_000)), 287);
    assert_eq!(a.reached(Coverage::Recorded), N);
    assert_eq!(a.reached(Coverage::NoInputAfter(TimeUs(1_100_000))), N);
    assert_eq!(a.reached(Coverage::EndedMidway(TimeUs(1_005_500))), 6);
    // A practice round aborted before its section started.
    assert_eq!(a.reached(Coverage::EndedMidway(TimeUs(999_999))), 0);
    assert_eq!(a.reached(Coverage::EndedMidway(TimeUs(1_000_000))), 1);
    // A span of 1 µs: its origin first, its end last.
    let one = Axis {
        origin: TimeUs(5),
        end: TimeUs(6),
    };
    assert_eq!(
        [4, 5, 6, 7].map(|t| one.column(TimeUs(t))),
        [0, 0, 287, 287]
    );
    // A span of 0, or an end before the origin: everything in the first.
    for end in [5, 4] {
        let zero = Axis {
            origin: TimeUs(5),
            end: TimeUs(end),
        };
        assert_eq!(
            [i64::MIN, 4, 5, 6, i64::MAX].map(|t| zero.column(TimeUs(t))),
            [0; 5]
        );
        assert_eq!(zero.reached(Coverage::EndedMidway(TimeUs(5))), 1);
    }
}

#[test]
fn condensing_keeps_extremes_and_carries_values() {
    let a = Axis {
        origin: TimeUs(0),
        end: TimeUs(288_000),
    };
    let p = |t: i64, v: i64| GaugePoint {
        time: TimeUs(t),
        value: GaugeValue(v),
        reference: GaugeValue(v),
    };
    let g = GaugeResult {
        start: GaugeValue(50),
        value: GaugeValue(70),
        reference: GaugeValue(70),
        failed_at: None,
        cleared: false,
        // Before the origin, two in column 2, one past the end.
        history: vec![p(-5, 40), p(2_100, 90), p(2_900, 10), p(400_000, 70)],
    };
    let mut cols = [GaugeColumn {
        low: GaugeValue(-1),
        high: GaugeValue(-1),
        last: GaugeValue(-1),
    }; GRAPH_COLUMNS];
    condense(&g, a, &mut cols);
    let c = |low, high, last| GaugeColumn {
        low: GaugeValue(low),
        high: GaugeValue(high),
        last: GaugeValue(last),
    };
    assert_eq!(cols[0], c(40, 50, 40));
    assert_eq!(cols[1], c(40, 40, 40));
    assert_eq!(cols[2], c(10, 90, 10));
    assert_eq!(cols[3], c(10, 10, 10));
    assert_eq!(cols[286], c(10, 10, 10));
    // The point after the end is kept, in the last column.
    assert_eq!(cols[287], c(10, 70, 70));
}

#[test]
fn listing_keeps_the_emphasized_gauge() {
    assert_eq!(listed(2, Some(1)), ([0, 1, 2, 3, 4, 5], 2));
    assert_eq!(listed(6, Some(5)), ([0, 1, 2, 3, 4, 5], 6));
    // Seven: five listed and two more; the emphasized sixth or seventh
    // takes the fifth slot.
    assert_eq!(listed(7, Some(0)), ([0, 1, 2, 3, 4, 5], 5));
    assert_eq!(listed(7, Some(4)), ([0, 1, 2, 3, 4, 5], 5));
    assert_eq!(listed(7, Some(6)), ([0, 1, 2, 3, 6, 5], 5));
    assert_eq!(listed(9, Some(5)), ([0, 1, 2, 3, 5, 5], 5));
    assert_eq!(SHOWN_GAUGES, 6);
}

fn history() -> impl Strategy<Value = (Axis, GaugeResult)> {
    (
        -1_000_i64..1_000,
        0_i64..5_000,
        0_i64..1_000,
        proptest::collection::vec((-2_000_i64..8_000, 0_i64..1_000), 0..60),
    )
        .prop_map(|(origin, span, start, mut points)| {
            points.sort_by_key(|p| p.0);
            let history: Vec<GaugePoint> = points
                .iter()
                .map(|&(t, v)| GaugePoint {
                    time: TimeUs(t),
                    value: GaugeValue(v),
                    reference: GaugeValue(v),
                })
                .collect();
            let value = history.last().map_or(GaugeValue(start), |p| p.value);
            let axis = Axis {
                origin: TimeUs(origin),
                end: TimeUs(origin + span),
            };
            let g = GaugeResult {
                start: GaugeValue(start),
                value,
                reference: value,
                failed_at: None,
                cleared: false,
                history,
            };
            (axis, g)
        })
}

proptest! {
    /// G1: every point lies within its column's range, the first column
    /// holds the start, each column's last value is the last point at or
    /// before it (or the start), and the final one is the end value.
    #[test]
    fn condensed_columns_hold_every_point((axis, g) in history()) {
        let mut cols = [GaugeColumn { low: GaugeValue(0), high: GaugeValue(0), last: GaugeValue(0) }; GRAPH_COLUMNS];
        condense(&g, axis, &mut cols);
        for p in &g.history {
            let c = cols[usize::from(axis.column(p.time))];
            prop_assert!(c.low <= p.value && p.value <= c.high, "{p:?} {c:?}");
        }
        prop_assert!(cols[0].low <= g.start && g.start <= cols[0].high);
        for (i, c) in cols.iter().enumerate() {
            let expected = g
                .history
                .iter()
                .rfind(|p| usize::from(axis.column(p.time)) <= i)
                .map_or(g.start, |p| p.value);
            prop_assert_eq!(c.last, expected);
            prop_assert!(c.low <= c.last && c.last <= c.high);
        }
        prop_assert_eq!(cols[GRAPH_COLUMNS - 1].last, g.value);
    }
}

/// G2: twelve misses fail the survival gauge: its failure column is the
/// one its failure time falls in, its value there is 0 and stays 0; the
/// normal gauge's does not.
#[test]
fn survival_failure_is_marked_and_flat_after() {
    let chart = taps(20);
    let preset = rules::tapestry();
    let end = deadline(&chart, &preset);
    let rec = record(&preset, Vec::new(), EndStatus::PlayedToEnd, end);
    let r = re(&chart, &rec, &preset);
    let axis = Axis {
        origin: TimeUs(0),
        end,
    };
    let s = summary(&r, &preset, axis);
    let failed_at = r.evaluation.gauges[1].failed_at.unwrap();
    assert_eq!(s.gauges[1].unwrap().failed_at, Some(failed_at));
    assert_eq!(s.gauges[0].unwrap().failed_at, None);
    let survival = s.graphs.traces[1].unwrap();
    let col = axis.column(failed_at);
    assert_eq!(survival.failed, Some(col));
    assert_eq!(survival.columns[usize::from(col)].last, GaugeValue::EMPTY);
    assert!(survival.columns[usize::from(col)].high > GaugeValue::EMPTY);
    for c in &survival.columns[usize::from(col) + 1..] {
        assert_eq!(
            (c.low, c.high, c.last),
            (GaugeValue::EMPTY, GaugeValue::EMPTY, GaugeValue::EMPTY)
        );
    }
    for c in &survival.columns[..usize::from(col)] {
        assert!(c.low > GaugeValue::EMPTY);
    }
    assert_eq!(s.graphs.traces[0].unwrap().failed, None);
    assert_eq!(s.graphs.reached, N);
    assert_eq!(s.graphs.no_input_from, None);
    assert_eq!(s.graphs.timing, Timing::NONE);
}

/// G3, G4: an aborted play draws up to its watermark's column; a what-if
/// of a completed play past its recorded watermark marks that column.
#[test]
fn coverage_sets_reached_and_no_input() {
    let chart = taps(4);
    let preset = rules::tapestry();
    let end = deadline(&chart, &preset);
    let axis = Axis {
        origin: TimeUs(0),
        end,
    };
    let w = TimeUs(1_600 * MS);
    let rec = record(&preset, Vec::new(), EndStatus::EndedMidway, w);
    let r = re(&chart, &rec, &preset);
    assert_eq!(r.coverage, Coverage::EndedMidway(w));
    let s = summary(&r, &preset, axis);
    let expected = axis.column(w) + 1;
    assert_eq!(s.graphs.reached, expected);
    assert!(expected < N);
    // Played to the end, the watermark recorded early: a what-if runs past
    // it.
    let rec = record(&preset, Vec::new(), EndStatus::PlayedToEnd, w);
    let other = RulePreset {
        id: "other".to_owned(),
        ..rules::tapestry()
    };
    let r = re(&chart, &rec, &other);
    assert_eq!(r.coverage, Coverage::NoInputAfter(w));
    let s = summary(&r, &other, axis);
    assert_eq!(s.graphs.reached, N);
    assert_eq!(s.graphs.no_input_from, Some(expected - 1));
}

/// G5, units: early, late and on time by judgment, the bins and both
/// sides beyond them; a miss, an empty POOR and a held head's feedback
/// count nothing.
#[test]
fn timing_counts_the_judged_presses() {
    use eval::{JudgeEvent, JudgeEventKind};
    let judged = |judgment, d: Option<i64>| JudgeEvent {
        seq: 0,
        time: TimeUs(0),
        origin: None,
        kind: JudgeEventKind::Judged {
            lane: lane(1),
            note: 0,
            judgment,
            deviation: d.map(TimeUs),
            release: None,
            combo: 0,
        },
    };
    let events = [
        judged(Judgment::Perfect, Some(-1)),
        judged(Judgment::Perfect, Some(0)),
        judged(Judgment::Perfect, Some(1_999)),
        judged(Judgment::Great, Some(-64_000)),
        judged(Judgment::Great, Some(-64_001)),
        judged(Judgment::Good, Some(63_999)),
        judged(Judgment::Bad, Some(64_000)),
        judged(Judgment::Poor, Some(150_000)),
        judged(Judgment::Poor, None),
        JudgeEvent {
            kind: JudgeEventKind::EmptyPoor { lane: lane(1) },
            ..judged(Judgment::Poor, None)
        },
        JudgeEvent {
            kind: JudgeEventKind::LnHeld {
                lane: lane(1),
                note: 0,
                judgment: Judgment::Perfect,
                deviation: TimeUs(-5_000),
            },
            ..judged(Judgment::Poor, None)
        },
    ];
    let t = Timing::of(&events);
    let per = |perfect, great, good, bad, poor| PerJudgment {
        perfect,
        great,
        good,
        bad,
        poor,
    };
    assert_eq!(t.early, per(1, 2, 0, 0, 0));
    assert_eq!(t.late, per(1, 0, 1, 1, 1));
    assert_eq!(t.exact, 1);
    assert_eq!(t.earlier, 1);
    assert_eq!(t.later, 2);
    let mut bins = [0; 64];
    bins[0] = 1; // −64 ms
    bins[31] = 1; // −1 µs
    bins[32] = 2; // 0 and 1.999 ms
    bins[63] = 1; // 63.999 ms
    assert_eq!(t.bins, bins);
    assert_eq!(t.samples(), 8);
    assert_eq!(game::press_deviations(&events).count(), 8);
}

/// G5 through a re-evaluation: a long note pressed 20 ms early and held
/// through its end counts once, early, under its judgment at the end; a
/// tap 5 ms late counts late; a missed tap and an empty POOR count nothing.
#[test]
fn timing_counts_a_long_note_once_by_its_head() {
    let chart = ChartBuilder::new(150_000)
        .long(lane(1), TimeUs(1_000 * MS), TimeUs(1_500 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .tap(lane(3), TimeUs(3_000 * MS))
        .build()
        .unwrap();
    let preset = rules::tapestry();
    let events = vec![
        press(980 * MS, 0, lane(1)),
        release(1_600 * MS, lane(1)),
        press(2_005 * MS, 0, lane(2)),
        release(2_050 * MS, lane(2)),
    ];
    let end = deadline(&chart, &preset);
    let rec = record(&preset, events, EndStatus::PlayedToEnd, end);
    let r = re(&chart, &rec, &preset);
    let t = Timing::of(&r.evaluation.events);
    let judged = common::judgments(&r.evaluation.events);
    assert_eq!(judged.len(), 3);
    let (_, _, ln, ln_dev) = judged[0];
    assert_eq!(ln_dev, Some(-20 * MS));
    assert_eq!(t.samples(), 2);
    assert_eq!(t.early.get(ln), &1);
    assert_eq!(
        t.early.perfect + t.early.great + t.early.good + t.early.bad + t.early.poor,
        1
    );
    assert_eq!(
        t.late.perfect + t.late.great + t.late.good + t.late.bad + t.late.poor,
        1
    );
    assert_eq!(t.exact, 0);
    assert_eq!(t.bins[(64_000 - 20_000) / 2_000], 1);
    assert_eq!(t.bins[(64_000 + 5_000) / 2_000], 1);
}

/// G6, G8: the play's own result counts the presses the recommended offset
/// reads, and emphasizes the gauge the play displayed, by ID.
#[test]
fn own_timing_matches_the_recommendation_and_emphasis_follows_the_display() {
    let chart = taps(40);
    let preset = rules::tapestry();
    // Alternately 3 ms early and 4 ms late.
    let mut events = Vec::new();
    for i in 0..40 {
        let t = 1_000 * MS + i * 500 * MS + if i % 2 == 0 { -3 * MS } else { 4 * MS };
        events.push(press(t, 0, lane(1)));
        events.push(release(t + 50 * MS, lane(1)));
    }
    let end = deadline(&chart, &preset);
    let rec = record(&preset, events, EndStatus::PlayedToEnd, end);
    let ctx = ResultContext {
        record: &rec,
        chart: &chart,
        preset: &preset,
        live: &[],
    };
    let result = ReEvaluate.on_play_end(&ctx);
    let offset = result.recommended_offset.unwrap();
    assert_eq!(offset.samples, 40);
    let tally = result.evaluation.as_ref().unwrap().tally;
    let report = game::PlayReport {
        attempt: 1,
        end: game::PlayEnd::Completed,
        tail_cut: None,
        reload_error: None,
        output_error: None,
        input_error: None,
        live: Vec::new(),
        tally,
        displayed_gauge: GaugeIndex(1),
        record: Some(rec),
        saved: None,
        result: Some(result),
        best: None,
        preset: preset.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    // A user preset with the gauges in the other order, and one without
    // the displayed gauge's ID.
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let mut swapped = preset.clone();
    swapped.id = "swapped".to_owned();
    swapped.gauges.reverse();
    std::fs::write(presets.join("swapped.toml"), game::preset_to_toml(&swapped)).unwrap();
    let mut renamed = preset.clone();
    renamed.id = "renamed".to_owned();
    renamed.gauges[1].id = "other".to_owned();
    std::fs::write(presets.join("renamed.toml"), game::preset_to_toml(&renamed)).unwrap();
    let shelf = game::Shelf::open(presets, "tapestry").unwrap();
    let mut screen = ResultScreen::new(&report, &shelf, &chart, game::RECORDING_MEMORY);
    let ViewStatus::Ready(own) = screen.status() else {
        panic!("{:?}", screen.status());
    };
    assert_eq!(own.graphs.timing.samples(), 40);
    assert_eq!(
        own.graphs.timing.early.perfect + own.graphs.timing.early.great,
        20
    );
    assert_eq!(own.graphs.emphasized, Some(1));
    let mut emphasized = Vec::new();
    for _ in 0..screen.entries().len() {
        screen.step(1, &report, &chart);
        if let ViewStatus::Ready(s) = screen.status() {
            let name = screen.viewed().unwrap().name.clone();
            emphasized.push((name, s.graphs.emphasized));
        }
    }
    assert!(emphasized.contains(&("swapped".to_owned(), Some(0))));
    assert!(emphasized.contains(&("renamed".to_owned(), Some(0))));
    assert!(emphasized.contains(&("tapestry".to_owned(), Some(1))));
}

/// The tapestry preset named `id` with BAD's late edge at `late_ms`, so
/// its final deadline moves with it.
fn late_edge(id: &str, late_ms: i64) -> RulePreset {
    let mut p = rules::tapestry();
    p.id = id.to_owned();
    for t in [&mut p.tables.keys, &mut p.tables.scratch] {
        t.windows.bad.late = TimeUs(late_ms * MS);
    }
    p
}

/// Without an evaluation of its own (the record names another preset),
/// the screen's axis is fixed by the first what-if, and a later what-if
/// whose final deadline differs is condensed on that same axis.
#[test]
fn the_first_what_if_fixes_the_axis_without_an_own_evaluation() {
    let chart = taps(4);
    let preset = rules::tapestry();
    let events: Vec<_> = (0..4)
        .flat_map(|i| {
            let t = 1_000 * MS + i * 500 * MS + 2 * MS;
            [press(t, 0, lane(1)), release(t + 50 * MS, lane(1))]
        })
        .collect();
    let end = deadline(&chart, &preset);
    let rec = record(&preset, events, EndStatus::PlayedToEnd, end);
    let tally = re(&chart, &rec, &preset).evaluation.tally;
    let report = game::PlayReport {
        attempt: 1,
        end: game::PlayEnd::Completed,
        tail_cut: None,
        reload_error: None,
        output_error: None,
        input_error: None,
        live: Vec::new(),
        tally,
        displayed_gauge: GaugeIndex(0),
        record: Some(rec),
        saved: None,
        result: Some(game::ResultSummary {
            evaluation: Err(game::ReEvaluationError::PresetMismatch),
            matches_live: false,
            recommended_offset: None,
        }),
        best: None,
        preset: preset.clone(),
    };
    let dir = tempfile::tempdir().unwrap();
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    for (name, ms) in [("early", 110), ("late", 220)] {
        let toml = game::preset_to_toml(&late_edge(name, ms));
        std::fs::write(presets.join(format!("{name}.toml")), toml).unwrap();
    }
    let shelf = game::Shelf::open(presets, "tapestry").unwrap();
    let mut screen = ResultScreen::new(&report, &shelf, &chart, game::RECORDING_MEMORY);
    assert!(matches!(screen.status(), ViewStatus::Failed(_)));
    assert_eq!(screen.axis(), None);
    let mut step_to = |name: &str| {
        while screen.viewed().unwrap().name != name {
            screen.step(-1, &report, &chart);
        }
        let ViewStatus::Ready(s) = screen.status() else {
            panic!("{:?}", screen.status());
        };
        (s.clone(), screen.axis().unwrap())
    };
    // From tapestry back one: `late` first, then `early`.
    let (_, axis) = step_to("late");
    let late_end = deadline(&chart, &late_edge("late", 220));
    assert_eq!(
        axis,
        Axis {
            origin: TimeUs(0),
            end: late_end,
        }
    );
    let (early, still) = step_to("early");
    assert_eq!(still, axis);
    let ctx = ResultContext {
        record: report.record.as_ref().unwrap(),
        chart: &chart,
        preset: &preset,
        live: &[],
    };
    let r = ReEvaluate::under(&ctx, &late_edge("early", 110)).unwrap();
    assert!(r.evaluation.final_deadline < late_end);
    let mut on_axis = [early.graphs.traces[0].unwrap().columns[0]; GRAPH_COLUMNS];
    let mut on_own = on_axis;
    condense(&r.evaluation.gauges[0], axis, &mut on_axis);
    let own_axis = Axis {
        end: r.evaluation.final_deadline,
        ..axis
    };
    condense(&r.evaluation.gauges[0], own_axis, &mut on_own);
    assert_eq!(early.graphs.traces[0].unwrap().columns, on_axis);
    assert_ne!(on_axis, on_own);
}
