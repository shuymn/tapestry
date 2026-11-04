//! An image's `src` and the `source` record it names: a destination
//! whose image, slider, graph, LIFT or HIDDEN+ names no source record is
//! ignored whatever its role, as the source makes no object of it; a
//! defined source whose file is missing stays an asset, reproduced or
//! refused; every other consumer keeps its outcome with the undefined
//! source as its reason.

mod common;

use common::lanes::{NOTES, body, cover, note};
use common::{Loaded, import_body, import_dir, kinds, png, skin_with, write};
use skin_beatoraja::{CriticalPart, ImportError, NoteKind, Severity};

/// A failure-timed destination of `id`, off the lanes.
fn failed(id: &str) -> String {
    format!(r#"{{"id": "{id}", "timer": 3, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}"#)
}

/// A decorative destination of `id`, off the lanes.
fn plain(id: &str) -> String {
    format!(r#"{{"id": "{id}", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}"#)
}

/// The quads, file attempts, images and frames of an import.
fn cost(l: &Loaded) -> (u32, u32, u32, u32) {
    let u = l.report().usage;
    (
        l.ok().skin.quad_capacity(),
        u.file_attempts,
        u.images,
        u.frames,
    )
}

/// The lanes skin with `images` beside the parts, `objects` beside the
/// note, and `dst` after the notes.
fn lanes_with(images: &str, objects: &str, dst: &str) -> Loaded {
    let sep = if objects.is_empty() { "" } else { ", " };
    let objects = format!("{}{sep}{objects}", note(""));
    let dsts: Vec<&str> = [NOTES, dst].into_iter().filter(|d| !d.is_empty()).collect();
    import_body(&body(images, &objects, &dsts))
}

#[test]
fn a_destination_whose_source_is_undefined_is_ignored_whatever_its_role() {
    let base = lanes_with("", "", "");
    let mut wrong = Vec::new();
    for src in [r#""src": "0", "#, ""] {
        let image = format!(r#"{{"id": "u", {src}"w": 16, "h": 16}}"#);
        let group =
            format!(r#"{{"id": "u", {src}"w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}}"#);
        let slider = format!(
            r#""slider": [{{"id": "u", {src}"w": 16, "h": 16, "type": 4, "angle": 2, "range": 600}}]"#
        );
        let graph = format!(r#""graph": [{{"id": "u", {src}"w": 16, "h": 16, "type": 0}}]"#);
        let lift =
            format!(r#""liftCover": [{{"id": "u", {src}"w": 16, "h": 16, "disapearLine": 200}}]"#);
        let hidden = format!(
            r#""hiddenCover": [{{"id": "u", {src}"w": 16, "h": 16, "disapearLine": 200}}]"#
        );
        let over = |rect: &str| format!(r#"{{"id": "u", "dst": [{rect}]}}"#);
        let cases = [
            ("decorative", image.clone(), String::new(), plain("u")),
            ("failure", image.clone(), String::new(), failed("u")),
            ("group", group, String::new(), failed("u")),
            (
                "SUDDEN+ image",
                image,
                String::new(),
                cover("").replace(r#""id": "cover""#, r#""id": "u""#),
            ),
            (
                "SUDDEN+ slider",
                String::new(),
                slider,
                over(r#"{"x": 20, "y": 800, "w": 430, "h": 600}"#),
            ),
            ("graph", String::new(), graph, failed("u")),
            (
                "LIFT",
                String::new(),
                lift,
                over(r#"{"x": 20, "y": 0, "w": 430, "h": 200}"#),
            ),
            (
                "HIDDEN+",
                String::new(),
                hidden,
                over(r#"{"x": 20, "y": -400, "w": 430, "h": 600}"#),
            ),
        ];
        for (case, images, objects, dst) in cases {
            let l = lanes_with(&images, &objects, &dst);
            let r = match &l.result {
                Ok(i) => &i.report,
                Err(e) => {
                    wrong.push(format!("{case} {src}: {e:?}"));
                    continue;
                }
            };
            let undefined: Vec<_> = r
                .notes
                .iter()
                .filter(|n| n.kind == NoteKind::UndefinedSource)
                .map(|n| (n.severity, n.at.is_some()))
                .collect();
            let rest: Vec<_> = kinds(r)
                .into_iter()
                .filter(|&k| k != NoteKind::UndefinedSource)
                .collect();
            if undefined != [(Severity::Ignored, true)]
                || rest != kinds(base.report())
                || cost(&l) != cost(&base)
            {
                wrong.push(format!("{case} {src}: {:?}", r.notes));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn a_lua_failure_image_with_no_source_is_ignored() {
    // The shape of a close animation whose image names a source id the
    // skin never defines.
    let lua = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
               if not skin_config then return skin end\n\
               skin.source = { { id = \"closed\", path = \"a.png\" } }\n\
               skin.image = { { id = 7, src = 0, x = 0, y = 0, w = 8, h = 8 } }\n\
               skin.destination = { { id = 7, loop = -1, timer = 3, dst = {\n\
               { time = 0, x = 0, y = 0, w = 1920, h = 1080, a = 200 },\n\
               { time = 400, a = 0 } } } }\n\
               return skin\n";
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "s.luaskin", lua.as_bytes());
    png(d.path(), "a.png", 64, 64);
    let i = common::lua_import(d.path()).unwrap().imported;
    let r = &i.report;
    let undefined: Vec<_> = r
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::UndefinedSource)
        .collect();
    assert_eq!(undefined.len(), 1, "{:?}", r.notes);
    assert_eq!(undefined[0].severity, Severity::Ignored);
    // Nothing is read, charged or kept for it.
    let u = r.usage;
    assert_eq!((u.images, u.frames, u.file_attempts), (0, 0, 0));
    assert_eq!(i.skin.quad_capacity(), 0);
}

/// A skin of `sources` and `images` (with `a.png`, 64×64) drawing `dsts`.
fn sourced(sources: &str, images: &str, dsts: &str) -> Loaded {
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{sources}],
            "image": [{images}], "destination": [{dsts}]}}"#
    );
    let dir = skin_with(&json, &[("a.png", 64, 64)]);
    let result = import_dir(dir.path());
    Loaded { dir, result }
}

#[track_caller]
fn assert_asset(l: Loaded) {
    match l.err() {
        ImportError::Asset { .. } => {}
        e => panic!("{e:?}"),
    }
}

#[test]
fn the_last_source_record_of_an_id_decides() {
    let image = r#"{"id": "i", "src": "s", "w": 8, "h": 8}"#;
    let missing_last = r#"{"id": "s", "path": "a.png"}, {"id": "s", "path": "m.png"}"#;
    assert_asset(sourced(missing_last, image, &failed("i")));
    let found_last = r#"{"id": "s", "path": "m.png"}, {"id": "s", "path": "a.png"}"#;
    let l = sourced(found_last, image, &failed("i"));
    assert_eq!(l.report().usage.images, 1);
    assert!(l.ok().skin.quad_capacity() > 0);
    assert!(!kinds(l.report()).contains(&NoteKind::UndefinedSource));
}

#[test]
fn an_undefined_source_differs_from_a_defined_one_with_no_file() {
    let a = r#"{"id": "a", "path": "a.png"}"#;
    // No record of the id: ignored before any role applies.
    let l = sourced(
        a,
        r#"{"id": "i", "src": "u", "w": 8, "h": 8}"#,
        &failed("i"),
    );
    assert_eq!(
        kinds(l.report()),
        [NoteKind::UndefinedSource],
        "{:?}",
        l.report().notes
    );
    // A record with no `path`: a missing asset, refused as failure
    // presentation without reading a file, dropped when decorative.
    let no_path = r#"{"id": "a", "path": "a.png"}, {"id": "p"}"#;
    let image = r#"{"id": "i", "src": "p", "w": 8, "h": 8}"#;
    match sourced(no_path, image, &failed("i")).err() {
        ImportError::Critical { part, why, .. } => {
            assert_eq!((part, why), (CriticalPart::Failure, NoteKind::MissingAsset));
        }
        e => panic!("{e:?}"),
    }
    let l = sourced(no_path, image, &plain("i"));
    let r = l.report();
    assert_eq!(r.usage.file_attempts, 0);
    assert!(
        r.notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::MissingAsset),
        "{:?}",
        r.notes
    );
    assert!(!kinds(r).contains(&NoteKind::UndefinedSource));
}

#[test]
fn a_defined_source_whose_file_is_missing_stays_an_asset() {
    let sources = r#"{"id": "a", "path": "a.png"}, {"id": "m", "path": "m.png"}"#;
    let image = r#"{"id": "i", "src": "m", "w": 8, "h": 8}"#;
    assert_asset(sourced(sources, image, &failed("i")));
    let l = sourced(sources, image, &plain("i"));
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::MissingAsset),
        "{:?}",
        l.report().notes
    );
}

/// A skin of `members` drawing `dsts`.
fn members(members: &str, dsts: &str) -> Loaded {
    import_body(&format!(r#"{members}, "destination": [{dsts}]"#))
}

/// A number of property `r` whose source is undefined.
fn number(r: i32) -> String {
    format!(
        r#""value": [{{"id": "v", "src": "u", "w": 10, "h": 1, "divx": 10,
                      "digit": 4, "ref": {r}}}]"#
    )
}

#[test]
fn a_critical_number_whose_source_is_undefined_refuses() {
    match members(&number(71), &plain("v")).err() {
        ImportError::Critical { part, why, .. } => {
            assert_eq!(
                (part, why),
                (CriticalPart::ExScore, NoteKind::UndefinedSource)
            );
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn a_decorative_number_whose_source_is_undefined_is_dropped() {
    let l = members(&number(110), &plain("v"));
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::UndefinedSource),
        "{:?}",
        l.report().notes
    );
}

#[test]
fn an_image_set_entry_whose_source_is_undefined_is_a_hole() {
    let set = r#""image": [{"id": "i", "src": "a", "w": 8, "h": 8}, {"id": "u", "w": 8, "h": 8}],
                 "imageset": [{"id": "s", "images": ["i", "u"], "ref": 40}]"#;
    let l = members(set, &plain("s"));
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Ignored && n.kind == NoteKind::UndefinedSource),
        "{:?}",
        l.report().notes
    );
    assert_eq!(l.ok().skin.choice_ref_count(), 2);
}

#[test]
fn a_shared_id_keeps_its_handling() {
    // HIDDEN+ sharing the note's id: the source would fall through to the
    // note; the existing handling drops it.
    let hidden = r#""hiddenCover": [{"id": "notes", "w": 16, "h": 16, "disapearLine": 200}]"#;
    let l = lanes_with("", hidden, "");
    let r = l.report();
    assert!(
        r.notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::UndefinedSource),
        "{:?}",
        r.notes
    );
    // LIFT sharing a BGA's id: the source keeps looking; LIFT refuses.
    let lift = r#""liftCover": [{"id": "b", "w": 16, "h": 16, "disapearLine": 200}],
                  "bga": {"id": "b"}"#;
    match lanes_with(
        "",
        lift,
        r#"{"id": "b", "dst": [{"x": 20, "y": 0, "w": 430, "h": 200}]}"#,
    )
    .err()
    {
        ImportError::Critical { part, why, .. } => {
            assert_eq!((part, why), (CriticalPart::Lift, NoteKind::UndefinedSource));
        }
        e => panic!("{e:?}"),
    }
}
