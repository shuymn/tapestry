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
use common::lanes::{NOTES, body, cover, lift, lift_object, note, with};
use common::play::{Drawn, MS};
use common::{Loaded, has, import_body, import_config, import_sized};
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue,
    PropertyKind, Severity, TimerWhy,
};
use skin_beatoraja::{ImportLimit, Object, Usage};
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

/// An image record whose source does not exist: a critical element
/// drawing it refuses, a decorative one is dropped.
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

#[test]
fn lift_and_sudden_plus_share_the_lane() {
    let l = with(&lift_object(""), &[&cover(""), &lift("")]);
    assert_eq!(added_notes(&l), []);
    // LIFT 200 ‰ takes 120 px, raising the line to 750; SUDDEN+ 500 ‰
    // takes half of the 480 px left: its paint's lower edge at 520 (the
    // source's 1080 − 800 + 240), LIFT's top at 760 (the source's `hl`),
    // and a note half the visible time ahead halfway down the 240 px
    // left. The paint is drawn whole, LIFT cut to the lanes.
    assert_eq!(
        rects(&l, 1_300, 500, 200),
        [[100, 620, 50, 20], [20, -80, 430, 600], [20, 760, 430, 120]]
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
    let missing = (CriticalPart::Cover, NoteKind::MissingAsset);
    // A full cover before the notes, past destinations naming nothing,
    // reaches the lanes: SUDDEN+'s paint, whose missing file refuses.
    let early = ghostly(&cover(""));
    for none in [
        r#"{"dst": [{"w": 1, "h": 1}]}"#,
        r#"{"id": "nothing", "dst": [{"w": 1, "h": 1}]}"#,
    ] {
        let l = import_body(&body(GHOST, &note(""), &[none, &early, NOTES]));
        assert_eq!(critical(l), missing, "{none}");
    }
    // Lanes that cannot be read ahead leave the span unknown.
    let all = common::lanes::lane_rects(200, 600);
    let seven: Vec<&str> = all.rsplitn(2, "}, ").collect();
    let seven = seven[1].to_owned() + "}";
    let notes = common::lanes::note_with(&seven, "");
    let off = ghostly(&deco("", r#"{"x": 0, "y": 0, "w": 10, "h": 10}"#));
    let l = import_body(&body(GHOST, &notes, &[&off, NOTES]));
    assert_eq!(critical(l), missing);
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
    // Each is SUDDEN+'s paint, play-critical: drawing a missing file, it
    // refuses.
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
        assert_eq!(critical(l).0, CriticalPart::Cover, "{d}");
    }
    // Frames selected by an expression: an image group and image sets.
    // A `ref` is an image-index property, 40 the shown gauge's type and 42
    // one the model cannot read; a `value` of 40 is no value property, so
    // the `ref` chooses. Over the lanes each is SUDDEN+'s paint showing
    // its choice, but the unreadable one refuses; off them each draws but
    // the unreadable one.
    let selectors = r#""imageset": [{"id": "byref", "images": ["cover"], "ref": 42},
                                    {"id": "byvalue", "images": ["cover"], "value": 71},
                                    {"id": "bykind", "images": ["cover"], "value": 40, "ref": 40}]"#;
    let group = r#"{"id": "group", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2},
                   {"id": "kindgroup", "src": "a", "w": 16, "h": 16, "divx": 2, "len": 2, "ref": 40}"#;
    let unreadable = NoteKind::UnsupportedProperty {
        kind: PropertyKind::ImageIndex,
        id: Some(42),
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
    // Play-critical, it may not depend on when the state started
    // without a timer of its own (D3): a window ending is refused.
    let ending = cover(r#""loop": -1"#).replace(
        r#""dst": [{"x": 20, "y": 800, "w": 430, "h": 600}]"#,
        r#""dst": [{"time": 0, "x": 20, "y": 800, "w": 430, "h": 600},
                       {"time": 500, "x": 20, "y": 800, "w": 430, "h": 600}]"#,
    );
    assert_eq!(
        critical(with("", &[&ending])),
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

#[test]
fn the_lanes_blend_ignores_the_outer_blend() {
    // The notes and LIFT draw in the lanes' run, SUDDEN+'s paint in its
    // own alpha-blended one: an additive note destination would leave a
    // black cover see-through.
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

/// The rows of its frame the one quad drawn over `rect` keeps, from its
/// uv's aspect against the frame's `width` texels (the atlas pages are
/// square).
fn rows_at(l: &Loaded, rect: [f32; 4], width: f32) -> f32 {
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
    let [u0, v0, u1, v1] = q[0].uv;
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

#[test]
fn other_stretches_refuse_the_covers() {
    // SUDDEN+'s paint refuses the other trimming stretches; LIFT every
    // stretch that changes its frame.
    for mode in [1, 2, 3, 6, 7, 8, 9, 10] {
        let want = NoteKind::Stretch(mode);
        let paint = tall(WIDE, &[&stretched("cw", mode)]);
        if matches!(mode, 3 | 7 | 10) {
            assert_eq!(critical(paint), (CriticalPart::Cover, want), "{mode}");
        } else {
            paint.ok();
        }
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
fn a_slider_range_scales_to_the_reference_height() {
    // ⌊1080 × range / height⌋: 499.5 truncates to 499 on a 720 px skin;
    // 1 000 ‰ moves it exactly that far.
    let (object, d) = slider(r#", "type": 4, "angle": 2, "range": 333"#, "");
    let l = import_sized(
        [1_280, 720],
        &body("", &format!("{}, {object}", note("")), &[NOTES, &d]),
        &[],
    );
    // The paint is drawn last, after any note 1 000 ‰ uncovers.
    let at = |c: u16| rects(&l, 5_000, c, 0).last().unwrap()[1];
    assert_eq!(at(1_000) - at(0), 499);
    // Truncated once at the reference height (D1): range 5 of a 720 px
    // skin is 7 reference pixels, and 3/8 of the lane moves it 2.625,
    // 1.75 pixels of a 1 280×720 target; the source keeps 5 at that
    // resolution and moves it 1.875.
    let (object, d) = slider(r#", "type": 4, "angle": 2, "range": 5"#, "");
    let l = import_sized(
        [1_280, 720],
        &body("", &format!("{}, {object}", note("")), &[NOTES, &d]),
        &[],
    );
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut drawn = Drawn::over(l.ok().skin.clone(), &chart);
    let mut y = |c: u16| {
        drawn.view.cover = c;
        drawn.frame(1_000, &[]).last().unwrap().rect[1]
    };
    let (still, moved) = (y(0), y(375));
    assert!((moved - still - 2.625).abs() < 1e-3, "{still} {moved}");
    // A range beyond the coordinates refuses.
    let (object, d) = slider(r#", "type": 4, "angle": 2, "range": 2000000000"#, "");
    match with(&object, &[&d]).err() {
        ImportError::Range { field, .. } => assert_eq!(field, "range"),
        e => panic!("{e:?}"),
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
    assert_eq!(
        critical(ghost(r#"{"x": 20, "y": 800, "w": 430, "h": 600}"#)),
        (CriticalPart::Cover, NoteKind::MissingAsset)
    );
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
fn selecting_paint_may_not_depend_on_the_state_start() {
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
    // An ending window, a moving track and an untimed cycle refuse, for
    // an image set and an image group alike.
    for (images, fields, keys) in [
        (r#""cover""#, r#", "loop": -1"#, ending),
        (r#""cover""#, "", moving),
        (r#""cy", "cover""#, "", OVER),
    ] {
        let l = cover_set("", cycling, images, fields, keys);
        assert_eq!(critical(l), untimed, "{images} {fields} {keys}");
        // With a timer of their own each imports.
        let timed = format!(r#", "timer": 41{fields}"#);
        cover_set("", cycling, images, &timed, keys).ok();
    }
    assert_eq!(critical(group_over("")), untimed);
    group_over(r#", "timer": 41"#).ok();
    // A static, perpetual one without a cycle imports untimed.
    cover_set("", "", r#""cover", "cover""#, "", OVER).ok();
}
