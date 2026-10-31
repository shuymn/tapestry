//! The judge: its grades, combos, placement and refusals, through
//! `build`.
//!
//! Grade k's source rect is (100, 500, 80 + k, 20): drawn at
//! [100, 560, 80 + k, 20], its width naming its slot.

mod common;

use bms_model::{Lane, TimeUs};
use common::play::{Drawn, MS, judged, tally};
use common::{Loaded, has, import_sized};
use eval::{JudgeEvent, JudgeEventKind};
use rules::{GaugeValue, Judgment};
use skin_beatoraja::{CriticalPart, ImportError, NoteKind};

/// Grade `k`'s destination: image `gk` with `extra` fields.
fn grade(k: usize, extra: &str) -> String {
    format!(
        r#"{{"id": "g{k}"{extra}, "dst": [{{"x": 100, "y": 500, "w": {}, "h": 20}}]}}"#,
        80 + k
    )
}

/// Grades 0..n.
fn grades(n: usize) -> Vec<String> {
    (0..n).map(|k| grade(k, "")).collect()
}

/// A combo destination of value `v` at x `x`, relative to the grade.
fn combo(v: &str, x: i32) -> String {
    format!(r#"{{"id": "{v}", "dst": [{{"x": {x}, "y": 0, "w": 10, "h": 20}}]}}"#)
}

/// Value records: `ten` of 10 glyphs, `eleven` of 11, `eleven2` two sets
/// of 11, `twelve` 24 glyphs in a row (two sets of 11), `grid` 24 in two rows, `five` five digits, `two`
/// two digits; three digits unless named.
const VALUES: &str = r#""value": [
    {"id": "ten", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3},
    {"id": "eleven", "src": "n", "w": 110, "h": 20, "divx": 11, "digit": 3},
    {"id": "eleven2", "src": "n", "w": 110, "h": 20, "divx": 11, "divy": 2, "digit": 3},
    {"id": "twelve", "src": "n", "w": 120, "h": 20, "divx": 24, "digit": 3},
    {"id": "grid", "src": "n", "w": 120, "h": 20, "divx": 12, "divy": 2, "digit": 3},
    {"id": "five", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 5},
    {"id": "two", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 2},
    {"id": "zero", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 0},
    {"id": "wide", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 13},
    {"id": "offs", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3,
     "offset": [{"x": 1}]}]"#;

/// Seven grade images over `a`, and `gone` from a missing file.
fn images() -> String {
    let v: Vec<String> = (0..7)
        .map(|k| {
            format!(
                r#"{{"id": "g{k}", "src": "a", "x": {}, "w": 8, "h": 8}}"#,
                8 * k
            )
        })
        .collect();
    format!(
        r#""image": [{}, {{"id": "gone", "src": "gone", "w": 8, "h": 8}}]"#,
        v.join(", ")
    )
}

const DST: &str = r#"{"id": "j", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;

/// A skin of judges `judges` drawn by `dsts`, with `values`.
fn skin(values: &str, judges: &str, dsts: &[&str]) -> Loaded {
    let sep = if values.is_empty() { "" } else { ", " };
    let body = format!(
        r#"{}{sep}{values}, "judge": [{judges}], "destination": [{}]"#,
        images(),
        dsts.join(", ")
    );
    import_sized([1_920, 1_080], &body, &[])
}

/// Judge `j` with `images`, `numbers` and `extra` fields.
fn judge_rec(images: &[String], numbers: &[String], extra: &str) -> String {
    format!(
        r#"{{"id": "j", "index": 0, "images": [{}], "numbers": [{}]{extra}}}"#,
        images.join(", "),
        numbers.join(", ")
    )
}

fn judge(images: &[String], numbers: &[String]) -> Loaded {
    // Without numbers the skin needs no values.
    let values = if numbers.is_empty() {
        r#""value": []"#
    } else {
        VALUES
    };
    skin(values, &judge_rec(images, numbers, ""), &[DST])
}

/// The rects at `ms` after `ev` with combo `combo`, and the gauge full
/// when `full`.
fn shown(l: &Loaded, ev: JudgeEventKind, combo: u32, full: bool) -> Vec<[i32; 4]> {
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    d.tally = tally(0, combo);
    if full {
        d.gauges[0].value = GaugeValue::FULL;
        let max = JudgeEventKind::Gauge {
            gauge: 0,
            value: GaugeValue::FULL,
            reference: GaugeValue::FULL,
        };
        d.frame(900, &[event(900, max)]);
    }
    d.frame(1_000, &[event(1_000, ev)]);
    d.rects(1_001)
}

fn event(ms: i64, kind: JudgeEventKind) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(ms * MS),
        origin: None,
        kind,
    }
}

fn of(j: Judgment, combo: u32) -> JudgeEventKind {
    judged(1_000, j, combo).kind
}

fn empty_poor() -> JudgeEventKind {
    JudgeEventKind::EmptyPoor {
        lane: Lane::new(1).unwrap(),
    }
}

#[track_caller]
fn critical(l: Loaded) -> (CriticalPart, NoteKind) {
    match l.err() {
        ImportError::Critical { part, why, .. } => (part, why),
        e => panic!("{e:?}"),
    }
}

#[test]
fn each_slot_shows_its_grade() {
    let l = judge(&grades(7), &[]);
    for (k, j) in [
        Judgment::Perfect,
        Judgment::Great,
        Judgment::Good,
        Judgment::Bad,
        Judgment::Poor,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            shown(&l, of(j, 0), 0, false),
            [[100, 560, 80 + k as i32, 20]]
        );
    }
    assert_eq!(shown(&l, empty_poor(), 0, false), [[100, 560, 85, 20]]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 0), 0, true),
        [[100, 560, 86, 20]]
    );
    // With no slot 6, a PERFECT at a full gauge shows slot 0.
    let l = judge(&grades(6), &[]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 0), 0, true),
        [[100, 560, 80, 20]]
    );
}

#[test]
fn combos_sit_under_the_grade_centred() {
    // Ten glyphs: blank padding, centred. The raw x 10 less half of
    // three 10 px cells is −5; "12" fills two of three cells, so it
    // moves half a cell right: 100 and 110.
    let l = judge(&grades(1), &[combo("ten", 10)]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[100, 560, 10, 20], [110, 560, 10, 20], [100, 560, 80, 20]]
    );
    // A one-digit combo: one cell, a whole cell right.
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 5), 5, false),
        [[105, 560, 10, 20], [100, 560, 80, 20]]
    );
    // Two digits: the raw shift is one whole width (x 10 → 0).
    let l = judge(&grades(1), &[combo("two", 10)]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[100, 560, 10, 20], [110, 560, 10, 20], [100, 560, 80, 20]]
    );
    // Eleven glyphs pad with the eleventh: three cells drawn.
    for v in ["eleven", "eleven2", "twelve"] {
        let l = judge(&grades(1), &[combo(v, 10)]);
        assert_eq!(
            shown(&l, of(Judgment::Perfect, 12), 12, false),
            [
                [95, 560, 10, 20],
                [105, 560, 10, 20],
                [115, 560, 10, 20],
                [100, 560, 80, 20]
            ],
            "{v}"
        );
    }
    // 24 glyphs in two rows: the 22 used do not fill whole rows.
    assert_eq!(
        critical(judge(&grades(1), &[combo("grid", 10)])),
        (
            CriticalPart::Judge,
            NoteKind::UnsupportedField { field: "divx" }
        )
    );
    // The shift wraps as the source's `int` does, out of range here.
    let far = judge(&grades(1), &[combo("ten", i32::MIN)]);
    assert!(
        matches!(far.err(), ImportError::Range { field: "dst", .. }),
        "wrapped"
    );
}

#[test]
fn combos_exist_for_four_slots() {
    let numbers: Vec<String> = (0..7).map(|_| combo("ten", 10)).collect();
    let l = judge(&grades(7), &numbers);
    let ignored = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Ignored { field: "numbers" })
        .count();
    assert_eq!(ignored, 3);
    assert_eq!(
        shown(&l, of(Judgment::Bad, 12), 12, false),
        [[100, 560, 83, 20]]
    );
    assert_eq!(shown(&l, of(Judgment::Good, 12), 12, false).len(), 3);
    // A combo of no digits shows nothing.
    let l = judge(&grades(1), &[combo("zero", 10)]);
    assert!(has(l.report(), NoteKind::Ignored { field: "numbers" }));
}

#[test]
fn judge_entries_need_an_id() {
    let refused = |field| (CriticalPart::Judge, NoteKind::UnsupportedField { field });
    let no_id = r#"{"dst": [{"x": 100, "y": 500, "w": 80, "h": 20}]}"#.to_owned();
    // An id naming no record leaves its slot empty.
    let mut images = grades(1);
    images.push(NONE.to_owned());
    judge(&images, &[combo("ten", 10), NONE.to_owned()]).ok();
    // A missing one refuses, for an image past the slots too.
    let mut images = grades(1);
    images.push(no_id.clone());
    assert_eq!(
        critical(judge(&images, &[combo("ten", 10), NONE.to_owned()])),
        refused("images")
    );
    let mut images = grades(7);
    images.push(no_id.clone());
    let numbers = vec![NONE.to_owned(); 8];
    assert_eq!(critical(judge(&images, &numbers)), refused("images"));
    assert_eq!(
        critical(judge(&grades(2), &[combo("ten", 10), no_id.clone()])),
        refused("numbers")
    );
    // A number past the images is never read.
    judge(&grades(1), &[combo("ten", 10), no_id]).ok();
}

#[test]
fn quads_count_the_widest_state() {
    let l = judge(&grades(2), &[combo("ten", 10), combo("five", 10)]);
    assert_eq!(l.report().usage.widget_quads, 6);
}

#[test]
fn blank_cells_past_the_models_shift_half_a_cell_each() {
    // 13 digits: the raw x 130 less 65 is 65 from the grade's left (100);
    // the model draws 11 cells, and the two blank cells past them shift
    // it by one 10 px cell to 75. "12" centred in 11 cells moves 45 more:
    // 220 and 230, where the source centres it in 13 cells from 165.
    let l = judge(&grades(1), &[combo("wide", 130)]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[220, 560, 10, 20], [230, 560, 10, 20], [100, 560, 80, 20]]
    );
}

#[test]
fn judges_refuse_what_the_model_cannot_share() {
    // A second index splits the lanes among the judges.
    let one = judge_rec(&grades(1), &[], "").replace(r#""index": 0"#, r#""index": 1"#);
    assert_eq!(
        critical(skin(VALUES, &one, &[DST])),
        (
            CriticalPart::Judge,
            NoteKind::UnsupportedField { field: "index" }
        )
    );
    let both = format!(
        "{}, {}",
        judge_rec(&grades(1), &[], ""),
        one.replace(r#""id": "j""#, r#""id": "k""#)
    );
    let k = DST.replace(r#""j""#, r#""k""#);
    assert_eq!(
        critical(skin(r#""value": []"#, &both, &[DST, &k])).1,
        NoteKind::UnsupportedField { field: "index" }
    );
    // Fewer numbers than images, while the skin has values.
    let short = judge_rec(&grades(2), &[combo("ten", 10)], "");
    assert_eq!(
        critical(skin(VALUES, &short, &[DST])).1,
        NoteKind::UnsupportedField { field: "numbers" }
    );
    let l = skin(r#""value": []"#, &short, &[DST]);
    assert_eq!(l.report().usage.widget_quads, 1);
    // Drawn twice.
    assert_eq!(
        critical(judge_two()).1,
        NoteKind::UnsupportedField { field: "judge" }
    );
    // A grade moved by LIFT.
    let lifted = judge_rec(&[grade(0, r#", "offset": 3"#)], &[combo("ten", 10)], "");
    assert_eq!(
        critical(skin(VALUES, &lifted, &[DST])),
        (CriticalPart::Judge, NoteKind::RuntimeOffset(3))
    );
    // Per-digit offsets.
    let l = judge(&grades(1), &[combo("offs", 10)]);
    assert_eq!(
        critical(l),
        (
            CriticalPart::Judge,
            NoteKind::UnsupportedField { field: "offset" }
        )
    );
    // A missing texture.
    let gone = judge_rec(
        &[grade(0, "").replace("g0", "gone")],
        &[combo("ten", 10)],
        "",
    );
    assert_eq!(
        critical(skin(VALUES, &gone, &[DST])),
        (CriticalPart::Judge, NoteKind::MissingAsset)
    );
}

fn judge_two() -> Loaded {
    skin(
        r#""value": []"#,
        &judge_rec(&grades(1), &[], ""),
        &[DST, DST],
    )
}

/// A number that resolves to no value.
const NONE: &str = r#"{"id": "none", "dst": []}"#;

#[test]
fn a_stretch_may_not_move_a_combos_anchor() {
    // Fit inner: the square image in an 80×20 box draws 20×20 centred,
    // moving its left edge from the source's anchor.
    let fit = |k: usize| grade(k, r#", "stretch": 1"#);
    // PERFECT's grade paired with the full-gauge slot's combo.
    let mut images = vec![fit(0)];
    images.extend((1..6).map(|k| grade(k, "")));
    images.push(NONE.replace("none", "g9"));
    let mut numbers = vec![NONE.to_owned(); 6];
    numbers.push(combo("ten", 10));
    assert_eq!(
        critical(judge(&images, &numbers)),
        (CriticalPart::Judge, NoteKind::Stretch(1))
    );
    // The full-gauge slot's grade paired with PERFECT's combo.
    let mut images = grades(6);
    images.push(fit(6));
    let mut numbers = vec![combo("ten", 10)];
    numbers.extend((1..7).map(|_| NONE.to_owned()));
    assert_eq!(
        critical(judge(&images, &numbers)),
        (CriticalPart::Judge, NoteKind::Stretch(1))
    );
    // The same stretch where no combo pairs with it.
    let mut images = grades(3);
    images.push(fit(3));
    let numbers: Vec<String> = (0..4).map(|_| combo("ten", 10)).collect();
    judge(&images, &numbers).ok();
    // A stretch that keeps the left and bottom edges.
    let square = r#"{"id": "g0", "stretch": 1, "dst": [{"x": 100, "y": 500, "w": 20, "h": 20}]}"#;
    let l = judge(&[square.to_owned()], &[combo("ten", 10)]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[100, 560, 10, 20], [110, 560, 10, 20], [100, 560, 20, 20]]
    );
}
