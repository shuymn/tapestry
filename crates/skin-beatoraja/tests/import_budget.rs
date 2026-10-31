//! The importer's budgets through the mapping, each at its cap and one
//! past it: the charged counters (`Report::usage`) and what the skin
//! keeps (its sheet, ops and quads). The precedence witnesses show a
//! limit wins over an asset failure met later.

mod common;

use std::fs;

use common::{Loaded, font, hd, import_with, join, png, skin};
use skin_beatoraja::{ImportError, ImportLimit};
use skin_model::{
    MAX_ELEMENTS, MAX_FILES, MAX_FONTS, MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS,
    MAX_TEXT_ELEMENTS, MAX_TOTAL_FILE_BYTES, MAX_WIDGET_QUADS,
};

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap()
}

/// Imports `body` with sources `a` (a 64×64 `a.png`), `bad` (`bad.png`)
/// and `gone` (`gone.png`), after `setup` writes more files.
fn load(body: &str, setup: impl FnOnce(&std::path::Path)) -> Loaded {
    let json = hd(&format!(
        r#""source": [{{"id": "a", "path": "a.png"}}, {{"id": "bad", "path": "bad.png"}},
                      {{"id": "gone", "path": "gone.png"}}], {body}"#
    ));
    let dir = skin(&[("s.json", &json)]);
    png(dir.path(), "a.png", 64, 64);
    setup(dir.path());
    let result = import_with(dir.path(), |_| {});
    Loaded { dir, result }
}

#[track_caller]
fn limit(l: Loaded, want: ImportLimit) {
    match l.result {
        Err(ImportError::Limit { limit, at }) => {
            assert_eq!(limit, want);
            assert!(at.is_some());
        }
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

const KEY: &str = r#"{"w": 1, "h": 1}"#;

#[test]
fn elements() {
    let body = |n: usize| {
        let dsts = join((0..n).map(|_| format!(r#"{{"id": "i", "dst": [{KEY}]}}"#)));
        format!(r#""image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}], "destination": [{dsts}]"#)
    };
    let l = load(&body(MAX_ELEMENTS), |_| {});
    assert_eq!(l.report().usage.elements, as_u32(MAX_ELEMENTS));
    assert_eq!(l.ok().skin.quad_capacity(), as_u32(MAX_ELEMENTS));
    assert_eq!(l.ok().skin.sheet().image_count(), 1);
    limit(load(&body(MAX_ELEMENTS + 1), |_| {}), ImportLimit::Elements);
}

#[test]
fn keys() {
    let track = join((0..256).map(|t| format!(r#"{{"time": {t}, "w": 1, "h": 1}}"#)));
    let body = |extra: bool| {
        let mut dsts: Vec<String> = (0..MAX_SKIN_KEYS / 256)
            .map(|_| format!(r#"{{"id": "i", "loop": 255, "dst": [{track}]}}"#))
            .collect();
        if extra {
            dsts.push(format!(r#"{{"id": "i", "dst": [{KEY}]}}"#));
        }
        format!(
            r#""image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
               "destination": [{}]"#,
            dsts.join(",")
        )
    };
    let l = load(&body(false), |_| {});
    assert_eq!(l.report().usage.keys, as_u32(MAX_SKIN_KEYS));
    assert_eq!(l.ok().skin.quad_capacity(), as_u32(MAX_SKIN_KEYS / 256));
    limit(load(&body(true), |_| {}), ImportLimit::Keys);
}

/// Ops of a destination with `ops`, and of a two-cell count.
fn ops_skin(big: usize, small: usize, extra: bool) -> String {
    let big_ops = join((230..=240).chain(-240..=-230).map(|i| i.to_string()));
    let mut dsts: Vec<String> = (0..big)
        .map(|_| format!(r#"{{"id": "i", "op": [{big_ops}], "dst": [{KEY}]}}"#))
        .collect();
    dsts.extend(
        (0..small + usize::from(extra)).map(|_| format!(r#"{{"id": "v", "dst": [{KEY}]}}"#)),
    );
    format!(
        r#""image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
           "value": [{{"id": "v", "src": "a", "w": 60, "h": 1, "divx": 10, "digit": 1,
                      "ref": 110}}],
           "destination": [{}]"#,
        dsts.join(",")
    )
}

#[test]
fn ops() {
    // The ops of one condition list, and of one number's value.
    let big = load(&ops_skin(1, 0, false), |_| {}).report().usage.ops as usize;
    let small = load(&ops_skin(0, 1, false), |_| {}).report().usage.ops as usize;
    assert_eq!(small, 1);
    let n = MAX_SKIN_OPS / big;
    let rest = MAX_SKIN_OPS - n * big;
    let l = load(&ops_skin(n, rest, false), |_| {});
    assert_eq!(l.report().usage.ops, as_u32(MAX_SKIN_OPS));
    assert_eq!(l.ok().skin.ops().total(), MAX_SKIN_OPS);
    limit(load(&ops_skin(n, rest, true), |_| {}), ImportLimit::Ops);
}

#[test]
fn images() {
    let body = |n: usize| {
        let images =
            join((0..n).map(|i| format!(r#"{{"id": "{i}", "src": "a", "w": 1, "h": 1}}"#)));
        let dsts = join((0..n).map(|i| format!(r#"{{"id": "{i}", "dst": [{KEY}]}}"#)));
        format!(r#""image": [{images}], "destination": [{dsts}]"#)
    };
    let l = load(&body(MAX_ELEMENTS), |_| {});
    let u = l.report().usage;
    assert_eq!(
        (u.images, u.frames),
        (as_u32(MAX_ELEMENTS), as_u32(MAX_ELEMENTS))
    );
    assert_eq!(l.ok().skin.sheet().image_count(), MAX_ELEMENTS);
    // One more image needs one more element: the element cap comes first
    // (the image cap past it is charged directly in `budget.rs`).
    limit(load(&body(MAX_ELEMENTS + 1), |_| {}), ImportLimit::Elements);
}

/// 32 images of 1,024 frames, each drawn with both filters, then `extra`.
fn frames_skin(extra: &str) -> String {
    let images = join((0..32).map(|i| {
        format!(r#"{{"id": "{i}", "src": "a", "w": 64, "h": 64, "divx": 32, "divy": 32}}"#)
    }));
    let dsts = join((0..64).map(|k| {
        format!(
            r#"{{"id": "{}", "filter": {}, "dst": [{KEY}]}}"#,
            k / 2,
            k % 2
        )
    }));
    format!(
        r#""image": [{images}, {{"id": "one", "src": "a", "w": 1, "h": 1}},
                     {{"id": "bad", "src": "bad", "w": 1, "h": 1}}],
           "value": [{{"id": "ex", "src": "bad", "w": 10, "h": 1, "divx": 10, "digit": 4,
                      "ref": 71}}],
           "destination": [{dsts} {extra}]"#
    )
}

#[test]
fn frames_with_filter_variants() {
    let l = load(&frames_skin(""), |_| {});
    let u = l.report().usage;
    assert_eq!((u.images, u.frames), (64, as_u32(MAX_SKIN_FRAMES)));
    let sheet = l.ok().skin.sheet();
    assert_eq!(
        (sheet.image_count(), sheet.frame_count()),
        (64, MAX_SKIN_FRAMES)
    );
    let extra = format!(r#", {{"id": "one", "dst": [{KEY}]}}"#);
    limit(load(&frames_skin(&extra), |_| {}), ImportLimit::Frames);
}

#[test]
fn a_frame_overflow_wins_over_a_malformed_file() {
    // A critical number of a malformed PNG: alone it is an asset error;
    // after the frames are spent its image charge comes first.
    let bad = |d: &std::path::Path| fs::write(d.join("bad.png"), b"not a png").unwrap();
    let alone = r#""value": [{"id": "ex", "src": "bad", "w": 10, "h": 1, "divx": 10,
                               "digit": 4, "ref": 71}],
                   "destination": [{"id": "ex", "dst": [{"w": 1, "h": 1}]}]"#;
    assert!(matches!(load(alone, bad).err(), ImportError::Asset { .. }));
    let extra = r#", {"id": "ex", "dst": [{"w": 1, "h": 1}]}"#;
    limit(load(&frames_skin(extra), bad), ImportLimit::Frames);
}

#[test]
fn an_element_overflow_wins_over_a_missing_file() {
    let body = |n: usize| {
        let mut dsts: Vec<String> = (0..n)
            .map(|_| format!(r#"{{"id": "i", "dst": [{KEY}]}}"#))
            .collect();
        dsts.push(r#"{"id": "ex", "dst": [{"w": 1, "h": 1}]}"#.to_owned());
        format!(
            r#""image": [{{"id": "i", "src": "a", "w": 1, "h": 1}}],
               "value": [{{"id": "ex", "src": "gone", "w": 10, "h": 1, "divx": 10,
                          "digit": 4, "ref": 71}}],
               "destination": [{}]"#,
            dsts.join(",")
        )
    };
    assert!(matches!(
        load(&body(0), |_| {}).err(),
        ImportError::Asset { .. }
    ));
    limit(load(&body(MAX_ELEMENTS), |_| {}), ImportLimit::Elements);
}

/// `n` texts in font `f`, or in fonts 0..n when `fonts`.
fn texts_skin(n: usize, fonts: bool) -> String {
    let font_list = if fonts {
        join((0..n).map(|i| format!(r#"{{"id": "{i}", "path": "f{i}.ttf"}}"#)))
    } else {
        r#"{"id": "0", "path": "f0.ttf"}"#.to_owned()
    };
    let texts = join((0..n).map(|i| {
        let f = if fonts { i } else { 0 };
        format!(r#"{{"id": "t{i}", "font": "{f}", "constantText": "x"}}"#)
    }));
    let dsts = join((0..n).map(|i| format!(r#"{{"id": "t{i}", "dst": [{{"w": 10, "h": 10}}]}}"#)));
    format!(r#""font": [{font_list}], "text": [{texts}], "destination": [{dsts}]"#)
}

fn fonts(d: &std::path::Path) {
    for i in 0..=MAX_FONTS {
        font(d, &format!("f{i}.ttf"));
    }
}

#[test]
fn texts() {
    let l = load(&texts_skin(MAX_TEXT_ELEMENTS, false), fonts);
    let u = l.report().usage;
    assert_eq!((u.texts, u.widget_quads, u.fonts), (64, 64, 1));
    assert_eq!(l.ok().skin.text_slots(), MAX_TEXT_ELEMENTS);
    limit(
        load(&texts_skin(MAX_TEXT_ELEMENTS + 1, false), fonts),
        ImportLimit::Texts,
    );
}

#[test]
fn fonts_cap() {
    let l = load(&texts_skin(MAX_FONTS, true), fonts);
    assert_eq!(l.report().usage.fonts, as_u32(MAX_FONTS));
    assert_eq!(l.ok().skin.text_slots(), MAX_FONTS);
    limit(
        load(&texts_skin(MAX_FONTS + 1, true), fonts),
        ImportLimit::Fonts,
    );
}

/// `full` eleven-cell counts, a five-cell one, and `extra` one-cell ones.
fn quads_skin(full: usize, extra: usize) -> String {
    let dsts = join(
        std::iter::repeat_n("v11", full)
            .chain(["v5"])
            .chain(std::iter::repeat_n("v1", extra))
            .map(|id| format!(r#"{{"id": "{id}", "dst": [{KEY}]}}"#)),
    );
    let value = |id: &str, digit: u32| {
        format!(
            r#"{{"id": "{id}", "src": "a", "w": 60, "h": 1, "divx": 10, "digit": {digit},
                "ref": 110}}"#
        )
    };
    format!(
        r#""value": [{}, {}, {}], "destination": [{dsts}]"#,
        value("v11", 11),
        value("v5", 5),
        value("v1", 1)
    )
}

#[test]
fn widget_quads() {
    let full = (MAX_WIDGET_QUADS as usize - 5) / 11;
    assert_eq!(full * 11 + 5, MAX_WIDGET_QUADS as usize);
    let l = load(&quads_skin(full, 0), |_| {});
    assert_eq!(l.report().usage.widget_quads, MAX_WIDGET_QUADS);
    assert_eq!(l.ok().skin.quad_capacity(), MAX_WIDGET_QUADS);
    limit(load(&quads_skin(full, 1), |_| {}), ImportLimit::WidgetQuads);
}

/// `n` images, each of its own file `p{i}.png`.
fn files_skin(n: usize) -> String {
    let sources = join((0..n).map(|i| format!(r#"{{"id": "s{i}", "path": "p{i}.png"}}"#)));
    let images = join((0..n).map(|i| format!(r#"{{"id": "{i}", "src": "s{i}", "w": 1, "h": 1}}"#)));
    let dsts = join((0..n).map(|i| format!(r#"{{"id": "{i}", "dst": [{KEY}]}}"#)));
    format!(r#""source": [{sources}], "image": [{images}], "destination": [{dsts}]"#)
}

/// A folder of `n` one-texel PNGs.
fn pngs(n: usize) -> impl Fn(&std::path::Path) {
    move |d: &std::path::Path| {
        for i in 0..n {
            png(d, &format!("p{i}.png"), 1, 1);
        }
    }
}

/// Imports `body` without the shared source `a`.
fn load_bare(body: &str, setup: impl FnOnce(&std::path::Path)) -> Loaded {
    let dir = skin(&[("s.json", &hd(body))]);
    setup(dir.path());
    let result = import_with(dir.path(), |_| {});
    Loaded { dir, result }
}

#[test]
fn file_attempts() {
    let l = load_bare(&files_skin(MAX_FILES), pngs(MAX_FILES));
    let u = l.report().usage;
    assert_eq!(
        (u.file_attempts, u.files),
        (as_u32(MAX_FILES), as_u32(MAX_FILES))
    );
    assert_eq!(l.ok().skin.sheet().image_count(), MAX_FILES);
    limit(
        load_bare(&files_skin(MAX_FILES + 1), pngs(MAX_FILES + 1)),
        ImportLimit::FileAttempts,
    );
}

#[test]
fn total_file_bytes() {
    // Valid headers padded (sparsely) to the per-file maximum: 16 reach
    // the total exactly; one more byte is past it.
    let per = MAX_TOTAL_FILE_BYTES / 16;
    let padded = |n: usize, last: u64| {
        move |d: &std::path::Path| {
            for i in 0..n {
                png(d, &format!("p{i}.png"), 1, 1);
                let len = if i == 16 { last } else { per };
                let f = fs::OpenOptions::new()
                    .write(true)
                    .open(d.join(format!("p{i}.png")))
                    .unwrap();
                f.set_len(len).unwrap();
            }
        }
    };
    let l = load_bare(&files_skin(16), padded(16, 0));
    assert_eq!(l.report().usage.files, 16);
    let small = fs::metadata(l.dir.path().join("p0.png")).unwrap().len();
    assert_eq!(small, per);
    // The 17th is a one-texel PNG of its own length.
    let png_len = {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "x.png", 1, 1);
        fs::metadata(d.path().join("x.png")).unwrap().len()
    };
    limit(
        load_bare(&files_skin(17), padded(17, png_len)),
        ImportLimit::FileTotals,
    );
}

#[test]
fn grids_past_the_frames_are_the_limit() {
    // 65 536 × 65 536 frames do not fit a `u32`: the grid is refused as
    // the frame limit before its cycle or glyph set is read.
    let grid = r#""divx": 65536, "divy": 65536"#;
    let image = format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64, {grid}, "timer": 41, "cycle": 100}}],
           "destination": [{{"id": "i", "dst": [{KEY}]}}]"#
    );
    limit(load(&image, |_| {}), ImportLimit::Frames);
    let number = format!(
        r#""value": [{{"id": "v", "src": "a", "w": 64, "h": 64, {grid}, "digit": 4, "ref": 71}}],
           "destination": [{{"id": "v", "timer": 41, "dst": [{KEY}]}}]"#
    );
    limit(load(&number, |_| {}), ImportLimit::Frames);
    // One grid one past the skin's frames.
    let past = format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64, "divx": {}, "divy": 1}}],
           "destination": [{{"id": "i", "dst": [{KEY}]}}]"#,
        MAX_SKIN_FRAMES + 1
    );
    limit(load(&past, |_| {}), ImportLimit::Frames);
}
