//! FAST and SLOW (design.md "Rule Presets", the tally): each scored
//! judgment other than PERFECT counts FAST when its press came before its
//! note and SLOW when after, a long note once at its judgment by its
//! head's press; misses, empty POORs, mines and feedback count neither.
//! The edges are the preset's prepared windows for each lane class, both
//! PERFECT edges inclusive.

mod common;

use bms_model::{Chart, ChartBuilder, JudgmentHeader, Lane, TimeUs};
use common::{MS, device, judgments, lane, press, rank, release};
use eval::{
    EvalInput, Evaluation, Evaluator, InputStart, JudgeEventKind, PhysicalInputEvent, Section,
    evaluate,
};
use rules::Judgment::{self, Bad, Good, Great, Perfect, Poor};
use rules::{RulePreset, Window};

const BPM: i64 = 150_000;
/// Every single note's time.
const T: i64 = 1_000 * MS;

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

fn fast_slow(e: &Evaluation) -> (u32, u32) {
    (e.tally.fast, e.tally.slow)
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
/// its judgment, FAST and SLOW.
fn tap(
    preset: &RulePreset,
    header: Option<JudgmentHeader>,
    l: Lane,
    d: i64,
) -> (Judgment, u32, u32) {
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
    (judgment, e.tally.fast, e.tally.slow)
}

/// The prepared windows of `l`'s class under `header`, as the evaluator
/// prepares them.
fn prepared(preset: &RulePreset, header: Option<JudgmentHeader>, l: Lane) -> rules::JudgeWindows {
    let headers: Vec<_> = header.into_iter().collect();
    let rank = preset.headers.read(&headers).rank;
    let tables = preset.window_policy.prepare(&preset.tables, rank).unwrap();
    tables.of_lane(l).windows
}

/// At each PERFECT edge, scaled or not, for the keys and the scratch:
/// the edge itself and 1 µs inside are PERFECT and count neither; 1 µs
/// outside is GREAT and counts FAST before the note, SLOW after it. A
/// press on the note counts neither.
#[test]
fn perfect_edges_are_inclusive_and_one_us_outside_counts() {
    let preset = asymmetric();
    for header in [None, Some(rank(0)), Some(rank(3))] {
        for l in [Lane::SCRATCH, lane(1), lane(7)] {
            let w = prepared(&preset, header, l);
            let (early, late) = (w.perfect.early.0, w.perfect.late.0);
            // The class's own asymmetric edges, scaled by the rank.
            assert_ne!(early, late, "{header:?} {l:?}");
            let at = |d| tap(&preset, header, l, d);
            assert_eq!(at(0), (Perfect, 0, 0));
            for d in [-early, -early + 1, late - 1, late] {
                assert_eq!(at(d), (Perfect, 0, 0), "{header:?} {l:?} {d}");
            }
            assert_eq!(at(-early - 1), (Great, 1, 0), "{header:?} {l:?}");
            assert_eq!(at(late + 1), (Great, 0, 1), "{header:?} {l:?}");
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
            assert_eq!(at(-window.early.0), (judgment, 1, 0), "{l:?}");
            assert_eq!(at(window.late.0), (judgment, 0, 1), "{l:?}");
        }
    }
}

/// A long note counts once, at its judgment, by its head's press and its
/// final grade: a head 1 µs early that its release makes BAD counts FAST;
/// a head on time that its release makes BAD counts neither; a GREAT head
/// held through counts SLOW once; a BAD head decides the note at its press.
#[test]
fn a_long_note_counts_once_by_its_head_and_final_grade() {
    let preset = rules::tapestry();
    let ln = |head: i64, release_at: i64| {
        let chart = ChartBuilder::new(BPM)
            .long(lane(1), TimeUs(T), TimeUs(T + 1_000 * MS))
            .build()
            .unwrap();
        let e = whole(
            &chart,
            &preset,
            vec![press(T + head, 0, 1), release(release_at, 0, 1)],
        );
        let held = e
            .events
            .iter()
            .any(|e| matches!(e.kind, JudgeEventKind::LnHeld { .. }));
        let [(_, _, judgment, deviation)] = judgments(&e.events)[..] else {
            panic!("{:?}", e.events);
        };
        assert_eq!(deviation, Some(head));
        (held, judgment, fast_slow(&e))
    };
    // Released 500 ms before the end: below every LN-end edge, BAD.
    let early = T + 500 * MS;
    let through = T + 2_000 * MS;
    assert_eq!(ln(-1, early), (true, Bad, (1, 0)));
    assert_eq!(ln(0, early), (true, Bad, (0, 0)));
    assert_eq!(ln(1, early), (true, Bad, (0, 1)));
    assert_eq!(ln(30 * MS, through), (true, Great, (0, 1)));
    assert_eq!(ln(-30 * MS, through), (true, Great, (1, 0)));
    assert_eq!(ln(-5 * MS, through), (true, Perfect, (0, 0)));
    // Within GREAT's LN-end edge: the head's GREAT stays, counted once.
    assert_eq!(ln(30 * MS, T + 900 * MS), (true, Great, (0, 1)));
    // A BAD head is judged at the press, never held.
    assert_eq!(ln(-150 * MS, through), (false, Bad, (1, 0)));
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
    assert_eq!((ev.tally().fast, ev.tally().slow), (0, 0));
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

/// Empty POORs, misses, notes a press passed and mines count neither,
/// whatever the preset makes of an empty POOR.
#[test]
fn empty_poors_misses_and_mines_count_neither() {
    for counted in [false, true] {
        let mut preset = rules::tapestry();
        preset.empty_poor.counted = counted;
        let chart = ChartBuilder::new(BPM)
            .tap(lane(1), TimeUs(T))
            .tap(lane(2), TimeUs(T))
            .mine(lane(3), TimeUs(T), 10)
            .build()
            .unwrap();
        // Lane 1 pressed 400 ms early, inside the empty POOR range; lane 2
        // left alone; lane 3 held across its mine.
        let e = whole(
            &chart,
            &preset,
            vec![
                press(T - 400 * MS, 0, 1),
                release(T - 390 * MS, 0, 1),
                press(T - 100 * MS, 0, 3),
                release(T + 100 * MS, 0, 3),
            ],
        );
        let kinds: Vec<_> = e.events.iter().map(|e| e.kind).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, JudgeEventKind::EmptyPoor { .. }))
        );
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, JudgeEventKind::MineHit { .. }))
        );
        assert_eq!(e.tally.empty_poor, 1);
        assert_eq!(e.tally.counts.poor, if counted { 3 } else { 2 });
        assert_eq!(fast_slow(&e), (0, 0), "counted {counted}");
    }
    // A press that passes an earlier note: the passed note is a POOR
    // without a press, the later one counts by its own side.
    let preset = rules::beatoraja_style();
    let chart = ChartBuilder::new(BPM)
        .tap(lane(1), TimeUs(T))
        .tap(lane(1), TimeUs(T + 200 * MS))
        .build()
        .unwrap();
    // Past the first note's late GOOD edge (150 ms), inside the second's
    // early GOOD edge: combo priority takes the second.
    let e = whole(
        &chart,
        &preset,
        vec![press(T + 160 * MS, 0, 1), release(T + 165 * MS, 0, 1)],
    );
    assert_eq!(
        judgments(&e.events),
        [(1, 0, Poor, None), (1, 1, Great, Some(-40 * MS))]
    );
    assert_eq!(fast_slow(&e), (1, 0));
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
    assert_eq!(fast_slow(&e), (1, 1));
}
