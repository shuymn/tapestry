//! The chart's own images on a play skin (S10-12): a destination of
//! system image 100 or 101 becomes a chart image, its keys unbaked and its
//! stretch, colour, blend and filter kept for placement; the trimmed
//! stretches are dropped; and on the other screens they stay dropped.
//! Conditions 190, 191, 194 and 195 read the images' presence, and while
//! loading none of them is evaluated until the images settle.

mod common;

use common::play::{Drawn, Loader, round};
use common::{import_body, kinds};
use skin_beatoraja::{NoteKind, Object, Severity};
use skin_model::chart_image::{ChartImage, ChartKey, ChartStretch, ChartTable, ChartUv};
use skin_model::{Blend, FrameUv};

const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A 64×32 stage file and a 40×80 back image in layer 9.
const STAGE: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.25, 0.125],
        layer: 9,
    },
    native: [64, 32],
};
const BACK: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.5, 0.0, 0.625, 0.25],
        layer: 9,
    },
    native: [40, 80],
};

fn table(stage: bool, back: bool) -> ChartTable {
    ChartTable::new(stage.then_some(STAGE), back.then_some(BACK)).keyed(KEY)
}

/// The quads of `l`'s first frame with `table` published.
fn drawn(skin: skin_model::Skin, table: ChartTable) -> Vec<skin_model::Quad> {
    let mut d = Drawn::new(skin, 0);
    d.play.set_chart_live(Some(KEY));
    d.play.set_chart_images(table);
    d.frame(0, &[])
}

/// The model rect (y down, 1080 high) of a reference rect (y up).
fn model([x, y, w, h]: [i32; 4]) -> [f32; 4] {
    [x as f32, (1_080 - y - h) as f32, w as f32, h as f32]
}

/// Each kept stretch mode becomes its placement stretch: a destination of
/// the back image at (100, 200) 300×120 with a 40×80 image draws where
/// that stretch places it; the colour (64, 128, 192, 200), the additive
/// blend and the filter are kept. Modes 0, −1 and 11 are the destination
/// as given.
#[test]
fn a_chart_image_keeps_its_stretch_colour_blend_and_filter() {
    let rect = [100, 200, 300, 120];
    for (mode, stretch) in [
        (0, ChartStretch::Fill),
        (-1, ChartStretch::Fill),
        (11, ChartStretch::Fill),
        (1, ChartStretch::Inner),
        (2, ChartStretch::Outer),
        (4, ChartStretch::FitWidth),
        (6, ChartStretch::FitHeight),
        (8, ChartStretch::NoExpanding),
        (9, ChartStretch::NoResize),
    ] {
        for filter in [0, 1] {
            let l = import_body(&format!(
                r#""destination": [{{"id": "-101", "blend": 2, "filter": {filter}, "stretch": {mode},
                    "dst": [{{"x": 100, "y": 200, "w": 300, "h": 120, "a": 200, "r": 64, "g": 128, "b": 192}}]}}]"#
            ));
            let r = l.report();
            assert!(r.notes.is_empty(), "{mode}: {:?}", kinds(r));
            assert_eq!(r.usage.images, 0, "a chart image charges no image");
            let quads = drawn(l.ok().skin.clone(), table(false, true));
            let [q] = quads.as_slice() else {
                panic!("{mode}: {quads:?}");
            };
            // 8 and 9 keep the native 40×80, centred in the destination.
            let want = match stretch {
                ChartStretch::NoExpanding | ChartStretch::NoResize => [230.0, 780.0, 40.0, 80.0],
                _ => stretch.place(model(rect), BACK.native).0,
            };
            assert_eq!(round(q.rect), round(want), "mode {mode}");
            assert_eq!(q.uv, BACK.uv.uv);
            assert_eq!(
                q.color,
                [64.0 / 255.0, 128.0 / 255.0, 192.0 / 255.0, 200.0 / 255.0],
                "{mode}"
            );
            let linear = q.filter == skin_model::Filter::Linear;
            assert_eq!(linear, filter == 1, "{mode} {filter}");
        }
    }
}

/// The blend of a chart image is its destination's: 2 additive.
#[test]
fn a_chart_image_keeps_its_blend() {
    let l = import_body(
        r#""destination": [{"id": "-100", "blend": 2, "dst": [{"x": 0, "y": 0, "w": 64, "h": 32}]},
            {"id": "-101", "dst": [{"x": 100, "y": 0, "w": 40, "h": 80}]}]"#,
    );
    let mut d = Drawn::new(l.ok().skin.clone(), 0);
    d.play.set_chart_live(Some(KEY));
    d.play.set_chart_images(table(true, true));
    d.frame(0, &[]);
    let blends: Vec<Blend> = d.batch.runs().iter().map(|r| r.key.blend).collect();
    assert_eq!(blends, [Blend::Add, Blend::Alpha]);
}

/// A chart image's keys are not baked: between two keys its destination
/// moves, and its stretch is applied to the interpolated destination.
#[test]
fn a_chart_image_is_stretched_at_each_sample() {
    let l = import_body(
        r#""destination": [{"id": "-100", "stretch": 1, "dst": [
            {"time": 0, "x": 100, "y": 200, "w": 300, "h": 120},
            {"time": 1000, "x": 140, "y": 220, "w": 200, "h": 160}]}]"#,
    );
    let mut d = Drawn::new(l.ok().skin.clone(), 0);
    d.play.set_chart_live(Some(KEY));
    d.play.set_chart_images(table(true, false));
    let q = d.frame(500, &[]);
    let at = model([120, 210, 250, 140]);
    let want = ChartStretch::Inner.place(at, STAGE.native).0;
    assert_eq!(round(q[0].rect), round(want));
}

/// The trimmed stretches (3, 5, 7, 10) are dropped with a note, as a
/// system image's are; the banner (102) stays unsupported.
#[test]
fn trimmed_stretches_and_the_banner_are_dropped() {
    for mode in [3, 5, 7, 10] {
        let l = import_body(&format!(
            r#""destination": [{{"id": "-101", "stretch": {mode}, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}}]"#
        ));
        assert_eq!(kinds(l.report()), [NoteKind::Stretch(mode)], "{mode}");
        assert!(drawn(l.ok().skin.clone(), table(true, true)).is_empty());
    }
    let l = import_body(
        r#""destination": [{"id": "-102", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}]"#,
    );
    assert_eq!(
        kinds(l.report()),
        [NoteKind::Unsupported {
            object: Object::System
        }]
    );
    assert!(
        l.report()
            .notes
            .iter()
            .all(|n| n.severity == Severity::Dropped)
    );
}

/// On the selection, result and decision screens 100 and 101 stay
/// dropped as unsupported system objects: the skins are admitted.
#[test]
fn chart_images_stay_dropped_off_play() {
    let dsts = r#""destination": [{"id": "-100", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]},
        {"id": "-101", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}]"#;
    let system = NoteKind::Unsupported {
        object: Object::System,
    };
    let ours = r#"{"id": "-100", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]},
        {"id": "-101", "dst": [{"x": 0, "y": 0, "w": 10, "h": 10}]}, "#;
    let selection = common::selection::minimal("").replacen(
        "\"destination\": [",
        &format!("\"destination\": [{ours}"),
        1,
    );
    let s = common::selection::folder(&selection);
    let i = common::selection::import_selection(s.path()).unwrap();
    assert!(!i.skin.has_chart_images());
    assert_eq!(
        i.report.notes.iter().filter(|n| n.kind == system).count(),
        2
    );
    let r = common::result::folder(&common::result::graph_json("", ours.trim_end_matches(", ")));
    let i = common::result::import_result(r.path()).unwrap();
    assert!(!i.skin.has_chart_images());
    assert_eq!(
        i.report.notes.iter().filter(|n| n.kind == system).count(),
        2
    );
    let d = common::decision::folder(dsts);
    let i = common::decision::import_decision(d.path()).unwrap();
    assert!(!i.skin.has_chart_images());
    assert_eq!(
        i.report.notes.iter().filter(|n| n.kind == system).count(),
        2
    );
}

/// Conditions on the images' presence: 191 and 195 while present, 190 and
/// 194 while absent, the negated ops the reverse, 192 still unsupported.
#[test]
fn the_image_conditions_read_the_published_table() {
    let mark = |op: &str, x: i32| {
        format!(r#"{{"id": "a", "op": [{op}], "dst": [{{"x": {x}, "y": 0, "w": 8, "h": 8}}]}}"#)
    };
    let body = format!(
        r#""image": [{{"id": "a", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8}}],
            "destination": [{}, {}, {}, {}, {}, {}]"#,
        mark("190", 0),
        mark("191", 10),
        mark("194", 20),
        mark("195", 30),
        mark("-194", 40),
        mark("-190", 50),
    );
    let l = import_body(&body);
    assert!(l.report().notes.is_empty(), "{:?}", kinds(l.report()));
    let xs = |stage: bool, back: bool| -> Vec<i32> {
        let mut x: Vec<i32> = drawn(l.ok().skin.clone(), table(stage, back))
            .iter()
            .map(|q| round(q.rect)[0])
            .collect();
        x.sort_unstable();
        x
    };
    assert_eq!(xs(true, true), [10, 30, 40, 50]);
    assert_eq!(xs(false, false), [0, 20]);
    assert_eq!(xs(true, false), [10, 20, 50]);
    let l = import_body(
        r#""image": [{"id": "a", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8}],
           "destination": [{"id": "a", "op": [192], "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}]"#,
    );
    assert!(!l.report().notes.is_empty(), "192 stays unsupported");
}

/// While loading, conditions on the images are not evaluated until they
/// settle: the plain, the negated and the Lua `not` forms alike stay
/// hidden while unavailable, then follow the table.
#[test]
fn image_conditions_wait_for_the_images_while_loading() {
    let d = common::skin(&[(
        "s.luaskin",
        "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = { { id = \"a\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
         skin.destination = {\n\
           { id = \"a\", op = { 80, 194 }, dst = { { x = 0, y = 0, w = 8, h = 8 } } },\n\
           { id = \"a\", op = { 80, -194 }, dst = { { x = 10, y = 0, w = 8, h = 8 } } },\n\
           { id = \"a\", draw = function() return not ms.option(194) end, dst = { { x = 20, y = 0, w = 8, h = 8 } } },\n\
           { id = -101, op = { 80 }, dst = { { x = 30, y = 0, w = 40, h = 80 } } },\n\
         }\n\
         return skin\n",
    )]);
    common::png(d.path(), "a.png", 64, 64);
    let skin = common::lua_import(d.path()).unwrap().imported.skin;
    let mut l = Loader::new(skin);
    l.loading.set_chart_live(Some(KEY));
    let xs = |l: &mut Loader| -> Vec<i32> {
        let mut x: Vec<i32> = l.frame(0).iter().map(|q| round(q.rect)[0]).collect();
        x.sort_unstable();
        x
    };
    assert!(xs(&mut l).is_empty(), "unavailable: nothing evaluated");
    l.loading.set_chart_images(Some(table(false, false)));
    assert_eq!(xs(&mut l), [0], "absent: 194 holds");
    l.loading.set_chart_images(Some(table(false, true)));
    assert_eq!(
        xs(&mut l),
        [10, 20, 30],
        "present: −194, not 194, the image"
    );
}
