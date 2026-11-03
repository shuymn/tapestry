//! Early and late (design.md "Evaluation Stages", accounting): each
//! scored judgment counts once on one side, early when pressed at or
//! before its note (a deviation of 0 included) and late after it or when
//! missed; a long note at its judgment, on the side of whichever of its
//! head and its release lies farther from its time, the release on a tie;
//! an empty POOR by the note it is against. FAST and SLOW are the early
//! and late judgments GREAT to POOR plus the empty POORs. The edges are
//! the preset's prepared windows for each lane class.

mod common;

use bms_model::{Chart, ChartBuilder, JudgmentHeader, Lane, TimeUs};
use common::{MS, device, judgments, lane, press, rank, release};
use eval::{
    EvalInput, Evaluation, Evaluator, InputStart, JudgeEventKind, PhysicalInputEvent, Section,
    Tally, evaluate,
};
use rules::Judgment::{self, Bad, Good, Great, Perfect, Poor};
use rules::{RulePreset, Window};

const BPM: i64 = 150_000;
/// Every single note's time.
const T: i64 = 1_000 * MS;

/// A side of a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Early,
    Late,
}
use Side::{Early, Late};

fn whole(chart: &Chart, preset: &RulePreset, events: Vec<PhysicalInputEvent>) -> Evaluation {
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

/// The one scored judgment's side in `t`, checking that it is counted
/// once and that FAST and SLOW take it unless it is PERFECT.
fn side_of(t: &Tally, j: Judgment) -> Side {
    let (early, late) = (*t.early.get(j), *t.late.get(j));
    let all = |p: &rules::PerJudgment<u32>| p.perfect + p.great + p.good + p.bad + p.poor;
    assert_eq!(
        (all(&t.early) + all(&t.late), early + late),
        (1, 1),
        "{t:?}"
    );
    let counted = u32::from(j != Perfect);
    let side = if early == 1 { Early } else { Late };
    let totals = match side {
        Early => (counted, 0),
        Late => (0, counted),
    };
    assert_eq!((t.fast(), t.slow()), totals, "{t:?}");
    side
}

/// Tapestry's rules with asymmetric PERFECT and GREAT windows of their own
/// for the keys and the scratch.
fn asymmetric() -> RulePreset {
    let mut p = rules::tapestry();
    let w = |early: i64, late: i64| Window {
        early: TimeUs(early * MS),
        late: TimeUs(late * MS),
    };
    p.tables.keys.windows.perfect = w(15, 25);
    p.tables.keys.windows.great = w(40, 50);
    p.tables.scratch.windows.perfect = w(22, 31);
    p.tables.scratch.windows.great = w(44, 55);
    assert_eq!(p.validate(), Ok(()));
    p
}

/// One tap on `l` at [`T`] under the rank `header`, pressed at `T + d`:
/// its judgment and side.
fn tap(preset: &RulePreset, header: Option<JudgmentHeader>, l: Lane, d: i64) -> (Judgment, Side) {
    let mut chart = ChartBuilder::new(BPM).tap(l, TimeUs(T)).build().unwrap();
    chart.meta.judgment_headers = header.into_iter().collect();
    let n = u8::try_from(l.index()).unwrap();
    let e = whole(
        &chart,
        preset,
        vec![press(T + d, 0, n), release(T + d + 5 * MS, 0, n)],
    );
    let [(_, _, judgment, deviation)] = judgments(&e.events)[..] else {
        panic!("{:?}", e.events);
    };
    assert_eq!(deviation, Some(d));
    (judgment, side_of(&e.tally, judgment))
}

/// The prepared windows of `l`'s class under `header`, as the evaluator
/// prepares them.
fn prepared(preset: &RulePreset, header: Option<JudgmentHeader>, l: Lane) -> rules::JudgeWindows {
    let headers: Vec<_> = header.into_iter().collect();
    let rank = preset.headers.read(&headers).rank;
    let tables = preset.window_policy.prepare(&preset.tables, rank).unwrap();
    tables.of_lane(l).windows
}

/// At each PERFECT edge, scaled or not, for the keys and the scratch: a
/// press on the note and every press up to the early edge is early, every
/// press after the note late, PERFECT on and inside both edges, GREAT 1 µs
/// outside.
#[test]
fn a_press_on_its_note_is_early_and_the_perfect_edges_are_inclusive() {
    let preset = asymmetric();
    for header in [None, Some(rank(0)), Some(rank(3))] {
        for l in [Lane::SCRATCH, lane(1), lane(7)] {
            let w = prepared(&preset, header, l);
            let (early, late) = (w.perfect.early.0, w.perfect.late.0);
            // The class's own asymmetric edges, scaled by the rank.
            assert_ne!(early, late, "{header:?} {l:?}");
            let at = |d| tap(&preset, header, l, d);
            assert_eq!(at(0), (Perfect, Early), "{header:?} {l:?}");
            assert_eq!(at(-1), (Perfect, Early), "{header:?} {l:?}");
            assert_eq!(at(1), (Perfect, Late), "{header:?} {l:?}");
            for d in [-early, -early + 1] {
                assert_eq!(at(d), (Perfect, Early), "{header:?} {l:?} {d}");
            }
            for d in [late - 1, late] {
                assert_eq!(at(d), (Perfect, Late), "{header:?} {l:?} {d}");
            }
            assert_eq!(at(-early - 1), (Great, Early), "{header:?} {l:?}");
            assert_eq!(at(late + 1), (Great, Late), "{header:?} {l:?}");
        }
    }
}

/// Every other grade counts by its side, up to the BAD edges, which are
/// inclusive too.
#[test]
fn every_other_grade_counts_by_its_side() {
    let preset = asymmetric();
    for l in [Lane::SCRATCH, lane(4)] {
        let w = prepared(&preset, None, l);
        for (judgment, window) in [(Great, w.great), (Good, w.good), (Bad, w.bad)] {
            let at = |d| tap(&preset, None, l, d);
            assert_eq!(at(-window.early.0), (judgment, Early), "{l:?}");
            assert_eq!(at(window.late.0), (judgment, Late), "{l:?}");
        }
    }
}

/// A long note counts once, at its judgment, on the side of whichever of
/// its head's press and its release lies farther from its time, the
/// release on a tie; a note held through its end, or decided at its
/// head's press, by its head.
#[test]
fn a_long_note_takes_the_side_of_its_farther_endpoint() {
    let preset = rules::tapestry();
    let end = T + 1_000 * MS;
    let ln = |head: i64, release_at: i64| {
        let chart = ChartBuilder::new(BPM)
            .long(lane(1), TimeUs(T), TimeUs(end))
            .build()
            .unwrap();
        let e = whole(
            &chart,
            &preset,
            vec![press(T + head, 0, 1), release(release_at, 0, 1)],
        );
        let [(_, _, judgment, deviation)] = judgments(&e.events)[..] else {
            panic!("{:?}", e.events);
        };
        assert_eq!(deviation, Some(head));
        (judgment, side_of(&e.tally, judgment))
    };
    let through = end + 1_000 * MS;
    // Held through its end: the head's side, on time early.
    assert_eq!(ln(30 * MS, through), (Great, Late));
    assert_eq!(ln(-30 * MS, through), (Great, Early));
    assert_eq!(ln(0, through), (Perfect, Early));
    // Released 500 ms early, BAD by the LN-end edges: the release is
    // farther, so early, whatever the head.
    assert_eq!(ln(1, end - 500 * MS), (Bad, Early));
    assert_eq!(ln(30 * MS, end - 500 * MS), (Bad, Early));
    // Released 120 ms early, GREAT: farther than a head 30 ms late, but
    // not than one 130 ms late (BAD, decided at its press).
    assert_eq!(ln(30 * MS, end - 120 * MS), (Great, Early));
    assert_eq!(ln(130 * MS, end - 120 * MS), (Bad, Late));
    // Released 10 ms early under a head 10 ms late: a tie goes to the
    // release. Released 5 ms early: the head is farther.
    assert_eq!(ln(10 * MS, end - 10 * MS), (Perfect, Early));
    assert_eq!(ln(10 * MS, end - 5 * MS), (Perfect, Late));
    // Released at its end: the head's side.
    assert_eq!(ln(10 * MS, end), (Perfect, Late));
}

/// A long note still held when the play ends has no judgment and counts
/// nothing, live and in the record's evaluation alike; its `LnHeld` only
/// gives feedback.
#[test]
fn an_aborted_hold_counts_nothing() {
    let preset = rules::tapestry();
    let chart = ChartBuilder::new(BPM)
        .long(lane(1), TimeUs(T), TimeUs(T + 1_000 * MS))
        .build()
        .unwrap();
    let inputs = vec![press(T + 30 * MS, 0, 1)];
    let dev = device(&[0]);
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), None, 8).unwrap();
    for &i in &inputs {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    let aborted = T + 500 * MS;
    ev.advance_to(TimeUs(aborted), &mut out).unwrap();
    assert!(matches!(
        out.last().map(|e| e.kind),
        Some(JudgeEventKind::LnHeld {
            judgment: Great,
            ..
        })
    ));
    assert_eq!(*ev.tally(), Tally::default());
    let input = EvalInput {
        events: inputs,
        device: dev.clone(),
        start: InputStart::default(),
        finalized: Some(TimeUs(aborted)),
        section: None,
    };
    let batch = evaluate(&chart, &input, &preset).unwrap();
    assert_eq!(batch.tally, *ev.tally());
    assert_eq!(batch.events, out);
}

/// A miss and a note a press passed are late POORs.
#[test]
fn misses_are_late() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .build()
        .unwrap();
    let e = whole(&chart, &rules::tapestry(), Vec::new());
    assert_eq!(judgments(&e.events), [(1, 0, Poor, None)]);
    assert_eq!(side_of(&e.tally, Poor), Late);
    // Past the first note's late GOOD edge (150 ms), inside the second's
    // early GOOD edge: combo priority takes the second, 40 ms early, and
    // passes the first.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .tap(lane(1), TimeUs(T + 200 * MS))
        .build()
        .unwrap();
    let e = whole(
        &chart,
        &rules::beatoraja_style(),
        vec![press(T + 160 * MS, 0, 1), release(T + 165 * MS, 0, 1)],
    );
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Poor, None), (1, 1, Great, Some(-40 * MS))]
    );
    assert_eq!((e.tally.late.poor, e.tally.early.great), (1, 1));
    assert_eq!((e.tally.fast(), e.tally.slow()), (1, 1));
}

/// An empty POOR is early when pressed at or before the note it is
/// against and late after it, apart from the POORs even when the preset
/// counts it as one, and FAST and SLOW take it; mines count nothing.
#[test]
fn empty_poors_count_by_the_note_they_are_against() {
    for counted in [false, true] {
        let mut preset = rules::tapestry();
        preset.empty_poor.counted = counted;
        let chart = ChartBuilder::new(BPM)
            .tap(lane(1), TimeUs(T))
            .tap(lane(2), TimeUs(T))
            .mine(lane(3), TimeUs(T), 10)
            .build()
            .unwrap();
        // Lane 1 pressed 400 ms before its note, which it then misses;
        // lane 2 on time, then again 100 ms after its judged note; lane 3
        // held across its mine.
        let e = whole(
            &chart,
            &preset,
            vec![
                press(T - 400 * MS, 0, 1),
                release(T - 390 * MS, 0, 1),
                press(T - 100 * MS, 0, 3),
                press(T, 0, 2),
                release(T + 10 * MS, 0, 2),
                press(T + 100 * MS, 0, 2),
                release(T + 100 * MS, 0, 3),
                release(T + 110 * MS, 0, 2),
            ],
        );
        let kinds: Vec<_> = e.events.iter().map(|e| e.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, JudgeEventKind::MineHit { .. }))
        );
        let t = &e.tally;
        assert_eq!(t.empty_poor, 2);
        assert_eq!((t.empty_poor_early, t.empty_poor_late), (1, 1));
        assert_eq!(t.counts.poor, if counted { 3 } else { 1 });
        // The miss, late; the empty POORs are not among the POORs.
        assert_eq!((t.early.poor, t.late.poor), (0, 1));
        assert_eq!((t.early.perfect, t.late.perfect), (1, 0));
        assert_eq!((t.fast(), t.slow()), (1, 2), "counted {counted}");
    }
    // Exactly on a judged note: early.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .tap(lane(1), TimeUs(T + 400 * MS))
        .build()
        .unwrap();
    let e = whole(
        &chart,
        &rules::tapestry(),
        vec![
            press(T - 10 * MS, 0, 1),
            release(T - 5 * MS, 0, 1),
            press(T, 0, 1),
            release(T + 5 * MS, 0, 1),
            press(T + 400 * MS, 0, 1),
            release(T + 405 * MS, 0, 1),
        ],
    );
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Perfect, Some(-10 * MS)), (1, 1, Perfect, Some(0))]
    );
    assert_eq!((e.tally.empty_poor_early, e.tally.empty_poor_late), (1, 0));
}

/// A practice section counts only its own targets: a press in the
/// lead-in that judges the first target counts, a press before the
/// lead-in counts nothing, and so does a note outside the section.
#[test]
fn a_section_counts_its_own_targets() {
    let preset = rules::tapestry();
    let s = 1_000 * MS;
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(5 * s))
        .tap(lane(1), TimeUs(10 * s))
        .tap(lane(2), TimeUs(12 * s))
        .tap(lane(2), TimeUs(25 * s))
        .build()
        .unwrap();
    let section = Section {
        start: TimeUs(10 * s),
        end: TimeUs(20 * s),
        lead_in: TimeUs(2 * s),
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
    };
    let input = EvalInput {
        events: vec![
            // Before the lead-in, 60 ms late on the note at 5 s.
            press(5 * s + 60 * MS, 0, 1),
            release(5 * s + 70 * MS, 0, 1),
            // In the lead-in, 30 ms early on the section's first target.
            press(10 * s - 30 * MS, 0, 1),
            release(10 * s - 20 * MS, 0, 1),
            press(12 * s + 60 * MS, 0, 2),
            release(12 * s + 70 * MS, 0, 2),
            // After the section, 30 ms late on a note outside it.
            press(25 * s + 30 * MS, 0, 2),
            release(25 * s + 40 * MS, 0, 2),
        ],
        device: device(&[0]),
        start: InputStart::default(),
        finalized: None,
        section: Some(section),
    };
    let e = evaluate(&chart, &input, &preset).unwrap();
    assert_eq!(
        judgments(&e.events),
        [(1, 1, Great, Some(-30 * MS)), (2, 0, Good, Some(60 * MS))]
    );
    assert_eq!((e.tally.early.great, e.tally.late.good), (1, 1));
    assert_eq!((e.tally.fast(), e.tally.slow()), (1, 1));
}
