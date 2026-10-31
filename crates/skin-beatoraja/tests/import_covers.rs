//! SUDDEN+, LIFT and HIDDEN+: which elements become the covers, which
//! offset-4 elements stay decorative, and what each refuses, through
//! `build`.
//!
//! The default lanes (r = 1, y 200, h 600, size 20): top 270, line 870,
//! bottom 880, SUDDEN+ anchored at 280, LIFT at 880, a 600 px lane over
//! x [20, 450].

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::lanes::{NOTES, body, cover, lift, lift_object, note};
use common::play::{Drawn, MS};
use common::{Loaded, has, import_body, import_config};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue, Severity,
};
use skin_model::Blend;

/// The lanes with `objects` beside the note and `dsts` after it.
fn with(objects: &str, dsts: &[&str]) -> Loaded {
    let objects = if objects.is_empty() {
        note("")
    } else {
        format!("{}, {objects}", note(""))
    };
    let mut all = vec![NOTES];
    all.extend_from_slice(dsts);
    import_body(&body("", &objects, &all))
}

/// The rects at 1,000 ms over a tap on lane 1 at `tap` ms, with the
/// covers at `cover` and `lift` ‰.
fn rects(l: &Loaded, tap: i64, cover: u16, lift: u16) -> Vec<[i32; 4]> {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(tap * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    d.view.cover = cover;
    d.view.lift = lift;
    d.rects(1_000)
}

#[track_caller]
fn critical(l: Loaded) -> (CriticalPart, NoteKind) {
    match l.err() {
        ImportError::Critical { part, why, .. } => (part, why),
        e => panic!("{e:?}"),
    }
}

#[track_caller]
fn refused(l: Loaded, part: CriticalPart, field: &'static str) {
    assert_eq!(critical(l), (part, NoteKind::UnsupportedField { field }));
}

/// An offset-4 image destination of `deco` with `fields` and `keys`.
fn deco(fields: &str, keys: &str) -> String {
    format!(r#"{{"id": "deco", "offset": 4{fields}, "dst": [{keys}]}}"#)
}

fn d9(a: LaneApprox) -> NoteKind {
    NoteKind::Deviation(DeviationId::D9(a))
}

#[test]
fn sudden_plus_maps_from_its_anchor() {
    let l = with("", &[&cover("")]);
    assert!(has(l.report(), d9(LaneApprox::CoverAbove)));
    assert!(!has(l.report(), d9(LaneApprox::CoverOrder)));
    // 500 ‰ of the lane: the cover's lower edge at 280 + 300.
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 280, 430, 300]]);
    // 0 ‰ draws no cover; 10 ‰ a strip from the anchor.
    assert_eq!(rects(&l, 5_000, 0, 0), Vec::<[i32; 4]>::new());
    assert_eq!(rects(&l, 5_000, 10, 0), [[20, 280, 430, 6]]);
}

#[test]
fn lift_and_sudden_plus_share_the_lane() {
    let l = with(&lift_object(""), &[&cover(""), &lift("")]);
    assert!(has(l.report(), d9(LaneApprox::LiftStretch)));
    // LIFT 200 ‰ takes 120 px, raising the line to 750; SUDDEN+ 500 ‰
    // takes half of the 480 px left: its edge at 520 (the source's
    // 1080 − 800 + 240), LIFT's top at 760 (the source's `hl`), and a
    // note half the visible time ahead halfway down the 240 px left.
    assert_eq!(
        rects(&l, 1_300, 500, 200),
        [[100, 620, 50, 20], [20, 280, 430, 240], [20, 760, 430, 120]]
    );
}

#[test]
fn offset_four_elements_off_the_lanes_stay_decorative() {
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
        assert!(
            r.notes
                .iter()
                .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::RuntimeOffset(4)),
            "{d}: {:?}",
            r.notes
        );
        assert_eq!(rects(&l, 5_000, 500, 0), Vec::<[i32; 4]>::new(), "{d}");
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

#[test]
fn an_unknown_span_keeps_offset_four_critical() {
    // A full cover before the notes, past destinations naming nothing,
    // reaches the lanes: no SUDDEN+ maps before them.
    let early = cover("");
    for none in [
        r#"{"dst": [{"w": 1, "h": 1}]}"#,
        r#"{"id": "nothing", "dst": [{"w": 1, "h": 1}]}"#,
    ] {
        refused(
            import_body(&body("", &note(""), &[none, &early, NOTES])),
            CriticalPart::Cover,
            "offset",
        );
    }
    // Lanes that cannot be read ahead leave the span unknown.
    let all = common::lanes::lane_rects(200, 600);
    let seven: Vec<&str> = all.rsplitn(2, "}, ").collect();
    let seven = seven[1].to_owned() + "}";
    let notes = common::lanes::note_with(&seven, "");
    let off = deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#);
    refused(
        import_body(&body("", &notes, &[&off, NOTES])),
        CriticalPart::Cover,
        "offset",
    );
}

#[test]
fn offset_four_elements_that_may_reach_the_lanes_refuse() {
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
    for d in &cases {
        let l = import_config(
            [1_920, 1_080],
            &body("", &format!("{header}, {}", note("")), &[NOTES, d]),
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
        assert_eq!(critical(l).0, CriticalPart::Cover, "{d}");
    }
    // Frames selected by an expression: an image group and image sets.
    let selectors = r#""imageset": [{"id": "byref", "images": ["cover"], "ref": 1},
                                    {"id": "byvalue", "images": ["cover"], "value": 71}]"#;
    let group = r#"{"id": "group", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2}"#;
    for (id, field) in [("byref", "ref"), ("byvalue", "value"), ("group", "len")] {
        let over = cover("").replace(r#""id": "cover""#, &format!(r#""id": "{id}""#));
        let off = deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#)
            .replace(r#""id": "deco""#, &format!(r#""id": "{id}""#));
        let objects = format!("{}, {selectors}", note(""));
        let l = import_body(&body(group, &objects, &[NOTES, &over]));
        assert_eq!(
            critical(l),
            (CriticalPart::Cover, NoteKind::UnsupportedField { field }),
            "{id}"
        );
        let l = import_body(&body(group, &objects, &[NOTES, &off]));
        assert!(
            l.report()
                .notes
                .iter()
                .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::RuntimeOffset(4)),
            "{id}"
        );
    }
    // A number listing offset 4.
    let number = r#""value": [{"id": "num", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3, "ref": 71}]"#;
    let num = r#"{"id": "num", "offset": 4, "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    assert_eq!(
        critical(with(number, &[num])),
        (CriticalPart::Cover, NoteKind::RuntimeOffset(4))
    );
}

#[test]
fn only_an_exact_cover_maps() {
    let at = |rect: &str| cover("").replace(r#""x": 20, "y": 800, "w": 430, "h": 600"#, rect);
    for (d, field) in [
        // Split, one pixel wider, too short, too high.
        (at(r#""x": 20, "y": 800, "w": 215, "h": 600"#), "dst"),
        (at(r#""x": 20, "y": 800, "w": 431, "h": 600"#), "dst"),
        (at(r#""x": 20, "y": 800, "w": 430, "h": 599"#), "dst"),
        (at(r#""x": 20, "y": 801, "w": 430, "h": 600"#), "dst"),
        // Not white or not alpha-blended.
        (cover(r#""blend": 2"#), "dst"),
        (
            cover("").replace(r#""h": 600}"#, r#""h": 600, "a": 200}"#),
            "dst",
        ),
        // Its own timer, conditions or window.
        (cover(r#""timer": 41"#), "dst"),
        (cover(r#""op": [240]"#), "dst"),
        (
            cover(r#""loop": -1"#).replace(
                r#""dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]"#,
                r#""dst": [{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                           {"time": 500, "x": 20, "y": 800, "w": 430, "h": 600}]"#,
            ),
            "dst",
        ),
    ] {
        refused(with("", &[&d]), CriticalPart::Cover, field);
    }
    // A second exact cover, and one before the notes.
    refused(
        with("", &[&cover(""), &cover("")]),
        CriticalPart::Cover,
        "offset",
    );
    refused(
        import_body(&body("", &note(""), &[&cover(""), NOTES])),
        CriticalPart::Cover,
        "offset",
    );
    // Moved lanes.
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
    refused(moved, CriticalPart::Cover, "dst");
}

#[test]
fn the_lanes_blend_ignores_the_outer_blend() {
    // The notes, SUDDEN+ and LIFT draw in the lanes' run: an additive
    // note destination would leave a black cover see-through.
    for blend in [2, 3] {
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
        let keys: Vec<_> = d.batch.runs().iter().map(|r| r.key.blend).collect();
        assert_eq!(keys, [Blend::Alpha], "{blend}");
    }
}

#[test]
fn windows_compare_by_what_they_show() {
    // The lanes looping a still track show for good, as a held cover.
    let notes = r#"{"id": "notes", "loop": 1000, "dst": [
        {"time": 0, "x": 0, "y": 0, "w": 0, "h": 0},
        {"time": 1000, "x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    let l = import_body(&body("", &note(""), &[notes, &cover(r#""loop": 0"#)]));
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 280, 430, 300]]);
}

#[test]
fn a_kept_element_between_notes_and_cover_is_noted() {
    let plain = r#"{"id": "deco", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}"#;
    let l = with(&lift_object(""), &[plain, &cover(""), &lift("")]);
    let order = l
        .report()
        .notes
        .iter()
        .filter(|n| n.kind == d9(LaneApprox::CoverOrder))
        .count();
    assert_eq!(order, 2);
}

#[test]
fn only_an_exact_lift_maps() {
    refused(
        with(
            &lift_object(r#""disapearLine": -1"#).replace(r#""disapearLine": 200, "#, ""),
            &[&lift("")],
        ),
        CriticalPart::Lift,
        "disapearLine",
    );
    for extra in [
        r#""disapearLine": 201"#,
        r#""isDisapearLineLinkLift": true"#,
    ] {
        let obj = lift_object(extra).replace(r#""disapearLine": 200, "#, "");
        let obj = if extra.contains("Link") {
            lift_object(extra)
        } else {
            obj
        };
        refused(with(&obj, &[&lift("")]), CriticalPart::Lift, "disapearLine");
    }
    assert_eq!(
        critical(with(&lift_object(""), &[&lift(r#""offset": 5"#)])),
        (CriticalPart::Lift, NoteKind::RuntimeOffset(5))
    );
    refused(
        with(
            &lift_object(""),
            &[&lift("").replace(r#""h": 200"#, r#""h": 199"#)],
        ),
        CriticalPart::Lift,
        "dst",
    );
    // Before the notes.
    refused(
        import_body(&body(
            "",
            &format!("{}, {}", note(""), lift_object("")),
            &[&lift(""), NOTES],
        )),
        CriticalPart::Lift,
        "liftCover",
    );
    // A user offset on it.
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
                    y: 1,
                    ..OffsetValue::default()
                },
            );
        },
    );
    refused(l, CriticalPart::Lift, "dst");
    // A second LIFT: the same destination again, or another object.
    refused(
        with(&lift_object(""), &[&lift(""), &lift("")]),
        CriticalPart::Lift,
        "liftCover",
    );
    let two = lift_object("").replace(
        r#""disapearLine": 200}]"#,
        r#""disapearLine": 200}, {"id": "lift2", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8, "disapearLine": 200}]"#,
    );
    let second = lift("").replace("liftc", "lift2");
    refused(
        with(&two, &[&lift(""), &second]),
        CriticalPart::Lift,
        "liftCover",
    );
}

#[test]
fn hidden_plus_is_never_drawn() {
    let images = with("", &[]).report().usage.images;
    for (obj, d) in [
        (
            r#"{"id": "hid", "src": "a"}"#,
            r#"{"id": "hid", "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
        ),
        (
            r#"{"id": "hid", "src": "a"}"#,
            r#"{"id": "hid", "dst": [{"time": 0, "x": 20, "y": 200, "w": 430, "h": 600, "r": 0}, {"time": 100, "x": 20, "y": 200, "w": 430, "h": 600}]}"#,
        ),
        (
            r#"{"id": "hid", "src": "a"}"#,
            r#"{"id": "hid", "timer": 41, "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
        ),
        (
            r#"{"id": "hid", "src": "a"}"#,
            r#"{"id": "hid", "offsets": [5], "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
        ),
        // A missing source still imports.
        (
            r#"{"id": "hid", "src": "nothing"}"#,
            r#"{"id": "hid", "dst": [{"x": 20, "y": 200, "w": 430, "h": 600}]}"#,
        ),
    ] {
        let l = with(&format!(r#""hiddenCover": [{obj}]"#), &[d]);
        let r = l.report();
        assert!(
            r.notes.iter().any(|n| n.severity == Severity::Ignored
                && n.kind == NoteKind::Deviation(DeviationId::D11)),
            "{d}"
        );
        assert_eq!(r.usage.images, images, "{d}");
        assert_eq!(rects(&l, 5_000, 0, 0), Vec::<[i32; 4]>::new(), "{d}");
    }
}
