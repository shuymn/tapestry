//! SUDDEN+ and LIFT: which elements become the covers, which
//! offset-4 elements stay decorative, and what each refuses, through
//! `build`.
//!
//! The default lanes (r = 1, y 200, h 600, size 20): top 270, line 870,
//! bottom 880, SUDDEN+ anchored at 280, LIFT at 880, a 600 px lane over
//! x [20, 450].

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::lanes::{NOTES, body, cover, lift, lift_object, note, with};
use common::play::{Drawn, MS};
use common::{Loaded, has, import_body, import_config, import_sized};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue,
    PropertyKind, Severity,
};
use skin_beatoraja::{ImportLimit, Usage};
use skin_model::Blend;
use skin_model::MAX_IMAGES;

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
    // 500 ‰ of the lane: the cover's lower edge at 280 + 300, the cover
    // 600 high above it and cut at the lanes' top.
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 270, 430, 310]]);
    // At 0 ‰ its lower edge is the anchor: what shows is the strip
    // between the lanes' top and the anchor; 10 ‰ lowers it by 6.
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, 270, 430, 10]]);
    assert_eq!(rects(&l, 5_000, 10, 0), [[20, 270, 430, 16]]);
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

#[test]
fn lift_and_sudden_plus_share_the_lane() {
    let l = with(&lift_object(""), &[&cover(""), &lift("")]);
    assert!(!has(l.report(), d9(LaneApprox::CoverOrder)));
    // LIFT 200 ‰ takes 120 px, raising the line to 750; SUDDEN+ 500 ‰
    // takes half of the 480 px left: its edge at 520 (the source's
    // 1080 − 800 + 240), LIFT's top at 760 (the source's `hl`), and a
    // note half the visible time ahead halfway down the 240 px left.
    // Each cover keeps its own height, cut to the lanes.
    assert_eq!(
        rects(&l, 1_300, 500, 200),
        [[100, 620, 50, 20], [20, 270, 430, 250], [20, 760, 430, 120]]
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
    // A `ref` is an image-index property, 40 the shown gauge's type and 42
    // one the model cannot read; a `value` of 40 is no value property, so
    // the `ref` chooses. Off the lanes each draws but the unreadable one.
    let selectors = r#""imageset": [{"id": "byref", "images": ["cover"], "ref": 42},
                                    {"id": "byvalue", "images": ["cover"], "value": 71},
                                    {"id": "bykind", "images": ["cover"], "value": 40, "ref": 40}]"#;
    let group = r#"{"id": "group", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2},
                   {"id": "kindgroup", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}"#;
    let unreadable = NoteKind::UnsupportedProperty {
        kind: PropertyKind::ImageIndex,
        id: Some(42),
    };
    for (id, field, why) in [
        ("byref", "ref", vec![unreadable]),
        ("byvalue", "value", vec![]),
        ("bykind", "image selection", vec![]),
        ("group", "len", vec![]),
        ("kindgroup", "image selection", vec![]),
    ] {
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
    // An image set whose `ref` is no image-index property selects nothing:
    // SUDDEN+ by its first image.
    let null = r#""imageset": [{"id": "s", "images": ["cover"], "ref": 1}]"#;
    let over = cover("").replace(r#""id": "cover""#, r#""id": "s""#);
    let l = with(null, &[&over]);
    assert!(has(l.report(), d9(LaneApprox::CoverAbove)));
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 270, 430, 310]]);
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
fn only_an_exact_cover_maps() {
    let at = |rect: &str| cover("").replace(r#""x": 20, "y": 800, "w": 430, "h": 600"#, rect);
    for (d, field) in [
        // Split, one pixel wider, without height, too high.
        (at(r#""x": 20, "y": 800, "w": 215, "h": 600"#), "dst"),
        (at(r#""x": 20, "y": 800, "w": 431, "h": 600"#), "dst"),
        (at(r#""x": 20, "y": 800, "w": 430, "h": 0"#), "dst"),
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
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 270, 430, 310]]);
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

/// A SUDDEN+ destination of image `id` with stretch `mode` over the
/// default lanes.
fn stretched(id: &str, mode: i32) -> String {
    cover(&format!(r#""stretch": {mode}"#)).replace(r#""id": "cover""#, &format!(r#""id": "{id}""#))
}

/// The rows of its frame SUDDEN+ keeps at 1 000 ‰ (see [`rows_at`]).
fn rows_drawn(l: &Loaded, width: f32) -> f32 {
    rows_at(l, [20.0, 280.0, 430.0, 600.0], width)
}

/// The rows of its frame the one quad drawn over `rect` keeps, from its
/// uv's aspect against the frame's `width` texels (the atlas pages are
/// square).
fn rows_at(l: &Loaded, rect: [f32; 4], width: f32) -> f32 {
    let chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let mut d = Drawn::over(l.ok().skin.clone(), &chart);
    // At 1 000 ‰ a 600 px SUDDEN+ spans [280, 880], uncut.
    d.view.cover = 1_000;
    let q: Vec<_> = d
        .frame(1_000, &[])
        .into_iter()
        .filter(|q| q.rect == rect)
        .collect();
    assert_eq!(q.len(), 1);
    let [u0, v0, u1, v1] = q[0].uv;
    width * (v1 - v0) / (u1 - u0)
}

const WIDE: &str = r#"{"id": "cw", "src": "a", "x": 0, "y": 24, "w": 43, "h": 30}"#;

#[test]
fn fit_width_covers_take_the_frames_aspect() {
    // A 43×30 frame over 430 px is 300 high, centred on the 600 px
    // destination: its lower edge at 130, 150 above the region's top.
    for mode in [4, 5] {
        let l = tall(WIDE, &[&stretched("cw", mode)]);
        // 500 ‰ lowers it by 300 to 430, cut at the lanes' top.
        assert_eq!(rects(&l, 5_000, 500, 0), [[20, 270, 430, 160]], "{mode}");
        // At 1 000 ‰ it spans 430–730: the tap at its judgment time
        // stays visible below it, on the line.
        assert_eq!(
            rects(&l, 1_000, 1_000, 0),
            [[100, 860, 50, 20], [20, 430, 430, 300]],
            "{mode}"
        );
    }
}

#[test]
fn a_fit_width_cover_takes_its_height_from_the_frame() {
    // A destination with no height: stretch 4 makes the 43×30 frame 300
    // high, centred on the destination's lower edge, 280: from 130 to 430.
    let flat = |h: i32| stretched("cw", 4).replace(r#""h": 600"#, &format!(r#""h": {h}"#));
    let l = tall(WIDE, &[&flat(0)]);
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, 270, 430, 160]]);
    // Without a stretch it has no height, and a negative height refuses.
    let none = cover("").replace(r#""h": 600"#, r#""h": 0"#);
    refused(tall(WIDE, &[&none]), CriticalPart::Cover, "dst");
    refused(tall(WIDE, &[&flat(-1)]), CriticalPart::Cover, "dst");
}

#[test]
fn fit_width_trimmed_covers_crop_the_frames_rows() {
    // A 43×120 frame fit to 430 would be 1 200 high: stretch 5 keeps the
    // 600 px destination and the frame's middle 60 rows (30 to 90).
    let frame = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 43, "h": 120}"#;
    let l = tall(frame, &[&stretched("ct", 5)]);
    assert_eq!(rects(&l, 5_000, 500, 0), [[20, 270, 430, 310]]);
    assert_eq!(rows_drawn(&l, 43.0), 60.0);
    // Stretch 4 keeps every row and fits the height: 1 200 centred on
    // the destination puts its lower edge at 580.
    let l = tall(frame, &[&stretched("ct", 4)]);
    assert_eq!(rects(&l, 5_000, 0, 0), [[20, 270, 430, 310]]);
    // Only a single frame crops: two rows of frames refuse.
    let grid = r#"{"id": "ct", "src": "a", "x": 0, "y": 0, "w": 43, "h": 240, "divy": 2}"#;
    assert_eq!(
        critical(tall(grid, &[&stretched("ct", 5)])),
        (CriticalPart::Cover, NoteKind::Stretch(5))
    );
    // Uncropped, a grid fits as stretch 4 does.
    let grid = grid.replace(r#""h": 240"#, r#""h": 40"#);
    tall(&grid, &[&stretched("ct", 5)]).ok();
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

#[test]
fn other_stretches_refuse_the_covers() {
    for mode in [1, 2, 3, 6, 7, 8, 9, 10] {
        let want = NoteKind::Stretch(mode);
        assert_eq!(
            critical(tall(WIDE, &[&stretched("cw", mode)])),
            (CriticalPart::Cover, want),
            "{mode}"
        );
        let l = with(&lift_object(""), &[&lift(&format!(r#""stretch": {mode}"#))]);
        assert_eq!(critical(l), (CriticalPart::Lift, want), "{mode}");
    }
    // Unknown modes are identity.
    tall(WIDE, &[&stretched("cw", 11)]).ok();
}

#[test]
fn lift_fits_its_width_only_in_place() {
    // The source clips LIFT to its destination before stretching it: a
    // fit that changes the 200 px height refuses. A 16×16 frame over
    // 430 px grows to 430; a 43×10 frame shrinks to 100.
    let lift_mode = |mode: i32| lift(&format!(r#""stretch": {mode}"#));
    let small = lift_object("").replace(r#""w": 16, "h": 16"#, r#""w": 43, "h": 10"#);
    for (object, mode) in [(lift_object(""), 4), (small.clone(), 4), (small, 5)] {
        let l = with(&object, &[&lift_mode(mode)]);
        assert_eq!(
            critical(l),
            (CriticalPart::Lift, NoteKind::Stretch(mode)),
            "{object} {mode}"
        );
    }
    // Stretch 5 crops the 16×16 frame's rows and keeps the destination.
    let l = with(&lift_object(""), &[&lift_mode(5)]);
    assert_eq!(rects(&l, 5_000, 0, 200), [[20, 760, 430, 120]]);
    // A 43×20 frame fits 430×200 exactly and maps.
    let exact = lift_object("").replace(r#""w": 16, "h": 16"#, r#""w": 43, "h": 20"#);
    for mode in [4, 5] {
        let l = with(&exact, &[&lift(&format!(r#""stretch": {mode}"#))]);
        // LIFT 200 ‰ raises its upper edge 120 px to 760.
        assert_eq!(rects(&l, 5_000, 0, 200), [[20, 760, 430, 120]], "{mode}");
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
