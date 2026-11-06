//! C19: a Lua host read and a JSON field of the same selection id get the
//! same treatment and draw the same frame (docs/stage3.md S10-09d, D12).

mod common;

use common::selection::{DIGITS, IMAGES, SET, Sel, TEXT_T, best, chart, folder_row, view};
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{
    Cancel, Config, ImportFacts, LuaOutcome, NoteKind, PropertyKind, read_lua_header,
};
use skin_model::ScreenKind;

/// The JSON skin: number 71 by `ref`, number 410, an image under op 2 and
/// one under `draw` 625.
fn json_body() -> String {
    format!(
        r#""imageset": [{SET}], "text": [{TEXT_T}],
           "value": [{{"id": "ex", {DIGITS}, "digit": 4, "ref": 71}},
                     {{"id": "ep", {DIGITS}, "digit": 4, "ref": 410}}],
           "songlist": {{"id": "list", "center": 0,
               "liston": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "listoff": [{{"id": "set", "dst": [{{"x": 0, "y": 900, "w": 300, "h": 40}}]}}],
               "text": [{{"id": "t", "dst": [{{"x": 0, "y": 0, "w": 100, "h": 10}}]}}]}},
           "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}},
               {{"id": "ex", "dst": [{{"x": 0, "y": 100, "w": 4, "h": 8}}]}},
               {{"id": "ep", "dst": [{{"x": 0, "y": 200, "w": 4, "h": 8}}]}},
               {{"id": "dot", "op": [2], "dst": [{{"x": 500, "y": 0, "w": 8, "h": 8}}]}},
               {{"id": "lab", "draw": 625, "dst": [{{"x": 600, "y": 0, "w": 8, "h": 8}}]}}]"#
    )
}

/// The Lua skin of the same elements, each read through `main_state`.
fn lua_entry() -> String {
    let images: String = [
        ("b0", 0),
        ("b1", 8),
        ("b5", 16),
        ("lamp", 24),
        ("lab", 32),
        ("bar", 40),
        ("dot", 48),
    ]
    .iter()
    .map(|(id, x)| format!("{{ id = \"{id}\", src = \"s\", x = {x}, y = 0, w = 8, h = 8 }}"))
    .collect::<Vec<_>>()
    .join(", ");
    let _ = IMAGES;
    format!(
        "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {images} }}\n\
         skin.imageset = {{ {{ id = \"set\", images = {{ \"b0\", \"b1\", \"b0\", \"b0\", \"b0\", \"b5\" }} }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10 }} }}\n\
         skin.value = {{\n\
           {{ id = \"ex\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, digit = 4,\n\
             value = function() return ms.number(71) end }},\n\
           {{ id = \"ep\", src = \"s\", x = 0, y = 32, w = 40, h = 8, divx = 10, digit = 4,\n\
             value = function() return ms.number(410) end }} }}\n\
         skin.songlist = {{ id = \"list\", center = 0,\n\
           liston = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           listoff = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           text = {{ {{ id = \"t\", dst = {{ {{ x = 0, y = 0, w = 100, h = 10 }} }} }} }} }}\n\
         skin.destination = {{\n\
           {{ id = \"list\", dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }},\n\
           {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
           {{ id = \"ep\", dst = {{ {{ x = 0, y = 200, w = 4, h = 8 }} }} }},\n\
           {{ id = \"dot\", op = {{ function() return ms.option(2) end }},\n\
             dst = {{ {{ x = 500, y = 0, w = 8, h = 8 }} }} }},\n\
           {{ id = \"lab\", draw = function() return ms.option(625) end,\n\
             dst = {{ {{ x = 600, y = 0, w = 8, h = 8 }} }} }} }}\n\
         return skin\n"
    )
}

#[test]
fn lua_reads_and_json_fields_draw_the_same_frames() {
    let j = common::selection::folder(&json_body());
    let json = common::selection::import_selection(j.path()).unwrap();
    let l = common::selection::folder("");
    common::write(l.path(), "s.luaskin", lua_entry().as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(l.path(), "s.luaskin", &cancel).unwrap();
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    let lua = match h
        .import(&Config::defaults(h.header()), &facts, &cancel)
        .unwrap()
    {
        LuaOutcome::Imported(i) => i,
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    };
    // 71, option 2 and option 625 lower; 410 does not, and is dropped as
    // the JSON `ref` is.
    let lowered = lua
        .callbacks
        .iter()
        .filter(|d| **d == Disposition::Lowered)
        .count();
    assert_eq!(lowered, 3, "{:?}", lua.callbacks);
    // The same note on both paths, at the same severity.
    let dropped = |r: &skin_beatoraja::Report| -> Vec<_> {
        r.notes
            .iter()
            .filter(|n| n.severity == skin_beatoraja::Severity::Dropped)
            .map(|n| n.kind)
            .collect()
    };
    let note = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Integer,
        id: Some(410),
    };
    assert_eq!(dropped(&json.report), [note]);
    assert_eq!(dropped(&lua.imported.report), [note]);
    let mut a = Sel::new(json.skin);
    let mut b = Sel::new(lua.imported.skin);
    let mut row = chart("a");
    row.score = best(321, 0, 1);
    for v in [
        view(vec![row], Some(0), Some(500), None),
        view(vec![folder_row("f", 2)], Some(0), None, None),
    ] {
        a.view(&v);
        b.view(&v);
        assert_eq!(a.drawn(0), b.drawn(0));
    }
    // The chart shows its EX score and the op-2 image; the folder neither.
    let mut row = chart("a");
    row.score = best(321, 0, 1);
    a.view(&view(vec![row], Some(0), Some(500), None));
    assert_eq!(a.digits(0), "321");
}
