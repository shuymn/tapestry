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
    let mut r = readouts_for(LaneView::NONE);
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
        10, 43, 90, 170, 302, 342, 386, 450, 459, 466, 469, 508, 519, 1_510,
    ] {
        let fields = format!(r#", "ref": {id}"#);
        for l in [group(2, &fields), set(r#"["i0", "i2"]"#, &fields)] {
            assert_eq!(common::kinds(l.report()), [prop(id)], "{id}");
            assert_eq!(l.report().usage.images, 0, "{id}");
            assert_eq!(l.ok().skin.choice_ref_count(), 0, "{id}");
        }
    }
    // The 1P lanes' visual states select among both entries, with no
    // note.
    for id in [500, 507] {
        let fields = format!(r#", "ref": {id}"#);
        for l in [group(2, &fields), set(r#"["i0", "i2"]"#, &fields)] {
            assert!(l.report().notes.is_empty(), "{id}: {:?}", l.report().notes);
            assert_eq!(l.ok().skin.choice_ref_count(), 2, "{id}");
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
           "source": [{{"id": "a", "path": "a.png"}}, {{"id": "m", "path": "clip.mp4"}},
                      {{"id": "w", "path": "w.webp"}}, {{"id": "far", "path": "../far.png"}},
                      {{"id": "lost", "path": "lost.png"}}, {{"id": "wide", "path": "wide.png"}}],
           "image": [{{"id": "i0", "src": "a", "w": 16, "h": 16}},
                     {{"id": "web", "src": "w", "w": 16, "h": 16}},
                     {{"id": "far", "src": "far", "w": 16, "h": 16}},
                     {{"id": "lost", "src": "lost", "w": 16, "h": 16}},
                     {{"id": "wide", "src": "wide", "w": 16, "h": 16}},
                     {{"id": "i1", "src": "a", "x": 16, "w": 16, "h": 16, "len": 4, "ref": 43}},
                     {{"id": "i2", "src": "a", "x": 32, "w": 16, "h": 16}},
                     {{"id": "mv", "src": "m"}},
                     {{"id": "gone", "src": "lost"}}],
           "imageset": [{{"id": "s", "images": {images} {set}}}],
           "destination": [{{"id": "s", {KEY}}}]}}"#
    );
    let dir = common::skin_with(&json, &[("a.png", 64, 64)]);
    common::write(
        dir.path(),
        "w.webp",
        b"RIFF\x1a\0\0\0WEBPVP8L\x0d\0\0\0/\0\0\0\x10\x07\x10\x11\x11\x88\x88\xfe\x07\0",
    );
    if images.contains("wide") {
        // Under the file size cap, but its row buffer is past the header
        // reader's allocation cap.
        common::png(dir.path(), "wide.png", 1_100_000, 1);
    }
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
        (r#", "ref": 43"#, prop(PropertyKind::ImageIndex, Some(43))),
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
fn an_entry_the_source_could_draw_drops_the_set() {
    // A format only the source decodes, a name leaving the folder and a
    // header past the decoder's allocation cap are this importer's
    // refusals, not missing files: the whole set drops, however often the
    // entry repeats, the cap named when a cap refused it.
    let decoder = NoteKind::OverCap {
        cap: "decoder allocation",
    };
    for (item, why) in [
        (r#"["i0", "web"]"#, NoteKind::MissingAsset),
        (r#"["i0", "far"]"#, NoteKind::MissingAsset),
        (r#"["i0", "wide"]"#, decoder),
        (r#"["i0", "wide", "wide"]"#, decoder),
        (r#"["i0", "web", "web"]"#, NoteKind::MissingAsset),
    ] {
        let l = set(item, r#", "value": 71"#);
        let dropped: Vec<_> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(dropped, [why], "{item}");
        assert_eq!(l.report().usage.file_attempts, 2, "{item}");
        assert_eq!(l.ok().skin.choice_ref_count(), 0, "{item}");
        assert!(frames(&mut l.drawn(), 0).is_empty(), "{item}");
    }
    // A file that is not there is a hole beside the valid entry, each
    // time it repeats; it is looked for once.
    let l = set(r#"["i0", "lost", "lost"]"#, r#", "value": 71"#);
    assert!(
        l.report()
            .notes
            .iter()
            .all(|n| n.severity != Severity::Dropped),
        "{:?}",
        l.report().notes
    );
    let u = l.report().usage;
    assert_eq!((u.file_attempts, u.files), (2, 1));
    assert_eq!(l.ok().skin.choice_ref_count(), 3);
    let mut d = l.drawn();
    assert_eq!(frames(&mut d, 0), [0]);
    for ex in [1, 2] {
        d.tally = tally(ex, 0);
        assert!(frames(&mut d, 0).is_empty(), "{ex}");
    }
}

/// The CRC of a PNG chunk's type and data (ISO 3309, bit by bit).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// An 8-bit RGBA PNG header declaring `w`×`h` and the start of its image
/// data, with no pixels behind it.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut body = b"IHDR".to_vec();
    body.extend_from_slice(&w.to_be_bytes());
    body.extend_from_slice(&h.to_be_bytes());
    body.extend_from_slice(&[8, 6, 0, 0, 0]);
    out.extend_from_slice(&13_u32.to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
    out.extend_from_slice(&1_000_u32.to_be_bytes());
    out.extend_from_slice(b"IDAT\x78\x9c\0\0");
    out
}

/// A decorative image cut from a file past a per-file texel cap drops,
/// its note naming that cap rather than a missing file: a 6400×7941
/// animation sheet as a BMP passes the full-decode cap (as a PNG it would
/// be decoded in bands), and a PNG past 64 Mi texels the banded one.
#[test]
fn a_file_past_the_texel_cap_drops_naming_the_cap() {
    for (name, bytes, cap) in [
        (
            "close.bmp",
            common::bmp_header(6_400, 7_941),
            "full-decode texel",
        ),
        ("close.png", png_header(8_192, 8_193), "banded texel"),
    ] {
        let json = format!(
            r#"{{"type": 0, "w": 1920, "h": 1080,
               "source": [{{"id": "close", "path": "{name}"}}],
               "image": [{{"id": "i", "src": "close", "w": 1280, "h": 720}}],
               "destination": [{{"id": "i", {KEY}}}]}}"#
        );
        let dir = common::skin_with(&json, &[]);
        common::write(dir.path(), name, &bytes);
        let l = common::Loaded {
            result: common::import_dir(dir.path()),
            dir,
        };
        let why = NoteKind::OverCap { cap };
        let dropped: Vec<_> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(dropped, [why], "{name}");
        assert_eq!(why.to_string(), format!("a file over the {cap} cap"));
        assert!(l.quads(0).is_empty(), "{name}");
    }
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

/// A cut 10 texels wider than its 800-wide file: drawn at its whole
/// frame, its last ten columns repeating the file's last column, as the
/// source's texture clamped to its edge samples them
/// (`JsonSkinObjectLoader.java:601-621`), with no note.
#[test]
fn a_cut_past_the_file_s_edge_repeats_its_edge() {
    // `j` is a real 800-wide cut of the same file.
    let loaded = common::import_sized(
        [1_920, 1_080],
        r#""source": [{"id": "b", "path": "b.png"}],
           "image": [{"id": "i", "src": "b", "w": 810, "h": 60},
                     {"id": "j", "src": "b", "w": 800, "h": 60}],
           "destination": [{"id": "i", "dst": [{"w": 810, "h": 60}]},
                           {"id": "i", "dst": [{"y": 100, "w": 810, "h": 60}]},
                           {"id": "j", "dst": [{"y": 200, "w": 800, "h": 60}]},
                           {"id": "j", "dst": [{"y": 300, "w": 800, "h": 60}]}]"#,
        &[("b.png", 800, 300)],
    );
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    // The file's last ten columns red: the frame samples blue at its
    // middle and at column 785, red from column 790 to its right end.
    common::png_with(loaded.dir.path(), "b.png", 800, 300, |x, _| {
        if x >= 790 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    let skin = loaded.ok().skin.clone();
    let atlas = skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let quads = loaded.quads(0);
    assert_eq!(quads.len(), 4);
    let q = quads[0];
    assert_eq!((q.rect[2], q.rect[3]), (810.0, 60.0));
    let at = |fu| common::play::texel_at(&atlas, &q, fu, 0.5)[0];
    assert_eq!(
        (at(0.5), at(785.5 / 810.0), at(791.0 / 810.0), at(0.995)),
        (0, 0, 255, 255)
    );
}

/// An image set's entry cut past its file's edge draws for each
/// destination drawing the set, its cached image reused, with no note.
#[test]
fn a_set_entry_past_the_file_s_edge_draws_each_time() {
    let loaded = common::import_sized(
        [1_920, 1_080],
        r#""source": [{"id": "b", "path": "b.png"}],
           "image": [{"id": "i", "src": "b", "w": 810, "h": 60}],
           "imageset": [{"id": "s", "images": ["i"]}],
           "destination": [{"id": "s", "dst": [{"w": 810, "h": 60}]},
                           {"id": "s", "dst": [{"y": 100, "w": 810, "h": 60}]}]"#,
        &[("b.png", 800, 300)],
    );
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    assert_eq!(loaded.quads(0).len(), 2);
}

/// A critical EX score whose glyphs are cut `src` from the 120×20 `n.png`.
fn critical(src: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", {src}, "divx": 10, "digit": 4, "ref": 71}}],
           "destination": [{{"id": "v", "timer": 41, "dst": [{{"w": 12, "h": 20}}]}}]"#
    ))
}

/// A mirrored source rectangle, or one starting left of or above its
/// file, drops a decorative element and refuses a critical one, with its
/// own reason; a cut starting inside its file and running past its right
/// or bottom edge is drawn with no note, a grid of frames included; `-1`
/// sizes still take the whole file (`JsonSkinObjectLoader.java:600-619`).
#[test]
fn mirrored_or_outside_source_rects() {
    for src in [r#""x": -1, "w": 32, "h": 32"#, r#""w": -2, "h": 32"#] {
        let loaded = cut(src, "");
        assert_eq!(
            common::kinds(loaded.report()),
            [NoteKind::SourceRect],
            "{src}"
        );
        assert!(loaded.quads(0).is_empty(), "{src}");
    }
    for src in [
        r#""x": 33, "w": 32, "h": 32"#,
        r#""y": 1, "w": -1, "h": -1"#,
        r#""w": 65, "h": 64"#,
    ] {
        let loaded = cut(src, "");
        assert!(loaded.report().notes.is_empty(), "{src}");
        assert_eq!(loaded.quads(0).len(), 1, "{src}");
    }
    for src in [r#""x": 32, "w": 32, "h": 64"#, r#""w": -1, "h": -1"#] {
        let loaded = cut(src, "");
        assert!(loaded.report().notes.is_empty(), "{src}");
        assert_eq!(loaded.quads(0).len(), 1, "{src}");
    }
    // A critical grid starting inside the file: ten 11-px cells of a
    // 116-px strip at x 4 stay in the 120-px file, ten 12-px ones run 4
    // px past it, both drawn; one starting above it is refused.
    for src in [
        r#""x": 4, "w": 116, "h": 20"#,
        r#""x": 4, "w": 120, "h": 20"#,
    ] {
        assert!(critical(src).report().notes.is_empty(), "{src}");
    }
    {
        let src = r#""y": -20, "w": 120, "h": 20"#;
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

/// The quads at `ms` of `l`, its `a.png` painted with [`common::coord`]
/// first (the import reads the file's header, the atlas its texels), each
/// with its atlas texel at `(fu, fv)`.
fn painted_at(l: common::Loaded, ms: i64, at: &[(f32, f32)]) -> Vec<Vec<[u8; 4]>> {
    common::png_with(l.dir.path(), "a.png", 64, 64, common::coord);
    let skin = l.ok().skin.clone();
    let atlas = skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    l.quads(ms)
        .iter()
        .map(|q| {
            at.iter()
                .map(|&(fu, fv)| common::play::texel_at(&atlas, q, fu, fv))
                .collect()
        })
        .collect()
}

/// Cuts starting inside the 64×64 `a.png` and running past its right or
/// bottom edge, per axis: each texel past the file repeats the file's
/// nearest edge texel, as the source's texture clamped to its edge samples
/// (`JsonSkinObjectLoader.java:601-621`), for a grid's frames on either
/// axis and a frame past both edges; a frame wholly right of the file and
/// partly below it keeps its edge column, padded down its rows. None is
/// noted.
#[test]
fn cuts_past_the_edge_repeat_it_per_axis() {
    let c = common::coord;
    let one = |src: &str, ms: i64, at: &[(f32, f32)]| {
        let loaded = cut(src, "");
        assert!(
            loaded.report().notes.is_empty(),
            "{src}: {:?}",
            loaded.report()
        );
        let got = painted_at(loaded, ms, at);
        assert_eq!(got.len(), 1, "{src}");
        got.into_iter().next().unwrap()
    };
    // Two 20-px columns from x 40, cycling over 1 s: frame 1 (500 ms)
    // covers columns 60 to 79, the file's 63 from 64 on.
    let cols = r#""x": 40, "w": 40, "h": 64, "divx": 2, "cycle": 1000"#;
    let across = [(2.5 / 20.0, 0.5), (10.5 / 20.0, 0.5), (19.5 / 20.0, 0.5)];
    assert_eq!(one(cols, 500, &across), [c(62, 32), c(63, 32), c(63, 32)]);
    assert_eq!(one(cols, 0, &across), [c(42, 32), c(50, 32), c(59, 32)]);
    // The same down two 20-px rows from y 40.
    let rows = r#""y": 40, "w": 64, "h": 40, "divy": 2, "cycle": 1000"#;
    let down = [(0.5, 2.5 / 20.0), (0.5, 10.5 / 20.0), (0.5, 19.5 / 20.0)];
    assert_eq!(one(rows, 500, &down), [c(32, 62), c(32, 63), c(32, 63)]);
    // One 30-px frame from (50, 50): past both edges.
    let corner = [
        (5.5 / 30.0, 5.5 / 30.0),
        (20.5 / 30.0, 20.5 / 30.0),
        (5.5 / 30.0, 20.5 / 30.0),
    ];
    assert_eq!(
        one(r#""x": 50, "y": 50, "w": 30, "h": 30"#, 0, &corner),
        [c(55, 55), c(63, 63), c(55, 63)]
    );
    // Wholly right of the file, partly below it: column 63, rows padded.
    let mixed = [(0.5, 5.5 / 30.0), (0.5, 20.5 / 30.0)];
    assert_eq!(
        one(r#""x": 70, "y": 50, "w": 10, "h": 30"#, 0, &mixed),
        [c(63, 55), c(63, 63)]
    );
}

/// A single frame wholly past its file keeps its one-texel edge cell, so
/// a long one still fits a page: an 8 192-texel line right of the file.
#[test]
fn a_long_frame_wholly_past_the_file_keeps_its_edge_cell() {
    let loaded = cut(r#""x": 64, "w": 8192, "h": 1"#, "");
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    assert_eq!(loaded.quads(0).len(), 1);
}

/// A single frame lying wholly at or past the file's right or bottom edge
/// is that edge's last column or row, which the source's texture,
/// clamped to its edge, samples (`JsonSkinObjectLoader.java:601-621`,
/// libGDX 1.9.9 `GLTexture` defaults): each row (column) of the line
/// kept, the same across the frame, with no note.
#[test]
fn a_frame_past_the_edge_draws_the_edge_line() {
    let c = common::coord;
    // Rows 0, 31 and 63 sampled at u 0.1 and 0.9.
    let rows: Vec<(f32, f32)> = [0.5, 31.5, 63.5]
        .iter()
        .flat_map(|r| [(0.1, r / 64.0), (0.9, r / 64.0)])
        .collect();
    for x in [64, 100] {
        let l = cut(&format!(r#""x": {x}, "w": 8, "h": 64"#), "");
        assert!(l.report().notes.is_empty(), "{x}: {:?}", l.report());
        let want = [0, 0, 31, 31, 63, 63].map(|y| c(63, y));
        assert_eq!(painted_at(l, 0, &rows), [want.to_vec()], "x {x}");
    }
    let columns: Vec<(f32, f32)> = rows.iter().map(|&(v, u)| (u, v)).collect();
    for y in [64, 100] {
        let l = cut(&format!(r#""y": {y}, "w": 64, "h": 8"#), "");
        assert!(l.report().notes.is_empty(), "{y}: {:?}", l.report());
        let want = [0, 0, 31, 31, 63, 63].map(|x| c(x, 63));
        assert_eq!(painted_at(l, 0, &columns), [want.to_vec()], "y {y}");
    }
    // Several frames on the clamped axis: one column of four row frames
    // each samples its own quarter of the edge column.
    let ends = [(0.1, 0.1), (0.9, 0.1), (0.1, 0.9), (0.9, 0.9)];
    for frame in 0..4 {
        let l = cut(
            r#""x": 64, "w": 8, "h": 64, "divy": 4, "timer": 41, "cycle": 400"#,
            "",
        );
        assert!(l.report().notes.is_empty(), "{:?}", l.report());
        let top = 16 * frame;
        let want = [top + 1, top + 1, top + 14, top + 14].map(|y| c(63, y));
        assert_eq!(
            painted_at(l, i64::from(frame) * 100 + 50, &ends),
            [want.to_vec()],
            "frame {frame}"
        );
    }
}

/// A frame past the edge whose coordinates leave the source's `int`
/// sums, where its uvs wrap and it draws no edge line, or one of several
/// frames on the axis it leaves, stays a dropped source rectangle.
#[test]
fn past_edge_cuts_the_source_cannot_clamp_drop() {
    for src in [
        r#""x": 64, "w": 2147483647, "h": 64"#,
        r#""x": 2147483640, "w": 16, "h": 64"#,
        r#""y": 64, "w": 64, "h": 2147483647"#,
        r#""y": 2147483640, "w": 64, "h": 16"#,
        r#""x": 64, "w": 16, "h": 64, "divx": 2"#,
        r#""y": 64, "w": 64, "h": 16, "divy": 2"#,
    ] {
        let loaded = cut(src, "");
        assert_eq!(
            common::kinds(loaded.report()),
            [NoteKind::SourceRect],
            "{src}"
        );
        assert!(loaded.quads(0).is_empty(), "{src}");
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
