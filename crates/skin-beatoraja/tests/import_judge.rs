//! The judge: its grades, combos, placement and refusals, through
//! `build`.
//!
//! Grade k's source rect is (100, 500, 80 + k, 20): drawn at
//! [100, 560, 80 + k, 20], its width naming its slot.

mod common;

use bms_model::{Lane, TimeUs};
use common::play::{Drawn, MS, judged, tally};
use common::{Loaded, d3_notes, has, import_sized};
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
    {"id": "wide", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 13}]"#;

/// Seven grade images over `a`, `gone` from a missing file and `undef`
/// from a source no record defines.
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
        r#""image": [{}, {{"id": "gone", "src": "zz", "w": 8, "h": 8}},
                     {{"id": "undef", "src": "nope", "w": 8, "h": 8}}]"#,
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
        deviation: TimeUs(0),
        presses: 0,
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
}

/// A combo's glyphs cut past their file's right edge: ten 54-px cells
/// from x 228 of a 766-px `n.png` run two columns past it, which repeat
/// its last column (765) as the source's texture clamped to its edge
/// samples (`JsonSkinObjectLoader.java:601-621`), whether that column is
/// transparent or coloured.
#[test]
fn a_combo_cut_past_its_file_repeats_the_edge_column() {
    let values = r#""value": [{"id": "edge", "src": "n", "x": 228, "w": 540, "h": 20,
                                "divx": 10, "digit": 3}]"#;
    let body = format!(
        r#"{}, {values}, "judge": [{}], "destination": [{DST}]"#,
        images(),
        judge_rec(&grades(1), &[combo("edge", 10)], "")
    );
    let inner = |x: u32, y: u32| {
        [
            u8::try_from(x % 251).unwrap(),
            u8::try_from(y).unwrap(),
            90,
            255,
        ]
    };
    for edge in [[0, 0, 0, 0], [200, 10, 90, 255]] {
        let l = import_sized([1_920, 1_080], &body, &[("n.png", 766, 20)]);
        assert!(!has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
        common::png_with(l.dir.path(), "n.png", 766, 20, |x, y| {
            if x == 765 { edge } else { inner(x, y) }
        });
        let skin = l.ok().skin.clone();
        let atlas = skin
            .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
            .unwrap();
        let mut d = Drawn::new(skin, 100);
        d.tally = tally(0, 9);
        d.frame(1_000, &[event(1_000, of(Judgment::Perfect, 9))]);
        // Glyph 9 (columns 714 to 767), one cell, beside the grade.
        let glyphs: Vec<_> = d
            .frame(1_001, &[])
            .into_iter()
            .filter(|q| common::play::round(q.rect)[2] == 10)
            .collect();
        let [q] = glyphs.as_slice() else {
            panic!("{glyphs:?}")
        };
        let at = |col: f32| common::play::texel_at(&atlas, q, (col + 0.5) / 54.0, 0.5);
        // Columns 760, 765 (the edge), and 766 and 767 past it.
        assert_eq!(
            [at(46.0), at(51.0), at(52.0), at(53.0)],
            [inner(760, 10), edge, edge, edge],
            "{edge:?}"
        );
    }
}

/// A combo of `ten` (three digits) with the keys `keys`.
fn combo_keys(keys: &str) -> String {
    format!(r#"{{"id": "ten", "dst": [{keys}]}}"#)
}

#[test]
fn a_combo_shift_past_the_coordinates_refuses() {
    let range = |keys: &str| {
        let l = judge(&grades(1), &[combo_keys(keys)]);
        assert!(
            matches!(l.err(), ImportError::Range { field: "dst", .. }),
            "{keys}"
        );
    };
    // A shift past `i32` is exact, and only its edges leave the model.
    range(r#"{"x": 0, "y": 0, "w": 2147483647, "h": 20}"#);
    range(r#"{"x": -2147483647, "y": 0, "w": 10, "h": 20}"#);
}

/// The combo `ten` (three digits) under `keys`, held from 2 s by its
/// `loop`, its rects at each of `elapsed` ms after a PERFECT at combo 12.
fn combo_frames(keys: &str, elapsed: &[i64]) -> Vec<Vec<[i32; 4]>> {
    let combo = format!(r#"{{"id": "ten", "loop": 2000, "dst": [{keys}]}}"#);
    let l = judge(&grades(1), &[combo]);
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    d.tally = tally(0, 12);
    d.frame(1_000, &[event(1_000, of(Judgment::Perfect, 12))]);
    elapsed.iter().map(|&e| d.rects(1_000 + e)).collect()
}

/// A combo key giving only `x` or only `w` inherits the other from the
/// previous key, then moves left by its own `w` × digits / 2: it draws as
/// the same key with the inherited field written. A first key inherits 0:
/// `w` alone at x 0 moves to −15 ("12" a half cell right of it: 90 and
/// 100); `x` alone has no width, so its glyphs are empty at x 10.
#[test]
fn a_combo_key_inherits_x_or_w_alone() {
    const FIRST: &str = r#"{"time": 0, "x": 10, "y": 0, "w": 10, "h": 20}"#;
    let times = [1, 1_000, 2_000, 4_000, 5_000];
    for (alone, written) in [
        (
            r#"{"time": 2000, "x": 40}"#,
            r#"{"time": 2000, "x": 40, "y": 0, "w": 10, "h": 20}"#,
        ),
        (
            r#"{"time": 2000, "w": 20}"#,
            r#"{"time": 2000, "x": 10, "y": 0, "w": 20, "h": 20}"#,
        ),
    ] {
        let got = combo_frames(&format!("{FIRST}, {alone}"), &times);
        assert_eq!(
            got,
            combo_frames(&format!("{FIRST}, {written}"), &times),
            "{alone}"
        );
        assert_ne!(got, combo_frames(FIRST, &times), "{alone}");
    }
    let first = |keys: &str| {
        let l = judge(&grades(1), &[combo_keys(keys)]);
        shown(&l, of(Judgment::Perfect, 12), 12, false)
    };
    assert_eq!(
        first(r#"{"y": 0, "w": 10, "h": 20}"#),
        [[90, 560, 10, 20], [100, 560, 10, 20], [100, 560, 80, 20]]
    );
    assert_eq!(
        first(r#"{"x": 10, "y": 0, "h": 20}"#),
        [[110, 560, 0, 20], [110, 560, 0, 20], [100, 560, 80, 20]]
    );
}

/// The shift is exact on a 1280 px skin: the raw x 10 less half of three
/// 11 px widths is −6.5, −9.75 reference pixels, and only the drawn
/// edges round, putting "12" at 149. Halving the raw widths first would
/// leave −6, −9, and 150.
#[test]
fn a_combo_centres_exactly() {
    let judge = judge_rec(
        &grades(1),
        &[combo_keys(r#"{"x": 10, "y": 0, "w": 11, "h": 20}"#)],
        "",
    );
    let body = format!(
        r#"{}, {VALUES}, "judge": [{judge}], "destination": [{DST}]"#,
        images()
    );
    let l = import_sized([1_280, 720], &body, &[]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[149, 300, 17, 30], [166, 300, 17, 30], [150, 300, 120, 30]]
    );
}

#[test]
fn a_combo_key_omitting_x_and_w_inherits_the_shifted_x() {
    let one = judge(&grades(1), &[combo("ten", 10)]);
    let two = judge(
        &grades(1),
        &[combo_keys(
            r#"{"time": 0, "x": 10, "y": 0, "w": 10, "h": 20},
               {"time": 5000, "y": 0, "h": 20}"#,
        )],
    );
    let at = |l: &Loaded| shown(l, of(Judgment::Perfect, 12), 12, false);
    assert_eq!(at(&two), at(&one));
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
    // A missing one refuses in a slot that is drawn.
    let mut images = grades(1);
    images.push(no_id.clone());
    assert_eq!(
        critical(judge(&images, &[combo("ten", 10), NONE.to_owned()])),
        refused("images")
    );
    assert_eq!(
        critical(judge(&grades(2), &[combo("ten", 10), no_id.clone()])),
        refused("numbers")
    );
    // Entries never drawn are not read: an image past the slots, a
    // number of a slot without a combo, a number past the images.
    let mut images = grades(7);
    images.push(no_id.clone());
    let l = judge(&images, &vec![NONE.to_owned(); 8]);
    assert!(has(l.report(), NoteKind::Ignored { field: "images" }));
    let mut numbers = vec![NONE.to_owned(); 3];
    numbers.push(no_id.clone());
    judge(&grades(4), &numbers).ok();
    judge(&grades(1), &[combo("ten", 10), no_id]).ok();
}

/// An entry the judge never draws is not parsed: malformed fields in a
/// number of a slot without a combo, or in an image past the slots, do
/// not refuse; a number naming a value is still noted as ignored.
#[test]
fn undrawn_judge_entries_are_not_parsed() {
    let malformed = [
        r#"{"id": "ten", "blend": []}"#,
        r#"{"id": "ten", "timer": "late", "dst": [{"x": 0}]}"#,
        r#"{"id": "ten", "dst": 7}"#,
        r#"{"id": "ten", "op": [{}], "offset": "far"}"#,
    ];
    for entry in malformed {
        let mut numbers = vec![NONE.to_owned(); 3];
        numbers.push(entry.to_owned());
        let l = judge(&grades(4), &numbers);
        assert!(
            has(l.report(), NoteKind::Ignored { field: "numbers" }),
            "{entry}"
        );
        let mut images = grades(7);
        images.push(entry.replace("ten", "g0"));
        let l = judge(&images, &[]);
        assert!(
            has(l.report(), NoteKind::Ignored { field: "images" }),
            "{entry}"
        );
    }
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
    // Fewer numbers than images leave the rest without combos.
    let short = judge_rec(&grades(2), &[combo("ten", 10)], "");
    let l = skin(VALUES, &short, &[DST]);
    assert_eq!(l.report().usage.widget_quads, 4);
    // Drawn twice.
    assert_eq!(
        critical(judge_two()).1,
        NoteKind::UnsupportedField { field: "judge" }
    );
    // HIDDEN+'s offset on a combo (a grade follows it).
    let hidden = combo("ten", 10).replace(r#""id": "ten""#, r#""id": "ten", "offsets": [3, 5]"#);
    let hidden = judge_rec(&grades(1), &[hidden], "");
    assert_eq!(
        critical(skin(VALUES, &hidden, &[DST])),
        (CriticalPart::Judge, NoteKind::RuntimeOffset(5))
    );
    // A missing texture refuses with its file's error, never falling
    // back to another grade.
    let gone = judge_rec(
        &[grade(0, "").replace("g0", "gone")],
        &[combo("ten", 10)],
        "",
    );
    match skin(VALUES, &gone, &[DST]).err() {
        ImportError::Asset { .. } => {}
        e => panic!("{e:?}"),
    }
    // A grade whose source no record defines: the source leaves it blank,
    // a part of the judge the import does not drop.
    let undef = judge_rec(
        &[grade(0, "").replace("g0", "undef")],
        &[combo("ten", 10)],
        "",
    );
    assert_eq!(
        critical(skin(VALUES, &undef, &[DST])),
        (CriticalPart::Judge, NoteKind::UndefinedSource)
    );
}

#[test]
fn a_count_whose_source_is_undefined_refuses() {
    // The source drops the combo (`SkinJudge.java:109-123`), a part of
    // the judge the import does not drop.
    let values = VALUES.replace(
        r#"{"id": "ten", "src": "n""#,
        r#"{"id": "ten", "src": "nope""#,
    );
    let count = judge_rec(&grades(1), &[combo("ten", 10)], "");
    assert_eq!(
        critical(skin(&values, &count, &[DST])),
        (CriticalPart::Judge, NoteKind::UndefinedSource)
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

/// Grade `k` fitted with `stretch`.
fn fit(k: usize, stretch: i32) -> String {
    grade(k, &format!(r#", "stretch": {stretch}"#))
}

/// The keys charged for attachments, after asserting nothing was noted.
#[track_caller]
fn anchor_keys(l: &Loaded) -> u32 {
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    l.report().usage.grade_anchor_keys
}

#[test]
fn a_stretched_grade_keeps_its_combo_on_its_destination() {
    // Fit inner: the square image in the 80×20 box at 100 draws 20×20 at
    // 130; the combo stays on the box's bottom-left, as the source
    // attaches it before the stretch (`play/SkinJudge.java:115-125`):
    // "12" at 100 and 110, 580 − 20.
    let cells = [[100, 560, 10, 20], [110, 560, 10, 20]];
    let l = judge(&[fit(0, 1)], &[combo("ten", 10)]);
    assert_eq!(anchor_keys(&l), 1);
    assert_eq!(l.report().usage.keys, 4);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [cells[0], cells[1], [130, 560, 20, 20]]
    );
    // With the shift, half the baseline width 20 moves the paint alone.
    let shifted = judge_rec(&[fit(0, 1)], &[combo("ten", 10)], r#", "shift": true"#);
    let l = skin(VALUES, &shifted, &[DST]);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [cells[0], cells[1], [120, 560, 20, 20]]
    );
    // Fit width moves the bottom edge: the 80×80 paint from 530, the
    // combo still on 580.
    let l = judge(&[fit(0, 4)], &[combo("ten", 10)]);
    assert_eq!(anchor_keys(&l), 1);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [cells[0], cells[1], [100, 530, 80, 80]]
    );
    // Outer and fit height move the left edge.
    for stretch in [2, 6] {
        let l = judge(&[fit(0, stretch)], &[combo("ten", 10)]);
        assert_eq!(anchor_keys(&l), 1);
        assert_eq!(
            shown(&l, of(Judgment::Perfect, 12), 12, false)[..2],
            cells,
            "{stretch}"
        );
    }
}

#[test]
fn a_stretched_grade_attaches_through_either_fallback() {
    let cells = [[100, 560, 10, 20], [110, 560, 10, 20]];
    // PERFECT's grade standing in, with the full-gauge slot's combo.
    let mut images = vec![fit(0, 1)];
    images.extend((1..6).map(|k| grade(k, "")));
    images.push(NONE.replace("none", "g9"));
    let mut numbers = vec![NONE.to_owned(); 6];
    numbers.push(combo("ten", 10));
    let l = judge(&images, &numbers);
    assert_eq!(anchor_keys(&l), 1);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, true),
        [cells[0], cells[1], [130, 560, 20, 20]]
    );
    // The full-gauge slot's grade with PERFECT's combo standing in; the
    // 86 px box centres the paint at 133.
    let mut images = grades(6);
    images.push(fit(6, 1));
    let mut numbers = vec![combo("ten", 10)];
    numbers.extend((1..7).map(|_| NONE.to_owned()));
    let l = judge(&images, &numbers);
    assert_eq!(anchor_keys(&l), 1);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, true),
        [cells[0], cells[1], [133, 560, 20, 20]]
    );
}

#[test]
fn only_a_paired_moved_grade_keeps_an_attachment() {
    // BAD has no combo: its fitted grade keeps no attachment keys.
    let mut images = grades(3);
    images.push(fit(3, 1));
    let numbers: Vec<String> = (0..4).map(|_| combo("ten", 10)).collect();
    let l = judge(&images, &numbers);
    assert_eq!(l.report().usage.grade_anchor_keys, 0);
    // Nor with a combo that is never drawn (zero digits).
    let l = judge(&[fit(0, 1)], &[combo("zero", 10)]);
    assert_eq!(l.report().usage.grade_anchor_keys, 0);
    // Nor a stretch that keeps the left and bottom edges.
    let square = r#"{"id": "g0", "stretch": 1, "dst": [{"x": 100, "y": 500, "w": 20, "h": 20}]}"#;
    let l = judge(&[square.to_owned()], &[combo("ten", 10)]);
    assert_eq!(anchor_keys(&l), 0);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[100, 560, 10, 20], [110, 560, 10, 20], [100, 560, 20, 20]]
    );
    // Nor a native size, fitted at placement inside the destination.
    let g = r#"{"id": "g0", "stretch": 9, "dst": [{"x": 100, "y": 500, "w": 80, "h": 20}]}"#;
    let l = judge(&[g.to_owned()], &[combo("ten", 10)]);
    assert_eq!(anchor_keys(&l), 0);
}

#[test]
fn trimmed_and_branch_changing_grade_stretches_still_refuse() {
    for stretch in [3, 5, 7, 10] {
        assert_eq!(
            critical(judge(&[fit(0, stretch)], &[combo("ten", 10)])),
            (CriticalPart::Judge, NoteKind::Stretch(stretch)),
            "{stretch}"
        );
    }
    // Fit inner by height, then by width.
    let turning = r#"{"id": "g0", "stretch": 1, "timer": 46, "dst": [
        {"time": 0, "x": 100, "y": 500, "w": 80, "h": 20},
        {"time": 100, "x": 100, "y": 500, "w": 20, "h": 80}]}"#;
    assert_eq!(
        critical(judge(&[turning.to_owned()], &[combo("ten", 10)])),
        (CriticalPart::Judge, NoteKind::Stretch(1))
    );
}

#[test]
fn the_attachment_runs_on_the_grade_s_clock() {
    // The grade grows over 100 ms from its judgment at 1 000 ms; the
    // combo, on READY (timer 40), slides right 0.1 px a millisecond from READY. At
    // 1 050 ms the destination is [150, 580, 80, 30] (y down), so the
    // combo sits on 610 at 150 plus its own offset, while the paint is
    // the 30 px square centred at 175. Both loop from 0 at 100 ms.
    let g = r#"{"id": "g0", "stretch": 1, "timer": 46, "dst": [
        {"time": 0, "x": 100, "y": 500, "w": 80, "h": 20},
        {"time": 100, "x": 200, "y": 440, "w": 80, "h": 40}]}"#;
    let n = r#"{"id": "ten", "timer": 40, "dst": [
        {"time": 0, "x": 10, "y": 0, "w": 10, "h": 20},
        {"time": 10000, "x": 1010, "y": 0, "w": 10, "h": 20}]}"#;
    let l = judge(&[g.to_owned()], &[n.to_owned()]);
    assert_eq!(anchor_keys(&l), 2);
    let mut d = Drawn::new(l.ok().skin.clone(), 100);
    d.tally = tally(0, 12);
    d.frame(1_000, &[event(1_000, of(Judgment::Perfect, 12))]);
    let ready = d.play.timer(skin_model::timer::READY).unwrap().0 / MS;
    for (ms, [ax, bottom], paint) in [
        (1_000, [100, 580], [130, 560, 20, 20]),
        (1_050, [150, 610], [175, 580, 30, 30]),
        (1_100, [100, 580], [130, 560, 20, 20]),
        (1_125, [125, 595], [153, 570, 25, 25]),
    ] {
        // The combo's own x at its own elapsed time: −5 + 0.1 a ms, and
        // half a cell for the blank one.
        let cx = -5.0 + 0.1 * (ms - ready) as f32 + 5.0;
        let got = d.rects(ms);
        let x = (ax as f32 + cx).round() as i32;
        assert_eq!(
            got,
            [
                [x, bottom - 20, 10, 20],
                [x + 10, bottom - 20, 10, 20],
                paint
            ],
            "{ms}"
        );
    }
}

#[test]
fn an_attachment_keeps_the_static_offsets() {
    // Offset 40 raises the destination 10: paint and attachment alike.
    let header =
        r#""offset": [{"name": "a", "id": 40, "x": false, "y": true, "w": false, "h": false}]"#;
    let rec = judge_rec(
        &[grade(0, r#", "stretch": 1, "offset": 40"#)],
        &[combo("ten", 10)],
        "",
    );
    let body = format!(
        r#"{header}, {}, {VALUES}, "judge": [{rec}], "destination": [{DST}]"#,
        images()
    );
    let l = common::import_config([1_920, 1_080], &body, &[], |c| {
        c.offsets.insert(
            40,
            skin_beatoraja::OffsetValue {
                y: 10,
                ..skin_beatoraja::OffsetValue::default()
            },
        );
    });
    assert_eq!(l.report().usage.grade_anchor_keys, 1);
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[100, 550, 10, 20], [110, 550, 10, 20], [130, 550, 20, 20]]
    );
}

#[test]
fn attachment_edges_round_on_their_own() {
    // At 1280×720 the scale is 3/2: the box [101, 500, 81, 21] is
    // [151.5, 298.5, 121.5, 31.5] y down. Its edges round half up to
    // [152, 299, 121, 31]; the 31.5 px square fitted inside it from
    // 196.5 rounds to [197, 299, 31, 31]. The combo [−7.5, −30, 15, 30]
    // relative rounds to [−7, −30, 15, 30]: "12" from 152 − 7 + 7.5.
    let g = r#"{"id": "g0", "stretch": 1, "dst": [{"x": 101, "y": 500, "w": 81, "h": 21}]}"#;
    let body = format!(
        r#"{}, {VALUES}, "judge": [{}], "destination": [{DST}]"#,
        images(),
        judge_rec(&[g.to_owned()], &[combo("ten", 10)], "")
    );
    let l = import_sized([1_280, 720], &body, &[]);
    assert_eq!(anchor_keys(&l), 1);
    let q = {
        let mut d = Drawn::new(l.ok().skin.clone(), 100);
        d.tally = tally(0, 12);
        d.frame(1_000, &[event(1_000, of(Judgment::Perfect, 12))]);
        d.frame(1_001, &[])
    };
    let rects: Vec<[f32; 4]> = q.iter().map(|q| q.rect).collect();
    assert_eq!(
        rects,
        [
            [152.5, 300.0, 15.0, 30.0],
            [167.5, 300.0, 15.0, 30.0],
            [197.0, 299.0, 31.0, 31.0]
        ]
    );
}

#[test]
fn native_grades_keep_the_combos_anchor() {
    // No resize: the 8×8 grade centred in its 80×20 box and each 12×20
    // glyph in its 10×20 cell, the combo anchored to the box
    // (`play/SkinJudge.java:124`).
    let g = r#"{"id": "g0", "stretch": 9, "dst": [{"x": 100, "y": 500, "w": 80, "h": 20}]}"#;
    let n = r#"{"id": "ten", "stretch": 9, "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]}"#;
    let l = judge(&[g.to_owned()], &[n.to_owned()]);
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    assert_eq!(
        shown(&l, of(Judgment::Perfect, 12), 12, false),
        [[99, 560, 12, 20], [109, 560, 12, 20], [136, 566, 8, 8]]
    );
}

/// The refusal of a skin whose judge `j` shares its id with `record`
/// in `table` (the image table gets it beside the grades).
fn shared_id(table: &str, record: &str) -> (CriticalPart, NoteKind) {
    let images = if table == "image" {
        images().replacen(r#""image": ["#, &format!(r#""image": [{record}, "#), 1)
    } else {
        format!(r#"{}, "{table}": [{record}]"#, images())
    };
    let body = format!(
        r#"{images}, "value": [], "judge": [{}], "destination": [{DST}]"#,
        judge_rec(&grades(1), &[], "")
    );
    critical(import_sized([1_920, 1_080], &body, &[]))
}

const SHARED: (CriticalPart, NoteKind) = (
    CriticalPart::Judge,
    NoteKind::UnsupportedField { field: "id" },
);

// The source makes the judge, and its region, for an id it shares when
// the earlier object comes out null; whether it does is a load-order rule
// not reproduced, so the id is refused.

#[test]
fn an_image_may_not_share_the_judges_id() {
    // Null when its file fails to load (`JsonSkinObjectLoader.java:71`).
    for src in ["gone", "a"] {
        let image = format!(r#"{{"id": "j", "src": "{src}", "w": 8, "h": 8}}"#);
        assert_eq!(shared_id("image", &image), SHARED, "{src}");
    }
}

#[test]
fn a_text_may_not_share_the_judges_id() {
    // Null when no font has its id (`JsonSkinObjectLoader.java:387-391, 628`).
    assert_eq!(
        shared_id("text", r#"{"id": "j", "font": "none", "ref": 12}"#),
        SHARED
    );
}

#[test]
fn a_slider_may_not_share_the_judges_id() {
    // Null without a texture (`JsonSkinObjectLoader.java:396-416`).
    assert_eq!(shared_id("slider", r#"{"id": "j", "src": "gone"}"#), SHARED);
}

#[test]
fn a_graph_may_not_share_the_judges_id() {
    // Null without a texture, on either branch
    // (`JsonSkinObjectLoader.java:420-453`).
    for kind in [-1, 0] {
        let graph = format!(r#"{{"id": "j", "src": "gone", "type": {kind}}}"#);
        assert_eq!(shared_id("graph", &graph), SHARED, "{kind}");
    }
}

#[test]
fn a_lifted_grade_carries_its_combo_once() {
    use common::lanes::{NOTES, body, note};
    use skin_beatoraja::{DeviationId, LaneApprox};
    // Over the default lanes (600 px), LIFT at 300 ‰ raises the line
    // 180: the grade listing offset 3 rises 180, and its combo, listing
    // 3 and 4 several times, rises with it once. The combo's own
    // offsets move nothing; only the grade is noted.
    let combo = combo("ten", 10).replace(
        r#""id": "ten""#,
        r#""id": "ten", "offsets": [3, 4, 3], "offset": 3"#,
    );
    let judge = judge_rec(&[grade(0, r#", "offsets": [3, 3]"#)], &[combo], "");
    let objects = format!(r#"{}, {VALUES}, "judge": [{judge}]"#, note(""));
    let l = common::import_body(&body("", &objects, &[NOTES, DST]));
    let motion = NoteKind::Deviation(DeviationId::D9(LaneApprox::CoverMotion));
    let kinds = common::kinds(l.report());
    assert_eq!(kinds.iter().filter(|&&k| k == motion).count(), 1);
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, NoteKind::RuntimeOffset(_)))
    );
    let at = |lift: u16| {
        // One note, judged and past: the lanes draw nothing.
        let chart = bms_model::ChartBuilder::new(150_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut d = Drawn::over(l.ok().skin.clone(), &chart);
        d.tally = tally(0, 12);
        d.view.lift = lift;
        d.frame(
            1_000,
            &[event(1_000, judged(1_000, Judgment::Perfect, 12).kind)],
        );
        d.rects(1_001)
    };
    let still = at(0);
    // The combo's two glyphs and the grade.
    assert_eq!(still.len(), 3);
    let lifted: Vec<_> = still
        .iter()
        .map(|&[x, y, w, h]| [x, y - 180, w, h])
        .collect();
    assert_eq!(at(300), lifted);
}

#[test]
fn a_grade_following_hidden_plus_hides_alone() {
    use common::lanes::{NOTES, body, note};
    use common::play::readouts_for;
    // Over the default lanes (600 px), HIDDEN+ at 300 ‰ raises the grade
    // listing offset 5 by 180, its combo with it; while HIDDEN+ is off
    // the grade is transparent and its combo still draws where it
    // attaches, as the source only clears the grade's alpha
    // (`skin/SkinObject.java:474-481`, `play/SkinJudge.java:121-137`).
    let judge = judge_rec(&[grade(0, r#", "offset": 5"#)], &[combo("ten", 10)], "");
    let objects = format!(r#"{}, {VALUES}, "judge": [{judge}]"#, note(""));
    let l = common::import_body(&body("", &objects, &[NOTES, DST]));
    assert!(!common::kinds(l.report()).contains(&NoteKind::RuntimeOffset(5)));
    let at = |hidden: u16, on: bool| {
        let chart = bms_model::ChartBuilder::new(150_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let mut d = Drawn::over(l.ok().skin.clone(), &chart);
        d.tally = tally(0, 12);
        d.view.hidden = if on { hidden } else { 0 };
        let mut readouts = readouts_for(d.view);
        readouts.covers.hidden_enabled = on;
        d.readouts = Some(readouts);
        d.frame(
            1_000,
            &[event(1_000, judged(1_000, Judgment::Perfect, 12).kind)],
        );
        d.rects(1_001)
    };
    let still = at(0, true);
    assert_eq!(still.len(), 3);
    let raised: Vec<_> = still
        .iter()
        .map(|&[x, y, w, h]| [x, y - 180, w, h])
        .collect();
    assert_eq!(at(300, true), raised);
    // The combo's two cells, unmoved; no grade.
    assert_eq!(at(300, false), still[..2]);
}

#[test]
fn a_lift_may_not_share_the_judges_id() {
    // Skipped without a texture (`JsonPlaySkinObjectLoader.java:200-213`).
    assert_eq!(
        shared_id("liftCover", r#"{"id": "j", "src": "gone"}"#),
        SHARED
    );
}

#[test]
fn a_kind_condition_is_noted_at_its_part() {
    use skin_beatoraja::DeviationId;
    // The grade and the combo each under a gauge-family condition, on
    // lines of their own: one D4 note at each part.
    let combo = combo("ten", 10).replace(r#""id": "ten""#, "\"id\": \"ten\",\n\"op\": [-43]");
    let judge = judge_rec(&[grade(0, ",\n\"op\": [42]")], &[combo], "");
    let l = skin(VALUES, &judge, &[DST]);
    let d4: Vec<u32> = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Deviation(DeviationId::D4))
        .map(|n| n.at.expect("located").line)
        .collect();
    assert_eq!(d4.len(), 2, "{:?}", l.report().notes);
    assert_ne!(d4[0], d4[1]);
}

/// An untimed judge whose window ends runs on `screen` (D3): noted once,
/// its grades shown until the window's last key and hidden after it; on
/// a timer of its own it is not noted.
#[test]
fn an_untimed_ending_judge_runs_on_screen() {
    let outer = |fields: &str| {
        format!(
            r#"{{"id": "j"{fields}, "loop": -1, "dst": [{{"time": 0, "x": 0, "y": 0, "w": 0, "h": 0}},
                                                       {{"time": 2000, "x": 0, "y": 0, "w": 0, "h": 0}}]}}"#
        )
    };
    let rec = judge_rec(&grades(7), &[], "");
    let l = skin(r#""value": []"#, &rec, &[&outer("")]);
    assert_eq!(d3_notes(&l), 1);
    let at = |ms: i64| {
        let mut d = Drawn::new(l.ok().skin.clone(), 100);
        d.frame(ms, &[event(ms, of(Judgment::Perfect, 1))]);
        d.rects(ms + 1).len()
    };
    assert_eq!((at(1_000), at(2_500)), (1, 0));
    let timed = skin(r#""value": []"#, &rec, &[&outer(r#", "timer": 41"#)]);
    assert_eq!(d3_notes(&timed), 0);
    // A judge drawing no grade is not retained: no D3.
    let empty = skin(r#""value": []"#, &judge_rec(&[], &[], ""), &[&outer("")]);
    assert_eq!(d3_notes(&empty), 0);
}
