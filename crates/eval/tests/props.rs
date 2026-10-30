//! Properties of the evaluator over generated charts, presets, input logs
//! and delivery schedules (design.md "Testing Strategy").
//!
//! Live and batch evaluation share `merge` and `Evaluator`, so these
//! properties check what does not depend on how input was delivered; the
//! hand-calculated examples in the other test files check the values
//! themselves. Generated plays often evaluate a section, and
//! `section_excerpt_matches_mid_chart_start` checks a section excerpted
//! from a full log against a mid-chart start.
//! `generators_reach_the_fragile_spots` checks that the generators still
//! reach the cases the properties need, so a generator change cannot make
//! a property vacuous.

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
    ConversionState, EvalInput, Evaluation, Evaluator, JudgeEventKind, PhysicalInputEvent,
    SourceId, Staged, evaluate, evaluation_time, merge,
};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rules::{
    DefaultTotal, GaugeKind, GaugeValue, HeaderPolicy, MineRule, RulePreset, TotalCorrection,
    WindowPolicy,
};

use live::{Delivery, Live, live};
use strategy::{
    Excerpt, Play, build_delivery, build_excerpt, build_held_through, build_play, build_widening,
    end_of, interleave, is_target, raw_delivery, raw_excerpt, raw_held_through, raw_play,
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
            GaugeKind::Survival { fails_below } => {
                let f = g.history.iter().position(|p| p.value == GaugeValue::EMPTY);
                let (before, after) = g.history.split_at(f.unwrap_or(g.history.len()));
                prop_assert!(before.iter().all(|p| p.value == p.reference));
                // Alive means at or above the threshold.
                prop_assert!(before.iter().all(|p| p.value >= fails_below));
                // It fails where the reference first goes 0 or below it.
                prop_assert!(
                    after.first().is_none_or(
                        |p| p.reference <= GaugeValue::EMPTY || p.reference < fails_below
                    )
                );
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

/// The long-note rules on one completed evaluation: every judgeable note
/// (of the section, if any) has exactly one judgment, and only a
/// judgment, an empty POOR or a mine hit changes the gauges. A held head comes at most once per long note,
/// before its judgment, with the judgment's deviation and never worse
/// than it; while it is held, its lane sees no press. Every long note whose
/// head a press took before its end (held, or a BAD) ends its visual hold
/// exactly once, at or before its end and before a held one's judgment. A
/// release distance is given only for a long note, and never positive.
/// Under [`rules::HeadEligibility::NotLateBad`], no head is taken late BAD:
/// a long note judged with a head deviation and no release is not BAD on
/// the late side.
fn check_long_notes(play: &Play, e: &Evaluation) -> Result<(), TestCaseError> {
    use eval::JudgeEventKind as K;
    use std::collections::BTreeSet;
    prop_assert!(e.completed);
    let note_of = |lane: bms_model::Lane, note: usize| play.chart.lanes[lane.index()][note];
    let mut judged = BTreeSet::new();
    let mut held: BTreeMap<(usize, usize), (rules::Judgment, TimeUs)> = BTreeMap::new();
    let mut released = BTreeSet::new();
    // Long notes whose head a press took before their end.
    let mut visual = BTreeSet::new();
    // Per lane, the long note held and not yet judged.
    let mut holding: [Option<usize>; bms_model::LANE_COUNT] = [None; bms_model::LANE_COUNT];
    let mut scoring = false;
    for ev in &e.events {
        if let K::Gauge { .. } = ev.kind {
            prop_assert!(scoring, "a gauge change after {:?}", ev);
            continue;
        }
        scoring = matches!(
            ev.kind,
            K::Judged { .. } | K::EmptyPoor { .. } | K::MineHit { .. }
        );
        match ev.kind {
            K::Judged {
                lane,
                note,
                judgment,
                deviation,
                release,
                ..
            } => {
                let l = lane.index();
                prop_assert!(judged.insert((l, note)), "judged twice: {:?}", ev);
                let long = matches!(note_of(lane, note).kind, bms_model::NoteKind::Long { .. });
                prop_assert!(release.is_none() || long);
                prop_assert!(release.is_none_or(|r| r.0 <= 0));
                prop_assert!(
                    !(long
                        && play.preset.ln_mode.head_eligibility
                            == rules::HeadEligibility::NotLateBad
                        && release.is_none()
                        && deviation.is_some_and(|d| d.0 > 0)
                        && judgment == rules::Judgment::Bad),
                    "a late BAD head taken: {:?}",
                    ev
                );
                if long
                    && deviation.is_some()
                    && release.is_none()
                    && ev.time < note_of(lane, note).end()
                    && !held.contains_key(&(l, note))
                {
                    visual.insert((l, note));
                }
                if let Some(&(head, d)) = held.get(&(l, note)) {
                    prop_assert!(head <= judgment, "{:?} held, then {:?}", head, ev);
                    prop_assert_eq!(deviation, Some(d));
                    prop_assert!(
                        released.contains(&(l, note)),
                        "judged before its hold ended: {:?}",
                        ev
                    );
                    prop_assert_eq!(holding[l], Some(note));
                    holding[l] = None;
                } else {
                    prop_assert!(holding[l].is_none(), "a press while held: {:?}", ev);
                }
            }
            K::LnHeld {
                lane,
                note,
                judgment,
                deviation,
            } => {
                let l = lane.index();
                prop_assert!(holding[l].is_none(), "a press while held: {:?}", ev);
                prop_assert!(held.insert((l, note), (judgment, deviation)).is_none());
                visual.insert((l, note));
                prop_assert!(judgment < rules::Judgment::Bad);
                prop_assert!(ev.time < note_of(lane, note).end());
                holding[l] = Some(note);
            }
            K::LnHoldEnded { lane, note } => {
                prop_assert!(released.insert((lane.index(), note)));
                prop_assert!(visual.contains(&(lane.index(), note)), "{:?}", ev);
                prop_assert!(ev.time <= note_of(lane, note).end());
            }
            K::EmptyPoor { lane } | K::PlaySound { lane, .. } => {
                prop_assert!(
                    holding[lane.index()].is_none(),
                    "a press while held: {:?}",
                    ev
                );
            }
            K::MineHit { .. } | K::Gauge { .. } | K::SongEnd => {}
        }
    }
    let notes: BTreeSet<(usize, usize)> = play
        .chart
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(l, notes)| {
            notes.iter().enumerate().filter_map(move |(i, n)| {
                (!matches!(n.kind, bms_model::NoteKind::Mine { .. })
                    && is_target(play.input.section.as_ref(), n))
                .then_some((l, i))
            })
        })
        .collect();
    prop_assert_eq!(judged, notes);
    prop_assert_eq!(released, visual);
    Ok(())
}

proptest! {
    /// Long notes: one judgment each, held heads and visual holds as the
    /// rule says, no scoring outside a judgment, and no press while held.
    #[test]
    fn every_judgeable_note_is_judged_once(play in play()) {
        check_play(&play)?;
        let whole = eval_ok(&play, &play.input)?;
        check_long_notes(&play, &whole)?;
    }

    /// A long note pressed inside its scaled BAD window and held at least
    /// as long as it lasts gets exactly its head's judgment, under any
    /// valid preset and rank scale: its release is at or after `h` from
    /// its end, which the LN-end edges grade no worse than the head. A
    /// head the press would grade late BAD is not taken under
    /// [`rules::HeadEligibility::NotLateBad`], and is missed instead.
    #[test]
    fn holding_as_long_as_the_long_note_keeps_the_head_judgment(raw in raw_held_through()) {
        let p = build_held_through(&raw);
        prop_assert_eq!(p.preset.validate(), Ok(()));
        let e = evaluate(&p.chart, &p.input, &p.preset)
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        // The head's judgment by the windows at the chart's rank, by hand.
        let expected = p
            .windows
            .table()
            .into_iter()
            .find(|(_, w)| -w.early.0 <= p.deviation && p.deviation <= w.late.0)
            .map(|(j, _)| j)
            .unwrap();
        let judgments: Vec<_> = e
            .events
            .iter()
            .filter_map(|ev| match ev.kind {
                JudgeEventKind::Judged { judgment, deviation, .. } => Some((judgment, deviation)),
                _ => None,
            })
            .collect();
        let not_taken = p.preset.ln_mode.head_eligibility == rules::HeadEligibility::NotLateBad
            && expected == rules::Judgment::Bad
            && p.deviation > 0;
        if not_taken {
            prop_assert_eq!(judgments, vec![(rules::Judgment::Poor, None)]);
        } else {
            prop_assert_eq!(judgments, vec![(expected, Some(TimeUs(p.deviation)))]);
        }
    }
}

proptest! {
    /// P6: a section excerpted from a full log evaluates as a mid-chart
    /// start whose snapshot leaves the same conversion state at the
    /// lead-in start, with the same prefix and later input: input before
    /// the lead-in judges nothing.
    #[test]
    fn section_excerpt_matches_mid_chart_start(raw in raw_excerpt()) {
        let x = build_excerpt(&raw);
        prop_assert_eq!(x.chart.validate(), Ok(()));
        // The prefix before the lead-in start is never empty.
        let device = &x.full.device;
        prop_assert!(x.mid.events.iter().any(|e| evaluation_time(e, device) < x.lead_in_start));
        let run = |input: &EvalInput| {
            evaluate(&x.chart, input, &x.preset).map_err(|e| TestCaseError::fail(format!("{e:?}")))
        };
        let (full, mid) = (run(&x.full)?, run(&x.mid)?);
        prop_assert!(full.completed);
        check_stream(&full)?;
        prop_assert_eq!(full, mid);
    }
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

    /// P5: widening the judgment windows and the LN-end edges never
    /// worsens a note's judgment when each lane holds one judgeable note
    /// and gets at most one press.
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
        play.input.section.as_ref(),
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

/// The choices a preset makes, one per choice enum.
fn choices(p: &RulePreset) -> impl PartialEq + use<> {
    let MineRule::WhileHeld { .. } = p.mine;
    (
        p.miss,
        p.same_time_order,
        p.assignment,
        p.ln_mode,
        p.total_mode,
        p.denominator,
    )
}

/// Whether the generated preset draws what each standard preset uses: every
/// choice it makes, asymmetric windows, and survival gauges with and
/// without reduced damage.
fn count_preset_shapes(p: &RulePreset, c: &mut Coverage) {
    c.hit(
        "choices_of_custom",
        choices(p) == choices(&rules::tapestry()),
    );
    c.hit(
        "choices_of_beatoraja_style",
        choices(p) == choices(&rules::beatoraja_style()),
    );
    c.hit(
        "choices_of_lr2_style",
        choices(p) == choices(&rules::lr2_style()),
    );
    // LR2-style's whole combination: its choices, the interpolated windows,
    // its header policy and default #TOTAL, and a damage-corrected gauge
    // with a failure threshold.
    let lr2 = rules::lr2_style();
    c.hit(
        "policies_of_lr2_style",
        choices(p) == choices(&lr2)
            && matches!(p.window_policy, WindowPolicy::Interpolated(_))
            && p.headers == lr2.headers
            && p.default_total == lr2.default_total
            && p.gauges.iter().any(|g| {
                g.correction == TotalCorrection::ScaleDamage
                    && matches!(g.kind, GaugeKind::Survival { fails_below } if fails_below > GaugeValue::EMPTY)
            }),
    );
    c.hit("headers_decimals", p.headers == HeaderPolicy::Decimals);
    c.hit(
        "headers_def_ex_rank_first",
        p.headers == HeaderPolicy::DefExRankFirst,
    );
    let bad = p.tables.keys.windows.bad;
    c.hit("asymmetric_bad", bad.early != bad.late);
    c.hit("scratch_own_table", p.tables.keys != p.tables.scratch);
    c.hit(
        "window_policy_thousandths",
        matches!(p.window_policy, WindowPolicy::Thousandths(_)),
    );
    c.hit(
        "window_policy_scaled",
        matches!(p.window_policy, WindowPolicy::Scaled(_)),
    );
    c.hit(
        "window_policy_interpolated",
        matches!(p.window_policy, WindowPolicy::Interpolated(_)),
    );
    let ln_end = p.tables.keys.ln_end;
    c.hit(
        "ln_end_tied",
        ln_end.perfect == ln_end.great && ln_end.great == ln_end.good,
    );
    let survival = |bands: fn(usize) -> bool| {
        p.gauges
            .iter()
            .any(|g| matches!(g.kind, GaugeKind::Survival { .. }) && bands(g.low_damage.len()))
    };
    c.hit("survival_with_low_damage", survival(|n| n == 1));
    c.hit("survival_with_low_damage_bands", survival(|n| n >= 2));
    c.hit("survival_without_low_damage", survival(|n| n == 0));
    c.hit(
        "survival_with_threshold",
        p.gauges.iter().any(
            |g| matches!(g.kind, GaugeKind::Survival { fails_below } if fails_below > GaugeValue::EMPTY),
        ),
    );
    for (spot, correction) in [
        ("correction_none", TotalCorrection::None),
        ("correction_limit_increase", TotalCorrection::LimitIncrease),
        ("correction_scale_damage", TotalCorrection::ScaleDamage),
    ] {
        c.hit(spot, p.gauges.iter().any(|g| g.correction == correction));
    }
    c.hit(
        "default_total_fixed",
        matches!(p.default_total, DefaultTotal::Fixed { .. }),
    );
    c.hit(
        "default_total_notes_curve",
        p.default_total == DefaultTotal::NotesCurve,
    );
    c.hit(
        "default_total_notes_linear",
        p.default_total == DefaultTotal::NotesLinear,
    );
}

fn count_play(play: &Play, c: &mut Coverage) {
    let declared = &play.chart.meta.judgment_headers;
    c.hit(
        "header_policies_differ",
        HeaderPolicy::Decimals.read(declared) != HeaderPolicy::DefExRankFirst.read(declared),
    );
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
    count_preset_shapes(&play.preset, c);
    let whole = evaluate(&play.chart, &play.input, &play.preset).unwrap();
    c.hit("joint_tie_order_shows", reversed_ties(play) != whole.events);
    let has = |f: &dyn Fn(&JudgeEventKind) -> bool| whole.events.iter().any(|e| f(&e.kind));
    c.hit(
        "mine_hit",
        has(&|k| matches!(k, JudgeEventKind::MineHit { .. })),
    );
    // A long note released before its end, judged by its LN-end edges
    // worse than its head.
    c.hit(
        "ln_release_downgrades",
        whole.events.iter().any(|e| match e.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                release: Some(r),
                ..
            } if r.0 < 0 => whole.events.iter().any(|h| {
                matches!(h.kind, JudgeEventKind::LnHeld { lane: hl, note: hn, judgment: hj, .. }
                    if hl == lane && hn == note && hj < judgment)
            }),
            _ => false,
        }),
    );
    // A long note held through its end, judged by its end deadline.
    c.hit(
        "ln_held_through",
        whole.events.iter().any(|e| {
            e.origin.is_none()
                && matches!(e.kind, JudgeEventKind::Judged { lane, note, deviation: Some(_), release: None, .. }
                if play.chart.lanes[lane.index()][note].end() == e.time)
        }),
    );
    c.hit(
        "ln_hold_ended",
        has(&|k| matches!(k, JudgeEventKind::LnHoldEnded { .. })),
    );
    // Under combo priority, a press that passes an unjudged note: a POOR
    // caused by an input.
    c.hit(
        "combo_priority_passes_a_note",
        play.preset.assignment == rules::Assignment::ComboPriority
            && whole.events.iter().any(|e| {
                e.origin.is_some()
                    && matches!(
                        e.kind,
                        JudgeEventKind::Judged {
                            judgment: rules::Judgment::Poor,
                            ..
                        }
                    )
            }),
    );
    // The other LN-head eligibility changes the play: a late BAD head
    // taken under one is passed or left under the other.
    let mut other = play.preset.clone();
    other.ln_mode.head_eligibility = match other.ln_mode.head_eligibility {
        rules::HeadEligibility::InsideBad => rules::HeadEligibility::NotLateBad,
        rules::HeadEligibility::NotLateBad => rules::HeadEligibility::InsideBad,
    };
    c.hit(
        "head_eligibility_shows",
        evaluate(&play.chart, &play.input, &other).unwrap().events != whole.events,
    );
    c.hit(
        "empty_poor",
        has(&|k| matches!(k, JudgeEventKind::EmptyPoor { .. })),
    );
    c.hit(
        "play_sound",
        has(&|k| matches!(k, JudgeEventKind::PlaySound { .. })),
    );
    // A scratch release deadline judging a long note before its end.
    c.hit(
        "scratch_release",
        whole.events.iter().any(|e| {
            e.origin.is_none()
                && matches!(e.kind, JudgeEventKind::Judged { release: Some(r), .. } if r.0 < 0)
        }),
    );
    c.hit(
        "joint_input_after_final_deadline",
        whole.events.iter().any(|e| {
            e.time > play.final_deadline && matches!(e.kind, JudgeEventKind::EmptyPoor { .. })
        }),
    );
    if let Some(s) = &play.input.section {
        count_section(play, s, &whole, c);
    }
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
            matches!(rule.kind, GaugeKind::Survival { .. })
                && failed.is_some_and(|f| {
                    g.history[f + 1..]
                        .iter()
                        .any(|p| p.reference > GaugeValue::EMPTY)
                }),
        );
        // A failure above 0, below the threshold.
        c.hit(
            "survival_failed_below_threshold",
            matches!(rule.kind, GaugeKind::Survival { .. })
                && failed.is_some_and(|f| g.history[f].reference > GaugeValue::EMPTY),
        );
    }
}

/// The section spots of one play: a long note cut by an edge, and lead-in
/// presses judged on the first target's early edge or ignored.
fn count_section(play: &Play, s: &eval::Section, whole: &Evaluation, c: &mut Coverage) {
    let cut = play.chart.lanes.iter().flatten().any(|n| {
        matches!(n.kind, bms_model::NoteKind::Long { .. })
            && ((n.time < s.start && n.end() >= s.start)
                || (s.start <= n.time && n.time < s.end && n.end() >= s.end))
    });
    c.hit("ln_cut_by_section", cut);
    let ev = Evaluator::new(
        &play.chart,
        &play.preset,
        &play.input.device,
        &play.input.start,
        Some(s),
        0,
    )
    .unwrap();
    let lead_in = TimeUs(s.start.0 - s.lead_in.0)..s.start;
    c.hit(
        "lead_in_press_on_early_edge",
        whole.events.iter().any(|e| {
            lead_in.contains(&e.time)
                && matches!(e.kind, JudgeEventKind::Judged { lane, deviation: Some(d), .. }
                    if d.0 == -ev.table(lane).windows.bad.early.0)
        }),
    );
    // A lead-in press the section ignores and a whole-chart play judges or
    // counts as an empty POOR.
    let open = evaluate(
        &play.chart,
        &EvalInput {
            section: None,
            ..play.input.clone()
        },
        &play.preset,
    )
    .unwrap();
    let acted = |e: &Evaluation, t: TimeUs| {
        e.events.iter().any(|ev| {
            ev.time == t
                && ev.origin.is_some()
                && matches!(
                    ev.kind,
                    JudgeEventKind::Judged { .. } | JudgeEventKind::EmptyPoor { .. }
                )
        })
    };
    c.hit(
        "lead_in_press_ignored",
        play.input.events.iter().any(|e| {
            let t = evaluation_time(e, &play.input.device);
            lead_in.contains(&t) && acted(&open, t) && !acted(whole, t)
        }),
    );
}

/// The conversion spots of one excerpt at its lead-in start: a lane held
/// and a scratch release pending there, and input before the prefix.
fn count_excerpt(x: &Excerpt, c: &mut Coverage) {
    let device = &x.mid.device;
    let mut conv = ConversionState::init(device, &x.mid.start).unwrap();
    let mut events: Vec<(TimeUs, PhysicalInputEvent)> = x
        .mid
        .events
        .iter()
        .map(|e| (evaluation_time(e, device), *e))
        .filter(|&(t, _)| t < x.lead_in_start)
        .collect();
    events.sort_by_key(|&(t, e)| (t, e.source));
    let due = |conv: &mut ConversionState, before: TimeUs| {
        while let Some((r, _, axis)) = conv.next_release()
            && r < before
        {
            conv.release_due(axis);
        }
    };
    for (t, e) in events {
        due(&mut conv, t);
        conv.apply(&e, t);
    }
    due(&mut conv, x.lead_in_start);
    c.hit(
        "held_at_lead_in_start",
        bms_model::Lane::all().any(|l| conv.is_held(l)),
    );
    c.hit(
        "release_pending_at_lead_in_start",
        conv.next_release().is_some(),
    );
    c.hit("excerpt_earlier_input", x.earlier > 0);
}

/// Counts the live spots of one delivery and returns whether it reached the
/// joint watermark spot, which counts once per play.
fn count_live(play: &Play, d: &Delivery, run: &Live, c: &mut Coverage) -> bool {
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
    run.watermarks.iter().zip(&run.step_ends).any(|(&w, &to)| {
        let events = &run.events[from..to];
        from = to;
        w < d.terminal
            && play.deadlines.binary_search(&w).is_ok()
            && events.iter().any(|e| e.origin.is_some() && e.time == w)
    })
}

/// The fragile spots each property depends on, reached by the generators
/// over a fixed number of deterministic samples.
#[test]
fn generators_reach_the_fragile_spots() {
    const SAMPLES: usize = 512;
    // Joint spots count once per play. Random generation alone reaches each
    // in about ten plays or fewer of these samples; its generator branch
    // lifts it well past this bound, so losing the branch fails here.
    const JOINT: usize = 32;
    let mut runner = TestRunner::deterministic();
    let mut c = Coverage::default();
    let plays = play_with_deliveries();
    for _ in 0..SAMPLES {
        let (play, deliveries) = plays.new_tree(&mut runner).unwrap().current();
        count_play(&play, &mut c);
        let mut joint = false;
        for d in &deliveries {
            let run = live(&play.chart, &play.preset, &play.input, d).unwrap();
            joint |= count_live(&play, d, &run, &mut c);
        }
        c.hit("joint_input_on_deadline_at_w", joint);
    }
    let excerpts = raw_excerpt();
    for _ in 0..SAMPLES {
        count_excerpt(
            &build_excerpt(&excerpts.new_tree(&mut runner).unwrap().current()),
            &mut c,
        );
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
        ("ln_release_downgrades", 1),
        ("ln_held_through", 1),
        ("ln_hold_ended", 1),
        ("empty_poor", 1),
        ("play_sound", 1),
        ("choices_of_custom", SAMPLES / 8),
        ("choices_of_beatoraja_style", SAMPLES / 8),
        ("choices_of_lr2_style", SAMPLES / 8),
        ("policies_of_lr2_style", 1),
        ("combo_priority_passes_a_note", 1),
        ("head_eligibility_shows", 1),
        ("headers_decimals", 1),
        ("headers_def_ex_rank_first", 1),
        ("header_policies_differ", 1),
        ("asymmetric_bad", 1),
        ("scratch_own_table", 1),
        ("window_policy_thousandths", 1),
        ("window_policy_scaled", 1),
        ("window_policy_interpolated", 1),
        ("ln_end_tied", 1),
        ("survival_with_low_damage", 1),
        ("survival_without_low_damage", 1),
        ("survival_with_low_damage_bands", 1),
        ("survival_with_threshold", 1),
        ("correction_none", 1),
        ("correction_limit_increase", 1),
        ("correction_scale_damage", 1),
        ("default_total_fixed", 1),
        ("default_total_notes_curve", 1),
        ("default_total_notes_linear", 1),
        ("survival_failed_below_threshold", 1),
        ("gauge_at_max", 1),
        ("gauge_at_positive_min", 1),
        ("survival_failed_then_rose", 1),
        ("widening_improved", 1),
        ("ln_cut_by_section", 1),
        ("lead_in_press_on_early_edge", 4),
        ("lead_in_press_ignored", 1),
        ("held_at_lead_in_start", 1),
        ("release_pending_at_lead_in_start", 1),
        ("excerpt_earlier_input", 1),
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
