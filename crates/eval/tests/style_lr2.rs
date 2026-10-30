//! Hand-written examples of the LR2-style rules, each naming the public
//! source it follows. `rules::lr2_style()` does not exist yet (stage 2,
//! PR8): until it does, the examples run under [`preset`], a preset built
//! here with only the values and policies they read.
//!
//! Sources are files of LR2oraja Endless Dream
//! (<https://github.com/seraxis/lr2oraja-endlessdream> at `d4995153`, under
//! `core/src/bms/player/beatoraja/`), cited as `file:line`.
//!
//! Window values are at `#RANK 3` (100 %): PERFECT ±21, GREAT ±60, GOOD
//! ±120 and BAD ±200 ms (`play/JudgeProperty.java:54-55`).

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, empty_poors, judgments, lane, press, release};
use eval::{EvalInput, Evaluation, InputStart, JudgeEventKind, PhysicalInputEvent, evaluate};
use rules::Judgment::{Bad, Poor};
use rules::{Assignment, HeadEligibility, JudgeWindows, RulePreset, Window};

const BPM: i64 = 150_000;
/// The first note's time.
const T: i64 = 1_000 * MS;
/// When this evaluator decides a miss: at the late BAD edge, 200 ms.
const MISSED: i64 = 200 * MS;

/// beatoraja-style with Endless Dream's 100 % note windows on both lane
/// classes (`play/JudgeProperty.java:54-55`), combo priority (Endless Dream
/// keeps beatoraja's default algorithm) and its LN-head eligibility
/// (`play/JudgeManager.java:435-437`). Everything else, its LN-end edges,
/// empty POOR ranges, window policy and gauges among them, is
/// beatoraja-style's stand-in until PR8 assembles `rules::lr2_style()`;
/// `#RANK 3` is 100 % under both.
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
    assert_eq!(p.validate(), Ok(()));
    p
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
fn judged(e: &Evaluation) -> Vec<(i64, usize, rules::Judgment, Option<i64>)> {
    let times = e.events.iter().filter_map(|ev| match ev.kind {
        JudgeEventKind::Judged { .. } => Some(ev.time.0),
        _ => None,
    });
    times
        .zip(judgments(&e.events))
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
        judged(&e),
        [(at, 0, Poor, None), (at, 1, Bad, Some(-140 * MS))]
    );
    // Without the eligibility rule, combo priority keeps the head.
    let mut inside = preset();
    inside.ln_mode.head_eligibility = HeadEligibility::InsideBad;
    let e = play_with(&chart, &inside, hold_at(at, 10));
    assert_eq!(judged(&e)[0], (at, 0, Bad, Some(160 * MS)));
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
        assert_eq!(judged(&e), [(T + MISSED, 0, Poor, None)], "{assignment:?}");
        assert_eq!(empty_poors(&e.events), 0);
        assert!(
            !e.events
                .iter()
                .any(|ev| matches!(ev.kind, JudgeEventKind::LnHoldEnded { .. })),
            "the press took no head"
        );
    }
    // A tap at 0 ms and a long note from 30 ms, pressed at 190 ms: the head
    // (160 ms late) is not the primary, which stays before it, and the head
    // is missed at its own deadline, 230 ms.
    let before = chart(|b| {
        b.tap(lane(1), TimeUs(T))
            .long(lane(1), TimeUs(T + 30 * MS), TimeUs(T + 80 * MS))
    });
    let e = play_with(&before, &preset(), hold_at(T + 190 * MS, 10));
    let head: Vec<_> = judged(&e).into_iter().filter(|j| j.1 == 1).collect();
    assert_eq!(head, [(T + 30 * MS + MISSED, 1, Poor, None)]);
}

/// G. A BAD long-note head decides its long note at the press, also under
/// the head eligibility rule: an early BAD head is eligible, and the
/// release that follows changes nothing.
#[test]
fn bad_ln_head_is_scored_at_the_press() {
    let chart = chart(|b| b.long(lane(1), TimeUs(T), TimeUs(T + 500 * MS)));
    let at = T - 150 * MS;
    let e = play_with(&chart, &preset(), hold_at(at, 600 * MS));
    assert_eq!(judged(&e), [(at, 0, Bad, Some(-150 * MS))]);
    let ended: Vec<_> = e
        .events
        .iter()
        .filter(|ev| matches!(ev.kind, JudgeEventKind::LnHoldEnded { .. }))
        .map(|ev| ev.time.0)
        .collect();
    assert_eq!(ended, [at + 600 * MS], "released 50 ms before the end");
}
