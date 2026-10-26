//! Properties of the evaluator over generated charts, presets, input logs
//! and delivery schedules (design.md "Testing Strategy").
//!
//! Live and batch evaluation share `merge` and `Evaluator`, so these
//! properties check what does not depend on how input was delivered; the
//! hand-calculated examples in the other test files check the values
//! themselves. `generators_reach_the_fragile_spots` checks that the
//! generators still reach the cases the properties need, so a generator
//! change cannot make a property vacuous.

// A crate root's modules sit beside it, so the paths name the directory
// that keeps them out of Cargo's test discovery.
mod common;
#[path = "props/live.rs"]
mod live;
#[path = "props/strategy.rs"]
mod strategy;

use std::cmp::Reverse;
use std::collections::BTreeMap;

use bms_model::TimeUs;
use eval::{
    EvalInput, Evaluation, Evaluator, JudgeEventKind, LnOutcome, PhysicalInputEvent, SourceId,
    Staged, evaluate, evaluation_time, merge,
};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rules::{GaugeKind, GaugeValue, RulePreset};

use live::{Delivery, Live, live};
use strategy::{
    Play, build_delivery, build_play, build_widening, end_of, interleave, raw_delivery, raw_play,
    raw_widening,
};

/// A generated play and 1 to 4 delivery schedules for it.
fn play_with_deliveries() -> impl Strategy<Value = (Play, Vec<Delivery>)> {
    (raw_play(), vec(raw_delivery(), 1..=4)).prop_map(|(raw, deliveries)| {
        let play = build_play(&raw);
        let deliveries = deliveries
            .iter()
            .map(|d| build_delivery(&play, d))
            .collect();
        (play, deliveries)
    })
}

fn play() -> impl Strategy<Value = Play> {
    raw_play().prop_map(|raw| build_play(&raw))
}

/// The generator's own checks: a generator bug fails here instead of
/// being skipped.
fn check_play(play: &Play) -> Result<(), TestCaseError> {
    prop_assert_eq!(play.preset.validate(), Ok(()));
    prop_assert_eq!(play.chart.validate(), Ok(()));
    for log in &play.logs {
        prop_assert!(log.is_sorted_by_key(|e| e.received));
    }
    Ok(())
}

fn eval_ok(play: &Play, input: &EvalInput) -> Result<Evaluation, TestCaseError> {
    evaluate(&play.chart, input, &play.preset)
        .map_err(|e| TestCaseError::fail(format!("evaluate: {e:?}")))
}

/// P2 (d): every evaluation numbers its events from 0 without gaps, in
/// time order.
fn check_stream(e: &Evaluation) -> Result<(), TestCaseError> {
    for (i, ev) in e.events.iter().enumerate() {
        prop_assert_eq!(ev.seq, u64::try_from(i).unwrap());
    }
    prop_assert!(e.events.is_sorted_by_key(|ev| ev.time));
    Ok(())
}

/// P1 for one schedule: the live run equals batch evaluation finalized at
/// the terminal watermark, and, once that watermark is past every deadline
/// and input, batch evaluation of the whole log.
fn check_live(play: &Play, delivery: &Delivery) -> Result<Live, TestCaseError> {
    let run =
        live(&play.chart, &play.preset, &play.input, delivery).map_err(TestCaseError::fail)?;
    let t = delivery.terminal;
    let at_t = EvalInput {
        finalized: Some(t),
        ..play.input.clone()
    };
    let batch = eval_ok(play, &at_t)?;
    check_stream(&batch)?;
    prop_assert_eq!(run.watermarks.last(), Some(&t));
    prop_assert_eq!(&run.events, &batch.events);
    prop_assert_eq!(run.tally, batch.tally);
    prop_assert_eq!(run.gauges.len(), batch.gauges.len());
    for (l, b) in run.gauges.iter().zip(&batch.gauges) {
        prop_assert_eq!(
            (l.value, l.reference, l.failed_at),
            (b.value, b.reference, b.failed_at)
        );
    }
    prop_assert_eq!(run.complete, batch.completed);
    if t >= end_of(play) {
        let whole = eval_ok(play, &play.input)?;
        prop_assert_eq!(&whole, &batch);
    }
    Ok(run)
}

/// P3 and P4 on one evaluation.
fn check_gauges(preset: &RulePreset, e: &Evaluation) -> Result<(), TestCaseError> {
    prop_assert_eq!(preset.gauges.len(), e.gauges.len());
    for (rule, g) in preset.gauges.iter().zip(&e.gauges) {
        let bounded = rule.min..=rule.max;
        let points = g
            .history
            .iter()
            .map(|p| (p.value, p.reference))
            .chain([(g.value, g.reference)]);
        for (value, reference) in points {
            prop_assert!(bounded.contains(&value), "{} value {value:?}", rule.id);
            prop_assert!(
                bounded.contains(&reference),
                "{} reference {reference:?}",
                rule.id
            );
        }
        match rule.kind {
            GaugeKind::EndThreshold { .. } => {
                prop_assert!(g.history.iter().all(|p| p.value == p.reference));
                prop_assert_eq!(g.value, g.reference);
                prop_assert_eq!(g.failed_at, None);
            }
            GaugeKind::Survival => {
                let f = g.history.iter().position(|p| p.value == GaugeValue::EMPTY);
                let (before, after) = g.history.split_at(f.unwrap_or(g.history.len()));
                prop_assert!(before.iter().all(|p| p.value == p.reference));
                prop_assert!(after.iter().all(|p| p.value == GaugeValue::EMPTY));
                prop_assert_eq!(g.failed_at, after.first().map(|p| p.time));
                if f.is_some() {
                    prop_assert_eq!(g.value, GaugeValue::EMPTY);
                    prop_assert!(!g.cleared);
                }
            }
        }
    }
    Ok(())
}

proptest! {
    /// P1: any legal delivery schedule ending in a common terminal
    /// watermark gives the batch result.
    #[test]
    fn live_matches_batch_for_every_delivery((play, deliveries) in play_with_deliveries()) {
        check_play(&play)?;
        for d in &deliveries {
            check_live(&play, d)?;
        }
    }

    /// P2 (a): the recording order across sources does not matter, only
    /// each source's own order.
    #[test]
    fn recording_interleaving_does_not_matter(
        (play, other) in (play(), vec(any::<u8>(), 0..=80)),
    ) {
        check_play(&play)?;
        let reordered = EvalInput {
            events: interleave(&play.logs, &other),
            ..play.input.clone()
        };
        let a = eval_ok(&play, &play.input)?;
        let b = eval_ok(&play, &reordered)?;
        check_stream(&a)?;
        prop_assert_eq!(a, b);
    }

    /// P2 (c): `merge` emits exactly the staged events at or before its
    /// bound, ordered by (evaluation time, source, order within the
    /// source), whatever the order of the staged slice, and keeps the rest.
    #[test]
    fn merge_orders_by_time_then_source(
        (play, order, bound) in (play(), any::<[u8; 3]>(), any::<(u16, u8)>()),
    ) {
        let device = &play.input.device;
        let n = play.logs.len();
        let mut slice: Vec<usize> = (0..n).collect();
        slice.sort_by_key(|&i| (order[i], i));
        let mut staged: Vec<Staged> = slice
            .iter()
            .map(|&i| {
                let mut s = Staged::with_capacity(SourceId(u16::try_from(i).unwrap()), play.logs[i].len());
                for &e in &play.logs[i] {
                    s.push(e).unwrap();
                }
                s
            })
            .collect();
        let t = play.times[usize::from(bound.0) % play.times.len()];
        let bound = TimeUs(t.0 + [0, -1, 1][usize::from(bound.1 % 3)]);
        let mut out = Vec::new();
        merge(&mut staged, device, bound, &mut out);
        let mut rest = Vec::new();
        merge(&mut staged, device, TimeUs(i64::MAX), &mut rest);
        prop_assert!(staged.iter().all(Staged::is_empty));
        let key = |e: &PhysicalInputEvent| (evaluation_time(e, device), e.source);
        prop_assert!(out.iter().all(|e| evaluation_time(e, device) <= bound));
        prop_assert!(rest.iter().all(|e| evaluation_time(e, device) > bound));
        prop_assert!(out.is_sorted_by_key(key));
        prop_assert!(rest.is_sorted_by_key(key));
        for (i, log) in play.logs.iter().enumerate() {
            let of = |v: &[PhysicalInputEvent]| {
                v.iter().filter(|e| e.source.index() == i).copied().collect::<Vec<_>>()
            };
            prop_assert_eq!([of(&out), of(&rest)].concat(), log.clone());
        }
    }

    /// P3 and P4: gauges stay within their bounds, and a survival gauge
    /// that reaches 0 stays failed from then on.
    #[test]
    fn gauges_stay_in_range_and_survival_failure_is_permanent(play in play()) {
        check_play(&play)?;
        let whole = eval_ok(&play, &play.input)?;
        check_stream(&whole)?;
        check_gauges(&play.preset, &whole)?;
    }

    /// P5: widening only the judgment windows never worsens a note's
    /// judgment when each lane holds one judgeable note and gets at most
    /// one press.
    #[test]
    fn widening_the_windows_never_worsens_a_judgment(raw in raw_widening()) {
        let w = build_widening(&raw);
        prop_assert_eq!(w.narrow.validate(), Ok(()));
        prop_assert_eq!(w.wide.validate(), Ok(()));
        let run = |p: &RulePreset| {
            evaluate(&w.chart, &w.input, p).map_err(|e| TestCaseError::fail(format!("{e:?}")))
        };
        let (narrow, wide) = (run(&w.narrow)?, run(&w.wide)?);
        prop_assert!(narrow.completed && wide.completed);
        let notes: usize = w.chart.lanes.iter().map(Vec::len).sum();
        let (jn, jw) = (judgments(&narrow), judgments(&wide));
        prop_assert_eq!(jn.len(), notes);
        prop_assert_eq!(jw.len(), notes);
        for ((ln, a), (lw, b)) in jn.iter().zip(&jw) {
            prop_assert_eq!(ln, lw);
            prop_assert!(b <= a, "lane {} worsened from {:?} to {:?}", ln, a, b);
        }
    }
}

/// Each judged note's lane and judgment, sorted by lane (one note per
/// lane in the widening play).
fn judgments(e: &Evaluation) -> Vec<(usize, rules::Judgment)> {
    let mut v: Vec<_> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged { lane, judgment, .. } => Some((lane.index(), judgment)),
            _ => None,
        })
        .collect();
    v.sort_unstable_by_key(|&(l, _)| l);
    v
}

/// How many samples reached each fragile spot.
#[derive(Debug, Default)]
struct Coverage(BTreeMap<&'static str, usize>);

impl Coverage {
    fn hit(&mut self, spot: &'static str, reached: bool) {
        *self.0.entry(spot).or_default() += usize::from(reached);
    }

    fn get(&self, spot: &str) -> usize {
        self.0.get(spot).copied().unwrap_or(0)
    }
}

/// The play's events evaluated with ties across sources taken higher
/// source first: different from the real result only where the order of
/// a tie shows.
fn reversed_ties(play: &Play) -> Vec<eval::JudgeEvent> {
    let device = &play.input.device;
    let mut order: Vec<(TimeUs, Reverse<SourceId>, usize, PhysicalInputEvent)> = play
        .logs
        .iter()
        .flat_map(|log| log.iter().enumerate())
        .map(|(k, e)| (evaluation_time(e, device), Reverse(e.source), k, *e))
        .collect();
    order.sort_unstable_by_key(|&(t, s, k, _)| (t, s, k));
    let mut ev = Evaluator::new(
        &play.chart,
        &play.preset,
        device,
        &play.input.start,
        order.len(),
    )
    .unwrap();
    for &(.., e) in &order {
        ev.push_input(e).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(end_of(play), &mut out).unwrap();
    out
}

fn count_play(play: &Play, c: &mut Coverage) {
    let device = &play.input.device;
    let times: Vec<(TimeUs, SourceId)> = play
        .input
        .events
        .iter()
        .map(|e| (evaluation_time(e, device), e.source))
        .collect();
    c.hit(
        "cross_source_tie",
        times
            .iter()
            .any(|&(t, s)| times.iter().any(|&(u, r)| t == u && s != r)),
    );
    c.hit(
        "input_on_deadline",
        times
            .iter()
            .any(|(t, _)| play.deadlines.binary_search(t).is_ok()),
    );
    let whole = evaluate(&play.chart, &play.input, &play.preset).unwrap();
    c.hit("joint_tie_order_shows", reversed_ties(play) != whole.events);
    let has = |f: &dyn Fn(&JudgeEventKind) -> bool| whole.events.iter().any(|e| f(&e.kind));
    c.hit(
        "mine_hit",
        has(&|k| matches!(k, JudgeEventKind::MineHit { .. })),
    );
    c.hit(
        "ln_break",
        has(&|k| {
            matches!(
                k,
                JudgeEventKind::LnEnd {
                    outcome: LnOutcome::Broken,
                    ..
                }
            )
        }),
    );
    c.hit(
        "empty_poor",
        has(&|k| matches!(k, JudgeEventKind::EmptyPoor { .. })),
    );
    c.hit(
        "play_sound",
        has(&|k| matches!(k, JudgeEventKind::PlaySound { .. })),
    );
    // A scratch release deadline finishing a long note before its end.
    c.hit(
        "scratch_release",
        whole.events.iter().any(|e| {
            e.origin.is_none()
                && matches!(e.kind, JudgeEventKind::LnEnd { lane, note, .. }
                if play.chart.lanes[lane.index()][note].end() != e.time)
        }),
    );
    c.hit(
        "joint_input_after_final_deadline",
        whole.events.iter().any(|e| {
            e.time > play.final_deadline && matches!(e.kind, JudgeEventKind::EmptyPoor { .. })
        }),
    );
    for (rule, g) in play.preset.gauges.iter().zip(&whole.gauges) {
        c.hit(
            "gauge_at_max",
            g.history.iter().any(|p| p.value == rule.max),
        );
        c.hit(
            "gauge_at_positive_min",
            rule.min > GaugeValue::EMPTY && g.history.iter().any(|p| p.value == rule.min),
        );
        // A survival failure followed by a change that would have raised
        // the gauge.
        let failed = g.history.iter().position(|p| p.value == GaugeValue::EMPTY);
        c.hit(
            "survival_failed_then_rose",
            rule.kind == GaugeKind::Survival
                && failed.is_some_and(|f| {
                    g.history[f + 1..]
                        .iter()
                        .any(|p| p.reference > GaugeValue::EMPTY)
                }),
        );
    }
}

fn count_live(play: &Play, d: &Delivery, run: &Live, c: &mut Coverage) {
    let s = run.stats;
    c.hit("input_at_w", s.inputs_at_w > 0);
    c.hit("queue_full", s.queue_full > 0);
    c.hit("popped_beyond", s.popped_beyond > 0);
    c.hit("held_back", s.held_back > 0);
    c.hit("late_pushed", s.late_pushed > 0);
    c.hit("disconnect", s.disconnects > 0);
    c.hit("open_at_disconnect", s.open_at_disconnect > 0);
    c.hit("retirement", s.retirements > 0);
    c.hit(
        "terminal_before_final_deadline",
        d.terminal < play.final_deadline,
    );
    // An effective input whose evaluation time is a deadline and the W of
    // a step that is not the last.
    let mut from = 0;
    let joint = run.watermarks.iter().zip(&run.step_ends).any(|(&w, &to)| {
        let events = &run.events[from..to];
        from = to;
        w < d.terminal
            && play.deadlines.binary_search(&w).is_ok()
            && events.iter().any(|e| e.origin.is_some() && e.time == w)
    });
    c.hit("joint_input_on_deadline_at_w", joint);
}

/// The fragile spots each property depends on, reached by the generators
/// over a fixed number of deterministic samples.
#[test]
fn generators_reach_the_fragile_spots() {
    const SAMPLES: usize = 512;
    // Random generation alone reaches each joint spot about ten times or
    // fewer in these samples; its generator branch lifts it well past this
    // bound, so losing the branch fails here.
    const JOINT: usize = 32;
    let mut runner = TestRunner::deterministic();
    let mut c = Coverage::default();
    let plays = play_with_deliveries();
    for _ in 0..SAMPLES {
        let (play, deliveries) = plays.new_tree(&mut runner).unwrap().current();
        count_play(&play, &mut c);
        for d in &deliveries {
            let run = live(&play.chart, &play.preset, &play.input, d).unwrap();
            count_live(&play, d, &run, &mut c);
        }
    }
    let widenings = raw_widening();
    for _ in 0..SAMPLES {
        let w = build_widening(&widenings.new_tree(&mut runner).unwrap().current());
        let narrow = judgments(&evaluate(&w.chart, &w.input, &w.narrow).unwrap());
        let wide = judgments(&evaluate(&w.chart, &w.input, &w.wide).unwrap());
        c.hit(
            "widening_improved",
            narrow.iter().zip(&wide).any(|(a, b)| b.1 < a.1),
        );
    }
    let bounds = [
        ("cross_source_tie", 1),
        ("input_on_deadline", 1),
        ("input_at_w", 1),
        ("queue_full", 1),
        ("popped_beyond", 1),
        ("held_back", 1),
        ("late_pushed", 1),
        ("disconnect", 1),
        ("open_at_disconnect", 1),
        ("retirement", 1),
        ("terminal_before_final_deadline", 1),
        ("scratch_release", 1),
        ("mine_hit", 1),
        ("ln_break", 1),
        ("empty_poor", 1),
        ("play_sound", 1),
        ("gauge_at_max", 1),
        ("gauge_at_positive_min", 1),
        ("survival_failed_then_rose", 1),
        ("widening_improved", 1),
        ("joint_input_on_deadline_at_w", JOINT),
        ("joint_input_after_final_deadline", JOINT),
        ("joint_tie_order_shows", JOINT),
    ];
    let short: Vec<_> = bounds
        .iter()
        .filter(|&&(spot, min)| c.get(spot) < min)
        .collect();
    assert!(
        short.is_empty(),
        "below their bounds: {short:?}\nall: {:?}",
        c.0
    );
}
