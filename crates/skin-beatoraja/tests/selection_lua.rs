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
        // Every quad whole (rect, uv, layer, colour, rotation) and every
        // run (blend, clip).
        assert_eq!(a.frame(0), b.frame(0));
        assert_eq!(a.batch.runs(), b.batch.runs());
    }
    // The chart shows its EX score and the op-2 image; the folder neither.
    let mut row = chart("a");
    row.score = best(321, 0, 1);
    a.view(&view(vec![row], Some(0), Some(500), None));
    assert_eq!(a.digits(0), "321");
}

/// The configured pass of Lua selection skin `entry` in `l` (beside the
/// selection fixtures' sheet and font), on a selection scene.
fn import_lua_in(
    l: &tempfile::TempDir,
    entry: &str,
) -> Result<skin_beatoraja::LuaImported, skin_beatoraja::ImportError> {
    common::write(l.path(), "s.luaskin", entry.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(l.path(), "s.luaskin", &cancel)?;
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    match h.import(&Config::defaults(h.header()), &facts, &cancel)? {
        LuaOutcome::Imported(i) => Ok(i),
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// [`import_lua_in`] a folder of its own, dropped after.
fn import_lua(entry: &str) -> Result<skin_beatoraja::LuaImported, skin_beatoraja::ImportError> {
    import_lua_in(&common::selection::folder(""), entry)
}

/// A Lua selection skin: one placement whose `liston` holds `on` besides
/// its keys, the title, and `more` statements before it returns.
fn lua_skin(on: &str, more: &str) -> String {
    format!(
        "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"b0\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }},\n\
           {{ id = \"b1\", src = \"s\", x = 8, y = 0, w = 8, h = 8 }} }}\n\
         skin.imageset = {{ {{ id = \"set\", images = {{ \"b0\" }} }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10 }} }}\n\
         skin.songlist = {{ id = \"list\", center = 0,\n\
           liston = {{ {{ id = \"set\", {on} dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           listoff = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           text = {{ {{ id = \"t\", dst = {{ {{ x = 0, y = 0, w = 100, h = 10 }} }} }} }} }}\n\
         skin.destination = {{ {{ id = \"list\", dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }} }}\n\
         {more}\n\
         return skin\n"
    )
}

/// A Lua `draw` that lowers to a constant false folds as the JSON
/// condition of the same id does: on the selected row it refuses.
#[test]
fn a_lua_draw_known_false_folds_as_json_does() {
    let entry = lua_skin("draw = function() return ms.option(625) end,", "");
    match import_lua(&entry) {
        Err(skin_beatoraja::ImportError::Essential {
            part: skin_beatoraja::EssentialPart::SelectedRow,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|i| i.callbacks)),
    }
    // One reading a live option is kept.
    import_lua(&lua_skin("draw = function() return ms.option(2) end,", "")).unwrap();
    // A composed one that folds false (`not` of a constant true), on the
    // selected row, the title and the list element alike.
    let composed = "function() return not ms.option(50) end";
    for (on, more, part) in [
        (
            format!("draw = {composed},"),
            String::new(),
            skin_beatoraja::EssentialPart::SelectedRow,
        ),
        (
            String::new(),
            format!("skin.songlist.text[1].draw = {composed}"),
            skin_beatoraja::EssentialPart::RowTitle,
        ),
        (
            String::new(),
            format!("skin.destination[1].draw = {composed}"),
            skin_beatoraja::EssentialPart::SongList,
        ),
    ] {
        match import_lua(&lua_skin(&on, &more)) {
            Err(skin_beatoraja::ImportError::Essential { part: p, .. }) => assert_eq!(p, part),
            other => panic!("{part:?}: {:?}", other.map(|i| i.callbacks)),
        }
    }
}

/// A Lua timer read takes the selection profile's timers: 11 is the
/// change timer, drawn while it is on.
#[test]
fn lua_timer_reads_take_the_selection_timers() {
    let more = "table.insert(skin.destination, { id = \"b1\",\n\
                  draw = function() return ms.timer(11) ~= ms.timer_off_value end,\n\
                  dst = { { x = 700, y = 0, w = 8, h = 8 } } })";
    let l = common::selection::folder("");
    let i = import_lua_in(&l, &lua_skin("", more)).unwrap();
    assert_eq!(
        i.callbacks
            .iter()
            .filter(|d| **d == Disposition::Lowered)
            .count(),
        1,
        "{:?}",
        i.callbacks
    );
    let mut s = Sel::new(i.imported.skin);
    s.view(&view(vec![chart("a")], Some(0), None, None));
    let drawn = |s: &mut Sel| s.drawn(0).iter().any(|(_, r)| r[0] == 700.0);
    assert!(!drawn(&mut s));
    s.timers.change = Some(bms_model::TimeUs(0));
    assert!(drawn(&mut s));
}

/// Lua text and image index reads take the selection bridges as their
/// JSON `ref`s do: text 30 (the query) and image index 370 (the selected
/// row's clear type).
#[test]
fn lua_reads_take_the_selection_text_and_index_bridges() {
    let more = "table.insert(skin.text, { id = \"q\", font = \"f\", size = 10,\n\
                  value = function() return ms.text(30) end })\n\
                table.insert(skin.imageset, { id = \"ct\",\n\
                  images = { \"b0\", \"b0\", \"b0\", \"b0\", \"b0\", \"b1\" },\n\
                  value = function() return ms.event_index(370) end })\n\
                table.insert(skin.destination, { id = \"q\", dst = { { x = 0, y = 0, w = 100, h = 10 } } })\n\
                table.insert(skin.destination, { id = \"ct\", dst = { { x = 700, y = 0, w = 8, h = 8 } } })";
    let l = common::selection::folder("");
    let i = import_lua_in(&l, &lua_skin("", more)).unwrap();
    assert_eq!(
        i.callbacks
            .iter()
            .filter(|d| **d == Disposition::Lowered)
            .count(),
        2,
        "{:?}",
        i.callbacks
    );
    let mut s = Sel::new(i.imported.skin);
    let mut row = chart("a");
    row.score = best(1, 0, 1);
    let mut v = view(vec![row], Some(0), None, None);
    v.query = "Q".to_owned();
    s.view(&v);
    let (texts, _) = s.requests();
    assert!(texts.iter().any(|(_, t)| t == "Q"), "{texts:?}");
    // A normal clear: the source's clear type 5, image 5 of the set.
    let at_700: Vec<[u8; 2]> = s
        .drawn(0)
        .into_iter()
        .filter(|(_, r)| r[0] == 700.0)
        .map(|(t, _)| t)
        .collect();
    assert_eq!(at_700, [common::selection::B1]);
    // An unplayed chart: no clear type, nothing drawn.
    s.view(&view(vec![chart("u")], Some(0), None, None));
    assert!(s.drawn(0).iter().all(|(_, r)| r[0] != 700.0));
}
