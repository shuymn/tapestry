//! Text (§4.10): the effective string, the box, the fonts and the
//! fallbacks of D5–D7.

mod common;

use common::play::round;
use common::{Loaded, font, has, hd, import_dir, skin};
use skin_beatoraja::{DeviationId, NoteKind};
use skin_model::{ChartText, FontRef, Layout, TextRequest};

const CHART: ChartText<'static> = ChartText {
    title: "Title",
    subtitle: "Sub",
    artist: "Artist",
    subartist: "",
    genre: "Genre",
};

/// A skin of size `size` with fonts `f` (f.ttf) and `b` (b.fnt), and one
/// text of `fields` drawn at `dst`.
fn text(size: [u32; 2], fields: &str, dst: &str) -> Loaded {
    let [w, h] = size;
    let json = format!(
        r#"{{"type": 0, "w": {w}, "h": {h},
            "font": [{{"id": "f", "path": "f.ttf"}}, {{"id": "b", "path": "b.fnt"}},
                     {{"id": "m", "path": "missing.ttf"}}],
            "text": [{{"id": "t", "size": 24, {fields}}}],
            "destination": [{{"id": "t", "timer": 41, {dst}}}]}}"#
    );
    let dir = skin(&[("s.json", &json)]);
    font(dir.path(), "f.ttf");
    let result = import_dir(dir.path());
    Loaded { dir, result }
}

const BOX: &str = r#""dst": [{"x": 400, "y": 800, "w": 300, "h": 30}]"#;

fn requests(l: &Loaded) -> Vec<TextRequest> {
    l.ok()
        .skin
        .text_requests(&CHART, [1_920, 1_080], &Layout::AUTHOR)
}

fn d(id: DeviationId) -> NoteKind {
    NoteKind::Deviation(id)
}

#[test]
fn value_overrides_ref_which_overrides_the_constant() {
    let one = |fields: &str| {
        let l = text([1_920, 1_080], &format!(r#""font": "f", {fields}"#), BOX);
        requests(&l).into_iter().map(|r| r.text).collect::<Vec<_>>()
    };
    assert_eq!(one(r#""ref": 9999, "constantText": "hi""#), ["hi"]);
    assert_eq!(one(r#""value": 10, "ref": 14"#), ["Title"]);
    assert_eq!(one(r#""ref": 14, "constantText": "hi""#), ["Artist"]);
    assert_eq!(one(r#""ref": 12"#), ["Title Sub"]);
    // A known unsupported property drops; nothing at all is ignored.
    let l = text([1_920, 1_080], r#""font": "f", "ref": 1010"#, BOX);
    assert_eq!(l.report().usage.texts, 0);
    assert!(!l.report().notes.is_empty());
    let l = text([1_920, 1_080], r#""font": "f""#, BOX);
    assert!(has(
        l.report(),
        NoteKind::Ignored {
            field: "constantText"
        }
    ));
}

#[test]
fn literals_are_cleaned_and_cut() {
    let long = "a".repeat(257);
    let l = text(
        [1_920, 1_080],
        &format!(r#""font": "f", "constantText": "{long}""#),
        BOX,
    );
    assert!(has(l.report(), d(DeviationId::D6)));
    assert_eq!(requests(&l)[0].text.len(), 256);
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "a\u0007b""#,
        BOX,
    );
    assert!(has(l.report(), d(DeviationId::D6)));
    assert_eq!(requests(&l)[0].text, "ab");
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "\u0007""#,
        BOX,
    );
    assert!(has(
        l.report(),
        NoteKind::Ignored {
            field: "constantText"
        }
    ));
    assert_eq!(l.report().usage.texts, 0);
}

#[test]
fn the_box_moves_by_its_alignment() {
    // A 300-wide box at 400 shifted by 0, 150 or 300, a 30-wide line
    // (one em) at its left, middle or right.
    for (align, x) in [(0, 400), (1, 250 + 135), (2, 100 + 270)] {
        let l = text(
            [1_920, 1_080],
            &format!(r#""font": "f", "constantText": "x", "align": {align}"#),
            BOX,
        );
        assert!(l.report().notes.is_empty());
        let mut d = l.drawn();
        d.attach_lines(1.0, 1.0);
        let q = d.frame(0, &[]);
        assert_eq!(round(q[0].rect)[0], x, "align {align}");
    }
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x", "align": 3"#,
        BOX,
    );
    assert!(has(
        l.report(),
        NoteKind::UnsupportedField { field: "align" }
    ));
}

#[test]
fn bitmap_fonts_draw_with_the_default_font_at_their_size() {
    // r = 3/2: the em is 24 · 3/2 = 36, the box's top kept (1080 − 1245).
    let l = text(
        [1_280, 720],
        r#""font": "b", "constantText": "x""#,
        r#""dst": [{"x": 10, "y": 400, "w": 100, "h": 30}]"#,
    );
    assert!(has(l.report(), d(DeviationId::D7)));
    let r = requests(&l);
    assert_eq!(r[0].font, FontRef::Default);
    assert!((r[0].em_px - 36.0).abs() < 1e-3, "{}", r[0].em_px);
    let tt = text(
        [1_280, 720],
        r#""font": "f", "constantText": "x""#,
        r#""dst": [{"x": 10, "y": 400, "w": 100, "h": 30}]"#,
    );
    assert_eq!(requests(&tt)[0].font, FontRef::File(0));
    assert!((requests(&tt)[0].em_px - 45.0).abs() < 1e-3);
}

#[test]
fn overflow_styles_and_ignored_fields() {
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x", "overflow": 2"#,
        BOX,
    );
    assert!(has(l.report(), d(DeviationId::D6)));
    // A 20-em line (600 px) shrinks into the 300-wide box; spilling, it
    // keeps its width.
    let width = |l: Loaded| {
        let mut d = l.drawn();
        d.attach_lines(20.0, 1.0);
        round(d.frame(0, &[])[0].rect)[2]
    };
    assert_eq!(width(l), 300);
    let spill = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x", "overflow": 0"#,
        BOX,
    );
    assert_eq!(width(spill), 600);
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x", "overflow": 3"#,
        BOX,
    );
    assert!(has(
        l.report(),
        NoteKind::UnsupportedField { field: "overflow" }
    ));
    for style in [
        r#""outlineWidth": 2"#,
        r#""shadowOffsetX": 1.5"#,
        r#""wrapping": true"#,
    ] {
        let l = text(
            [1_920, 1_080],
            &format!(r#""font": "f", "constantText": "x", {style}"#),
            BOX,
        );
        assert!(has(l.report(), d(DeviationId::D6)), "{style}");
        assert_eq!(l.report().usage.texts, 1);
    }
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x", "editable": true"#,
        r#""stretch": 1, "dst": [{"x": 400, "y": 800, "w": 300, "h": 30, "angle": 5}]"#,
    );
    for field in ["editable", "stretch", "angle"] {
        assert!(has(l.report(), NoteKind::Ignored { field }), "{field}");
    }
    assert_eq!(l.quads(0).len(), 0, "no line attached");
}

#[test]
fn fonts_and_boxes_that_cannot_draw() {
    let l = text(
        [1_920, 1_080],
        r#""font": "none", "constantText": "x""#,
        BOX,
    );
    assert!(has(l.report(), NoteKind::Ignored { field: "font" }));
    let l = text([1_920, 1_080], r#""font": "m", "constantText": "x""#, BOX);
    assert!(has(l.report(), NoteKind::MissingAsset));
    assert_eq!(l.report().usage.texts, 0);
    let l = text(
        [1_920, 1_080],
        r#""font": "f", "constantText": "x""#,
        r#""dst": [{"x": 400, "y": 800, "w": 300, "h": 0}]"#,
    );
    assert!(has(l.report(), NoteKind::UnsupportedField { field: "dst" }));
    // The source file need not hold a text to be a play skin.
    let l = Loaded {
        result: import_dir(skin(&[("s.json", &hd(r#""text": []"#))]).path()),
        dir: tempfile::tempdir().unwrap(),
    };
    assert_eq!(l.report().usage.texts, 0);
}
