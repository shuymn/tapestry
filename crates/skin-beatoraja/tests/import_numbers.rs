//! Numbers: the effective property and its role, the glyph
//! sets and their cut, and the strip's exact layout.

mod common;

use common::{has, import_body, import_config, import_sized};
use skin_beatoraja::{CriticalPart, ImportError, NoteKind, OffsetValue, PropertyKind};

/// A number of n.png (120×20) with `image` fields drawn at `dst`.
fn number(size: [u32; 2], image: &str, dst: &str) -> common::Loaded {
    import_sized(
        size,
        &format!(
            r#""value": [{{"id": "v", "src": "n", "y": 0, "h": 20 {image}}}],
               "destination": [{{"id": "v", "timer": 41, "loop": 100, {dst}}}]"#
        ),
        &[],
    )
}

const TEN: &str = r#", "w": 120, "divx": 10"#;

/// Whether the number is critical: an undefined op refuses it.
fn critical(props: &str) -> Option<CriticalPart> {
    let r = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 4, {props}"#),
        r#""op": [7777], "dst": [{"w": 12, "h": 20}]"#,
    );
    match r.result {
        Ok(i) => {
            assert!(
                i.report
                    .notes
                    .iter()
                    .any(|n| n.kind == NoteKind::UndefinedOp(7_777))
            );
            None
        }
        Err(ImportError::Critical { part, .. }) => Some(part),
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn value_falls_back_to_ref() {
    for value in ["null", "9999", "true"] {
        assert_eq!(
            critical(&format!(r#""value": {value}, "ref": 71"#)),
            Some(CriticalPart::ExScore),
            "{value}"
        );
    }
    assert_eq!(
        critical(r#""value": 71, "ref": 110"#),
        Some(CriticalPart::ExScore)
    );
    assert_eq!(critical(r#""value": 105"#), Some(CriticalPart::MaxCombo));
    assert_eq!(critical(r#""value": "score""#), Some(CriticalPart::ExScore));
    assert_eq!(critical(r#""value": 110, "ref": 71"#), None);
    // The object's own id names nothing.
    let loaded = import_body(
        r#""value": [{"id": "score", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 110}],
           "destination": [{"id": "score", "op": [7777], "dst": [{"w": 12, "h": 20}]}]"#,
    );
    assert!(has(loaded.report(), NoteKind::UndefinedOp(7_777)));
    // An unknown id alone shows nothing; an unsupported one drops.
    let loaded = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 4, "value": 9999"#),
        r#""dst": [{"w": 12, "h": 20}]"#,
    );
    assert!(has(loaded.report(), NoteKind::Ignored { field: "value" }));
    assert_eq!(loaded.report().usage.images, 0);
    let loaded = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 4, "ref": 380"#),
        r#""dst": [{"w": 12, "h": 20}]"#,
    );
    let why = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Integer,
        id: Some(380),
    };
    assert!(has(loaded.report(), why));
    // The empty-poor count is decorative and drawn.
    let loaded = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 4, "ref": 420"#),
        r#""dst": [{"w": 12, "h": 20}]"#,
    );
    assert!(loaded.report().notes.is_empty());
    assert_eq!(loaded.rects(0), [[36, 1_060, 12, 20]]);
}

#[test]
fn a_constant_width_strip_is_exact() {
    // r = 3/2: X 15, W 22.5, P 22.5 + 1.5 = 24: w′ 23, space′ 24 − 23,
    // the last cell at rnd(15 + 4·24) = 111.
    let loaded = number(
        [1_280, 720],
        &format!(r#"{TEN}, "digit": 5, "space": 1, "ref": 110"#),
        r#""dst": [{"x": 10, "y": 0, "w": 15, "h": 20}]"#,
    );
    let mut d = loaded.drawn();
    assert_eq!(d.rects(0), [[111, 1_050, 23, 30]]);
    // Three digits, a pitch of 23 + 1 apart.
    d.tally.counts.perfect = 123;
    let mut lefts: Vec<i32> = d.rects(0).iter().map(|r| r[0]).collect();
    lefts.sort_unstable();
    assert_eq!(lefts, [63, 87, 111]);
}

#[test]
fn a_varying_width_strip_keeps_each_last_cell() {
    // W 22.5 → 24, P 24 → 25.5: space′ rnd(1.5) = 2, the last cell at
    // 111 and then rnd(15 + 4·25.5) = 117.
    let loaded = number(
        [1_280, 720],
        &format!(r#"{TEN}, "digit": 5, "space": 1, "ref": 110"#),
        r#""dst": [{"time": 0, "x": 10, "y": 0, "w": 15, "h": 20},
                   {"time": 100, "w": 16}]"#,
    );
    let mut d = loaded.drawn();
    assert_eq!(d.rects(0), [[111, 1_050, 23, 30]]);
    assert_eq!(d.rects(100), [[117, 1_050, 24, 30]]);
    // Three digits: cells a pitch of w′ + space′ apart, 23 + 2 at 0 ms
    // and 24 + 2 at 100 ms, ending at the last cell.
    d.tally.counts.perfect = 123;
    let lefts = |d: &mut common::play::Drawn, ms| {
        let mut x: Vec<i32> = d.rects(ms).iter().map(|r| r[0]).collect();
        x.sort_unstable();
        x
    };
    assert_eq!(lefts(&mut d, 0), [61, 86, 111]);
    assert_eq!(lefts(&mut d, 100), [65, 91, 117]);
}

fn with_offset(image: &str, dst: &str) -> common::Loaded {
    import_config(
        [1_920, 1_080],
        &format!(
            r#""offset": [{{"name": "o", "id": 40}}],
               "value": [{{"id": "v", "src": "n", "h": 20 {TEN} {image}}}],
               "destination": [{{"id": "v", "timer": 41, "offset": 40, {dst}}}]"#
        ),
        &[],
        |c| {
            c.offsets.insert(
                40,
                OffsetValue {
                    x: 7,
                    w: 4,
                    ..OffsetValue::default()
                },
            );
        },
    )
}

#[test]
fn offsets_move_the_strip_before_the_layout() {
    // X = 100 + 7 − 2, W = 14.
    let loaded = with_offset(
        r#", "digit": 1, "ref": 110"#,
        r#""dst": [{"x": 100, "w": 10, "h": 20}]"#,
    );
    assert_eq!(loaded.rects(0), [[105, 1_060, 14, 20]]);
    // 13 digits: 11 cells shifted by P·2, 0 or P, then the one glyph at
    // the right, the left or the middle of the 11 (P = 14, X = 5).
    for (align, x) in [(0, 5 + 28 + 140), (1, 5), (2, 5 + 14 + 5 * 14)] {
        let loaded = with_offset(
            &format!(r#", "digit": 13, "align": {align}, "ref": 110"#),
            r#""dst": [{"x": 0, "w": 10, "h": 20}]"#,
        );
        assert_eq!(loaded.rects(0), [[x, 1_060, 14, 20]], "align {align}");
    }
}

#[test]
fn glyph_sets_are_cut_to_whole_rows() {
    // 12 frames in a row: eleven glyphs, the twelfth cut.
    let loaded = number(
        [1_920, 1_080],
        r#", "w": 120, "divx": 12, "digit": 3, "ref": 110"#,
        r#""dst": [{"w": 10, "h": 20}]"#,
    );
    assert!(loaded.report().notes.is_empty());
    assert_eq!(loaded.ok().skin.sheet().frame_count(), 11);
    // 12 frames in a 4×3 grid: 11 is not whole rows.
    let loaded = import_body(
        r#""value": [{"id": "v", "src": "a", "w": 64, "h": 63, "divx": 4, "divy": 3,
                      "digit": 3, "ref": 110}],
           "destination": [{"id": "v", "dst": [{"w": 16, "h": 21}]}]"#,
    );
    assert!(has(
        loaded.report(),
        NoteKind::UnsupportedField { field: "divx" }
    ));
    // Fewer frames than a set: nothing to draw.
    let loaded = number(
        [1_920, 1_080],
        r#", "w": 120, "divx": 5, "digit": 3, "ref": 110"#,
        r#""dst": [{"w": 10, "h": 20}]"#,
    );
    assert!(has(loaded.report(), NoteKind::Ignored { field: "src" }));
}

#[test]
fn padding_and_cells() {
    let drop = |image: &str, field| {
        let loaded = number([1_920, 1_080], image, r#""dst": [{"w": 12, "h": 20}]"#);
        assert!(
            has(loaded.report(), NoteKind::UnsupportedField { field }),
            "{image}: {:?}",
            loaded.report()
        );
    };
    // The ten-glyph set has no padding glyph.
    drop(
        &format!(r#"{TEN}, "digit": 3, "padding": 2, "ref": 110"#),
        "padding",
    );
    // More than 11 cells only with blanks.
    drop(
        &format!(r#"{TEN}, "digit": 12, "padding": 1, "ref": 110"#),
        "digit",
    );
    drop(&format!(r#"{TEN}, "digit": -1, "ref": 110"#), "digit");
    // Twenty-four glyphs padded need two cells.
    drop(
        r#", "w": 120, "divx": 24, "digit": 1, "zeropadding": 1, "ref": 110"#,
        "digit",
    );
    let loaded = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 0, "ref": 110"#),
        r#""dst": [{"w": 12, "h": 20}]"#,
    );
    assert!(has(loaded.report(), NoteKind::Ignored { field: "digit" }));
    // Zero padding draws every cell.
    let loaded = number(
        [1_920, 1_080],
        &format!(r#"{TEN}, "digit": 3, "padding": 1, "ref": 110"#),
        r#""dst": [{"w": 12, "h": 20}]"#,
    );
    assert_eq!(loaded.ok().skin.quad_capacity(), 3);
    assert_eq!(loaded.quads(0).len(), 3);
    // A digit offset is not imported.
    drop(
        &format!(r#"{TEN}, "digit": 3, "ref": 110, "offset": [{{"x": 0}}, {{"y": 2}}]"#),
        "offset",
    );
}

/// The rate numbers, one row each from y = 100, each named alias once,
/// below an EX score of one cell at y = 0 that tells the glyphs apart:
/// `(id, property, cells, zero padding)`.
const RATES: [(&str, &str, i32, bool); 6] = [
    ("cp", r#""ref": 102"#, 3, false),
    ("cf", r#""value": "score_rate_afterdot""#, 2, true),
    ("wp", r#""value": 115"#, 3, false),
    ("wf", r#""ref": 116"#, 2, true),
    ("wp2", r#""value": "score_rate2""#, 3, false),
    ("wf2", r#""value": 40, "ref": 156"#, 2, true),
];

fn rates() -> common::Loaded {
    let mut values = vec![
        r#"{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}"#
            .to_owned(),
    ];
    let mut dsts = vec![r#"{"id": "ex", "dst": [{"x": 0, "y": 0, "w": 12, "h": 20}]}"#.to_owned()];
    for (i, (id, prop, digit, zero)) in RATES.iter().enumerate() {
        let padding = i32::from(*zero);
        values.push(format!(
            r#"{{"id": "{id}", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": {digit},
                 "padding": {padding}, {prop}}}"#
        ));
        let y = 100 * (i + 1);
        dsts.push(format!(
            r#"{{"id": "{id}", "dst": [{{"x": 0, "y": {y}, "w": 12, "h": 20}}]}}"#
        ));
    }
    import_body(&format!(
        r#""value": [{}], "destination": [{}]"#,
        values.join(","),
        dsts.join(",")
    ))
}

/// The rows' digits as drawn at `ms`, read through the EX score's glyph
/// for each digit; a blank cell is not drawn.
fn rows(d: &mut common::play::Drawn, ms: i64) -> Vec<String> {
    let tally = d.tally;
    let mut glyph = std::collections::HashMap::new();
    for digit in 0..10_u32 {
        d.tally.ex_score = digit;
        let q = d.frame(ms, &[]);
        let ex = q
            .iter()
            .max_by(|a, b| a.rect[1].total_cmp(&b.rect[1]))
            .unwrap();
        glyph.insert(
            ex.uv.map(f32::to_bits),
            char::from_digit(digit, 10).unwrap(),
        );
    }
    d.tally = tally;
    let q = d.frame(ms, &[]);
    let mut rows = vec![String::new(); RATES.len()];
    let mut cells: Vec<_> = q.iter().map(|q| (q.rect[1], q.rect[0], q.uv)).collect();
    cells.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.total_cmp(&b.1)));
    let ys: Vec<f32> = {
        let mut ys: Vec<f32> = cells.iter().map(|c| c.0).collect();
        ys.dedup();
        ys
    };
    // The first row is the EX score's.
    for (y, uv) in cells.iter().map(|c| (c.0, c.2)) {
        let row = ys.iter().position(|&r| r == y).unwrap();
        if row > 0 {
            rows[row - 1].push(glyph[&uv.map(f32::to_bits)]);
        }
    }
    rows
}

#[test]
fn rates_show_whole_percent_and_hundredths() {
    let loaded = rates();
    // One D4 note at each rate number, none elsewhere.
    let d4: Vec<_> = loaded
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Deviation(skin_beatoraja::DeviationId::D4))
        .map(|n| n.at.unwrap().line)
        .collect();
    assert_eq!(d4.len(), RATES.len(), "{:?}", loaded.report().notes);
    assert!(
        loaded
            .report()
            .notes
            .iter()
            .all(|n| n.kind == NoteKind::Deviation(skin_beatoraja::DeviationId::D4))
    );
    // The rates are decorative: an undefined op drops one.
    let mut d = loaded.drawn_over(4);
    // Before any judgment, and again after a retry: 100.00 and 0.00 (the
    // source draws no digits until it has a score).
    let fresh = ["100", "00", "0", "00", "0", "00"];
    assert_eq!(rows(&mut d, 0), fresh);
    // EX 3 over 2 judged of 4 targets: 75.00 and 37.50.
    d.tally.counts.perfect = 1;
    d.tally.counts.great = 1;
    d.tally.ex_score = 3;
    assert_eq!(rows(&mut d, 16), ["75", "00", "37", "50", "37", "50"]);
    d.retry(4);
    d.tally = common::play::tally(0, 0);
    assert_eq!(rows(&mut d, 0), fresh);
}

#[test]
fn rate_numbers_are_decorative() {
    for (_, prop, ..) in RATES {
        assert_eq!(critical(prop), None, "{prop}");
    }
}

#[test]
fn a_strip_past_the_model_is_a_range_error() {
    let e = number(
        [1_280, 720],
        r#", "w": 120, "divx": 10, "space": 2147483647, "digit": 2147483646, "ref": 110"#,
        r#""dst": [{"w": 2, "h": 20}]"#,
    )
    .err();
    assert!(matches!(e, ImportError::Range { .. }), "{e:?}");
}
