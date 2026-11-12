//! The cursor's chart's images on a selection skin (S10-12): a destination
//! of system image 100 or 102 becomes a chart image, its stretch kept for
//! placement; 101 is dropped as an unsupported system object, and the
//! trimmed stretches are dropped as on play. Options 190 to 193 read the
//! selection's image fields (`selected.stagefile`, `selected.banner`) with
//! no guard, in a JSON `op` and a Lua `main_state.option` alike.

mod common;

use common::play::round;
use common::selection::{SET, Sel, TEXT_T, chart, folder, import_selection, minimal, view};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, ImportFacts, LuaOutcome, NoteKind, Object, read_lua_header};
use skin_model::FrameUv;
use skin_model::ScreenKind;
use skin_model::chart_image::{ChartKey, ChartStretch, ChartTable, ChartUv};

const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

/// A 640×480 stage file and a 300×80 banner in layer 9.
const STAGE: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.0, 0.156_25, 0.117_187_5],
        layer: 9,
    },
    native: [640, 480],
};
const BANNER: ChartUv = ChartUv {
    uv: FrameUv {
        uv: [0.0, 0.5, 0.073_242_19, 0.519_531_25],
        layer: 9,
    },
    native: [300, 80],
};

fn table(stage: bool, banner: bool) -> ChartTable {
    ChartTable::new(stage.then_some(STAGE), None, banner.then_some(BANNER)).keyed(KEY)
}

/// The model rect (y down, 1080 high) of a reference rect (y up).
fn model([x, y, w, h]: [i32; 4]) -> [f32; 4] {
    [x as f32, (1_080 - y - h) as f32, w as f32, h as f32]
}

/// A mark of the fixture's dot under `op` at x `x`, y 500.
fn mark(op: &str, x: i32) -> String {
    format!(r#"{{"id": "dot", "op": [{op}], "dst": [{{"x": {x}, "y": 500, "w": 8, "h": 8}}]}}"#)
}

/// `s` drawn on a chart row with `table` set, its fields written as the
/// player writes them: the x of each mark at y 500, sorted, and the rects
/// drawn from each image.
fn frame(s: &mut Sel, table: Option<ChartTable>) -> (Vec<i32>, Vec<[i32; 4]>, Vec<[i32; 4]>) {
    s.view(&view(vec![chart("a")], Some(0), None, None));
    s.sel.set_chart_live(Some(KEY));
    s.sel.set_chart_images(table);
    s.sel.write_chart_fields();
    let quads = s.frame(0);
    let mut marks: Vec<i32> = quads
        .iter()
        .map(|q| round(q.rect))
        .filter(|q| q[1] == 1_080 - 500 - 8 && q[0] >= 1_000)
        .map(|q| q[0])
        .collect();
    marks.sort_unstable();
    let of = |uv: ChartUv| {
        quads
            .iter()
            .filter(|q| q.uv == uv.uv.uv && q.layer == 9)
            .map(|q| round(q.rect))
            .collect()
    };
    (marks, of(STAGE), of(BANNER))
}

/// The shapes of real selection skins lower: a banner (−102) and stage
/// files (−100) of stretch 1 and 4, and a stage file under 191 with the
/// default stretch, each drawing from the table where it places it; −101
/// is dropped with one system note and a trimmed stretch (3) with its
/// note. 190 to 193 (and a negated 191) read the fields.
#[test]
fn selection_chart_images_lower_and_read_the_table() {
    let dsts = format!(
        r#"{{"id": "-102", "stretch": 1, "dst": [{{"x": 100, "y": 800, "w": 600, "h": 160}}]}},
           {{"id": "-100", "stretch": 1, "dst": [{{"x": 100, "y": 200, "w": 640, "h": 400}}]}},
           {{"id": "-100", "stretch": 4, "dst": [{{"x": 800, "y": 200, "w": 400, "h": 400}}]}},
           {{"id": "-100", "op": [191], "dst": [{{"x": 1300, "y": 200, "w": 320, "h": 240}}]}},
           {{"id": "-101", "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}},
           {{"id": "-100", "stretch": 3, "dst": [{{"x": 0, "y": 0, "w": 10, "h": 10}}]}},
           {}, {}, {}, {}, {}"#,
        mark("190", 1_000),
        mark("191", 1_010),
        mark("192", 1_020),
        mark("193", 1_030),
        mark("-191", 1_040),
    );
    let body = minimal("").replacen(
        r#"{"id": "list", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#,
        &format!(r#"{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}, {dsts}"#),
        1,
    );
    let d = folder(&body);
    let i = import_selection(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut notes = common::kinds(&i.report);
    notes.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(
        notes,
        [
            NoteKind::Stretch(3),
            NoteKind::Unsupported {
                object: Object::System
            },
        ]
    );
    assert!(i.skin.has_chart_images());
    let mut s = Sel::new(i.skin);

    let (marks, stage, banner) = frame(&mut s, Some(table(true, true)));
    assert_eq!(marks, [1_010, 1_030]);
    let at = |stretch: ChartStretch, r: [i32; 4], native| round(stretch.place(model(r), native).0);
    let mut want = vec![
        at(ChartStretch::Inner, [100, 200, 640, 400], STAGE.native),
        at(ChartStretch::FitWidth, [800, 200, 400, 400], STAGE.native),
        at(ChartStretch::Fill, [1300, 200, 320, 240], STAGE.native),
    ];
    let mut stage = stage;
    stage.sort_unstable();
    want.sort_unstable();
    assert_eq!(stage, want);
    assert_eq!(
        banner,
        [at(ChartStretch::Inner, [100, 800, 600, 160], BANNER.native)]
    );

    let (marks, stage, banner) = frame(&mut s, Some(table(false, true)));
    assert_eq!(marks, [1_000, 1_030, 1_040]);
    assert!(stage.is_empty());
    assert_eq!(banner.len(), 1);

    // Not current: 190 and 192 hold, and nothing draws.
    let (marks, stage, banner) = frame(&mut s, None);
    assert_eq!(marks, [1_000, 1_020, 1_040]);
    assert!(stage.is_empty() && banner.is_empty());
}

/// The Lua skin's reads of 191 (`main_state.option(191)`, `option(-191)`
/// and `not option(191)`) and 193 read the same fields as the JSON `op`s.
#[test]
fn the_image_options_read_the_selection_fields_in_lua() {
    let draw = |x: i32, cond: &str| {
        format!(
            "{{ id = \"dot\", draw = function() return {cond} end,\n\
               dst = {{ {{ x = {x}, y = 500, w = 8, h = 8 }} }} }}"
        )
    };
    let dsts = [
        draw(1_000, "ms.option(191)"),
        draw(1_010, "ms.option(-191)"),
        draw(1_020, "not ms.option(191)"),
        draw(1_030, "ms.option(193)"),
        "{ id = -100, stretch = 1, dst = { { x = 100, y = 200, w = 640, h = 400 } } }".to_owned(),
        "{ id = -102, stretch = 1, dst = { { x = 100, y = 800, w = 600, h = 160 } } }".to_owned(),
    ]
    .join(",\n");
    let entry = format!(
        "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {} }}\n\
         skin.imageset = {{ {{ id = \"set\", images = {{ \"b0\", \"b1\", \"b0\", \"b0\", \"b0\", \"b5\" }} }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10 }} }}\n\
         skin.songlist = {{ id = \"list\", center = 0,\n\
           liston = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           listoff = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           text = {{ {{ id = \"t\", dst = {{ {{ x = 0, y = 0, w = 100, h = 10 }} }} }} }} }}\n\
         skin.destination = {{\n\
           {{ id = \"list\", dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }},\n\
           {dsts} }}\n\
         return skin\n",
        common::selection::lua_images(),
    );
    let l = folder("");
    common::write(l.path(), "s.luaskin", entry.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(l.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    let lua = match h
        .import(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap_or_else(|e| panic!("{e:?}"))
    {
        LuaOutcome::Imported(i) => i,
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    };
    assert!(
        lua.callbacks.iter().all(|d| *d == Disposition::Lowered),
        "{:?}",
        lua.callbacks
    );
    assert!(lua.imported.skin.has_chart_images());
    let json = format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}],
           "songlist": {{"id": "list", "center": 0,
               "liston": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "listoff": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "text": [{{"id": "t", "dst": [{{"x": 0, "y": 0, "w": 100, "h": 10}}]}}]}},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}},
               {}, {}, {}, {}]"#,
        mark("191", 1_000),
        mark("-191", 1_010),
        mark("190", 1_020),
        mark("193", 1_030),
    );
    let j = folder(&json);
    let json = import_selection(j.path()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut lua = Sel::new(lua.imported.skin);
    let mut json = Sel::new(json.skin);
    for (t, want) in [
        (Some(table(true, false)), vec![1_000]),
        (Some(table(false, true)), vec![1_010, 1_020, 1_030]),
        (None, vec![1_010, 1_020]),
    ] {
        assert_eq!(frame(&mut lua, t).0, want, "lua {t:?}");
        assert_eq!(frame(&mut json, t).0, want, "json {t:?}");
    }
    let (_, stage, banner) = frame(&mut lua, Some(table(true, true)));
    assert_eq!((stage.len(), banner.len()), (1, 1));
}
