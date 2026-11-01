//! Images and image sets: a selector group (`len` above 1) is
//! unsupported on an image object, while an image set without a frame
//! expression draws its first image's frames whatever that image's `len`.

mod common;

use common::{has, import_body};
use skin_beatoraja::{NoteKind, PropertyKind};

/// Image `g`, two 32×64 frames of `a.png` in a selector group of 2, and
/// image set `s` of it with `set` fields, drawn by `dsts`.
fn body(set: &str, dsts: &str) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "g", "src": "a", "w": 64, "h": 64, "divx": 2, "len": 2}}],
           "imageset": [{{"id": "s", "images": ["g"] {set}}}],
           "destination": [{dsts}]"#
    ))
}

const KEY: &str = r#""dst": [{"w": 10, "h": 10}]"#;

#[test]
fn a_selector_group_is_unsupported_on_the_image() {
    let l = body("", &format!(r#"{{"id": "g", {KEY}}}"#));
    assert!(has(l.report(), NoteKind::UnsupportedField { field: "len" }));
    assert_eq!(l.report().usage.images, 0);
}

#[test]
fn an_image_set_draws_its_image_whatever_its_len() {
    let l = body("", &format!(r#"{{"id": "s", {KEY}}}"#));
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.report().usage.images, 1);
    assert_eq!(l.ok().skin.sheet().frame_count(), 2);
    assert_eq!(l.quads(0).len(), 1);
    // A frame expression is S6c's.
    let l = body(r#", "ref": 1"#, &format!(r#"{{"id": "s", {KEY}}}"#));
    assert!(has(
        l.report(),
        NoteKind::UnsupportedProperty {
            kind: PropertyKind::Integer,
            id: None
        }
    ));
}

/// An image with `src` fields over the 64×64 `a.png`, drawn with `dst`
/// fields.
fn cut(src: &str, dst: &str) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", {src}}}],
           "destination": [{{"id": "i", {dst} {KEY}}}]"#
    ))
}

/// A critical EX score whose glyphs are cut `src` from the 120×20 `n.png`.
fn critical(src: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", {src}, "divx": 10, "digit": 4, "ref": 71}}],
           "destination": [{{"id": "v", "timer": 41, "dst": [{{"w": 12, "h": 20}}]}}]"#
    ))
}

/// A mirrored or out-of-file source rectangle drops a decorative element
/// and refuses a critical one, with its own reason; `-1` sizes still take
/// the whole file (`JsonSkinObjectLoader.java:600-619`).
#[test]
fn mirrored_or_outside_source_rects() {
    for src in [
        r#""x": -1, "w": 32, "h": 32"#,
        r#""w": -2, "h": 32"#,
        r#""x": 33, "w": 32, "h": 32"#,
        r#""y": 1, "w": -1, "h": -1"#,
        r#""w": 65, "h": 64"#,
    ] {
        let loaded = cut(src, "");
        assert_eq!(
            common::kinds(loaded.report()),
            [NoteKind::SourceRect],
            "{src}"
        );
        assert!(loaded.quads(0).is_empty(), "{src}");
    }
    for src in [r#""x": 32, "w": 32, "h": 64"#, r#""w": -1, "h": -1"#] {
        let loaded = cut(src, "");
        assert!(loaded.report().notes.is_empty(), "{src}");
        assert_eq!(loaded.quads(0).len(), 1, "{src}");
    }
    // Only the cells cut must lie inside: ten 11-px cells of a 116-px
    // strip at x 4 stay in the 120-px file, ten 12-px ones leave it.
    assert!(
        critical(r#""x": 4, "w": 116, "h": 20"#)
            .report()
            .notes
            .is_empty()
    );
    for src in [
        r#""x": 4, "w": 120, "h": 20"#,
        r#""y": -20, "w": 120, "h": 20"#,
    ] {
        let e = critical(src).err();
        assert!(
            matches!(
                e,
                skin_beatoraja::ImportError::Critical {
                    part: skin_beatoraja::CriticalPart::ExScore,
                    why: NoteKind::SourceRect,
                    ..
                }
            ),
            "{src}: {e:?}"
        );
    }
}

/// An element drawn only under the mouse is dropped, and refuses a
/// critical one (`SkinObject.java:603-607`): the play has no mouse.
#[test]
fn mouse_rects_drop_or_refuse() {
    let mouse = NoteKind::UnsupportedField { field: "mouseRect" };
    let rect = r#""mouseRect": {"x": 0, "y": 0, "w": 10, "h": 10},"#;
    let loaded = cut(r#""w": 64, "h": 64"#, rect);
    assert_eq!(common::kinds(loaded.report()), [mouse]);
    assert!(loaded.quads(0).is_empty());
    // A `mouseRect` that is no object is no rect.
    let loaded = cut(r#""w": 64, "h": 64"#, r#""mouseRect": 1,"#);
    assert!(loaded.report().notes.is_empty());
    let e = import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 71}}],
           "destination": [{{"id": "v", "timer": 41, {rect} "dst": [{{"w": 12, "h": 20}}]}}]"#
    ))
    .err();
    assert!(
        matches!(
            e,
            skin_beatoraja::ImportError::Critical {
                part: skin_beatoraja::CriticalPart::ExScore,
                why: NoteKind::UnsupportedField { field: "mouseRect" },
                ..
            }
        ),
        "{e:?}"
    );
}
