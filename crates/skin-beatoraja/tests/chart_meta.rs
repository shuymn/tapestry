//! The chart's metadata a play skin reads each frame (docs/stage3.md
//! "Ops and conditions"): its difficulty through ops 150 to 155, whether
//! it has a BGA through 170, its judgment class through 180 to 184 and its
//! song total through number 368, from the play context, and while loading
//! the difficulty and the BGA flag from the frame's chosen chart. JSON and
//! Lua alike.

mod common;

use bms_model::ChartBuilder;
use common::play::{Drawn, Loader};
use common::{import_body, write};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, LuaImported, read_lua_header};
use skin_model::ChartMetadata;

/// An image drawn `w` wide under each op of `ops`, by number or name.
fn drawn_under(ops: &[(&str, i32)]) -> String {
    let dsts: Vec<String> = ops
        .iter()
        .map(|(op, w)| format!(r#"{{"id": "i", "op": [{op}], "dst": [{{"w": {w}, "h": 10}}]}}"#))
        .collect();
    format!(
        r#""source": [{{"id": "a", "path": "a.png"}}],
           "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{}]"#,
        dsts.join(", ")
    )
}

fn widths(rects: &[[i32; 4]]) -> Vec<i32> {
    let mut w: Vec<i32> = rects.iter().map(|r| r[2]).collect();
    w.sort_unstable();
    w
}

/// The widths drawn over a chart of metadata `meta`.
fn drawn(body: &str, meta: ChartMetadata) -> Vec<i32> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut d = Drawn::over_meta(import_body(body).ok().skin.clone(), &chart, meta);
    widths(&d.rects(0))
}

/// Ops 150 to 155 hold for the difficulty 0 to 5 alone, 150 for 0 (none
/// or one outside 1 to 5); none holds while the difficulty is unknown,
/// where every negation does.
#[test]
fn difficulty_ops_read_the_charts_difficulty() {
    let body = drawn_under(&[
        ("150", 10),
        ("151", 11),
        ("\"chart_difficulty_2\"", 12),
        ("153", 13),
        ("154", 14),
        ("155", 15),
        ("-150", 20),
        ("-153", 21),
    ]);
    let meta = |difficulty| ChartMetadata {
        difficulty,
        ..ChartMetadata::UNKNOWN
    };
    assert_eq!(drawn(&body, meta(Some(0))), [10, 21]);
    for d in 1..=5_u8 {
        let mut want = vec![10 + i32::from(d), 20];
        if d != 3 {
            want.push(21);
        }
        want.sort_unstable();
        assert_eq!(drawn(&body, meta(Some(d))), want, "{d}");
    }
    assert_eq!(drawn(&body, meta(None)), [20, 21]);
    // Out of the model's range: unknown.
    assert_eq!(drawn(&body, meta(Some(6))), [20, 21]);
    let report = import_body(&body).ok().report.clone();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

/// 170 holds while the chart has no BGA and the flag is known; its
/// negation draws otherwise.
#[test]
fn op_170_reads_the_bga_flag() {
    let body = drawn_under(&[("170", 10), ("\"!song_no_bga\"", 11)]);
    let meta = |bga| ChartMetadata {
        bga,
        ..ChartMetadata::UNKNOWN
    };
    assert_eq!(drawn(&body, meta(Some(false))), [10]);
    assert_eq!(drawn(&body, meta(Some(true))), [11]);
    assert_eq!(drawn(&body, meta(None)), [11]);
}

/// Number 368 shows the song total with no note, by id and name, and
/// nothing while it is unknown or what a number reserves.
#[test]
fn number_368_shows_the_song_total() {
    for prop in [r#""ref": 368"#, r#""value": "chart_totalgauge""#] {
        let loaded = import_body(&format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                          {prop}}}],
               "destination": [{{"id": "v", "dst": [{{"w": 12, "h": 20}}]}}]"#
        ));
        assert!(loaded.report().notes.is_empty(), "{prop}");
        let chart = ChartBuilder::new(150_000).build().unwrap();
        let digits = |total| {
            let meta = ChartMetadata {
                total,
                ..ChartMetadata::UNKNOWN
            };
            let mut d = Drawn::over_meta(loaded.ok().skin.clone(), &chart, meta);
            d.rects(0).len()
        };
        assert_eq!(digits(Some(300)), 3, "{prop}");
        assert_eq!(digits(Some(0)), 1, "{prop}");
        assert_eq!(digits(Some(1_500)), 4, "{prop}");
        for unknown in [None, Some(-1), Some(i32::MAX)] {
            assert_eq!(digits(unknown), 0, "{prop} {unknown:?}");
        }
    }
}

/// Ops 180 to 184 hold for the class 0 to 4 alone, by id and by name,
/// with no note: the class is the reference's, from the chart's own rank.
/// None holds for a chart in no class or an unknown class, where each
/// negation does.
#[test]
fn class_ops_read_the_charts_class() {
    let body = drawn_under(&[
        ("180", 10),
        ("\"chart_judge_hard\"", 11),
        ("182", 12),
        ("183", 13),
        ("\"chart_judge_veryeasy\"", 14),
        ("-180", 20),
        ("\"!chart_judge_easy\"", 21),
    ]);
    let meta = |judge_class| ChartMetadata {
        judge_class,
        ..ChartMetadata::UNKNOWN
    };
    for c in 0..=4_u8 {
        let mut want = vec![10 + i32::from(c)];
        if c != 0 {
            want.push(20);
        }
        if c != 3 {
            want.push(21);
        }
        want.sort_unstable();
        assert_eq!(drawn(&body, meta(Some(c))), want, "{c}");
    }
    assert_eq!(drawn(&body, meta(None)), [20, 21]);
    // Out of the model's range: none.
    assert_eq!(drawn(&body, meta(Some(5))), [20, 21]);
    let report = import_body(&body).ok().report.clone();
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

/// While loading, the difficulty and the BGA flag are the frame's chosen
/// chart's, none holding without one; the song total and the class are
/// unavailable, so 368, a class condition and its negation draw nothing.
#[test]
fn loading_reads_the_chosen_charts_difficulty_and_bga() {
    let body = format!(
        r#"{},
           "value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                       "ref": 368}}]"#,
        drawn_under(&[
            ("152", 10),
            ("-152", 11),
            ("170", 12),
            ("-170", 13),
            ("182", 14),
            ("-182", 15),
        ])
    )
    .replace(
        r#""destination": ["#,
        r#""destination": [{"id": "v", "dst": [{"x": 500, "w": 12, "h": 20}]}, "#,
    );
    let mut l: Loader = import_body(&body).loading();
    assert_eq!(widths(&l.rects(0)), [] as [i32; 0]);
    l.difficulty = Some(2);
    l.bga = Some(false);
    assert_eq!(widths(&l.rects(10)), [10, 12]);
    l.difficulty = Some(0);
    l.bga = Some(true);
    assert_eq!(widths(&l.rects(20)), [11, 13]);
    l.difficulty = None;
    assert_eq!(widths(&l.rects(30)), [13]);
}

/// A Lua callback's `option(152)`, `option(-170)` and `option(181)` and
/// its `number(368)` lower and read the same fields at frame time.
#[test]
fn lua_callbacks_read_the_same_metadata() {
    let d = tempfile::tempdir().unwrap();
    write(
        d.path(),
        "s.luaskin",
        b"local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
          if not skin_config then return skin end\n\
          local ms = require(\"main_state\")\n\
          skin.source = { { id = \"a\", path = \"a.png\" } }\n\
          skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
          skin.destination = {\n\
            { id = \"i\", draw = function() return ms.option(152) end, dst = { { w = 10, h = 10 } } },\n\
            { id = \"i\", draw = function() return ms.option(-170) end, dst = { { w = 11, h = 10 } } },\n\
            { id = \"i\", draw = function() return ms.number(368) >= 300 end, dst = { { w = 12, h = 10 } } },\n\
            { id = \"i\", draw = function() return ms.option(181) end, dst = { { w = 13, h = 10 } } } }\n\
          return skin\n",
    );
    common::png(d.path(), "a.png", 64, 64);
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let i: LuaImported = common::lua_play(&h, &Config::default(), &cancel).unwrap();
    assert_eq!(i.callbacks, [Disposition::Lowered; 4]);
    let chart = ChartBuilder::new(150_000).build().unwrap();
    for (meta, want) in [
        (
            ChartMetadata {
                difficulty: Some(2),
                judge_class: Some(1),
                total: Some(300),
                bga: Some(true),
            },
            vec![10, 11, 12, 13],
        ),
        (
            ChartMetadata {
                difficulty: Some(3),
                judge_class: Some(2),
                total: Some(299),
                bga: Some(false),
            },
            vec![],
        ),
        (ChartMetadata::UNKNOWN, vec![11]),
    ] {
        let i = common::lua_play(&h, &Config::default(), &cancel).unwrap();
        let mut p = Drawn::over_meta(i.imported.skin, &chart, meta);
        assert_eq!(widths(&p.rects(0)), want, "{meta:?}");
    }
}
