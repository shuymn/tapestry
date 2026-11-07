//! The failure presentation (owner decision 16): on play, a destination
//! that can still draw once the configured options are folded and whose
//! timer resolves to `failed` is reproduced, or the skin is refused at
//! it. A part with its own role keeps it.

mod common;

use bms_model::TimeUs;
use common::lanes::{NOTES, body, cover, hidden, hidden_object, lift, lift_object, note};
use common::{Loaded, d3_notes, png, skin_with, write};
use skin_beatoraja::{
    Config, CriticalPart, ImportError, NoteKind, Object, PropertyKind, Severity, TimerWhy, TrackWhy,
};

/// The image records: `i` a whole 64×64 sheet; `cy` two frames cycling
/// without a timer, `ct` on timer 41 and `c3` on timer 3; `one` a single
/// frame and `still` two without a cycle; `gone` from a file missing for
/// the source too; `bad` from a file only this importer refuses; `grp`
/// a group selected by an unsupported image index.
const IMAGES: &str = r#""image": [
    {"id": "i", "src": "a", "w": 64, "h": 64},
    {"id": "cy", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100},
    {"id": "ct", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100, "timer": 41},
    {"id": "c3", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100, "timer": 3},
    {"id": "one", "src": "a", "w": 64, "h": 32, "cycle": 100},
    {"id": "still", "src": "a", "w": 64, "h": 32, "divx": 2},
    {"id": "gone", "src": "m", "w": 8, "h": 8},
    {"id": "bad", "src": "x", "w": 8, "h": 8},
    {"id": "grp", "src": "a", "w": 64, "h": 16, "divx": 4, "len": 2, "ref": 43}]"#;

/// Options 900 (selected by default) and 901.
const HEADER: &str = r#""property": [
    {"name": "p", "item": [{"name": "x", "op": 900}, {"name": "y", "op": 901}], "def": "x"}]"#;

/// The default destination rect: x 10, y 20, 100×50 (y up 1 010).
const KEY: &str = r#"{"x": 10, "y": 20, "w": 100, "h": 50}"#;

/// A mouse rectangle, which the play never draws.
const MOUSE: &str = r#""mouseRect": {"x": 0, "y": 0, "w": 1, "h": 1}"#;

/// A failure destination of `id` with `fields` and one key [`KEY`]; its
/// own start, `{"id": "<id>", "timer": 3`, is where a refusal points.
fn fail(id: &str, fields: &str) -> String {
    let sep = if fields.is_empty() { "" } else { ", " };
    format!(r#"{{"id": "{id}", "timer": 3{sep}{fields}, "dst": [{KEY}]}}"#)
}

/// The marker a refusal of [`fail`]`(id, ..)` points at.
fn at(id: &str) -> String {
    format!(r#"{{"id": "{id}", "timer": 3"#)
}

/// An import of a 1920×1080 play skin of `body`, over sources `a` (a
/// 64×64 `a.png`), `n` (a 120×20 `n.png`), `m` (missing), `x` (outside
/// the folder) and `c` (`c.bmp`, written by the test needing it), and
/// its text.
struct Run {
    json: String,
    loaded: Loaded,
}

fn json_of(body: &str) -> String {
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
"source": [{{"id": "a", "path": "a.png"}}, {{"id": "n", "path": "n.png"}},
{{"id": "m", "path": "m.png"}}, {{"id": "x", "path": "../x.png"}},
{{"id": "c", "path": "c.bmp"}}],
{body}}}"#
    )
}

fn run_with(body: &str, edit: impl FnOnce(&mut Config)) -> Run {
    let json = json_of(body);
    let dir = skin_with(&json, &[("a.png", 64, 64), ("n.png", 120, 20)]);
    let result = common::import_with(dir.path(), edit);
    Run {
        json,
        loaded: Loaded { dir, result },
    }
}

fn run(body: &str) -> Run {
    run_with(body, |_| {})
}

/// [`run`] of [`IMAGES`] and `objects` drawn by `dsts`.
fn images(objects: &str, dsts: &[&str]) -> Run {
    let sep = if objects.is_empty() { "" } else { ", " };
    run(&format!(
        r#"{HEADER}, {IMAGES}{sep}{objects}, "destination": [{}]"#,
        dsts.join(", ")
    ))
}

/// The line and column of `marker`'s first occurrence in `json`.
fn loc_of(json: &str, marker: &str) -> (u32, u32) {
    let i = json.find(marker).unwrap_or_else(|| panic!("{marker}"));
    assert_eq!(json.matches(marker).count(), 1, "{marker} is not unique");
    let before = &json[..i];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap().chars().count() + 1;
    (u32::try_from(line).unwrap(), u32::try_from(col).unwrap())
}

/// The part and reason of `r`'s critical refusal, checked to point at
/// `marker`.
#[track_caller]
fn refused(r: Run, marker: &str) -> (CriticalPart, NoteKind) {
    let want = loc_of(&r.json, marker);
    match r.loaded.err() {
        ImportError::Critical { at, part, why } => {
            assert_eq!((at.file.as_str(), (at.line, at.col)), ("s.json", want));
            (part, why)
        }
        e => panic!("{e:?}"),
    }
}

/// `r`'s refusal as the failure presentation, at `marker`, for `why`.
#[track_caller]
fn refused_as_failure(r: Run, marker: &str, why: NoteKind) {
    assert_eq!(refused(r, marker), (CriticalPart::Failure, why));
}

/// `r`'s asset refusal, checked to point at `marker`.
#[track_caller]
fn asset_refused(r: Run, marker: &str) {
    let want = loc_of(&r.json, marker);
    match r.loaded.err() {
        ImportError::Asset { at, .. } => {
            assert_eq!((at.file.as_str(), (at.line, at.col)), ("s.json", want));
        }
        e => panic!("{e:?}"),
    }
}

/// A fresh play of `l` over an empty chart, the shown gauge failed at
/// `failed` ms when one, the play entering Closing there.
fn drawn(l: &Loaded, failed: Option<i64>) -> common::play::Drawn {
    let mut d = common::play::Drawn::new(l.ok().skin.clone(), 0);
    let at = failed.map(|f| TimeUs(f * 1_000));
    for g in &mut d.gauges {
        g.failed_at = at;
    }
    d.closing = at;
    d
}

/// The quads [`drawn`] gives at `ms`.
fn quads(l: &Loaded, failed: Option<i64>, ms: i64) -> Vec<skin_model::Quad> {
    drawn(l, failed).frame(ms, &[])
}

/// The rounded rects [`drawn`] gives at `ms`.
fn rects(l: &Loaded, failed: Option<i64>, ms: i64) -> Vec<[i32; 4]> {
    drawn(l, failed).rects(ms)
}

fn unsupported_field(field: &'static str) -> NoteKind {
    NoteKind::UnsupportedField { field }
}

fn prop(kind: PropertyKind, id: Option<i32>) -> NoteKind {
    NoteKind::UnsupportedProperty { kind, id }
}

/// Whether `l`'s report drops `kind`.
fn dropped(l: &Loaded, kind: NoteKind) -> bool {
    l.report()
        .notes
        .iter()
        .any(|n| n.severity == Severity::Dropped && n.kind == kind)
}

#[test]
fn a_failure_image_is_kept_and_drawn_once_the_gauge_fails() {
    let r = images("", &[&fail("i", "")]);
    let l = r.loaded;
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    // Nothing before the failure; the image after it, at its place.
    assert!(rects(&l, None, 2_000).is_empty());
    assert_eq!(rects(&l, Some(1_000), 1_200), [[10, 1_010, 100, 50]]);
}

#[test]
fn a_moving_recoloured_failure_track_is_kept() {
    let d = r#"{"id": "i", "timer": 3, "dst": [
        {"time": 0, "x": 0, "y": 0, "w": 100, "h": 100, "a": 0, "r": 255},
        {"time": 100, "x": 100, "y": 0, "w": 100, "h": 100, "a": 255, "r": 0}]}"#;
    let l = images("", &[d]).loaded;
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    let at = |ms| {
        let q = quads(&l, Some(1_000), ms);
        assert_eq!(q.len(), 1, "{ms}");
        (common::play::round(q[0].rect), q[0].color)
    };
    assert_eq!(at(1_000), ([0, 980, 100, 100], [1.0, 1.0, 1.0, 0.0]));
    assert_eq!(at(1_050), ([50, 980, 100, 100], [0.5, 1.0, 1.0, 0.5]));
}

#[test]
fn an_untimed_cycle_on_failure_presentation_is_noted_once() {
    // Its timed destination does not time the cycle: it runs on
    // `screen`, so its phase depends on when the state started (D3).
    let l = images("", &[&fail("cy", "")]).loaded;
    assert_eq!(d3_notes(&l), 1);
    // One note per retained destination.
    let l = images("", &[&fail("cy", ""), &fail("cy", r#""blend": 2"#)]).loaded;
    assert_eq!(d3_notes(&l), 2);
    // An image set keeping two cycling entries is one destination.
    let set = r#""imageset": [{"id": "s", "images": ["cy", "cy"], "value": 110}]"#;
    let l = images(set, &[&fail("s", "")]).loaded;
    assert_eq!(d3_notes(&l), 1);
    // None for a timed cycle, a single frame, no cycle, or a dropped one.
    for id in ["ct", "c3", "one", "still"] {
        let l = images("", &[&fail(id, "")]).loaded;
        assert!(l.report().notes.is_empty(), "{id}: {:?}", l.report().notes);
        // Retained, and drawn once the gauge fails.
        assert_eq!(l.report().usage.elements, 1, "{id}");
        assert_eq!(
            rects(&l, Some(1_000), 1_250),
            [[10, 1_010, 100, 50]],
            "{id}"
        );
    }
    let off = format!(r#""op": [901], {MOUSE}"#);
    let l = images("", &[&fail("cy", &off)]).loaded;
    assert_eq!(d3_notes(&l), 0);
    assert!(dropped(&l, unsupported_field("mouseRect")));
}

#[test]
fn a_negative_bitmap_font_size_refuses_failure_presentation() {
    // The source draws a negative size mirrored; 0 it cannot draw.
    let fnt = concat!(
        "info face=\"t\" size=24 padding=0,0,0,0\n",
        "common lineHeight=24 base=20 scaleW=64 scaleH=32 pages=1\n",
        "page id=0 file=\"p.fnt.png\"\n",
        "char id=65 x=0 y=0 width=10 height=20 xoffset=0 yoffset=0 xadvance=12 page=0 chnl=15\n",
    );
    let skin = |size: i32, fields: &str| {
        let body = format!(
            r#"{HEADER}, "font": [{{"id": "f", "path": "p.fnt"}}],
               "text": [{{"id": "t", "font": "f", "size": {size}, "constantText": "A"}}],
               "destination": [{}]"#,
            fail("t", fields)
        );
        let json = json_of(&body);
        let dir = skin_with(&json, &[]);
        write(dir.path(), "p.fnt", fnt.as_bytes());
        png(dir.path(), "p.fnt.png", 64, 32);
        Run {
            loaded: Loaded {
                result: common::import_dir(dir.path()),
                dir,
            },
            json,
        }
    };
    // The same font draws at a positive size.
    let l = skin(24, "").loaded;
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(l.report().usage.texts, 1);
    refused_as_failure(skin(-24, ""), &at("t"), unsupported_field("size"));
    // Folded off, it is never drawn and imports; on another timer it is
    // dropped; at 0 it is ignored, as the source fails.
    let l = skin(-24, r#""op": [901]"#).loaded;
    assert_eq!(
        common::kinds(l.report()),
        [NoteKind::Ignored { field: "op" }]
    );
    let r = skin(-24, "");
    let decorative = r.json.replace(r#""timer": 3"#, r#""timer": 41"#);
    let dir = skin_with(&decorative, &[]);
    write(dir.path(), "p.fnt", fnt.as_bytes());
    png(dir.path(), "p.fnt.png", 64, 32);
    let l = Loaded {
        result: common::import_dir(dir.path()),
        dir,
    };
    assert!(dropped(&l, unsupported_field("size")));
    let l = skin(0, "").loaded;
    assert_eq!(
        common::kinds(l.report()),
        [NoteKind::Ignored { field: "size" }]
    );
}

#[test]
fn a_cycle_on_timer_3_does_not_make_its_destination_failure_presentation() {
    let d = format!(r#"{{"id": "c3", "timer": 41, {MOUSE}, "dst": [{KEY}]}}"#);
    let l = images("", &[&d]).loaded;
    assert!(dropped(&l, unsupported_field("mouseRect")));
}

#[test]
fn an_unsupported_part_refuses_at_its_destination() {
    let clip = r#"{"id": "i", "timer": 3, "dst": [{"x": 0, "y": 0, "w": 10, "h": 10,
        "clip_x": 0, "clip_y": 0, "clip_w": 5, "clip_h": 5}]}"#;
    let r = images("", &[clip]);
    refused_as_failure(r, &at("i"), unsupported_field("clip"));
    for (fields, why) in [
        (r#""stretch": 3"#, NoteKind::Stretch(3)),
        (MOUSE, unsupported_field("mouseRect")),
        (r#""blend": 9"#, NoteKind::Blend(9)),
        (r#""offset": 1"#, NoteKind::RuntimeOffset(1)),
        (r#""op": ["a_script()"]"#, prop(PropertyKind::Boolean, None)),
        (r#""draw": "nothing""#, prop(PropertyKind::Boolean, None)),
        (r#""op": [171]"#, prop(PropertyKind::Boolean, Some(171))),
        (r#""op": [-171]"#, prop(PropertyKind::Boolean, Some(-171))),
        (r#""draw": 171"#, prop(PropertyKind::Boolean, Some(171))),
    ] {
        let r = images("", &[&fail("i", fields)]);
        refused_as_failure(r, &at("i"), why);
    }
}

/// The judgment classes (180 to 184) are the source's datum, the class
/// it computes from the chart's own rank: a critical destination reading
/// one, by number, by name or negated, is kept with no note, as one
/// reading the difficulty is.
#[test]
fn judgment_class_conditions_keep_failure_presentation() {
    for fields in [
        r#""op": [180]"#,
        r#""op": [-184]"#,
        r#""draw": 182"#,
        r#""op": ["chart_judge_hard"]"#,
        r#""draw": "!chart_judge_easy""#,
        r#""op": [151]"#,
    ] {
        let l = images("", &[&fail("i", fields)]).loaded;
        assert!(l.result.is_ok(), "{fields}");
        assert!(
            l.report().notes.is_empty(),
            "{fields}: {:?}",
            l.report().notes
        );
    }
}

#[test]
fn a_failure_image_refuses_a_file_it_cannot_load() {
    // Missing for the source too, and refused by this importer alone:
    // both after the common part, both refusals with the file's error.
    for id in ["gone", "bad"] {
        asset_refused(images("", &[&fail(id, "")]), &at(id));
    }
    // A sheet past the full-decode texel cap.
    let json = json_of(&format!(
        r#""image": [{{"id": "i", "src": "c", "w": 1280, "h": 720}}],
           "destination": [{}]"#,
        fail("i", "")
    ));
    let dir = skin_with(&json, &[]);
    write(dir.path(), "c.bmp", &common::bmp_header(6_400, 7_941));
    let r = Run {
        loaded: Loaded {
            result: common::import_dir(dir.path()),
            dir,
        },
        json,
    };
    asset_refused(r, &at("i"));
}

#[test]
fn image_choices_refuse_but_keep_genuine_holes() {
    // A group by an unsupported index, a set by an unsupported value.
    let r = images("", &[&fail("grp", "")]);
    refused_as_failure(r, &at("grp"), prop(PropertyKind::ImageIndex, Some(43)));
    let set = r#""imageset": [{"id": "s", "images": ["i"], "value": 150}]"#;
    let r = images(set, &[&fail("s", "")]);
    refused_as_failure(r, &at("s"), prop(PropertyKind::Integer, Some(150)));
    // A set entry missing for the source too is a hole, as it is
    // anywhere; one only this importer refuses is not.
    let set = r#""imageset": [{"id": "s", "images": ["i", "gone"], "value": 110}]"#;
    let l = images(set, &[&fail("s", "")]).loaded;
    assert_eq!(common::kinds(l.report()), [NoteKind::MissingAsset]);
    assert_eq!(rects(&l, Some(1_000), 1_100).len(), 1);
    let set = r#""imageset": [{"id": "s", "images": ["i", "bad"], "value": 110}]"#;
    asset_refused(images(set, &[&fail("s", "")]), &at("s"));
}

#[test]
fn numbers_and_text_refuse_as_failure_presentation() {
    let values = r#""value": [
        {"id": "neg", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": -1, "ref": 110},
        {"id": "unk", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, "ref": 380},
        {"id": "cnt", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, "ref": 110}]"#;
    for (id, why) in [
        ("neg", unsupported_field("digit")),
        ("unk", prop(PropertyKind::Integer, Some(380))),
        ("cnt", unsupported_field("mouseRect")),
    ] {
        let fields = if id == "cnt" { MOUSE } else { "" };
        let r = images(values, &[&fail(id, fields)]);
        refused_as_failure(r, &at(id), why);
    }
    let texts = r#""font": [{"id": "f", "path": "f.ttf"}], "text": [
        {"id": "rival", "font": "f", "ref": 1},
        {"id": "wide", "font": "f", "ref": 10, "overflow": 3},
        {"id": "nofile", "font": "f", "ref": 10}]"#;
    for (id, why) in [
        ("rival", prop(PropertyKind::String, Some(1))),
        ("wide", unsupported_field("overflow")),
        ("nofile", NoteKind::MissingAsset),
    ] {
        let r = images(texts, &[&fail(id, "")]);
        refused_as_failure(r, &at(id), why);
    }
}

#[test]
fn unsupported_objects_refuse_as_failure_presentation() {
    let objects = r#""slider": [{"id": "sl", "src": "a", "w": 8, "h": 8, "type": 0}],
        "graph": [{"id": "gr", "src": "a", "w": 8, "h": 8}],
        "bga": {"id": "bga"}"#;
    for (id, object) in [
        ("sl", Object::Slider),
        ("gr", Object::Graph),
        ("bga", Object::Bga),
        ("-100", Object::System),
    ] {
        let r = images(objects, &[&fail(id, "")]);
        refused_as_failure(r, &at(id), NoteKind::Unsupported { object });
    }
}

#[test]
fn content_parts_keep_their_own_roles() {
    let values = r#""value": [
        {"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, "ref": 71},
        {"id": "mc", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4, "ref": 75}]"#;
    for (id, part) in [
        ("ex", CriticalPart::ExScore),
        ("mc", CriticalPart::MaxCombo),
    ] {
        let r = images(values, &[&fail(id, MOUSE)]);
        assert_eq!(
            refused(r, &at(id)),
            (part, unsupported_field("mouseRect")),
            "{id}"
        );
    }
    // SUDDEN+'s paint, LIFT, the lanes.
    let lanes = |objects: &str, dsts: &[&str]| run(&body("", objects, dsts));
    let paint = cover(&format!(r#""timer": 3, {MOUSE}"#));
    let r = lanes(&note(""), &[NOTES, &paint]);
    assert_eq!(
        refused(r, r#"{"id": "cover", "offset": 4"#),
        (CriticalPart::Cover, unsupported_field("mouseRect"))
    );
    let objects = format!("{}, {}", note(""), lift_object(""));
    let l = lift(&format!(r#""timer": 3, {MOUSE}"#));
    let r = lanes(&objects, &[NOTES, &l]);
    assert_eq!(
        refused(r, r#"{"id": "liftc", "timer""#),
        (CriticalPart::Lift, unsupported_field("mouseRect"))
    );
    let notes = format!(r#"{{"id": "notes", "timer": 3, {MOUSE}, "dst": [{KEY}]}}"#);
    let r = lanes(&note(""), &[&notes]);
    assert_eq!(
        refused(r, r#"{"id": "notes", "timer": 3"#),
        (CriticalPart::Lanes, unsupported_field("mouseRect"))
    );
    // The gauge and the judge.
    let gauge = r#""image": [{"id": "n0", "src": "a", "w": 8, "h": 8}],
        "gauge": {"id": "g", "nodes": ["n0", "n0", "n0", "n0"], "parts": 10}"#;
    let r = run(&format!(
        r#"{gauge}, "destination": [{}]"#,
        fail("g", MOUSE)
    ));
    assert_eq!(
        refused(r, &at("g")),
        (CriticalPart::Gauge, unsupported_field("mouseRect"))
    );
    let judge = r#""image": [{"id": "g0", "src": "a", "w": 8, "h": 8}],
        "judge": [{"id": "j", "index": 0, "images": [
            {"id": "g0", "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}], "numbers": []}]"#;
    let r = run(&format!(
        r#"{judge}, "destination": [{}]"#,
        fail("j", MOUSE)
    ));
    assert_eq!(
        refused(r, &at("j")),
        (CriticalPart::Judge, unsupported_field("mouseRect"))
    );
}

#[test]
fn hidden_plus_refuses_as_failure_presentation() {
    let objects = format!("{}, {}", note(""), hidden_object(""));
    let dst = hidden(r#""timer": 3, "stretch": 3"#);
    let r = run(&body("", &objects, &[NOTES, &dst]));
    refused_as_failure(r, r#"{"id": "hid", "timer""#, NoteKind::Stretch(3));
}

/// A bar line group record of image `id` with `fields` and `keys`.
fn group(id: &str, fields: &str, keys: &str) -> String {
    format!(r#"{{"id": "{id}", "timer": 3{fields}, "dst": [{keys}]}}"#)
}

#[test]
fn bar_groups_refuse_as_failure_presentation() {
    let key = r#"{"x": 0, "y": 0, "w": 8, "h": 2}"#;
    let many = vec![key; skin_model::MAX_TRACK_KEYS + 1].join(", ");
    for (rec, why) in [
        (group("bar", r#", "stretch": 3"#, key), NoteKind::Stretch(3)),
        (group("bar", "", &many), NoteKind::Track(TrackWhy::Limits)),
        (group("nothing", "", key), NoteKind::UndefinedObject),
        (
            group("bar", &format!(", {MOUSE}"), key),
            unsupported_field("mouseRect"),
        ),
    ] {
        let objects = note(&format!(r#""group": [{rec}]"#));
        let r = run(&body("", &objects, &[NOTES]));
        let marker = rec.split(", \"dst\"").next().unwrap().to_owned();
        refused_as_failure(r, &marker, why);
    }
    // Folded off, it is dropped as before.
    let rec = group("bar", r#", "op": [901], "stretch": 3"#, key);
    let objects = note(&format!(r#""group": [{rec}]"#));
    let l = run(&format!("{HEADER}, {}", body("", &objects, &[NOTES]))).loaded;
    assert!(dropped(&l, NoteKind::Stretch(3)));
}

#[test]
fn guide_lists_refuse_failure_presentation_the_source_makes() {
    let key = r#"{"x": 0, "y": 0, "w": 8, "h": 2}"#;
    let bar = format!(r#"{{"id": "bar", "dst": [{key}]}}"#);
    let guide = |field: &str, groups: &str, rec: &str| {
        let extra = format!(r#""group": [{groups}], "{field}": [{rec}]"#);
        run(&format!("{HEADER}, {}", body("", &note(&extra), &[NOTES])))
    };
    for field in ["bpm", "stop", "time"] {
        let rec = format!(r#"{{"id": "bar", "timer": 3, "dst": [{key}]}}"#);
        let r = guide(field, &bar, &rec);
        let why = NoteKind::UnsupportedField {
            field: match field {
                "bpm" => "bpm",
                "stop" => "stop",
                _ => "time",
            },
        };
        refused_as_failure(r, r#"{"id": "bar", "timer": 3"#, why);
        // Without a bar line group the source makes none; folded off or
        // naming no image it is not failure presentation: dropped as
        // before.
        for (groups, rec) in [
            (String::new(), rec.clone()),
            (
                bar.clone(),
                format!(r#"{{"id": "bar", "timer": 3, "op": [901], "dst": [{key}]}}"#),
            ),
            (
                bar.clone(),
                format!(r#"{{"id": "none", "timer": 3, "dst": [{key}]}}"#),
            ),
        ] {
            let l = guide(field, &groups, &rec).loaded;
            assert!(dropped(&l, why), "{field} {groups} {rec}");
        }
    }
}

#[test]
fn custom_timers_resolving_to_failed_are_failure_presentation() {
    let timers = r#""customTimers": [
        {"id": 10000, "timer": 3}, {"id": 10001, "timer": 10000},
        {"id": 10002, "timer": 10001}, {"id": 10003, "timer": "a_script()"},
        {"id": 10004, "timer": 10005}, {"id": 10005, "timer": 10004}]"#;
    let dst = |timer: i32, fields: &str| {
        format!(r#"{{"id": "i", "timer": {timer}, {fields}"dst": [{KEY}]}}"#)
    };
    // An alias of 3 is critical, and drawn once the gauge fails.
    let l = images(timers, &[&dst(10_000, "")]).loaded;
    assert!(l.report().notes.is_empty(), "{:?}", l.report().notes);
    assert_eq!(rects(&l, Some(1_000), 1_200), [[10, 1_010, 100, 50]]);
    let m = format!("{MOUSE}, ");
    let r = images(timers, &[&dst(10_000, &m)]);
    refused_as_failure(
        r,
        r#"{"id": "i", "timer": 10000"#,
        unsupported_field("mouseRect"),
    );
    // A chain to 3 is failure presentation the model has no timer for.
    for id in [10_001, 10_002] {
        let r = images(timers, &[&dst(id, "")]);
        refused_as_failure(
            r,
            &format!(r#"{{"id": "i", "timer": {id}"#),
            NoteKind::Timer(TimerWhy::Custom(id)),
        );
    }
    // A script timer, a cycle, timer 0 and a negative timer are not:
    // dropped.
    for timer in [10_003, 10_004, 0, -3] {
        let l = images(timers, &[&dst(timer, &m)]).loaded;
        let why = unsupported_field("mouseRect");
        assert!(dropped(&l, why), "{timer}: {:?}", l.report().notes);
    }
    // Without the unsupported field, the script timer and the cycle
    // drop it for its timer, timer 0 never draws it.
    for (timer, why) in [
        (10_003, NoteKind::Timer(TimerWhy::Custom(10_003))),
        (10_004, NoteKind::Timer(TimerWhy::Custom(10_004))),
    ] {
        let l = images(timers, &[&dst(timer, "")]).loaded;
        assert!(dropped(&l, why), "{timer}: {:?}", l.report().notes);
    }
    let l = images(timers, &[&dst(0, "")]).loaded;
    assert_eq!(
        common::kinds(l.report()),
        [NoteKind::Timer(TimerWhy::Unset(0))]
    );
}

#[test]
fn configured_options_fold_before_the_role() {
    let m = format!(r#""op": [OP], {MOUSE}"#);
    let with_op = |op: &str| m.replace("OP", op);
    // Folded off, it is not failure presentation: today's notes, exactly
    // those of the same destination on another timer.
    for op in ["901", "-900", "900, -900", "7777"] {
        let l = images("", &[&fail("i", &with_op(op))]).loaded;
        let timed = format!(
            r#"{{"id": "i", "timer": 41, {}, "dst": [{KEY}]}}"#,
            with_op(op)
        );
        let other = images("", &[&timed]).loaded;
        assert_eq!(l.report().notes, other.report().notes, "{op}");
        assert!(!l.report().notes.is_empty(), "{op}");
    }
    // Op 0, a selected item and its negation's opposite still draw.
    for op in ["0", "900", "-901", "0, 900"] {
        let r = images("", &[&fail("i", &with_op(op))]);
        refused_as_failure(r, &at("i"), unsupported_field("mouseRect"));
    }
    // A built-in condition is not folded.
    let r = images("", &[&fail("i", &with_op("41"))]);
    refused_as_failure(r, &at("i"), unsupported_field("mouseRect"));
    // `draw` never folds an option item: it has no effect there.
    let r = images("", &[&fail("i", &format!(r#""draw": 901, {MOUSE}"#))]);
    refused_as_failure(r, &at("i"), unsupported_field("mouseRect"));
    // Configuring 901 makes its destination failure presentation.
    let body = format!(
        r#"{HEADER}, {IMAGES}, "destination": [{}]"#,
        fail("i", &with_op("901"))
    );
    let r = run_with(&body, |c| c.options = vec![Some(901)]);
    refused_as_failure(r, &at("i"), unsupported_field("mouseRect"));
}

#[test]
fn resolution_folds_its_branches_first() {
    let wrapped = format!(r#"{{"if": 901, "value": {}}}"#, fail("i", MOUSE));
    let body = format!(r#"{HEADER}, {IMAGES}, "destination": [{wrapped}]"#);
    let l = run(&body).loaded;
    assert_eq!(l.report().usage.elements, 0, "{:?}", l.report().notes);
    let r = run_with(&body, |c| c.options = vec![Some(901)]);
    refused_as_failure(r, &at("i"), unsupported_field("mouseRect"));
}

#[test]
fn a_lua_skin_folds_its_own_options() {
    let lua = |opt: i32| {
        format!(
            "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
             property = {{ {{ name = \"p\", item = {{ {{ name = \"x\", op = 900 }}, \
             {{ name = \"y\", op = 901 }} }}, def = \"x\" }} }} }}\n\
             if not skin_config then return skin end\n\
             skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
             skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
             skin.destination = {{}}\n\
             if skin_config.option.p == {opt} then\n\
             skin.destination = {{ {{ id = \"i\", timer = 3, \
             mouseRect = {{ x = 0, y = 0, w = 1, h = 1 }}, \
             dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}\n\
             end\n\
             return skin\n"
        )
    };
    let folder = |opt| {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "s.luaskin", lua(opt).as_bytes());
        png(d.path(), "a.png", 64, 64);
        d
    };
    let d = folder(901);
    let r = common::lua_import(d.path()).unwrap();
    assert_eq!(r.imported.report.usage.elements, 0);
    let d = folder(900);
    match common::lua_import(d.path()) {
        // A converted tree is located by its node ordinal, line 0.
        Err(ImportError::Critical { part, why, at }) => assert_eq!(
            (part, why, at.file.as_str(), at.line),
            (
                CriticalPart::Failure,
                unsupported_field("mouseRect"),
                "s.luaskin",
                0
            )
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bitmap_font_fallbacks_lost_here_refuse_failure_presentation() {
    // `f.fnt` lacks `scaleW`, which the source reads: only this importer
    // refuses it.
    let fnt = |name: &str, scale: bool| {
        let scale = if scale { " scaleW=64" } else { "" };
        format!(
            "info face=\"t\" size=24 padding=0,0,0,0\n\
             common lineHeight=24 base=20{scale} scaleH=32 pages=1\n\
             page id=0 file=\"{name}.png\"\n\
             char id=65 x=0 y=0 width=10 height=20 xoffset=0 yoffset=0 xadvance=12 page=0 chnl=15\n"
        )
    };
    let skin = |dsts: &[String]| {
        let body = format!(
            r#""font": [{{"id": "f", "path": "p.fnt", "fallback": [{{"path": "f.fnt"}}]}}],
               "text": [{{"id": "t", "font": "f", "size": 24, "constantText": "A"}}],
               "destination": [{}]"#,
            dsts.join(", ")
        );
        let json = json_of(&body);
        let dir = skin_with(&json, &[]);
        write(dir.path(), "p.fnt", fnt("p.fnt", true).as_bytes());
        png(dir.path(), "p.fnt.png", 64, 32);
        write(dir.path(), "f.fnt", fnt("f.fnt", false).as_bytes());
        png(dir.path(), "f.fnt.png", 64, 32);
        Run {
            loaded: Loaded {
                result: common::import_dir(dir.path()),
                dir,
            },
            json,
        }
    };
    let why = NoteKind::BitmapFont(skin_model::FntError {
        line: 2,
        kind: skin_model::FntErrorKind::Metadata("scaleW"),
    });
    let decorative = format!(r#"{{"id": "t", "timer": 41, "dst": [{KEY}]}}"#);
    // Decorative, the loss is dropped and the text kept.
    let l = skin(std::slice::from_ref(&decorative)).loaded;
    assert!(dropped(&l, why));
    assert_eq!(l.report().usage.texts, 1);
    // Failure presentation using it first, or after a decorative text
    // made the font, refuses.
    let failure = fail("t", "");
    refused_as_failure(skin(std::slice::from_ref(&failure)), &at("t"), why);
    refused_as_failure(skin(&[decorative, failure]), &at("t"), why);
}

#[test]
fn the_refusal_names_the_failure_presentation() {
    let r = images("", &[&fail("i", MOUSE)]);
    let (line, col) = loc_of(&r.json, &at("i"));
    let e = r.loaded.err();
    assert_eq!(
        e.to_string(),
        format!(
            "s.json:{line}:{col}: the failure presentation cannot be imported: {}",
            unsupported_field("mouseRect")
        )
    );
    assert_eq!(CriticalPart::Failure.to_string(), "failure presentation");
}

/// A Lua skin drawing, on `timer`, an image `i` with `draw` when one, or
/// a number `v` of `value` when one.
fn lua_callback_skin(timer: i32, draw: Option<&str>, value: Option<&str>) -> tempfile::TempDir {
    let (id, draw, value) = match (draw, value) {
        (Some(d), _) => (
            "i",
            format!("draw = function() return {d} end,"),
            "ref = 110".to_owned(),
        ),
        (None, Some(v)) => (
            "v",
            String::new(),
            format!("value = function() return {v} end"),
        ),
        (None, None) => ("i", String::new(), "ref = 110".to_owned()),
    };
    let lua = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         skin.value = {{ {{ id = \"v\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, \
         digit = 2, {value} }} }}\n\
         skin.destination = {{ {{ id = \"{id}\", timer = {timer}, {draw} \
         dst = {{ {{ x = 0, y = 0, w = 6, h = 6 }} }} }} }}\n\
         return skin\n"
    );
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "s.luaskin", lua.as_bytes());
    png(d.path(), "a.png", 64, 64);
    d
}

/// How many callables were lowered, replaced and refused.
fn dispositions(r: &skin_beatoraja::LuaImported) -> [usize; 3] {
    use skin_beatoraja::lua::Disposition;
    let n = |f: fn(&Disposition) -> bool| r.callbacks.iter().filter(|d| f(d)).count();
    [
        n(|d| *d == Disposition::Lowered),
        n(|d| matches!(d, Disposition::Replaced(_))),
        n(|d| *d == Disposition::Refused),
    ]
}

#[test]
fn lua_callbacks_on_failure_presentation_lower_or_refuse() {
    use skin_beatoraja::LuaWhy;
    use skin_beatoraja::lua::Unsupported;
    // Supported: lowered, kept and drawn once the gauge fails. A judgment
    // class (unknown here, so 184 does not hold) is the source's datum.
    for (draw, value) in [
        (Some("ms.option(273)"), None),
        (None, Some("ms.number(110)")),
        (Some("not ms.option(184)"), None),
        (None, Some("ms.option(181) and 1 or 0")),
    ] {
        let d = lua_callback_skin(3, draw, value);
        let r = common::lua_import(d.path()).unwrap();
        assert_eq!(dispositions(&r), [1, 0, 0], "{draw:?} {value:?}");
        let rep = &r.imported.report;
        assert!(
            !rep.notes.iter().any(|n| n.severity == Severity::Dropped),
            "{:?}",
            rep.notes
        );
        let l = Loaded {
            result: Ok(r.imported),
            dir: d,
        };
        assert!(rects(&l, None, 2_000).is_empty(), "{draw:?} {value:?}");
        assert!(
            !rects(&l, Some(1_000), 1_200).is_empty(),
            "{draw:?} {value:?}"
        );
    }
    // Unsupported: on `failed` refused at the callback, elsewhere
    // replaced and dropped with its reason.
    let unmapped = Unsupported::Unmapped { api: 1, id: 426 };
    // Located at the callback: the destination's line, or the number's.
    for (field, draw, value, line) in [
        ("draw", Some("ms.number(426) > 0"), None, 7),
        ("value", None, Some("ms.number(110) + ms.number(426)"), 6),
    ] {
        let d = lua_callback_skin(3, draw, value);
        match common::lua_import(d.path()) {
            Err(ImportError::Lua {
                at: Some(at),
                why: LuaWhy::Critical { field: f, why },
            }) => {
                assert_eq!((f, why), (field, unmapped));
                assert_eq!((at.file.as_str(), at.line), ("s.luaskin", line));
            }
            other => panic!("{field}: {other:?}"),
        }
        let d = lua_callback_skin(41, draw, value);
        let r = common::lua_import(d.path()).unwrap();
        assert_eq!(dispositions(&r), [0, 1, 0], "{field}");
        let dropped: Vec<_> = r
            .imported
            .report
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(
            dropped,
            [NoteKind::Callback {
                field,
                why: unmapped
            }],
            "{field}"
        );
    }
}
