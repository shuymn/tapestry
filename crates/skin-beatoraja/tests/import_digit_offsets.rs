//! Digit offsets: a `value`'s `offset` rows as per-cell paint deltas of
//! numbers and judge combos: their units and reflection, the cells they
//! address, their wrappers, their refusals and their caps.
//!
//! Unless stated: a ten-glyph strip of n.png (12×20 texels a glyph) on a
//! 1920×1080 skin, showing the EX score.

mod common;

use common::play::tally;
use common::{Loaded, has, import_sized, join, png};
use skin_beatoraja::limits::MAX_RESOLVED_DIGIT_OFFSETS;
use skin_beatoraja::{
    CriticalPart, ImportError, ImportLimit, InputLimit, NoteKind, import, read_header,
};
use skin_model::{MAX_COORD, MAX_DIGIT_OFFSETS};

/// A value `v` of `fields` drawn at `dst`, on a skin `size` pixels.
fn number_sized(size: [u32; 2], fields: &str, dst: &str) -> Loaded {
    import_sized(
        size,
        &format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, {fields}}}],
               "destination": [{{"id": "v", {dst}}}]"#
        ),
        &[],
    )
}

fn number(fields: &str, dst: &str) -> Loaded {
    number_sized([1_920, 1_080], fields, dst)
}

/// The rects drawn at 0 ms with EX score `ex`.
fn rects(l: Loaded, ex: u32) -> Vec<[i32; 4]> {
    let mut d = l.drawn();
    d.tally = tally(ex, 0);
    d.rects(0)
}

/// `a` less `b`, per component.
fn less(a: [i32; 4], b: [i32; 4]) -> [i32; 4] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]]
}

/// One cell at the model rect [100, 200, 20, 30].
const ONE: &str = r#""dst": [{"x": 100, "y": 850, "w": 20, "h": 30}]"#;

#[test]
fn a_row_moves_its_cell_in_target_pixels() {
    // y up: growing the height moves the top edge, so (3, 5, 4, 6) is
    // [+3, −5 − 6, +4, +6].
    let l = number(
        r#""digit": 1, "ref": 71, "offset": [{"x": 3, "y": 5, "w": 4, "h": 6}]"#,
        ONE,
    );
    assert_eq!(l.report().usage.resolved_digit_offsets, 1);
    assert_eq!(l.report().usage.digit_offsets, 1);
    assert_eq!(l.ok().skin.digit_offset_count(), 1);
    assert_eq!(rects(l, 7), [[103, 189, 24, 36]]);
    // Not the centred rule of a configured offset, and not scaled with
    // the destination: on a 1280×720 skin the cell is 1.5 times larger,
    // the delta the same.
    for (fields, want) in [
        (r#""digit": 1, "ref": 71"#, [0, 0, 0, 0]),
        (
            r#""digit": 1, "ref": 71, "offset": [{"x": 3, "y": 5, "w": 4, "h": 6}]"#,
            [3, -11, 4, 6],
        ),
    ] {
        let dst = r#""dst": [{"x": 60, "y": 500, "w": 20, "h": 20}]"#;
        let base = rects(
            number_sized([1_280, 720], r#""digit": 1, "ref": 71"#, dst),
            7,
        );
        let got = rects(number_sized([1_280, 720], fields, dst), 7);
        assert_eq!(base, [[90, 300, 30, 30]]);
        assert_eq!(less(got[0], base[0]), want, "{fields}");
    }
}

#[test]
fn the_native_size_fits_the_moved_cell() {
    // Stretch 9 keeps the 12×20 glyph, centred in the moved
    // [103, 189, 24, 36].
    let dst = r#""stretch": 9, "dst": [{"x": 100, "y": 850, "w": 20, "h": 30}]"#;
    let l = number(
        r#""digit": 1, "ref": 71, "offset": [{"x": 3, "y": 5, "w": 4, "h": 6}]"#,
        dst,
    );
    assert_eq!(rects(l, 7), [[109, 197, 12, 20]]);
    // A 20×30 glyph: the frame of the unmoved cell, fitted in the moved
    // one.
    let l = import_sized(
        [1_920, 1_080],
        &format!(
            r#""value": [{{"id": "v", "src": "n", "w": 200, "h": 30, "divx": 10, "digit": 1,
                 "ref": 71, "offset": [{{"x": 3, "y": 5, "w": 4, "h": 6}}]}}],
               "destination": [{{"id": "v", {dst}}}]"#
        ),
        &[("n.png", 200, 30)],
    );
    assert_eq!(rects(l, 7), [[105, 192, 20, 30]]);
}

#[test]
fn dropped_cells_skip_their_rows() {
    // 13 digits keep 11 cells after 2 blanks: retained cell j reads row
    // 2 + j, so "12" in cells 9 and 10 reads rows 11 and 12; the others
    // would move it 500 right.
    let dst = r#""dst": [{"x": 0, "y": 0, "w": 10, "h": 20}]"#;
    let far = join((0..11).map(|_| r#"{"x": 500}"#.to_owned()));
    let rows = format!(r#""offset": [{far}, {{"y": -1}}, {{"y": -2}}]"#);
    let base = rects(number(r#""digit": 13, "ref": 71"#, dst), 12);
    let l = number(&format!(r#""digit": 13, "ref": 71, {rows}"#), dst);
    assert_eq!(l.report().usage.resolved_digit_offsets, 13);
    assert_eq!(l.report().usage.digit_offsets, 11);
    let got = rects(l, 12);
    assert_eq!(got.len(), 2);
    assert_eq!(less(got[0], base[0]), [0, 1, 0, 0]);
    assert_eq!(less(got[1], base[1]), [0, 2, 0, 0]);
}

#[test]
fn short_tables_and_trailing_zero_rows() {
    // Three zero-padded cells "007": a short table moves cell 0 alone,
    // and trailing zero rows are not kept.
    let dst = r#""dst": [{"x": 0, "y": 0, "w": 10, "h": 20}]"#;
    let base = rects(number(r#""digit": 3, "padding": 1, "ref": 71"#, dst), 7);
    let l = number(
        r#""digit": 3, "padding": 1, "ref": 71, "offset": [{"x": 1}, {}, {"x": 0, "w": 0}]"#,
        dst,
    );
    assert_eq!(l.report().usage.resolved_digit_offsets, 3);
    assert_eq!(l.report().usage.digit_offsets, 1);
    let got = rects(l, 7);
    let moved: Vec<[i32; 4]> = got.iter().zip(&base).map(|(g, b)| less(*g, *b)).collect();
    assert_eq!(moved, [[1, 0, 0, 0], [0; 4], [0; 4]]);
    // An internal zero row keeps the later row's position.
    let l = number(
        r#""digit": 3, "padding": 1, "ref": 71, "offset": [{}, {}, {"x": 2}]"#,
        dst,
    );
    assert_eq!(l.report().usage.digit_offsets, 3);
    let got = rects(l, 7);
    assert_eq!(less(got[2], base[2]), [2, 0, 0, 0]);
}

/// Imports `json` as `s.json` beside n.png and `files`.
fn load(json: &str, files: &[(&str, &str)]) -> Loaded {
    let mut all = vec![("s.json", json)];
    all.extend_from_slice(files);
    let dir = common::skin(&all);
    png(dir.path(), "n.png", 120, 20);
    let result = read_header(dir.path(), "s.json").and_then(|(mut def, h)| {
        let config = skin_beatoraja::Config::defaults(&h);
        import(&mut def, &h, &config)
    });
    Loaded { dir, result }
}

/// A skin of value `v` with `fields`, three zero-padded cells at x 0.
fn json(offset: &str) -> String {
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{{"id": "n", "path": "n.png"}}],
  "value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3, "padding": 1,
    "ref": 71, "offset": {offset}}}],
  "destination": [{{"id": "v", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 20}}]}}]}}"#
    )
}

/// Each cell's x as drawn by an import of [`json`] showing "000".
fn xs(l: Loaded) -> Vec<i32> {
    l.rects(0).iter().map(|r| r[0]).collect()
}

#[test]
fn wrappers_insert_and_remove_rows() {
    // Option 901 is off: the first row goes, the second is row 0, and
    // the `values` add rows 1 and 2, the last from an included file.
    let offset = r#"[{"if": [901], "value": {"x": 1}}, {"x": 2},
        {"if": [-901], "values": [{"x": 5}, {"include": "row.json"}]}]"#;
    let l = load(&json(offset), &[("row.json", r#"[{"x": 6}]"#)]);
    assert_eq!(l.report().usage.resolved_digit_offsets, 3);
    assert_eq!(xs(l), [2, 15, 26]);
    // An included table, an object as one row, `null` as none.
    let l = load(
        &json(r#"{"include": "rows.json"}"#),
        &[("rows.json", r#"[{}, {"x": 4}]"#)],
    );
    assert_eq!(xs(l), [0, 14, 20]);
    assert_eq!(xs(load(&json(r#"{"x": 3}"#), &[])), [3, 10, 20]);
    let l = load(&json("null"), &[]);
    assert_eq!(l.report().usage.resolved_digit_offsets, 0);
    assert_eq!(xs(l), [0, 10, 20]);
    // Exact numbers: a whole decimal and a string are integers.
    let l = load(&json(r#"[{"x": 2.0}, {"x": "3"}, {"x": 1e1}]"#), &[]);
    assert_eq!(xs(l), [2, 13, 30]);
}

/// Where `marker` starts in `json`, 1-based.
fn loc_of(json: &str, marker: &str) -> (u32, u32) {
    let i = json.find(marker).unwrap();
    let before = &json[..i];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap().chars().count() + 1;
    (u32::try_from(line).unwrap(), u32::try_from(col).unwrap())
}

#[test]
fn unreadable_rows_refuse_at_the_row() {
    let typed = |offset: &str, marker: &str, field: &str, expected: &str| {
        let j = json(offset);
        match load(&j, &[]).result {
            Err(ImportError::Type {
                at,
                field: f,
                expected: e,
            }) => {
                assert_eq!((f, e), (field, expected), "{offset}");
                assert_eq!((at.line, at.col), loc_of(&j, marker), "{offset}");
            }
            other => panic!("{offset}: {other:?}"),
        }
    };
    // A choice that selects nothing, a null row and a scalar row: each
    // would otherwise move the rows after it.
    typed(
        r#"[{"x": 1}, [{"if": [901], "value": {"x": 9}}], {"x": 2}]"#,
        r#"[{"if""#,
        "offset",
        "a row",
    );
    typed(
        r#"[{"x": 1}, null, {"x": 2}]"#,
        "null",
        "offset",
        "an object",
    );
    typed(r#"[{"x": 1}, 7]"#, "7]", "offset", "an object");
    typed("5", "5}", "offset", "an array");
    typed(r#"[{"x": 1.5}]"#, "1.5", "x", "an integer");
    typed(r#"[{"w": true}]"#, "true", "w", "an integer");
    typed(r#"[{"h": null}]"#, "null", "h", "an integer");
    for (offset, marker, field) in [
        (r#"[{"x": 3e10}]"#, "3e10", "x"),
        (r#"[{"y": 2147483648}]"#, "2147483648", "y"),
        (r#"[{"w": 1e999999}]"#, "1e999999", "w"),
    ] {
        let j = json(offset);
        match load(&j, &[]).result {
            Err(ImportError::Range { at, field: f }) => {
                assert_eq!(f, field);
                assert_eq!((at.line, at.col), loc_of(&j, marker), "{offset}");
            }
            other => panic!("{offset}: {other:?}"),
        }
    }
}

#[test]
fn a_retained_row_past_the_model_refuses() {
    // y = 2^20 with h = 1 reflects to −2^20 − 1. Row 0 is dropped with
    // 13 digits, so the same row there is never read.
    let max = MAX_COORD;
    let j = json(&format!(r#"[{{"x": 1}}, {{"y": {max}, "h": 1}}]"#));
    match load(&j, &[]).result {
        Err(ImportError::Range { at, field }) => {
            assert_eq!(field, "offset");
            assert_eq!((at.line, at.col), loc_of(&j, r#"{"y""#));
        }
        other => panic!("{other:?}"),
    }
    let j = json(&format!(r#"[{{"y": {}, "h": 1}}]"#, max - 1));
    load(&j, &[]).ok();
    let rows = format!(r#""offset": [{{"y": {max}, "h": 1}}, {{}}]"#);
    let l = number(&format!(r#""digit": 13, "ref": 71, {rows}"#), ONE);
    assert_eq!(l.report().usage.digit_offsets, 0);
}

#[test]
fn an_aspect_stretch_keeps_rows_of_its_aspect() {
    // A 12×20 glyph in a 12×20 cell under stretch 1 is drawn as is; a
    // row growing it 6×10 keeps the aspect, 6×9 does not.
    let dst = r#""stretch": 1, "dst": [{"x": 0, "y": 0, "w": 12, "h": 20}]"#;
    let kept = number(
        r#""digit": 1, "ref": 110, "offset": [{"w": 6, "h": 10}]"#,
        dst,
    );
    assert!(kept.report().notes.is_empty(), "{:?}", kept.report());
    assert_eq!(kept.ok().skin.digit_offset_count(), 1);
    let dropped = number(
        r#""digit": 1, "ref": 110, "offset": [{"w": 6, "h": 9}]"#,
        dst,
    );
    assert!(has(dropped.report(), NoteKind::Stretch(1)));
    assert_eq!(dropped.report().usage.elements, 1);
    assert_eq!(dropped.ok().skin.digit_offset_count(), 0);
    let critical = number(
        r#""digit": 1, "ref": 71, "offset": [{"w": 6, "h": 9}]"#,
        dst,
    );
    assert!(matches!(
        critical.err(),
        ImportError::Critical {
            part: CriticalPart::ExScore,
            why: NoteKind::Stretch(1),
            ..
        }
    ));
    // Stretch 0 keeps any row.
    let plain = r#""dst": [{"x": 0, "y": 0, "w": 12, "h": 20}]"#;
    let l = number(
        r#""digit": 1, "ref": 71, "offset": [{"w": 6, "h": 9}]"#,
        plain,
    );
    assert!(l.report().notes.is_empty());
}

/// A combo judge: grade g0 and value `v` as PERFECT's combo, shifting.
fn judge(values: &str) -> Loaded {
    judge_at(values, r#""dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]"#)
}

/// [`judge`] with the combo drawn at `dst`.
fn judge_at(values: &str, dst: &str) -> Loaded {
    import_sized(
        [1_920, 1_080],
        &format!(
            r#""image": [{{"id": "g0", "src": "a", "w": 8, "h": 8}}],
               "value": [{values}],
               "judge": [{{"id": "j", "index": 0, "shift": true,
                 "images": [{{"id": "g0", "dst": [{{"x": 100, "y": 500, "w": 80, "h": 20}}]}}],
                 "numbers": [{{"id": "v", {dst}}}]}}],
               "destination": [{{"id": "j", "dst": [{{"w": 0, "h": 0}}]}}]"#
        ),
        &[],
    )
}

#[test]
fn combo_rows_move_glyphs_the_grade_shift_ignores() {
    let shown = |l: Loaded| {
        let mut d = l.drawn();
        d.tally = tally(0, 12);
        d.frame(
            1_000,
            &[common::play::judged(1_000, rules::Judgment::Perfect, 12)],
        );
        d.rects(1_001)
    };
    let value = r#"{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3"#;
    let base = shown(judge(&format!("{value}}}")));
    let l = judge(&format!(
        r#"{value}, "offset": [{{}}, {{"x": 2, "w": 4}}, {{"y": 3}}]}}"#
    ));
    assert_eq!(l.report().usage.digit_offsets, 3);
    let got = shown(l);
    // "12" in cells 1 and 2, then the grade moved by half their baseline
    // width.
    assert_eq!(got.len(), 3);
    assert_eq!(less(got[0], base[0]), [2, 0, 4, 0]);
    assert_eq!(less(got[1], base[1]), [0, -3, 0, 0]);
    assert_eq!(got[2], base[2]);
}

#[test]
fn a_combos_aspect_stretch_keeps_rows_of_its_aspect() {
    let value = |row: &str| {
        format!(
            r#"{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1,
                "offset": [{row}]}}"#
        )
    };
    let dst = r#""stretch": 1, "dst": [{"x": 10, "y": 0, "w": 12, "h": 20}]"#;
    let l = judge_at(&value(r#"{"w": 6, "h": 10}"#), dst);
    assert_eq!(l.ok().skin.digit_offset_count(), 1);
    assert!(matches!(
        judge_at(&value(r#"{"w": 6, "h": 9}"#), dst).err(),
        ImportError::Critical {
            part: CriticalPart::Judge,
            why: NoteKind::Stretch(1),
            ..
        }
    ));
}

/// The resolution cap: one value of `n` rows, one cell kept.
fn long_table(n: usize) -> Loaded {
    let rows = join((0..n).map(|i| format!(r#"{{"x": {}}}"#, i % 7 + 1)));
    load(&json(&format!("[{rows}]")), &[])
}

#[test]
fn resolved_rows_are_capped_before_they_are_read() {
    let l = long_table(MAX_RESOLVED_DIGIT_OFFSETS);
    let i = l.ok();
    assert_eq!(
        i.report.usage.resolved_digit_offsets as usize,
        MAX_RESOLVED_DIGIT_OFFSETS
    );
    assert_eq!(i.report.usage.digit_offsets, 3);
    match long_table(MAX_RESOLVED_DIGIT_OFFSETS + 1).result {
        Err(ImportError::Input {
            limit: InputLimit::DigitOffsets,
            at: Some(_),
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// `judges` judges of four combos each (24 rows kept, two quads drawn)
/// and numbers of 11 and `last` kept rows.
fn kept(judges: usize, last: usize) -> Loaded {
    let value = |id: &str, digit: usize| {
        let rows = join((0..digit).map(|_| r#"{"x": 1}"#.to_owned()));
        format!(
            r#"{{"id": "{id}", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": {digit},
                "ref": 110, "offset": [{rows}]}}"#
        )
    };
    let none = r#"{"id": "none", "dst": []}"#;
    let combo =
        |v: &str| format!(r#"{{"id": "{v}", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 20}}]}}"#);
    // Combos 1 and 2 have no grade: kept, never drawn.
    let numbers = join(
        [combo("c1"), combo("c11"), combo("c11")]
            .into_iter()
            .chain((0..3).map(|_| none.to_owned()))
            .chain([combo("c1")]),
    );
    let images = join(
        [r#"{"id": "g", "dst": [{"x": 0, "y": 0, "w": 80, "h": 20}]}"#.to_owned()]
            .into_iter()
            .chain((0..6).map(|_| none.to_owned())),
    );
    let records = join((0..judges).map(|i| {
        format!(r#"{{"id": "j{i}", "index": 0, "images": [{images}], "numbers": [{numbers}]}}"#)
    }));
    let dsts = join(
        (0..judges)
            .map(|i| format!(r#"{{"id": "j{i}", "dst": [{{"w": 0, "h": 0}}]}}"#))
            .chain(
                ["n11", "nl"]
                    .map(|id| format!(r#"{{"id": "{id}", "dst": [{{"w": 10, "h": 20}}]}}"#)),
            ),
    );
    let values = join(
        [
            value("c1", 1),
            value("c11", 11),
            value("n11", 11),
            value("nl", last),
        ]
        .into_iter(),
    );
    let j = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{{"id": "n", "path": "n.png"}}],
            "image": [{{"id": "g", "src": "n", "w": 12, "h": 20}}],
            "value": [{values}], "judge": [{records}], "destination": [{dsts}]}}"#
    );
    load(&j, &[])
}

#[test]
fn kept_rows_are_capped_across_every_copy() {
    // 2,730 judges of 24 kept rows, 11 and 5 more: exactly the cap.
    let judges = 2_730;
    assert_eq!(judges * 24 + 11 + 5, MAX_DIGIT_OFFSETS);
    let l = kept(judges, 5);
    let i = l.ok();
    assert_eq!(i.report.usage.digit_offsets as usize, MAX_DIGIT_OFFSETS);
    assert_eq!(i.skin.digit_offset_count(), MAX_DIGIT_OFFSETS);
    // Shared values resolve once.
    assert_eq!(i.report.usage.resolved_digit_offsets, 1 + 11 + 11 + 5);
    match kept(judges, 6).result {
        Err(ImportError::Limit {
            limit: ImportLimit::DigitOffsets,
            at: Some(_),
        }) => {}
        other => panic!("{other:?}"),
    }
}
