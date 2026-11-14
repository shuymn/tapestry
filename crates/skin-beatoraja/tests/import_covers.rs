//! SUDDEN+'s paint and LIFT: which elements paint SUDDEN+ (offset-4
//! images and image selections that may reach the lanes, type-4
//! sliders), which offset-4 elements stay decorative, how each moves,
//! and what each refuses, through `build`.
//!
//! The default lanes (r = 1, y 200, h 600, size 20): top 270, line 870,
//! bottom 880, SUDDEN+'s share measured from 280, LIFT at 880, a 600 px
//! lane over x [20, 450]. The default SUDDEN+ destination spans y −320 to
//! 280 (y down).

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::lanes::{NOTES, body, cover, hidden, hidden_object, lift, lift_object, note, with};
use common::play::{Drawn, MS};
use common::{Loaded, d3_notes, has, import_body, import_config, import_sized};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue,
    PropertyKind, Severity, TimerWhy,
};
use skin_beatoraja::{ImportLimit, Object, Usage};
use skin_model::Blend;
use skin_model::MAX_IMAGES;

/// The rects shown at 1,000 ms over a tap on lane 1 at `tap` ms, with the
/// covers at `cover` and `lift` ‰.
fn rects(l: &Loaded, tap: i64, cover: u16, lift: u16) -> Vec<[i32; 4]> {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(tap * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    d.view.cover = cover;
    d.view.lift = lift;
    d.shown(1_000)
}

#[track_caller]
fn critical(l: Loaded) -> (CriticalPart, NoteKind) {
    match l.err() {
        ImportError::Critical { part, why, .. } => (part, why),
        e => panic!("{e:?}"),
    }
}

/// An offset-4 image destination of `deco` with `fields` and `keys`.
fn deco(fields: &str, keys: &str) -> String {
    format!(r#"{{"id": "deco", "offset": 4{fields}, "dst": [{keys}]}}"#)
}

fn d9(a: LaneApprox) -> NoteKind {
    NoteKind::Deviation(DeviationId::D9(a))
}

/// Asserts `l` refused with a file's error: a critical element drawing a
/// file that does not exist.
#[track_caller]
fn assert_asset(l: Loaded, case: &str) {
    match l.err() {
        ImportError::Asset { .. } => {}
        e => panic!("{case}: {e:?}"),
    }
}

/// An image record whose source names a file that does not exist: a
/// critical element drawing it refuses, a decorative one is dropped.
const GHOST: &str = r#"{"id": "ghost", "src": "zz", "w": 16, "h": 16}"#;

/// `d` drawing [`GHOST`].
fn ghostly(d: &str) -> String {
    d.replace(r#""id": "cover""#, r#""id": "ghost""#)
        .replace(r#""id": "deco""#, r#""id": "ghost""#)
}

/// The notes of `l` the lanes alone do not carry, but
/// [`LaneApprox::CoverMotion`].
fn added_notes(l: &Loaded) -> Vec<NoteKind> {
    let lanes: Vec<NoteKind> = with("", &[])
        .report()
        .notes
        .iter()
        .map(|n| n.kind)
        .collect();
    l.report()
        .notes
        .iter()
        .map(|n| n.kind)
        .filter(|k| !lanes.contains(k) && *k != d9(LaneApprox::CoverMotion))
        .collect()
}

#[test]
fn sudden_plus_paint_moves_down_with_the_cover() {
    let l = with("", &[&cover("")]);
    assert_eq!(added_notes(&l), []);
    assert!(has(l.report(), d9(LaneApprox::CoverMotion)));
    // Its whole destination, uncut: at 0 ‰ where it stands, 500 ‰ of the
    // lane lowers it 300 px, 10 ‰ by 6.
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, -320, 430, 600]]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
    assert_eq!(rects(&l, 5_000, 10, 0), [[20, -314, 430, 600]]);
}

#[test]
fn sudden_plus_keeps_its_own_height() {
    let short = cover("").replace(r#""h": 600"#, r#""h": 300"#);
    let l = with("", &[&short]);
    // 500 ‰: a 300 px cover over [280, 580] leaves the lane above it.
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 280, 430, 300]]);
}

#[test]
fn lift_keeps_its_own_height() {
    let short = lift("").replace(
        r#""y": 0, "w": 430, "h": 200"#,
        r#""y": 100, "w": 430, "h": 100"#,
    );
    let l = with(&lift_object(""), &[&short]);
    // LIFT 500 ‰ raises its upper edge 300 px to 580; it is 100 high.
    assert_eq!(rects(&l, 5_000, 0, 500), [[20, 580, 430, 100]]);
}

/// A `liftCover` record wrapped in `if`/`value` is LIFT's object while its
/// option is selected, and no object otherwise.
#[test]
fn a_wrapped_lift_draws_when_selected() {
    let object = lift_object("");
    let record = object
        .strip_prefix(r#""liftCover": ["#)
        .and_then(|s| s.strip_suffix(']'))
        .unwrap();
    let wrapped = |def: &str| {
        let objects = format!(
            r#"{}, "liftCover": [{{"if": 900, "value": {record}}}]"#,
            note("")
        );
        import_body(&format!(
            r#""property": [{{"name": "p", "item": [{{"name": "on", "op": 900}},
                {{"name": "off", "op": 901}}], "def": "{def}"}}], {}"#,
            body("", &objects, &[NOTES, &lift("")])
        ))
    };
    let plain = with(&object, &[&lift("")]);
    let on = wrapped("on");
    assert!(!has(on.report(), NoteKind::UndefinedObject));
    assert_eq!(rects(&on, 5_000, 0, 500), rects(&plain, 5_000, 0, 500));
    let off = wrapped("off");
    assert!(has(off.report(), NoteKind::UndefinedObject));
    assert_eq!(
        rects(&off, 5_000, 0, 500),
        rects(&with("", &[]), 5_000, 0, 500)
    );
    assert_ne!(rects(&on, 5_000, 0, 500), rects(&off, 5_000, 0, 500));
}

#[test]
fn lift_and_sudden_plus_share_the_lane() {
    let l = with(&lift_object(""), &[&cover(""), &lift("")]);
    assert_eq!(added_notes(&l), []);
    // LIFT 200 ‰ takes 120 px, raising the line to 750; SUDDEN+ 500 ‰
    // takes half of the 480 px left: its paint's lower edge at 520 (the
    // source's 1080 − 800 + 240), LIFT's top at 760 (the source's `hl`).
    // The gain keeps the original lane's scale, 600 ms over its 600 px, so
    // a note 300 ms ahead is 300 px above where it would sit on the line,
    // under the paint (D4). The paint is drawn whole, LIFT cut to the
    // lanes.
    assert_eq!(
        rects(&l, 1_300, 500, 200),
        [[100, 440, 50, 20], [20, -80, 430, 600], [20, 760, 430, 120]]
    );
}

#[test]
fn offset_four_images_off_the_lanes_follow_sudden_plus() {
    for d in [
        // Left of the lanes, then touching their left edge.
        deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#),
        deco("", r#"{"x": 0, "y": 0, "w": 20, "h": 10}"#),
        // Animating between two places left of the lanes.
        deco(
            "",
            r#"{"time": 0, "x": 0, "y": 0, "w": 5, "h": 10}, {"time": 100, "x": 10, "y": 0, "w": 10, "h": 10}"#,
        ),
        // Right of them, also before the notes.
        deco("", r#"{"x": 450, "y": 0, "w": 10, "h": 10}"#),
    ] {
        let l = with("", &[&d]);
        let r = l.report();
        assert!(!has(r, NoteKind::RuntimeOffset(4)), "{d}: {:?}", r.notes);
        let motion = d9(LaneApprox::CoverMotion);
        assert_eq!(
            r.notes.iter().filter(|n| n.kind == motion).count(),
            1,
            "{d}"
        );
        // SUDDEN+ at 500 ‰ of the 600 px lane moves it 300 down.
        let still = rects(&l, 5_000, 0, 0);
        assert_eq!(still.len(), 1, "{d}");
        let moved: Vec<_> = still
            .iter()
            .map(|&[x, y, w, h]| [x, y + 300, w, h])
            .collect();
        assert_eq!(rects(&l, 5_000, 500, 0), moved, "{d}");
    }
    // Before the notes, the lanes' span is known ahead.
    let early = deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#);
    import_body(&body("", &note(""), &[&early, NOTES])).ok();
    // Past destinations naming nothing.
    let none = [
        r#"{"dst": [{"w": 1, "h": 1}]}"#,
        r#"{"id": "nothing", "dst": [{"w": 1, "h": 1}]}"#,
    ];
    import_body(&body("", &note(""), &[none[0], none[1], &early, NOTES])).ok();
}

/// An offset-4 image's keys are read twice, by the lane probe and by the
/// element; a clamped colour is reported once, at its key, whether it
/// paints SUDDEN+ over the lanes or follows it beside them.
#[test]
fn a_clamped_colour_over_the_lanes_is_reported_once() {
    for keys in [
        r#"{"x": 100, "y": 0, "w": 10, "h": 10, "a": 300}"#,
        r#"{"x": 0, "y": 0, "w": 10, "h": 10, "r": -1}"#,
    ] {
        let l = with("", &[&deco("", keys)]);
        let clamped: Vec<_> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.kind == NoteKind::ColourClamped)
            .collect();
        assert_eq!(clamped.len(), 1, "{keys}: {:?}", l.report().notes);
        assert_eq!(clamped[0].severity, Severity::Fallback, "{keys}");
        assert!(clamped[0].at.is_some(), "{keys}");
        let fine = with(
            "",
            &[&deco("", &keys.replace("300", "255").replace("-1", "0"))],
        );
        assert!(!has(fine.report(), NoteKind::ColourClamped), "{keys}");
    }
}

#[test]
fn an_unknown_span_keeps_offset_four_critical() {
    // A full cover before the notes, past destinations naming nothing,
    // reaches the lanes: SUDDEN+'s paint, whose missing file refuses.
    let early = ghostly(&cover(""));
    for none in [
        r#"{"dst": [{"w": 1, "h": 1}]}"#,
        r#"{"id": "nothing", "dst": [{"w": 1, "h": 1}]}"#,
    ] {
        let l = import_body(&body(GHOST, &note(""), &[none, &early, NOTES]));
        assert_asset(l, none);
    }
    // Lanes that cannot be read ahead leave the span unknown.
    let all = common::lanes::lane_rects(200, 600);
    let seven: Vec<&str> = all.rsplitn(2, "}, ").collect();
    let seven = seven[1].to_owned() + "}";
    let notes = common::lanes::note_with(&seven, "");
    let off = ghostly(&deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#));
    let l = import_body(&body(GHOST, &notes, &[&off, NOTES]));
    assert_asset(l, "unknown span");
    // Off readable lanes the same image is decorative: dropped.
    let l = import_body(&body(GHOST, &note(""), &[NOTES, &off]));
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::MissingAsset)
    );
}

#[test]
fn offset_four_elements_that_may_reach_the_lanes_are_sudden_plus() {
    let header = r#""offset": [{"name": "r", "id": 40, "r": true}]"#;
    let cases = [
        // A stretch on a static key that would stay left of the lanes:
        // the square cell fits a 200 px tall box 200 px wide, into them.
        deco(
            r#", "stretch": 6"#,
            r#"{"x": 0, "y": 20, "w": 10, "h": 200}"#,
        ),
        // A stretch whose two keys sit off the lanes, its frames between.
        deco(
            r#", "stretch": 1"#,
            r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 20}, {"time": 100, "x": 0, "y": 0, "w": 10, "h": 200}"#,
        ),
        // From left of the lanes to right of them.
        deco(
            "",
            r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 10}, {"time": 100, "x": 500, "y": 0, "w": 10, "h": 10}"#,
        ),
        // A negative width reaching in.
        deco("", r#"{"x": 30, "y": 0, "w": -20, "h": 10}"#),
        // Rotated, by its key, a configured offset or offset 1.
        deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10, "angle": 10}"#),
        deco(
            r#", "offsets": [40]"#,
            r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#,
        ),
        deco(
            r#", "offsets": [1]"#,
            r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#,
        ),
    ];
    // Each is SUDDEN+'s paint, play-critical: it refuses, as a cover or
    // with its missing file's error.
    for d in &cases {
        let d = &ghostly(d);
        let l = import_config(
            [1_920, 1_080],
            &body(GHOST, &format!("{header}, {}", note("")), &[NOTES, d]),
            &[],
            |c: &mut Config| {
                c.offsets.insert(
                    40,
                    OffsetValue {
                        r: 10,
                        ..OffsetValue::default()
                    },
                );
            },
        );
        match l.err() {
            ImportError::Asset { .. }
            | ImportError::Critical {
                part: CriticalPart::Cover,
                ..
            } => {}
            e => panic!("{d}: {e:?}"),
        }
    }
    // Frames selected by an expression: an image group and image sets.
    // A `ref` is an image-index property, 40 the shown gauge's type and 43
    // one the model cannot read; a `value` of 40 is no value property, so
    // the `ref` chooses. Over the lanes each is SUDDEN+'s paint showing
    // its choice, but the unreadable one refuses; off them each draws but
    // the unreadable one.
    let selectors = r#""imageset": [{"id": "byref", "images": ["cover"], "ref": 43},
                                    {"id": "byvalue", "images": ["cover"], "value": 71},
                                    {"id": "bykind", "images": ["cover"], "value": 40, "ref": 40}]"#;
    let group = r#"{"id": "group", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2},
                   {"id": "kindgroup", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}"#;
    let unreadable = NoteKind::UnsupportedProperty {
        kind: PropertyKind::ImageIndex,
        id: Some(43),
    };
    for (id, why) in [
        ("byref", vec![unreadable]),
        ("byvalue", vec![]),
        ("bykind", vec![]),
        ("group", vec![]),
        ("kindgroup", vec![]),
    ] {
        let over = cover("").replace(r#""id": "cover""#, &format!(r#""id": "{id}""#));
        let off = deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#)
            .replace(r#""id": "deco""#, &format!(r#""id": "{id}""#));
        let objects = format!("{}, {selectors}", note(""));
        let l = import_body(&body(group, &objects, &[NOTES, &over]));
        if let [why] = why.as_slice() {
            assert_eq!(critical(l), (CriticalPart::Cover, *why), "{id}");
        } else {
            let still = rects(&l, 5_000, 0, 0);
            let moved: Vec<_> = still
                .iter()
                .map(|&[x, y, w, h]| [x, y + 300, w, h])
                .collect();
            assert_eq!(rects(&l, 5_000, 500, 0), moved, "{id}");
        }
        // Off the lanes it is an ordinary image.
        let l = import_body(&body(group, &objects, &[NOTES, &off]));
        let dropped: Vec<NoteKind> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(dropped, why, "{id}");
    }
    // An image set whose `ref` is no image-index property shows its first
    // image; an image group without one its first frame.
    let null = r#""imageset": [{"id": "s", "images": ["cover"], "ref": 1}]"#;
    let over = cover("").replace(r#""id": "cover""#, r#""id": "s""#);
    let l = with(null, &[&over]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
    let objects = format!("{}, {selectors}", note(""));
    let over = cover("").replace(r#""id": "cover""#, r#""id": "group""#);
    let l = import_body(&body(group, &objects, &[NOTES, &over]));
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
}

#[test]
fn widgets_listing_offset_four_follow_sudden_plus() {
    // A number keeps its own role and follows: 300 down at 500 ‰.
    let number = r#""value": [{"id": "num", "src": "a", "w": 60, "h": 6, "divx": 10, "digit": 1, "ref": 71}]"#;
    let num = r#"{"id": "num", "offset": 4, "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let l = with(number, &[num]);
    assert!(has(l.report(), d9(LaneApprox::CoverMotion)));
    assert!(!has(l.report(), NoteKind::RuntimeOffset(4)));
    let still = rects(&l, 5_000, 0, 0);
    assert_eq!(still.len(), 1);
    let moved: Vec<_> = still
        .iter()
        .map(|&[x, y, w, h]| [x, y + 300, w, h])
        .collect();
    assert_eq!(rects(&l, 5_000, 500, 0), moved);
}

/// Blend 4 on a selecting image listing offset 4: over the lanes it is
/// SUDDEN+'s paint showing its table, and a cover paint never multiplies,
/// so the skin is refused as before; off them it is an image choice of its
/// own and multiplies.
#[test]
fn a_selecting_paint_does_not_multiply() {
    let group = r#"{"id": "group", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}"#;
    let objects = format!(
        r#"{}, "imageset": [{{"id": "set", "images": ["cover"], "value": 40, "ref": 40}}]"#,
        note("")
    );
    for id in ["group", "set"] {
        let over = cover(r#""blend": 4"#).replace(r#""id": "cover""#, &format!(r#""id": "{id}""#));
        let l = import_body(&body(group, &objects, &[NOTES, &over]));
        assert_eq!(
            critical(l),
            (CriticalPart::Cover, NoteKind::Blend(4)),
            "{id}"
        );
        let off = deco(r#", "blend": 4"#, r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#)
            .replace(r#""id": "deco""#, &format!(r#""id": "{id}""#));
        let l = import_body(&body(group, &objects, &[NOTES, &off]));
        assert!(!has(l.report(), NoteKind::Blend(4)), "{id}");
        let mut d = l.drawn();
        d.frame(5_000, &[]);
        let blends: Vec<_> = d.batch.runs().iter().map(|r| r.key.blend).collect();
        assert!(blends.contains(&Blend::Multiply), "{id}: {blends:?}");
    }
}

#[test]
fn selecting_images_listing_offset_four_follow_sudden_plus() {
    // Off the lanes, a group choosing by the gauge's type and a set
    // choosing by EX score each draw their choice and follow: 300 down at
    // 500 ‰.
    let group = r#"{"id": "g", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}"#;
    let objects = format!(
        r#"{}, "imageset": [{{"id": "s", "images": ["g", "g"], "value": 71}}]"#,
        note("")
    );
    for id in ["g", "s"] {
        let d = format!(
            r#"{{"id": "{id}", "offset": 4, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}"#
        );
        let l = import_body(&body(group, &objects, &[NOTES, &d]));
        assert!(has(l.report(), d9(LaneApprox::CoverMotion)), "{id}");
        assert!(
            has(l.report(), NoteKind::Deviation(DeviationId::D4)),
            "{id}"
        );
        let still = rects(&l, 5_000, 0, 0);
        assert_eq!(still.len(), 1, "{id}");
        let moved: Vec<_> = still
            .iter()
            .map(|&[x, y, w, h]| [x, y + 300, w, h])
            .collect();
        assert_eq!(rects(&l, 5_000, 500, 0), moved, "{id}");
    }
}

#[test]
fn static_offsets_keep_beside_repeated_cover_offsets() {
    // Offset 40 moves the image 5 right; 3 and 4, each listed twice and
    // 4 configured with a y the play overrides, move it once by both
    // covers, charging what the static offset alone charges.
    let header = r#""offset": [
        {"name": "s", "id": 40}, {"name": "l", "id": 3}, {"name": "c", "id": 4}
    ]"#;
    let load = |fields: &str| {
        let d =
            format!(r#"{{"id": "deco", {fields}, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}"#);
        let objects = format!("{}, {header}", note(""));
        import_config(
            [1_920, 1_080],
            &body("", &objects, &[NOTES, &d]),
            &[],
            |c| {
                c.offsets.insert(
                    40,
                    OffsetValue {
                        x: 5,
                        ..OffsetValue::default()
                    },
                );
                c.offsets.insert(
                    4,
                    OffsetValue {
                        y: 30,
                        ..OffsetValue::default()
                    },
                );
            },
        )
    };
    let l = load(r#""offset": 3, "offsets": [40, 4, 3, 4]"#);
    let fixed = load(r#""offsets": [40]"#);
    let motion = d9(LaneApprox::CoverMotion);
    assert_eq!(
        l.report().notes.iter().filter(|n| n.kind == motion).count(),
        1
    );
    let still = rects(&l, 5_000, 0, 0);
    assert_eq!(still, rects(&fixed, 5_000, 0, 0));
    assert_eq!(still.first().map(|r| r[0]), Some(5));
    // SUDDEN+ at 500 ‰ under LIFT at 200 ‰: 240 down, 120 up.
    let moved: Vec<_> = still
        .iter()
        .map(|&[x, y, w, h]| [x, y + 120, w, h])
        .collect();
    assert_eq!(rects(&l, 5_000, 500, 200), moved);
    // Only the motion note is added.
    let u = fixed.report().usage;
    assert_eq!(
        l.report().usage,
        Usage {
            notes: u.notes + 1,
            ..u
        }
    );
}

#[test]
fn covers_refuse_configured_components_besides_y() {
    // SUDDEN+ lists 4; the source adds 3 to LIFT and 3 and 5 to HIDDEN+
    // unlisted. Their y is the play's; any other component moves the
    // source's cover and is refused, HIDDEN+ dropped as decorative.
    let header =
        r#""offset": [{"name": "l", "id": 3}, {"name": "c", "id": 4}, {"name": "h", "id": 5}]"#;
    let objects = format!(
        "{}, {}, {}, {header}",
        note(""),
        lift_object(""),
        common::lanes::hidden_object("")
    );
    let load = |with_lift: bool, id: i32, v: OffsetValue| {
        let (c, l, h) = (cover(""), lift(""), common::lanes::hidden(""));
        let dsts: Vec<&str> = if with_lift {
            vec![NOTES, &c, &l, &h]
        } else {
            vec![NOTES, &h]
        };
        let dsts = dsts.join(", ");
        import_config([1_920, 1_080], &body("", &objects, &[&dsts]), &[], |c| {
            c.offsets.insert(id, v);
        })
    };
    let dropped = |l: &Loaded| -> Vec<NoteKind> {
        l.report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect()
    };
    let d = OffsetValue::default();
    for (name, v) in [
        ("x", OffsetValue { x: 16, ..d }),
        ("w", OffsetValue { w: 16, ..d }),
        ("h", OffsetValue { h: 16, ..d }),
        ("r", OffsetValue { r: 16, ..d }),
        ("a", OffsetValue { a: -16, ..d }),
    ] {
        assert_eq!(
            critical(load(true, 4, v)),
            (CriticalPart::Cover, NoteKind::RuntimeOffset(4)),
            "{name}"
        );
        assert_eq!(
            critical(load(true, 3, v)),
            (CriticalPart::Lift, NoteKind::RuntimeOffset(3)),
            "{name}"
        );
        for id in [3, 5] {
            let l = load(false, id, v);
            assert_eq!(dropped(&l), [NoteKind::RuntimeOffset(id)], "{name} {id}");
        }
    }
    for id in [3, 4, 5] {
        let l = load(true, id, OffsetValue { y: 16, ..d });
        l.ok();
        assert_eq!(dropped(&l), [], "{id}");
    }
}

#[test]
fn sudden_plus_paint_draws_as_authored() {
    let at = |rect: &str| cover("").replace(r#""x": 20, "y": 800, "w": 430, "h": 600"#, rect);
    // Split, wider, lower or without height: drawn where it stands,
    // 300 px lower at 500 ‰.
    for (d, want) in [
        (
            at(r#""x": 20, "y": 800, "w": 215, "h": 600"#),
            [20, -20, 215, 600],
        ),
        (
            at(r#""x": 0, "y": 801, "w": 500, "h": 600"#),
            [0, -21, 500, 600],
        ),
        (
            at(r#""x": 20, "y": 500, "w": 430, "h": 300"#),
            [20, 580, 430, 300],
        ),
    ] {
        let l = with("", &[&d]);
        assert_eq!(rects(&l, 5_000, 500, 0), [want], "{d}");
    }
    // Its own blend, colour, timer and conditions: each kept.
    for d in [
        cover(r#""blend": 2"#),
        cover("").replace(r#""h": 600}"#, r#""h": 600, "a": 200}"#),
        cover(r#""timer": 41"#),
        cover(r#""op": [240]"#),
    ] {
        let l = with("", &[&d]);
        assert_eq!(added_notes(&l), [], "{d}");
    }
    // Play-critical without a timer of its own (D3): a window ending on
    // `screen` is noted, a moving one refused.
    let keyed = |fields: &str, last: &str| {
        cover(fields).replace(
            r#""dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]"#,
            &format!(
                r#""dst": [{{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600}},
                           {{"time": 500, "x": 20, "y": {last}, "w": 430, "h": 600}}]"#
            ),
        )
    };
    let ending = with("", &[&keyed(r#""loop": -1"#, "800")]);
    assert_eq!(added_notes(&ending), [NoteKind::Deviation(DeviationId::D3)]);
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over(ending.ok().skin.clone(), &chart);
    assert_eq!((d.shown(500).len(), d.shown(501).len()), (1, 0));
    assert_eq!(
        critical(with("", &[&keyed("", "700")])),
        (CriticalPart::Cover, NoteKind::Timer(TimerWhy::Untimed))
    );
    let l = with("", &[&cover(r#""blend": 2"#)]);
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    d.frame(1_000, &[]);
    let keys: Vec<_> = d.batch.runs().iter().map(|r| r.key.blend).collect();
    assert_eq!(keys, [Blend::Add]);
    // Two paints, each drawn; one before the notes is drawn below them.
    let l = with("", &[&cover(""), &cover("")]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]; 2]);
    let l = import_body(&body("", &note(""), &[&cover(""), NOTES]));
    assert_eq!(
        rects(&l, 1_300, 0, 0),
        [[20, -320, 430, 600], [100, 560, 50, 20]]
    );
    // Moved lanes leave it where it stands.
    let header = r#""offset": [{"name": "x", "id": 40, "x": true}]"#;
    let notes = r#"{"id": "notes", "offset": 40, "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    let moved = import_config(
        [1_920, 1_080],
        &body("", &format!("{header}, {}", note("")), &[notes, &cover("")]),
        &[],
        |c: &mut Config| {
            c.offsets.insert(
                40,
                OffsetValue {
                    x: 1,
                    ..OffsetValue::default()
                },
            );
        },
    );
    assert_eq!(rects(&moved, 5_000, 500, 0), [[20, -20, 430, 600]]);
}

/// The outcome of SUDDEN+'s stretch-5 paint of record `cover` (a 16×16
/// cell) over `[x, 800, w, h]` (source coordinates, y up): its rects at
/// cover 0, or the critical refusal.
fn stretch_five(x: i32, w: i32, h: i32) -> Result<Vec<[i32; 4]>, (CriticalPart, NoteKind)> {
    let paint = format!(
        r#"{{"id": "cover", "offset": 4, "stretch": 5,
            "dst": [{{"x": {x}, "y": 800, "w": {w}, "h": {h}}}]}}"#
    );
    let l = with("", &[&paint]);
    match l.result {
        Ok(_) => Ok(rects(&l, 5_000, 0, 0)),
        Err(ImportError::Critical { part, why, .. }) => Err((part, why)),
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn stretch_five_takes_the_sources_branch_for_any_sign() {
    // The source trims when `h < w · ih / iw` (`skin/StretchType.java:
    // 65-68, 140-150`), else fits a frame `w · ih / iw` high about the
    // destination's centre. A model rect's y is 1080 − y − h.
    // w −100: −100 high, fitted.
    assert_eq!(
        stretch_five(300, -100, 50),
        Ok(vec![[300, 305, -100, -100]])
    );
    // w 0: 0 high, fitted.
    assert_eq!(stretch_five(300, 0, 50), Ok(vec![[300, 255, 0, 0]]));
    // A negative height is trimmed to a negative count of rows, which no
    // crop reproduces, under a zero width (the source's zero scale) as
    // under a positive one.
    let no_rows = Err((
        CriticalPart::Cover,
        NoteKind::UnsupportedField { field: "dst" },
    ));
    assert_eq!(stretch_five(300, 0, -50), no_rows);
    assert_eq!(stretch_five(200, 100, -50), no_rows);
}

#[test]
fn the_lanes_blend_ignores_the_outer_blend() {
    // The notes and SUDDEN+'s paint draw in alpha-blended runs, LIFT's in
    // its own under its clip: an additive note destination would leave a
    // black cover see-through. Blend 4 is ignored there too.
    for blend in [2, 3, 4] {
        let notes = format!(
            r#"{{"id": "notes", "blend": {blend}, "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}"#
        );
        let objects = format!("{}, {}", note(""), lift_object(""));
        let l = import_body(&body("", &objects, &[&notes, &cover(""), &lift("")]));
        assert!(
            has(l.report(), NoteKind::Ignored { field: "blend" }),
            "{blend}"
        );
        let chart = ChartBuilder::new(150_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_300 * MS))
            .build()
            .unwrap();
        let mut d = Drawn::over(l.ok().skin.clone(), &chart);
        d.view.cover = 500;
        d.view.lift = 200;
        assert_eq!(d.frame(1_000, &[]).len(), 3, "{blend}");
        let keys: Vec<_> = d
            .batch
            .runs()
            .iter()
            .map(|r| (r.key.blend, r.clip_rect.is_some()))
            .collect();
        assert_eq!(
            keys,
            [(Blend::Alpha, false), (Blend::Alpha, true)],
            "{blend}"
        );
    }
}

#[test]
fn windows_compare_by_what_they_show() {
    // The lanes looping a still track show for good, as a held paint.
    let notes = r#"{"id": "notes", "loop": 1000, "dst": [
        {"time": 0, "x": 0, "y": 0, "w": 0, "h": 0},
        {"time": 1000, "x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    let l = import_body(&body("", &note(""), &[notes, &cover(r#""loop": 0"#)]));
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
}

#[test]
fn the_covers_are_drawn_where_their_destinations_stand() {
    // A plain image, SUDDEN+'s paint and LIFT in either order: each drawn
    // in its destination's place, with nothing noted.
    let plain = r#"{"id": "deco", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let (p, c, li) = ([0, 1_070, 10, 10], [20, -80, 430, 600], [20, 760, 430, 120]);
    for (dsts, want) in [
        ([plain.to_owned(), cover(""), lift("")], [p, c, li]),
        ([cover(""), plain.to_owned(), lift("")], [c, p, li]),
        ([lift(""), cover(""), plain.to_owned()], [li, c, p]),
    ] {
        let dsts: Vec<&str> = dsts.iter().map(String::as_str).collect();
        let l = with(&lift_object(""), &dsts);
        assert_eq!(added_notes(&l), [], "{dsts:?}");
        assert_eq!(rects(&l, 5_000, 500, 200), want, "{dsts:?}");
    }
}

#[test]
fn every_lift_destination_paints() {
    let lifted = |l: &Loaded| rects(l, 5_000, 0, 200);
    // Without a line, whole; LIFT 200 ‰ raises it 120 px.
    let unlined = lift_object("").replace(
        r#""disapearLine": 200"#,
        r#""isDisapearLineLinkLift": false"#,
    );
    assert_eq!(lifted(&with(&unlined, &[&lift("")])), [[20, 760, 430, 200]]);
    // A negative line is none.
    let negative = lift_object("").replace("200", "-5");
    assert_eq!(
        lifted(&with(&negative, &[&lift("")])),
        [[20, 760, 430, 200]]
    );
    // A fixed line at 879 cuts it there.
    let higher = lift_object("").replace("200", "201");
    assert_eq!(lifted(&with(&higher, &[&lift("")])), [[20, 760, 430, 119]]);
    // A line rising with the lift reaches the raised upper edge: nothing.
    let linked = lift_object(r#""isDisapearLineLinkLift": true"#);
    assert!(lifted(&with(&linked, &[&lift("")])).is_empty());
    // Any height: 199 high from 881, raised to 761.
    let short = lift("").replace(r#""h": 200"#, r#""h": 199"#);
    assert_eq!(
        lifted(&with(&lift_object(""), &[&short])),
        [[20, 761, 430, 119]]
    );
    // Before the notes, twice, and another object besides.
    let two = lift_object("").replace(
        r#""disapearLine": 200}]"#,
        r#""disapearLine": 200}, {"id": "lift2", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8, "disapearLine": 200}]"#,
    );
    let second = lift("").replace("liftc", "lift2");
    let l = import_body(&body(
        "",
        &format!("{}, {two}", note("")),
        &[&lift(""), NOTES, &lift(""), &second],
    ));
    assert_eq!(lifted(&l).len(), 3);
    // A user offset moves it as any image's.
    let header = r#""offset": [{"name": "y", "id": 40, "y": true}]"#;
    let objects = format!("{header}, {}, {}", note(""), lift_object(""));
    let l = import_config(
        [1_920, 1_080],
        &body("", &objects, &[NOTES, &lift(r#""offsets": [40]"#)]),
        &[],
        |c: &mut Config| {
            c.offsets.insert(
                40,
                OffsetValue {
                    y: 10,
                    ..OffsetValue::default()
                },
            );
        },
    );
    assert_eq!(lifted(&l), [[20, 750, 430, 130]]);
    // Offset 5 makes it follow HIDDEN+ too: 200 ‰ of the 480 px LIFT
    // leaves raises it 96 more, and it is not drawn while HIDDEN+ is off.
    let l = with(&unlined, &[&lift(r#""offset": 5"#)]);
    let at = |on: bool| {
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let mut d = Drawn::over(l.ok().skin.clone(), &chart);
        (d.view.lift, d.view.hidden) = (200, if on { 200 } else { 0 });
        let mut readouts = common::play::readouts_for(d.view);
        readouts.covers.hidden_enabled = on;
        d.readouts = Some(readouts);
        d.shown(5_000)
    };
    assert_eq!(at(true), [[20, 664, 430, 200]]);
    assert!(at(false).is_empty());
}

/// The lanes over a 64×256 `a.png` with image records `extra` and `dsts`
/// after the note.
fn tall(extra: &str, dsts: &[&str]) -> Loaded {
    tall_with("", extra, dsts)
}

/// [`tall`] with `head` members before the body.
fn tall_with(head: &str, extra: &str, dsts: &[&str]) -> Loaded {
    let mut all = vec![NOTES];
    all.extend_from_slice(dsts);
    import_sized(
        [1_920, 1_080],
        &format!("{head}{}", body(extra, &note(""), &all)),
        &[("a.png", 64, 256)],
    )
}

/// A SUDDEN+ paint destination of image `id` with stretch `mode` over
/// the default lanes.
fn stretched(id: &str, mode: i32) -> String {
    cover(&format!(r#""stretch": {mode}"#)).replace(r#""id": "cover""#, &format!(r#""id": "{id}""#))
}

/// The rows of its frame SUDDEN+'s paint keeps at 1 000 ‰ (see
/// [`rows_at`]).
fn rows_drawn(l: &Loaded, width: f32) -> f32 {
    rows_at(l, [20.0, 280.0, 430.0, 600.0], width)
}

/// The one quad drawn over `rect` at 1 000 ‰ in `l`, a tap ahead.
fn cover_quad(l: &Loaded, rect: [f32; 4]) -> skin_model::Quad {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    // At 1 000 ‰ the default 600 px paint spans [280, 880].
    d.view.cover = 1_000;
    let q: Vec<_> = d
        .frame(1_000, &[])
        .into_iter()
        .filter(|q| q.rect == rect)
        .collect();
    assert_eq!(q.len(), 1);
    q[0]
}

/// The rows of its frame the one quad drawn over `rect` keeps, from its
/// uv's aspect against the frame's `width` texels (the atlas pages are
/// square).
fn rows_at(l: &Loaded, rect: [f32; 4], width: f32) -> f32 {
    let [u0, v0, u1, v1] = cover_quad(l, rect).uv;
    width * (v1 - v0) / (u1 - u0)
}

const WIDE: &str = r#"{"id": "cw", "src": "a", "x": 0, "y": 24, "w": 43, "h": 30}"#;

#[test]
fn fit_width_covers_take_the_frames_aspect() {
    // A 43×30 frame over 430 px is 300 high, centred on the 600 px
    // destination: from −170 to 130.
    for mode in [4, 5] {
        let l = tall(WIDE, &[&stretched("cw", mode)]);
        assert_eq!(rects(&l, 5_000, 0, 0), [[20, -170, 430, 300]], "{mode}");
        // At 1 000 ‰ it spans 430–730; no field is left, so the tap at
        // its judgment time is not drawn on the line (stage3.md R6).
        assert_eq!(rects(&l, 1_000, 1_000, 0), [[20, 430, 430, 300]], "{mode}");
    }
}

#[test]
fn a_fit_width_cover_takes_its_height_from_the_frame() {
    // A destination with no height: stretch 4 makes the 43×30 frame 300
    // high, centred on the destination's lower edge, 280: from 130 to 430.
    let flat = stretched("cw", 4).replace(r#""h": 600"#, r#""h": 0"#);
    let l = tall(WIDE, &[&flat]);
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, 130, 430, 300]]);
}

#[test]
fn fit_width_trimmed_covers_crop_the_frames_rows() {
    // A 43×120 frame fit to 430 would be 1 200 high: stretch 5 keeps the
    // 600 px destination and the frame's middle 60 rows (30 to 90).
    let frame = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 43, "h": 120}"#;
    let l = tall(frame, &[&stretched("ct", 5)]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
    assert_eq!(rows_drawn(&l, 43.0), 60.0);
    // Stretch 4 keeps every row and fits the height: 1 200 centred on
    // the destination.
    let l = tall(frame, &[&stretched("ct", 4)]);
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, -620, 430, 1_200]]);
    // Only a single frame crops: two rows of frames refuse.
    let grid = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 43, "h": 240, "divy": 2}"#;
    assert_eq!(
        critical(tall(grid, &[&stretched("ct", 5)])),
        (CriticalPart::Cover, NoteKind::Stretch(5))
    );
    // Uncropped, a grid fits as stretch 4 does.
    let grid = grid.replace(r#""h": 240"#, r#""h": 40"#);
    tall(&grid, &[&stretched("ct", 5)]).ok();
    // Only a static track crops: a moving one refuses.
    let moving = stretched("ct", 5).replace(
        r#""dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]"#,
        r#""dst": [{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                   {"time": 500, "x": 20, "y": 700, "w": 430, "h": 600}]"#,
    );
    assert_eq!(
        critical(tall(frame, &[&moving])),
        (CriticalPart::Cover, NoteKind::Stretch(5))
    );
}

#[test]
fn a_cropped_cover_is_its_own_image_of_the_same_file() {
    // The record drawn whole by a decorative image and cropped by
    // SUDDEN+: two images, one file.
    let frame = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 43, "h": 120}"#;
    let plain = r#"{"id": "ct", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let alone = tall(frame, &[&stretched("ct", 5)]);
    let both = tall(frame, &[plain, &stretched("ct", 5)]);
    let (a, b) = (alone.report().usage, both.report().usage);
    assert_eq!((b.images, b.frames), (a.images + 1, a.frames + 1));
    assert_eq!(b.files, a.files);
    // Drawn whole after the cover cropped it, it is added too. In either
    // order the cover keeps 60 rows and the plain image all 120.
    let after = tall(frame, &[&stretched("ct", 5), plain]);
    assert_eq!(after.report().usage.images, b.images);
    let whole = [0.0, 1_070.0, 10.0, 10.0];
    for l in [&both, &after] {
        assert_eq!(rows_drawn(l, 43.0), 60.0);
        assert_eq!(rows_at(l, whole, 43.0), 120.0);
    }
}

/// The texels the one quad drawn over `rect` at 1 000 ‰ in `l` samples at
/// each `(u, v)` fraction, `a.png` painted with [`common::coord`] first
/// (the import reads its header, the atlas its texels).
fn edge_paint(l: &Loaded, rect: [f32; 4], at: &[(f32, f32)]) -> Vec<[u8; 4]> {
    common::png_with(l.dir.path(), "a.png", 64, 256, common::coord);
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let q = cover_quad(l, rect);
    at.iter()
        .map(|&(u, v)| common::play::texel_at(&atlas, &q, u, v))
        .collect()
}

#[test]
fn a_cropped_cover_is_judged_by_the_rows_it_keeps() {
    // An 8 177-row frame fits no atlas page, but stretch 5 keeps 89 of its
    // rows: the sheet admits those, so SUDDEN+ stays, whatever else uses
    // the record. Drawn whole, the same record is dropped on its own, and
    // it refuses SUDDEN+ under a stretch that keeps every row.
    let frame = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 64, "h": 8177}"#;
    let plain = r#"{"id": "ct", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let over = NoteKind::CellTooLarge { w: 64, h: 8_177 };
    let run = |dsts: &[&str]| {
        let mut all = vec![NOTES];
        all.extend_from_slice(dsts);
        import_sized(
            [1_920, 1_080],
            &body(frame, &note(""), &all),
            &[("a.png", 64, 8_177)],
        )
    };
    let paint = stretched("ct", 5);
    let l = run(&[&paint]);
    assert!(!has(l.report(), over), "{:?}", l.report());
    assert_eq!(rows_drawn(&l, 64.0), 89.0);
    for dsts in [[plain, &*paint], [&*paint, plain]] {
        let l = run(&dsts);
        assert!(has(l.report(), over), "{:?}", l.report());
        assert_eq!(rows_drawn(&l, 64.0), 89.0);
    }
    assert_eq!(
        critical(run(&[&stretched("ct", 0)])),
        (CriticalPart::Cover, over)
    );
}

/// SUDDEN+'s stretch-5 paint of image `ce`, 80×320.
fn crop_paint() -> String {
    stretched("ce", 5).replace(r#""w": 430, "h": 600"#, r#""w": 80, "h": 320"#)
}

/// SUDDEN+ painting the 8×64 frame `ce` of a.png (64×256) at `at` (`"x":
/// …, "y": …`) under [`crop_paint`]: fit to 80 it would be 640 high, so
/// stretch 5 keeps the destination and the frame's middle 32 rows. The
/// texels at the left and right of its top and bottom rows (`want`, in
/// that order), whether the uncropped image was added before, after or
/// not at all, with no `SourceRect` note.
#[track_caller]
fn crop_ends(at: &str, want: [[u8; 4]; 4]) {
    let frame = format!(r#"{{"id": "ce", "src": "a", {at}, "w": 8, "h": 64}}"#);
    let paint = crop_paint();
    let plain = r#"{"id": "ce", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let ends = [(0.1, 0.01), (0.9, 0.01), (0.1, 0.99), (0.9, 0.99)];
    for dsts in [vec![&*paint], vec![plain, &paint], vec![&paint, plain]] {
        let l = tall(&frame, &dsts);
        assert!(!has(l.report(), NoteKind::SourceRect), "{at} {dsts:?}");
        let got = edge_paint(&l, [20.0, 560.0, 80.0, 320.0], &ends);
        assert_eq!(got, want, "{at} {dsts:?}");
    }
}

#[test]
fn a_trimmed_cover_past_the_edge_crops_its_cells_rows() {
    // Right of the file: the middle rows 16 to 47 of the edge column.
    crop_ends(
        r#""x": 64, "y": 0"#,
        [16, 16, 47, 47].map(|y| common::coord(63, y)),
    );
    let frame = r#"{"id": "ce", "src": "a", "x": 64, "y": 0, "w": 8, "h": 64}"#;
    let paint = crop_paint();
    let plain = r#"{"id": "ce", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    // A frame of more than one row cut to the file's last row has no
    // rows to crop, whether the uncropped image was added first or not.
    let below = frame.replace(r#""x": 64, "y": 0"#, r#""x": 0, "y": 256"#);
    for dsts in [vec![&*paint], vec![plain, &paint]] {
        assert_eq!(
            critical(tall(&below, &dsts)),
            (CriticalPart::Cover, NoteKind::SourceRect),
            "{dsts:?}"
        );
    }
    // A one-row frame there is the last row, which the source samples
    // too: 8×1 fit to 80 is 10 high, cropped to the 5 px destination.
    let row = below.replace(r#""h": 64"#, r#""h": 1"#);
    let thin = paint.replace(r#""h": 320"#, r#""h": 5"#);
    let l = tall(&row, &[&thin]);
    assert!(!has(l.report(), NoteKind::SourceRect));
    let mid = [(0.1, 0.5), (0.9, 0.5)];
    let got = edge_paint(&l, [20.0, 875.0, 80.0, 5.0], &mid);
    assert_eq!(got, [common::coord(0, 255), common::coord(7, 255)]);
}

#[test]
fn a_trimmed_cover_cropped_past_the_bottom_edge_draws_the_last_row() {
    let c = common::coord;
    // From row 240 it keeps rows 256 to 287, wholly below the file, where
    // the source samples its last row throughout.
    crop_ends(
        r#""x": 0, "y": 240"#,
        [c(0, 255), c(7, 255), c(0, 255), c(7, 255)],
    );
    // Columns 60 to 67 too run past the right edge: the last row's
    // columns 60 to 63, 63 repeated.
    crop_ends(
        r#""x": 60, "y": 240"#,
        [c(60, 255), c(63, 255), c(60, 255), c(63, 255)],
    );
    // Wholly right of the file: the corner texel.
    crop_ends(r#""x": 64, "y": 240"#, [c(63, 255); 4]);
    // From row 220 it keeps rows 236 to 267: in the file down to its last
    // row, which repeats below it.
    crop_ends(
        r#""x": 0, "y": 220"#,
        [c(0, 236), c(7, 236), c(0, 255), c(7, 255)],
    );
}

/// The texels at the middle of each end of every quad 430 px wide at x
/// 20 in the frame at 1,000 ms, with LIFT at `lift` ‰ and HIDDEN+ on at
/// `hidden` ‰ when it is not 0, `a.png` (64×64) painted with
/// [`common::coord`] first.
fn cover_ends(l: &Loaded, lift: u16, hidden: u16) -> Vec<[u8; 4]> {
    common::png_with(l.dir.path(), "a.png", 64, 64, common::coord);
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    (d.view.lift, d.view.hidden) = (lift, hidden);
    let mut readouts = common::play::readouts_for(d.view);
    (readouts.covers.hidden, readouts.covers.hidden_enabled) = (hidden, hidden > 0);
    d.readouts = Some(readouts);
    d.frame(1_000, &[])
        .iter()
        .filter(|q| {
            let [x, _, w, _] = common::play::round(q.rect);
            (x, w) == (20, 430)
        })
        .flat_map(|q| {
            [(0.1, 0.5), (0.9, 0.5)].map(|(u, v)| common::play::texel_at(&atlas, q, u, v))
        })
        .collect()
}

#[test]
fn trimmed_lift_and_hidden_covers_cropped_past_the_bottom_edge_draw_the_last_row() {
    // An 8×64 frame from row 40 of a.png (64×64) under stretch 5: LIFT's
    // 430×200 destination keeps its rows 70 to 73 and HIDDEN+'s 430×600
    // its rows 66 to 77, wholly below the file, where the source samples
    // its last row throughout.
    let frame = r#""x": 0, "y": 40, "w": 8, "h": 64"#;
    let want = [common::coord(0, 63), common::coord(7, 63)];
    let object = lift_object("").replace(r#""x": 16, "y": 24, "w": 16, "h": 16"#, frame);
    let l = with(&object, &[&lift(r#""stretch": 5"#)]);
    assert!(!has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
    assert_eq!(cover_ends(&l, 200, 0), want);
    let object = hidden_object("").replace(r#""x": 32, "y": 24, "w": 16, "h": 16"#, frame);
    let l = with(&object, &[&hidden(r#""stretch": 5"#)]);
    assert!(!has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
    assert_eq!(cover_ends(&l, 0, 200), want);
}

#[test]
fn a_trimmed_hidden_cover_cut_to_the_last_row_drops() {
    // HIDDEN+'s paint is decorative: an 8×64 frame below a.png's last row
    // (64×64), cropped under stretch 5, drops with `SourceRect`.
    let object = hidden_object("").replace(
        r#""x": 32, "y": 24, "w": 16, "h": 16"#,
        r#""x": 0, "y": 64, "w": 8, "h": 64"#,
    );
    let l = with(&object, &[&hidden(r#""stretch": 5"#)]);
    assert!(has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
}

#[test]
fn other_stretches_refuse_the_covers() {
    // SUDDEN+'s and LIFT's paint refuse the other trimming stretches and
    // map every other.
    for mode in [1, 2, 3, 6, 7, 8, 9, 10] {
        let want = NoteKind::Stretch(mode);
        let paint = tall(WIDE, &[&stretched("cw", mode)]);
        let l = with(&lift_object(""), &[&lift(&format!(r#""stretch": {mode}"#))]);
        if matches!(mode, 3 | 7 | 10) {
            assert_eq!(critical(paint), (CriticalPart::Cover, want), "{mode}");
            assert_eq!(critical(l), (CriticalPart::Lift, want), "{mode}");
        } else {
            paint.ok();
            l.ok();
        }
    }
    // Unknown modes are identity.
    tall(WIDE, &[&stretched("cw", 11)]).ok();
}

#[test]
fn a_fitted_lift_is_gated_by_its_destination() {
    // The source gates and clips LIFT by its destination before
    // stretching it: at LIFT 200 ‰ the destination [880, 1080] rises to
    // [760, 960] and is cut at 880. A 16×16 frame over 430 px grows to
    // 430 tall, a 43×10 one shrinks to 100: either is drawn whole, under
    // that clip.
    let lift_mode = |mode: i32| lift(&format!(r#""stretch": {mode}"#));
    let small = lift_object("").replace(r#""w": 16, "h": 16"#, r#""w": 43, "h": 10"#);
    let at = |object: &str, mode: i32| {
        let l = with(object, &[&lift_mode(mode)]);
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let mut d = Drawn::over(l.ok().skin.clone(), &chart);
        d.view.lift = 200;
        d.clipped(1_000)
    };
    let clip = Some([20, 760, 430, 120]);
    assert_eq!(at(&lift_object(""), 4), [([20, 645, 430, 430], clip)]);
    assert_eq!(at(&small, 4), [([20, 810, 430, 100], clip)]);
    assert_eq!(at(&small, 5), [([20, 810, 430, 100], clip)]);
    // Stretch 5 crops the 16×16 frame's rows and keeps the destination.
    assert_eq!(at(&lift_object(""), 5), [([20, 760, 430, 200], clip)]);
    // A 43×20 frame fits 430×200 exactly.
    let exact = lift_object("").replace(r#""w": 16, "h": 16"#, r#""w": 43, "h": 20"#);
    for mode in [4, 5] {
        assert_eq!(at(&exact, mode), [([20, 760, 430, 200], clip)], "{mode}");
    }
}

/// The lanes' six part images, `plain` decorative images and a cropped
/// SUDDEN+ over source `src`, which nothing else reads: `n` (`n.png`) or
/// `m`, a file that does not exist.
fn with_plain_images(plain: usize, src: &str) -> Loaded {
    let mut extra: Vec<String> = (0..plain)
        .map(|i| format!(r#"{{"id": "p{i}", "src": "a", "w": 1, "h": 1}}"#))
        .collect();
    extra.push(format!(
        r#"{{"id": "cf", "src": "{src}", "x": 0, "y": 0, "w": 4, "h": 20}}"#
    ));
    let dsts: Vec<String> = (0..plain)
        .map(|i| format!(r#"{{"id": "p{i}", "dst": [{{"x": 0, "y": 0, "w": 1, "h": 1}}]}}"#))
        .collect();
    let mut all: Vec<&str> = dsts.iter().map(String::as_str).collect();
    let cover = stretched("cf", 5);
    all.push(&cover);
    // The skin's own `source` list replaces the default one (the last
    // field wins), adding the missing `m.png`.
    let sources = r#""source": [{"id": "a", "path": "a.png"}, {"id": "n", "path": "n.png"},
        {"id": "m", "path": "m.png"}], "#;
    tall_with(sources, &extra.join(", "), &all)
}

#[test]
fn a_cropped_cover_is_charged_before_its_file_is_read() {
    // At the image cap the cover maps; its file missing, it refuses on
    // the file.
    let at_cap = with_plain_images(MAX_IMAGES - 7, "n");
    assert_eq!(at_cap.report().usage.images as usize, MAX_IMAGES);
    assert_eq!(rows_drawn(&at_cap, 4.0), 6.0);
    assert!(!matches!(
        with_plain_images(MAX_IMAGES - 7, "m").result,
        Ok(_) | Err(ImportError::Limit { .. })
    ));
    // One image more refuses at the cover's charge, before its missing
    // file is read.
    match with_plain_images(MAX_IMAGES - 6, "m").result {
        Err(ImportError::Limit { limit, .. }) => assert_eq!(limit, ImportLimit::Images),
        other => panic!("{:?}", other.map(|i| i.report)),
    }
}

/// A `slider` record of `a.png`'s cover cell with `fields`, drawn over
/// the default SUDDEN+ destination with `dst` fields.
fn slider(fields: &str, dst: &str) -> (String, String) {
    let object = format!(
        r#""slider": [{{"id": "sl", "src": "a", "x": 0, "y": 24, "w": 16, "h": 16{fields}}}]"#
    );
    let sep = if dst.is_empty() { "" } else { ", " };
    let d =
        format!(r#"{{"id": "sl"{sep}{dst}, "dst": [{{"x": 20, "y": 800, "w": 430, "h": 600}}]}}"#);
    (object, d)
}

const SLIDES: &str = r#", "type": 4, "angle": 2, "range": 600"#;

#[test]
fn a_type_four_slider_paints_sudden_plus() {
    let (object, d) = slider(SLIDES, "");
    let l = with(&object, &[&d]);
    assert_eq!(added_notes(&l), []);
    assert!(!has(l.report(), d9(LaneApprox::CoverMotion)));
    // Its range times SUDDEN+'s share of the whole lane: 600 × 500 ‰
    // lowers it 300; under LIFT 200 ‰, 600 × 500 ‰ × 800 ‰ = 240, as
    // far as offset 4 moves.
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, -320, 430, 600]]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
    assert_eq!(rects(&l, 5_000, 500, 200), [[20, -80, 430, 600]]);
    // Listing offset 4 too, it moves by both.
    let (_, d4) = slider(SLIDES, r#""offset": 4"#);
    let l = with(&object, &[&d4]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 280, 430, 600]]);
    // `changeable` needs nothing; an `event` is unused.
    let (object, d) = slider(&format!(r#"{SLIDES}, "changeable": false, "event": 1"#), "");
    let l = with(&object, &[&d]);
    assert!(has(l.report(), NoteKind::Ignored { field: "event" }));
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, -20, 430, 600]]);
}

#[test]
fn a_slider_range_scales_exactly() {
    // A 720 px skin with one type-4 slider of `range`.
    let sized = |range: i32| {
        let (object, d) = slider(&format!(r#", "type": 4, "angle": 2, "range": {range}"#), "");
        import_sized(
            [1_280, 720],
            &body("", &format!("{}, {object}", note("")), &[NOTES, &d]),
            &[],
        )
    };
    // The paint's y at cover `c`, drawn last, after any note it uncovers.
    let ys = |l: &Loaded, covers: [u16; 2]| {
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let mut drawn = Drawn::over(l.ok().skin.clone(), &chart);
        covers.map(|c| {
            drawn.view.cover = c;
            drawn.frame(1_000, &[]).last().unwrap().rect[1]
        })
    };
    // `1080 · range / h` exactly: 333 is 499.5, which 1 000 ‰ moves it.
    let [still, moved] = ys(&sized(333), [0, 1_000]);
    assert_eq!(moved - still, 499.5);
    // Range 5 is 7.5 reference pixels, so 3/8 of the lane moves it
    // 2.8125, 1.875 pixels of a 1 280×720 target.
    let [still, moved] = ys(&sized(5), [0, 375]);
    assert!((moved - still - 2.8125).abs() < 1e-3, "{still} {moved}");
    // A range past the coordinates refuses, its exact value counting:
    // 699 051 is 1 048 576.5, half a pixel past `MAX_COORD`.
    for range in [699_051, 2_000_000_000] {
        match sized(range).err() {
            ImportError::Range { field, .. } => assert_eq!(field, "range", "{range}"),
            e => panic!("{range}: {e:?}"),
        }
    }
}

#[test]
fn a_native_paint_counts_one_quad() {
    // Stretch 9 keeps the 16×16 cell: one image quad beside the lanes'
    // budget, whose reservations include the judgment line.
    let l = with("", &[&cover(r#""stretch": 9"#)]);
    assert_eq!(
        l.ok().skin.quad_capacity(),
        1 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED
    );
    let r = rects(&l, 5_000, 500, 0);
    assert_eq!(r.len(), 1);
    assert_eq!([r[0][2], r[0][3]], [16, 16]);
}

#[test]
fn other_sliders_are_dropped() {
    let dropped = |fields: &str| {
        let (object, d) = slider(fields, "");
        let l = with(&object, &[&d]);
        let kinds: Vec<NoteKind> = l
            .report()
            .notes
            .iter()
            .filter(|n| n.severity == Severity::Dropped)
            .map(|n| n.kind)
            .collect();
        assert_eq!(rects(&l, 5_000, 500, 0), Vec::<[i32; 4]>::new(), "{fields}");
        kinds
    };
    let field = |field| vec![NoteKind::UnsupportedField { field }];
    assert_eq!(
        dropped(r#", "type": 1, "angle": 2, "range": 600"#),
        [NoteKind::Unsupported {
            object: Object::Slider
        }]
    );
    assert_eq!(
        dropped(r#", "type": 4, "angle": 1, "range": 600"#),
        field("angle")
    );
    assert_eq!(
        dropped(r#", "type": 4, "angle": 2, "range": -1"#),
        field("range")
    );
    assert_eq!(
        dropped(&format!(r#"{SLIDES}, "value": 71"#)),
        field("value")
    );
    assert_eq!(
        dropped(&format!(r#"{SLIDES}, "isRefNum": true"#)),
        field("isRefNum")
    );
}

#[test]
fn a_slider_off_the_lanes_is_decorative() {
    // Drawing a missing file: refused over the lanes, dropped off them.
    let ghost = |rect: &str| {
        let object =
            format!(r#""slider": [{{"id": "sl", "src": "zz", "w": 16, "h": 16{SLIDES}}}]"#);
        let d = format!(r#"{{"id": "sl", "dst": [{rect}]}}"#);
        with(&object, &[&d])
    };
    assert_asset(ghost(r#"{"x": 20, "y": 800, "w": 430, "h": 600}"#), "over");
    let l = ghost(r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#);
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::MissingAsset)
    );
}

#[test]
fn a_slider_paints_without_lanes() {
    let (object, d) = slider(SLIDES, "");
    let l = import_body(&body("", &object, &[&d]));
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut drawn = Drawn::over(l.ok().skin.clone(), &chart);
    drawn.view.cover = 500;
    assert_eq!(drawn.rects(1_000), [[20, -20, 430, 600]]);
}

/// An offset-4 image set `{"id": "s", "images": [...], "value": 110}`
/// over the default lanes with destination `fields` and `keys`, its
/// images `extra` and the skin's own `sources`.
fn cover_set(sources: &str, extra: &str, images: &str, fields: &str, keys: &str) -> Loaded {
    let objects = format!(
        r#"{}, "imageset": [{{"id": "s", "images": [{images}], "value": 110}}]"#,
        note("")
    );
    let d = format!(r#"{{"id": "s", "offset": 4{fields}, "dst": [{keys}]}}"#);
    import_body(&format!("{sources}{}", body(extra, &objects, &[NOTES, &d])))
}

const OVER: &str = r#"{"x": 20, "y": 800, "w": 430, "h": 600}"#;

#[test]
fn selecting_paint_varies_only_its_presentation_phase_untimed() {
    let untimed = (CriticalPart::Cover, NoteKind::Timer(TimerWhy::Untimed));
    let ending = r#"{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                    {"time": 500, "x": 20, "y": 800, "w": 430, "h": 600}"#;
    let moving = r#"{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                    {"time": 500, "x": 20, "y": 700, "w": 430, "h": 600}"#;
    let cycling = r#"{"id": "cy", "src": "a", "w": 16, "h": 16, "divx": 2, "cycle": 100}"#;
    let group = r#"{"id": "s", "src": "a", "w": 16, "h": 16, "divx": 4, "len": 2, "cycle": 100}"#;
    let group_over = |fields: &str| {
        let d = format!(r#"{{"id": "s", "offset": 4{fields}, "dst": [{OVER}]}}"#);
        import_body(&body(group, &note(""), &[NOTES, &d]))
    };
    // A moving track refuses; an ending window and an untimed cycle are
    // noted, for an image set and an image group alike. An untimed cycle
    // runs on `screen` whatever the destination's own timer, so it is
    // noted on a timed paint too.
    let l = cover_set("", cycling, r#""cover""#, "", moving);
    assert_eq!(critical(l), untimed);
    let timed = cover_set("", cycling, r#""cover""#, r#", "timer": 41"#, moving);
    assert_eq!(d3_notes(&timed), 0);
    for (images, fields, keys, when_timed) in [
        (r#""cover""#, r#", "loop": -1"#, ending, 0),
        (r#""cy", "cover""#, "", OVER, 1),
    ] {
        let l = cover_set("", cycling, images, fields, keys);
        assert_eq!(d3_notes(&l), 1, "{images} {fields} {keys}");
        let timed = format!(r#", "timer": 41{fields}"#);
        let l = cover_set("", cycling, images, &timed, keys);
        assert_eq!(d3_notes(&l), when_timed, "{images} {fields} {keys}");
    }
    assert_eq!(d3_notes(&group_over("")), 1);
    assert_eq!(d3_notes(&group_over(r#", "timer": 41"#)), 1);
    // A static, perpetual one without a cycle imports untimed.
    assert_eq!(
        d3_notes(&cover_set("", "", r#""cover", "cover""#, "", OVER)),
        0
    );
}

#[test]
fn a_critical_set_keeps_missing_files_as_holes() {
    // `m.png` does not exist, for the source too: each reference to it is
    // a hole, noted, and the paint shows the entries that load.
    let sources = r#""source": [{"id": "a", "path": "a.png"}, {"id": "m", "path": "m.png"},
        {"id": "bad", "path": "../x.png"}], "#;
    let gone = r#"{"id": "gone", "src": "m", "w": 16, "h": 16}"#;
    let l = cover_set(sources, gone, r#""cover", "gone", "gone""#, "", OVER);
    let holes = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::MissingAsset)
        .count();
    assert_eq!(holes, 2);
    assert_eq!(rects(&l, 5_000, 500, 0).len(), 1);
    // An untimed cycle only a hole would show is not retained: no D3.
    let cycled_hole = r#"{"id": "gone", "src": "m", "w": 16, "h": 16, "divx": 2, "cycle": 100}"#;
    let l = cover_set(sources, cycled_hole, r#""cover", "gone""#, "", OVER);
    assert_eq!(d3_notes(&l), 0);
    // A file only this importer refuses is no hole: refused with its
    // error.
    let refused = r#"{"id": "gone", "src": "bad", "w": 16, "h": 16}"#;
    let l = cover_set(sources, refused, r#""cover", "gone""#, "", OVER);
    assert!(matches!(l.err(), ImportError::Asset { .. }));
}

/// SUDDEN+'s paint under the adjustment and BPM-change conditions imports
/// with one D4 note and draws only while the covers are adjusted over a
/// chart whose BPM changes.
#[test]
fn sudden_plus_paint_follows_the_bpm_change_condition() {
    let l = with("", &[&cover(r#""op": [270, 177]"#)]);
    assert_eq!(added_notes(&l), [NoteKind::Deviation(DeviationId::D4)]);
    let chart = ChartBuilder::new(150_000).build().unwrap();
    for (fact, adjusting, shown) in [
        (Some(true), true, true),
        (Some(true), false, false),
        (Some(false), true, false),
        (None, true, false),
    ] {
        let mut d = Drawn::over_with(l.ok().skin.clone(), &chart, None, fact);
        let mut r = common::play::readouts_for(d.view);
        r.covers.adjusting = adjusting;
        d.readouts = Some(r);
        let paints = d.shown(0).into_iter().filter(|r| r[2] == 430).count();
        assert_eq!(paints, usize::from(shown), "{fact:?}, {adjusting}");
    }
}

/// S10-09g D13: a cover cropped from a frame past its file's edge draws
/// the same texels through an atlas packed from another sheet of its
/// cells (one frame per cell, another frame order) and rebound to the
/// imported sheet, as through its own atlas: the edge's substitution and
/// the crop are cells, which rebinding finds by identity.
#[test]
fn an_edge_cut_cropped_cover_rebinds_to_its_cells() {
    let frame = r#"{"id": "ce", "src": "a", "x": 64, "y": 0, "w": 8, "h": 64}"#;
    let paint = stretched("ce", 5).replace(r#""w": 430, "h": 600"#, r#""w": 80, "h": 320"#);
    let plain = r#"{"id": "ce", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let ends = [(0.1, 0.01), (0.9, 0.01), (0.1, 0.99), (0.9, 0.99)];
    let want = [16, 16, 47, 47].map(|y| common::coord(63, y));
    let l = tall(frame, &[plain, &paint]);
    common::png_with(l.dir.path(), "a.png", 64, 256, common::coord);
    let sheet = l.ok().skin.sheet();
    let other = sheet.union(sheet).unwrap();
    let packed = other
        .atlas_reserving(
            skin_model::Variant::Lo,
            common::play::LIMITS,
            false,
            &|| false,
        )
        .unwrap();
    let rebound = packed
        .rebind(sheet, skin_model::Variant::Lo, false)
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(
        packed.shared_pages(),
        rebound.shared_pages()
    ));
    let q = cover_quad(&l, [20.0, 560.0, 80.0, 320.0]);
    let got: Vec<_> = ends
        .iter()
        .map(|&(u, v)| common::play::texel_at(&rebound, &q, u, v))
        .collect();
    assert_eq!(got, want);
    assert_eq!(edge_paint(&l, [20.0, 560.0, 80.0, 320.0], &ends), want);
}
