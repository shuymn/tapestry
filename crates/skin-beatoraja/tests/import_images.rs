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
