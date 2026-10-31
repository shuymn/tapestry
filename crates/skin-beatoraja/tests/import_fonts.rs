//! Bitmap `.fnt` fonts: the mapping to the model's bitmap fonts, the
//! size (D12), distance-field fonts (D13), fallbacks, failures and the
//! charges of their admission.

mod common;

use std::path::Path;

use common::{has, png, skin, write};
use skin_beatoraja::{DeviationId, ImportError, ImportLimit, NoteKind, Severity};
use skin_model::{FntError, FntErrorKind, Layout, MAX_FALLBACKS, MAX_IMAGES};

/// A glyph: code, left, width, height, x offset, raw y offset, advance.
type Ch = (u32, u32, u32, u32, i32, i32, i32);

/// `A`: 10×20 at x 2, raw y 3, advance 12.
const A: Ch = (65, 0, 10, 20, 2, 3, 12);
/// `M`, 18 tall: with base 20 the ascent is 2.
const M: Ch = (77, 40, 4, 18, 0, 0, 5);

/// A size-24 font `name` with one 64×32 page `<name>.png`.
fn fnt(dir: &Path, name: &str, chars: &[Ch]) {
    let mut s = format!(
        "info face=\"t\" size=24 padding=0,0,0,0\n\
         common lineHeight=24 base=20 scaleW=64 scaleH=32 pages=1\n\
         page id=0 file=\"{name}.png\"\n"
    );
    for &(id, x, w, h, xo, yo, adv) in chars {
        s.push_str(&format!(
            "char id={id} x={x} y=0 width={w} height={h} xoffset={xo} yoffset={yo} \
             xadvance={adv} page=0 chnl=15\n"
        ));
    }
    write(dir, name, s.as_bytes());
    png(dir, &format!("{name}.png"), 64, 32);
}

/// A `size` skin with `fonts` (the `font` array's items) and one text per
/// item of `texts` (`font`, `size`, title or literal), each at
/// x 400, y 800, 300×30.
fn json(size: [u32; 2], fonts: &str, texts: &[(&str, i32, &str)]) -> String {
    let [w, h] = size;
    let items: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(i, (font, size, value))| {
            let value = if value.is_empty() {
                r#""ref": 10"#.to_owned()
            } else {
                format!(r#""constantText": "{value}""#)
            };
            format!(r#"{{"id": "t{i}", "font": "{font}", "size": {size}, {value}}}"#)
        })
        .collect();
    let dsts: Vec<String> = (0..texts.len())
        .map(|i| format!(r#"{{"id": "t{i}", "dst": [{{"x": 400, "y": 800, "w": 300, "h": 30}}]}}"#))
        .collect();
    format!(
        r#"{{"type": 0, "w": {w}, "h": {h}, "font": [{fonts}], "text": [{}],
            "destination": [{}]}}"#,
        items.join(", "),
        dsts.join(", ")
    )
}

fn import(dir: &Path) -> Result<skin_beatoraja::Imported, ImportError> {
    common::import_dir(dir)
}

/// The rects of the first frame with the lines laid out for `title`.
fn rects(i: skin_beatoraja::Imported, title: &str) -> Vec<[f32; 4]> {
    let mut d = common::play::Drawn::new(i.skin, 10);
    d.attach_bitmap(title);
    d.frame(0, &[]).iter().map(|q| q.rect).collect()
}

fn close(a: [f32; 4], b: [f32; 4]) {
    assert!(
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3),
        "{a:?} vs {b:?}"
    );
}

fn bitmap(e: FntErrorKind) -> NoteKind {
    NoteKind::BitmapFont(FntError { line: 0, kind: e })
}

#[test]
fn a_bitmap_font_draws_its_glyphs() {
    let dir = skin(&[(
        "s.json",
        &json(
            [1_920, 1_080],
            r#"{"id": "b", "path": "b.fnt"}"#,
            &[("b", 24, "")],
        ),
    )]);
    fnt(dir.path(), "b.fnt", &[A, M]);
    let i = import(dir.path()).unwrap();
    assert!(i.report.notes.is_empty(), "{:?}", i.report.notes);
    assert!(
        i.skin
            .text_requests(&Default::default(), [1_920, 1_080], &Layout::AUTHOR)
            .is_empty()
    );
    assert_eq!((i.report.usage.fonts, i.report.usage.glyphs), (1, 3));
    // Its page is a file of the sheet.
    assert_eq!(i.report.usage.files, 1);
    // The box's top is 1080 − 830; the em is 24, the font's own size.
    let r = rects(i, "AA");
    close(r[0], [402.0, 251.0, 10.0, 20.0]);
    close(r[1], [414.0, 251.0, 10.0, 20.0]);
}

#[test]
fn the_size_rounds_to_a_whole_pixel() {
    // r = 3/2: 16 · 3/2 = 24 is whole; 15 · 3/2 = 22.5 rounds up to 23.
    for (size, src, h, d12) in [
        (16, [1_280, 720], 24.0, false),
        (15, [1_280, 720], 23.0, true),
    ] {
        let dir = skin(&[(
            "s.json",
            &json(src, r#"{"id": "b", "path": "b.fnt"}"#, &[("b", size, "A")]),
        )]);
        fnt(dir.path(), "b.fnt", &[A, M]);
        let i = import(dir.path()).unwrap();
        let d = NoteKind::Deviation(DeviationId::D12);
        assert_eq!(i.report.notes.iter().any(|n| n.kind == d), d12, "{size}");
        let r = rects(i, "");
        assert!((r[0][3] - 20.0 * h / 24.0).abs() < 1e-3, "{size}: {r:?}");
    }
}

#[test]
fn distance_field_fonts_and_bad_sizes_read_nothing() {
    for kind in [1, 2] {
        let fonts = format!(r#"{{"id": "b", "path": "b.fnt", "type": {kind}}}"#);
        let dir = skin(&[("s.json", &json([1_920, 1_080], &fonts, &[("b", 24, "A")]))]);
        let i = import(dir.path()).unwrap();
        assert!(has(&i.report, NoteKind::Deviation(DeviationId::D13)));
        assert_eq!(i.report.usage.texts, 1);
        assert_eq!(i.report.usage.file_attempts, 0);
    }
    for size in [0, -5] {
        let fonts = r#"{"id": "b", "path": "b.fnt"}"#;
        let dir = skin(&[("s.json", &json([1_920, 1_080], fonts, &[("b", size, "A")]))]);
        let i = import(dir.path()).unwrap();
        assert!(has(&i.report, NoteKind::Ignored { field: "size" }));
        assert_eq!((i.report.usage.texts, i.report.usage.file_attempts), (0, 0));
    }
}

#[test]
fn fonts_share_a_primary_and_keep_their_own_fallbacks() {
    let fonts = r#"{"id": "a", "path": "p.fnt", "fallback": [{"path": "f1.fnt"}]},
                   {"id": "b", "path": "p.fnt", "fallback": [null, {"type": 0}, {"path": "f2.fnt"}]}"#;
    let dir = skin(&[(
        "s.json",
        &json(
            [1_920, 1_080],
            fonts,
            &[("a", 24, "AB"), ("b", 24, "AB"), ("a", 24, "B")],
        ),
    )]);
    fnt(dir.path(), "p.fnt", &[A, M]);
    fnt(dir.path(), "f1.fnt", &[(66, 0, 4, 20, 0, 3, 12)]);
    fnt(dir.path(), "f2.fnt", &[(66, 0, 6, 20, 0, 3, 12)]);
    let i = import(dir.path()).unwrap();
    // One read per file: three files and their pages.
    assert_eq!(i.report.usage.fonts, 3);
    assert_eq!(i.report.usage.file_attempts, 6);
    let widths: Vec<f32> = rects(i, "").iter().map(|r| r[2]).collect();
    assert_eq!(widths, [10.0, 4.0, 10.0, 6.0, 4.0]);
}

#[test]
fn a_failing_fallback_is_skipped() {
    let fonts = r#"{"id": "a", "path": "p.fnt", "fallback": [{"path": "f.fnt"}]}"#;
    let dir = skin(&[("s.json", &json([1_920, 1_080], fonts, &[("a", 24, "AB")]))]);
    fnt(dir.path(), "p.fnt", &[A, M]);
    fnt(dir.path(), "f.fnt", &[(66, 0, 4, 20, 0, 3, 12)]);
    write(dir.path(), "f.fnt.png", b"\x89PNG\r\n\x1a\nbroken");
    let i = import(dir.path()).unwrap();
    let note = i
        .report
        .notes
        .iter()
        .find(|n| n.severity == Severity::Ignored)
        .unwrap();
    assert_eq!(note.kind, bitmap(FntErrorKind::PageBroken { page: 0 }));
    assert!(note.at.is_some());
    assert_eq!(rects(i, "").len(), 1);
}

#[test]
fn a_failing_primary_is_ignored_or_dropped() {
    let fonts = r#"{"id": "a", "path": "p.fnt"}"#;
    let cases: [(&str, Severity, NoteKind); 4] = [
        (
            "page",
            Severity::Ignored,
            bitmap(FntErrorKind::PageMissing { page: 0 }),
        ),
        (
            "broken",
            Severity::Ignored,
            bitmap(FntErrorKind::PageBroken { page: 0 }),
        ),
        (
            "scale",
            Severity::Dropped,
            NoteKind::BitmapFont(FntError {
                line: 2,
                kind: FntErrorKind::Metadata("scaleW"),
            }),
        ),
        ("file", Severity::Dropped, NoteKind::MissingAsset),
    ];
    for (case, severity, kind) in cases {
        let dir = skin(&[(
            "s.json",
            &json([1_920, 1_080], fonts, &[("a", 24, "A"), ("a", 24, "A")]),
        )]);
        fnt(dir.path(), "p.fnt", &[A, M]);
        match case {
            "page" => std::fs::remove_file(dir.path().join("p.fnt.png")).unwrap(),
            "broken" => write(dir.path(), "p.fnt.png", b"\x89PNG\r\n\x1a\nbroken"),
            "scale" => {
                let s = std::fs::read_to_string(dir.path().join("p.fnt")).unwrap();
                write(
                    dir.path(),
                    "p.fnt",
                    s.replacen(" scaleW=64", "", 1).as_bytes(),
                );
            }
            _ => std::fs::remove_file(dir.path().join("p.fnt")).unwrap(),
        }
        let i = import(dir.path()).unwrap();
        let notes: Vec<_> = i
            .report
            .notes
            .iter()
            .map(|n| (n.severity, n.kind))
            .collect();
        // Each text reports; the file is read once.
        assert_eq!(notes, [(severity, kind); 2], "{case}");
        assert_eq!(i.report.usage.texts, 0, "{case}");
        assert!(i.report.usage.file_attempts <= 2, "{case}");
    }
}

#[test]
fn the_fallback_list_is_capped() {
    // The entries are counted before any is read: missing files, each
    // skipped when read, count too.
    let list = |n: usize, path: &str| {
        let items = vec![format!(r#"{{"path": "{path}"}}"#); n].join(", ");
        format!(r#"{{"id": "a", "path": "p.fnt", "fallback": [{items}]}}"#)
    };
    for (n, ok, path) in [
        (MAX_FALLBACKS, true, "f.fnt"),
        (MAX_FALLBACKS + 1, false, "f.fnt"),
        (MAX_FALLBACKS, true, "none.fnt"),
        (MAX_FALLBACKS + 1, false, "none.fnt"),
    ] {
        let fonts = list(n, path);
        let dir = skin(&[("s.json", &json([1_920, 1_080], &fonts, &[("a", 24, "A")]))]);
        fnt(dir.path(), "p.fnt", &[A, M]);
        fnt(dir.path(), "f.fnt", &[A]);
        match import(dir.path()) {
            Ok(i) => assert!(ok && i.report.usage.texts == 1),
            Err(ImportError::Limit { limit, .. }) => {
                assert!(!ok);
                assert_eq!(limit, ImportLimit::Fonts);
            }
            Err(e) => panic!("{e:?}"),
        }
    }
}

#[test]
fn a_page_past_the_image_budget_stops_before_it_is_read() {
    // All but one image allowance spent on images that failed: each
    // destination charges its image before its file is looked for.
    let images = MAX_IMAGES - 1;
    let dsts = vec![r#"{"id": "i", "dst": [{"x": 0, "y": 0, "w": 1, "h": 1}]}"#; images].join(", ");
    let s = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "image": [{{"id": "i", "src": "none"}}],
            "font": [{{"id": "a", "path": "p.fnt"}}],
            "text": [{{"id": "t", "font": "a", "size": 24, "constantText": "A"}}],
            "destination": [{dsts}, {{"id": "t", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}]}}"#
    );
    let dir = skin(&[("s.json", &s)]);
    let two = "info face=\"t\" size=24 padding=0,0,0,0\n\
               common lineHeight=24 base=20 scaleW=64 scaleH=32 pages=2\n\
               page id=0 file=\"a.png\"\npage id=1 file=\"b.png\"\n\
               char id=65 x=0 y=0 width=1 height=1 xoffset=0 yoffset=0 xadvance=1 page=1\n";
    write(dir.path(), "p.fnt", two.as_bytes());
    png(dir.path(), "a.png", 4, 4);
    png(dir.path(), "b.png", 4, 4);
    match import(dir.path()) {
        Err(ImportError::Limit { limit, .. }) => assert_eq!(limit, ImportLimit::Images),
        other => panic!("{:?}", other.map(|i| i.report.usage)),
    }
}

#[test]
fn page_bytes_stay_charged_when_a_font_fails() {
    // Two fonts of 16 pages of 32 MiB each (a sparse PNG), each failing
    // after its pages' bytes were charged: the first fills the 512 MiB,
    // so the second's first page passes it.
    let fonts = r#"{"id": "a", "path": "a.fnt"}, {"id": "b", "path": "b.fnt"}"#;
    let dir = skin(&[(
        "s.json",
        &json([1_920, 1_080], fonts, &[("a", 24, "A"), ("b", 24, "A")]),
    )]);
    png(dir.path(), "big.png", 4, 4);
    std::fs::File::options()
        .write(true)
        .open(dir.path().join("big.png"))
        .unwrap()
        .set_len(skin_model::MAX_FILE_BYTES)
        .unwrap();
    let pages: String = (0..16)
        .map(|p| format!("page id={p} file=\"big.png\"\n"))
        .collect();
    let text = format!(
        "info face=\"t\" size=24 padding=0,0,0,0\n\
         common lineHeight=24 base=20 scaleW=4 scaleH=4 pages=16\n{pages}\
         char id=65 x=0 y=0 width=8 height=4 xoffset=0 yoffset=0 xadvance=4 page=0\n"
    );
    write(dir.path(), "a.fnt", text.as_bytes());
    write(dir.path(), "b.fnt", text.as_bytes());
    assert_eq!(
        16 * skin_model::MAX_FILE_BYTES,
        skin_model::MAX_TOTAL_FILE_BYTES
    );
    match import(dir.path()) {
        Err(ImportError::Limit { limit, .. }) => assert_eq!(limit, ImportLimit::FileTotals),
        other => panic!("{:?}", other.map(|i| i.report.notes)),
    }
    // One such font alone fails by itself, its bytes at the total.
    let one = r#"{"id": "a", "path": "a.fnt"}"#;
    write(
        dir.path(),
        "s.json",
        json([1_920, 1_080], one, &[("a", 24, "A")]).as_bytes(),
    );
    let i = import(dir.path()).unwrap();
    assert_eq!(
        common::kinds(&i.report),
        [NoteKind::BitmapFont(FntError {
            line: 0,
            kind: FntErrorKind::GlyphOutside { code: 65 },
        })]
    );
}

#[test]
fn a_page_with_corrupt_pixels_fails_its_font_not_the_atlas() {
    let fonts = r#"{"id": "a", "path": "p.fnt", "fallback": [{"path": "f.fnt"}]}"#;
    let dir = skin(&[("s.json", &json([1_920, 1_080], fonts, &[("a", 24, "AB")]))]);
    fnt(dir.path(), "p.fnt", &[A, M]);
    fnt(dir.path(), "f.fnt", &[(66, 0, 4, 20, 0, 3, 12)]);
    // The header is whole; the pixel data is cut short.
    let page = std::fs::read(dir.path().join("f.fnt.png")).unwrap();
    write(dir.path(), "f.fnt.png", &page[..page.len() - 24]);
    let i = import(dir.path()).unwrap();
    let notes: Vec<_> = i
        .report
        .notes
        .iter()
        .map(|n| (n.severity, n.kind))
        .collect();
    assert_eq!(
        notes,
        [(
            Severity::Ignored,
            bitmap(FntErrorKind::PageBroken { page: 0 })
        )]
    );
    let limits = skin_model::AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    assert!(i.skin.atlas(skin_model::Variant::Lo, limits).is_ok());
    assert_eq!(rects(i, "").len(), 1);
}

#[test]
fn a_fallback_refused_only_by_our_rules_is_dropped() {
    let fonts = r#"{"id": "a", "path": "p.fnt", "fallback": [{"path": "f.fnt"}]}"#;
    let dir = skin(&[("s.json", &json([1_920, 1_080], fonts, &[("a", 24, "AB")]))]);
    fnt(dir.path(), "p.fnt", &[A, M]);
    fnt(dir.path(), "f.fnt", &[(66, 0, 4, 20, 0, 3, 12)]);
    let s = std::fs::read_to_string(dir.path().join("f.fnt")).unwrap();
    write(
        dir.path(),
        "f.fnt",
        s.replacen(" scaleW=64", "", 1).as_bytes(),
    );
    let i = import(dir.path()).unwrap();
    let notes: Vec<_> = i
        .report
        .notes
        .iter()
        .map(|n| (n.severity, n.kind))
        .collect();
    let kind = NoteKind::BitmapFont(FntError {
        line: 2,
        kind: FntErrorKind::Metadata("scaleW"),
    });
    assert_eq!(notes, [(Severity::Dropped, kind)]);
    assert_eq!(i.report.usage.texts, 1);
}
