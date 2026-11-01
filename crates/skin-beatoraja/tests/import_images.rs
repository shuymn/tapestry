//! Images and image selection: an image group (`len` above 1) selects a
//! run of its frames by its `ref`, an image set one of its images by its
//! `value` or else its `ref`; source rectangles and mouse rects.

mod common;

use common::play::{Drawn, readouts_for, tally};
use common::{has, import_body};
use skin_beatoraja::{DeviationId, NoteKind, PropertyKind, Severity, TrackWhy};
use skin_model::{FrameReadouts, LaneView};

const KEY: &str = r#""dst": [{"w": 10, "h": 10}]"#;

/// Each quad's frame: its index in the uv table.
fn frames(d: &mut Drawn, ms: i64) -> Vec<usize> {
    let quads = d.frame(ms, &[]);
    quads
        .iter()
        .map(|q| {
            d.uvs
                .frames()
                .iter()
                .position(|f| f.uv == q.uv && f.layer == q.layer)
                .expect("a frame of the sheet")
        })
        .collect()
}

/// The readouts with the lane cover switched `on`.
fn cover_switch(on: bool) -> FrameReadouts {
    let mut r = readouts_for(LaneView {
        visible_time: bms_model::TimeUs(0),
        cover: 0,
        lift: 0,
        hidden: 0,
    });
    r.covers.cover_enabled = on;
    r
}

/// A group of `len` over ten 10×10 frames of `a.png`, five per row,
/// cycled over 300 ms, with `extra` fields, drawn on `ready`.
fn group(len: i32, extra: &str) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "g", "src": "a", "w": 50, "h": 20, "divx": 5, "divy": 2,
                      "cycle": 300, "len": {len} {extra}}}],
           "destination": [{{"id": "g", {KEY}}}]"#
    ))
}

#[test]
fn a_group_cuts_its_frames_into_runs() {
    // Ten frames in three runs of three: 0–2, 3–5 (across the row) and
    // 6–8; frame 9 is never shown. Ref 330 selects by the lane cover's
    // switch, ref 40 by the gauge's look (normal: 2).
    let run = |r: i32, on: bool| {
        let mut d = group(3, &format!(r#", "ref": {r}"#)).drawn();
        d.readouts = Some(cover_switch(on));
        [0, 99, 100, 200, 299, 300, 900]
            .map(|ms| frames(&mut d, ms))
            .concat()
    };
    assert_eq!(run(330, false), [0, 0, 1, 2, 2, 0, 0]);
    assert_eq!(run(330, true), [3, 3, 4, 5, 5, 3, 3]);
    assert_eq!(run(40, true), [6, 6, 7, 8, 8, 6, 6]);
    // Its image is admitted once, with every frame, and no sheet cycle.
    let l = group(3, r#", "ref": 40"#);
    let u = l.report().usage;
    assert_eq!((u.images, u.frames, u.choice_refs), (1, 10, 3));
    assert_eq!(l.ok().skin.choice_ref_count(), 3);
}

#[test]
fn switching_a_cover_off_mid_play_selects_its_off_image() {
    // The play's switch, not the configuration's (D4): switched on at
    // the start, off at 500 ms.
    let l = group(2, r#", "ref": 330"#);
    assert!(has(l.report(), NoteKind::Deviation(DeviationId::D4)));
    let mut d = l.drawn();
    d.readouts = Some(cover_switch(true));
    assert_eq!(frames(&mut d, 0), [5]);
    d.readouts = Some(cover_switch(false));
    assert_eq!(frames(&mut d, 600), [0]);
}

#[test]
fn one_frame_runs_ignore_their_cycle_timer() {
    // Timer 2 is unsupported: a run of one frame never cycles, so it
    // does not matter; a run of two does.
    let l = group(10, r#", "ref": 40, "timer": 2"#);
    assert!(
        l.report()
            .notes
            .iter()
            .all(|n| n.severity != Severity::Dropped)
    );
    assert_eq!(frames(&mut l.drawn(), 0), [2]);
    let l = group(5, r#", "ref": 40, "timer": 2"#);
    assert_eq!(
        common::kinds(l.report()),
        [NoteKind::Timer(skin_beatoraja::TimerWhy::Unsupported(2))]
    );
    // Runs of no frame: never drawn, nothing admitted.
    let l = group(11, r#", "ref": 40"#);
    assert_eq!(
        common::kinds(l.report()),
        [NoteKind::Track(TrackWhy::NeverDrawn)]
    );
    let u = l.report().usage;
    assert_eq!((u.images, u.choice_refs), (0, 0));
}

#[test]
fn group_refs_resolve_as_image_indices() {
    // A null image index (unknown, 0, absent, past the factory) selects
    // run 0, with no note; a known one the model cannot read drops it
    // with its id.
    for extra in ["", r#", "ref": 0"#, r#", "ref": 1"#, r#", "ref": 70000"#] {
        let l = group(2, extra);
        assert!(
            l.report().notes.is_empty(),
            "{extra}: {:?}",
            l.report().notes
        );
        assert_eq!(frames(&mut l.drawn(), 0), [0], "{extra}");
    }
    let prop = |id| NoteKind::UnsupportedProperty {
        kind: PropertyKind::ImageIndex,
        id: Some(id),
    };
    // A group and an image set alike.
    for id in [
        10, 42, 90, 170, 302, 342, 386, 450, 459, 466, 469, 500, 507, 1_510,
    ] {
        let fields = format!(r#", "ref": {id}"#);
        for l in [group(2, &fields), set(r#"["i0", "i2"]"#, &fields)] {
            assert_eq!(common::kinds(l.report()), [prop(id)], "{id}");
            assert_eq!(l.report().usage.images, 0, "{id}");
            assert_eq!(l.ok().skin.choice_ref_count(), 0, "{id}");
        }
    }
    // A `len` of 1 or less is an ordinary image whatever its `ref`.
    let l = group(1, r#", "ref": 42"#);
    assert!(l.report().notes.is_empty());
    assert_eq!(l.ok().skin.choice_ref_count(), 0);
}

/// Images `i0`–`i2`, the 16×16 cells of `a.png` at x 0, 16 and 32; image
/// set `s` of `images` with `set` fields, drawn on `ready`.
fn set(images: &str, set: &str) -> common::Loaded {
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
           "source": [{{"id": "a", "path": "a.png"}}, {{"id": "m", "path": "clip.mp4"}}],
           "image": [{{"id": "i0", "src": "a", "w": 16, "h": 16}},
                     {{"id": "i1", "src": "a", "x": 16, "w": 16, "h": 16, "len": 4, "ref": 42}},
                     {{"id": "i2", "src": "a", "x": 32, "w": 16, "h": 16}},
                     {{"id": "mv", "src": "m"}},
                     {{"id": "gone", "src": "nothing"}}],
           "imageset": [{{"id": "s", "images": {images} {set}}}],
           "destination": [{{"id": "s", {KEY}}}]}}"#
    );
    let dir = common::skin_with(&json, &[("a.png", 64, 64)]);
    let result = common::import_dir(dir.path());
    common::Loaded { dir, result }
}

#[test]
fn an_image_set_selects_by_value_then_by_ref() {
    let three = r#"["i0", "i1", "i2"]"#;
    // EX score selects (D4: the source hides it before its score
    // exists); 3 is past the table and selects entry 0.
    let shown = |fields: &str, ex: u32| {
        let l = set(three, fields);
        let mut d = l.drawn();
        d.tally = tally(ex, 0);
        frames(&mut d, 0)
    };
    for (ex, want) in [(0, [0]), (1, [1]), (2, [2]), (3, [0]), (99, [0])] {
        assert_eq!(shown(r#", "value": 71"#, ex), want, "{ex}");
    }
    let notes = |fields: &str| common::kinds(set(three, fields).report());
    assert_eq!(
        notes(r#", "value": 71"#),
        [NoteKind::Deviation(DeviationId::D4)]
    );
    assert_eq!(notes(r#", "value": 110"#), []);
    // A known value wins over ref 40; value 40 is no value property, so
    // ref 40 (normal: 2) selects; a null value and no ref select 0.
    assert_eq!(shown(r#", "value": 101, "ref": 40"#, 1), [1]);
    assert_eq!(shown(r#", "value": 40, "ref": 40"#, 1), [2]);
    assert_eq!(shown(r#", "ref": 40"#, 1), [2]);
    assert_eq!(shown(r#", "value": 40"#, 1), [0]);
    assert_eq!(shown(r#", "ref": 1"#, 1), [0]);
    // A value the model cannot show, or a script, does not fall back.
    let prop = |kind, id| NoteKind::UnsupportedProperty { kind, id };
    for (fields, why) in [
        (
            r#", "value": 150, "ref": 40"#,
            prop(PropertyKind::Integer, Some(150)),
        ),
        (
            r#", "value": "myscript", "ref": 40"#,
            prop(PropertyKind::Integer, None),
        ),
        (r#", "ref": 42"#, prop(PropertyKind::ImageIndex, Some(42))),
    ] {
        let l = set(three, fields);
        assert_eq!(common::kinds(l.report()), [why], "{fields}");
        assert_eq!(l.report().usage.images, 0, "{fields}");
    }
    // A value that may be unknown hides while it is.
    let l = set(three, r#", "value": 160"#);
    assert!(frames(&mut l.drawn(), 0).is_empty());
    // Every entry is admitted, the selector constant or not.
    let l = set(three, "");
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.report().usage.images, 3);
}

#[test]
fn an_image_set_keeps_holes_and_repeats() {
    // A null item, an undefined id, a movie and a missing file are holes,
    // located at their item; a repeat shares its image.
    let l = set(
        r#"["i2", null, "none", "mv", "gone", "i2"]"#,
        r#", "value": 71"#,
    );
    let r = l.report();
    let u = r.usage;
    // The movie and the missing file were charged before they failed.
    assert_eq!((u.images, u.choice_refs), (3, 6));
    assert_eq!(l.ok().skin.sheet().image_count(), 1);
    assert_eq!(l.ok().skin.choice_ref_count(), 6);
    let holes: Vec<_> = r
        .notes
        .iter()
        .filter(|n| n.severity == Severity::Ignored)
        .map(|n| n.kind)
        .collect();
    assert_eq!(
        holes,
        [
            NoteKind::UndefinedObject,
            NoteKind::Unsupported {
                object: skin_beatoraja::Object::Movie
            },
            NoteKind::MissingAsset,
        ]
    );
    let at: Vec<_> = r.notes.iter().filter_map(|n| n.at.as_ref()).collect();
    assert!(
        at.windows(2)
            .all(|w| (w[0].line, w[0].col) < (w[1].line, w[1].col))
    );
    let mut d = l.drawn();
    for (ex, want) in [(0, vec![0]), (1, vec![]), (2, vec![]), (5, vec![0])] {
        d.tally = tally(ex, 0);
        assert_eq!(frames(&mut d, 0), want, "{ex}");
    }
    // Only holes: never drawn.
    let l = set(r#"[null, "none"]"#, "");
    assert!(has(l.report(), NoteKind::Track(TrackWhy::NeverDrawn)));
    assert_eq!(l.ok().skin.choice_ref_count(), 0);
}

#[test]
fn an_image_drawn_alone_keeps_its_cycle_beside_a_set() {
    // Four 16×16 frames over 400 ms, admitted for the set first: the set
    // cycles them as its entry, the image as its own sheet cycle.
    let l = import_body(
        r#""image": [{"id": "s", "src": "a", "w": 64, "h": 16, "divx": 4, "cycle": 400}],
           "imageset": [{"id": "t", "images": ["s"]}],
           "destination": [{"id": "t", "dst": [{"w": 10, "h": 10}]},
                           {"id": "s", "dst": [{"w": 10, "h": 10}]}]"#,
    );
    assert_eq!(l.report().usage.images, 2);
    let mut d = l.drawn();
    assert_eq!(frames(&mut d, 0), [0, 0]);
    assert_eq!(frames(&mut d, 150), [1, 1]);
}

#[test]
fn an_image_set_s_images_must_be_an_array_of_names() {
    // Absent or empty: never drawn; null, an object or an object item: a
    // type error.
    let absent = import_body(&format!(
        r#""imageset": [{{"id": "s"}}], "destination": [{{"id": "s", {KEY}}}]"#
    ));
    for l in [set("[]", ""), absent] {
        assert_eq!(
            common::kinds(l.report()),
            [NoteKind::Track(TrackWhy::NeverDrawn)]
        );
    }
    for images in ["null", "{}", r#"[{}]"#, r#"["i0", []]"#] {
        let e = set(images, "").err();
        assert!(
            matches!(
                e,
                skin_beatoraja::ImportError::Type {
                    field: "images",
                    ..
                }
            ),
            "{images}: {e:?}"
        );
    }
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
