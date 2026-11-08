//! A destination's `op` that is not a table, through the import path:
//! one of a Lua skin's converted table reads as no condition, as the
//! source's Lua loader reads any array field
//! (`skin/lua/LuaSkinLoader.java:171-186` at `0.8.9`), with an ignored
//! note at the value. Tables and JSON values keep their handling: a Lua
//! table with string keys, whose entries the source reads, and a JSON
//! value, a file the Lua skin includes too, still fail the skin
//! (`skin/json/JSONSkinLoader.java:109`).

mod common;

use skin_beatoraja::{
    Cancel, Config, EssentialPart, ImportError, ImportFacts, LuaOutcome, NoteKind, Severity,
    read_lua_header,
};
use skin_model::ScreenKind;
use tempfile::TempDir;

/// A Lua play skin whose option `opt` has items 900 (the default) and
/// 901, drawing a 64×64 image under the destination list `dst`, written
/// as Lua. The header pass returns the list too, so both passes read it.
fn lua_text(dst: &str) -> String {
    format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
         property = {{ {{ name = \"opt\", item = {{ {{ name = \"a\", op = 900 }}, {{ name = \"b\", op = 901 }} }}, def = \"a\" }} }},\n\
         destination = {dst} }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         return skin\n"
    )
}

/// [`lua_text`]'s skin with one destination under `op`.
fn lua_skin(op: &str) -> TempDir {
    let dst = format!(
        "{{ {{ id = \"i\", op = {op}, dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}"
    );
    skin(&lua_text(&dst))
}

/// A folder holding `s.luaskin` with `text` and its 64×64 `a.png`.
fn skin(text: &str) -> TempDir {
    let d = common::skin(&[("s.luaskin", text)]);
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// [`lua_text`]'s skin with its destination list read from an included
/// `d.json` holding `dst`.
fn included(dst: &str) -> TempDir {
    let d = skin(&lua_text("{ { include = \"d.json\" } }"));
    common::write(d.path(), "d.json", dst.as_bytes());
    d
}

/// Both passes of `d`'s skin with the default configuration.
fn lua(d: &TempDir) -> Result<skin_beatoraja::LuaImported, ImportError> {
    let h = read_lua_header(d.path(), "s.luaskin", &Cancel::new())?;
    common::lua_play(&h, &Config::defaults(h.header()), &Cancel::new())
}

/// The rects drawn at 0 ms by `d`'s Lua skin.
fn drawn(d: &TempDir) -> Vec<[i32; 4]> {
    let i = lua(d).unwrap_or_else(|e| panic!("{e:?}"));
    common::play::Drawn::new(i.imported.skin, 100).rects(0)
}

#[test]
fn a_lua_op_that_is_not_a_table_is_no_condition() {
    // The control: as a table, 901 is a condition the default leaves off.
    assert!(drawn(&lua_skin("{ 901 }")).is_empty());
    for op in [
        "901",
        "-900",
        "\"x\"",
        "\"901\"",
        "true",
        "false",
        "901.5",
        "function() return false end",
        // Never executed: the source does not call it either.
        "function() error(\"x\") end",
    ] {
        let d = lua_skin(op);
        let i = lua(&d).unwrap_or_else(|e| panic!("{op}: {e:?}"));
        let notes: Vec<_> = i
            .imported
            .report
            .notes
            .iter()
            .map(|n| (n.severity, n.kind))
            .collect();
        assert_eq!(
            notes,
            [(Severity::Ignored, NoteKind::Ignored { field: "op" })],
            "{op}"
        );
        assert_eq!(drawn(&d), [[0, 1_016, 64, 64]], "{op}");
    }
}

#[test]
fn a_lua_op_note_is_located_at_the_value() {
    let i = lua(&lua_skin("901")).unwrap();
    let [note] = i.imported.report.notes.as_slice() else {
        panic!("{:?}", i.imported.report.notes)
    };
    // A converted table's location is its node's ordinal in file 0: the
    // `op` value's, not its destination record's.
    let at = note.at.unwrap();
    assert_eq!((at.source, at.line, at.col), (0, 0, 11), "{at:?}");
}

#[test]
fn a_lua_op_table_with_string_keys_still_fails() {
    match lua(&lua_skin("{ a = 901 }")) {
        Err(ImportError::Type {
            field: "op",
            expected: "an array",
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_json_op_included_by_a_lua_skin_still_fails() {
    let dst = |op: &str| {
        format!(r#"{{"id": "i", "op": {op}, "dst": [{{"x": 0, "y": 0, "w": 64, "h": 64}}]}}"#)
    };
    // The control: an array there reads as conditions.
    assert!(drawn(&included(&dst("[901]"))).is_empty());
    match lua(&included(&dst("901"))) {
        Err(ImportError::Type {
            at,
            field: "op",
            expected: "an array",
        }) => assert_eq!(
            (at.file.as_str(), at.line, at.col),
            ("d.json", 1, 19),
            "{at:?}"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_json_op_that_is_not_an_array_still_fails() {
    let d = common::skin_with(
        r#"{"type": 0, "w": 1920, "h": 1080,
            "source": [{"id": "a", "path": "a.png"}],
            "image": [{"id": "i", "src": "a", "x": 0, "y": 0, "w": 64, "h": 64}],
            "destination": [{"id": "i", "op": 901, "dst": [{"x": 0, "y": 0, "w": 64, "h": 64}]}]}"#,
        &[("a.png", 64, 64)],
    );
    match common::import_dir(d.path()) {
        Err(ImportError::Type {
            at,
            field: "op",
            expected: "an array",
        }) => assert_eq!((at.line, at.col), (4, 47), "{at:?}"),
        other => panic!("{other:?}"),
    }
}

/// A Lua selection skin whose one song list placement, a destination
/// nested in `songlist.liston`, has `op`.
fn song_list(op: &str) -> Result<skin_beatoraja::LuaImported, ImportError> {
    let text = format!(
        "local skin = {{ type = 5, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
         skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.image = {{ {{ id = \"b0\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
         skin.imageset = {{ {{ id = \"set\", images = {{ \"b0\" }} }} }}\n\
         skin.text = {{ {{ id = \"t\", font = \"f\", size = 10, ref = 10 }} }}\n\
         skin.songlist = {{ id = \"list\", center = 0,\n\
           liston = {{ {{ id = \"set\", op = {op}, dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           listoff = {{ {{ id = \"set\", dst = {{ {{ x = 0, y = 900, w = 300, h = 40 }} }} }} }},\n\
           text = {{ {{ id = \"t\", dst = {{ {{ x = 0, y = 0, w = 100, h = 10 }} }} }} }} }}\n\
         skin.destination = {{ {{ id = \"list\", dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }} }}\n\
         return skin\n"
    );
    let d = common::selection::folder("");
    common::write(d.path(), "s.luaskin", text.as_bytes());
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel)?;
    let facts = ImportFacts::unavailable(ScreenKind::Selection);
    match h.import(&Config::defaults(h.header()), &facts, &cancel)? {
        LuaOutcome::Imported(i) => Ok(i),
        LuaOutcome::Pending(p) => panic!("{p:?}"),
    }
}

/// A nested destination the importer reads twice (the selected row, read
/// for its slot and again for its placement) is noted once.
#[test]
fn a_lua_op_read_twice_is_noted_once() {
    // The control: as a table, the undefined 901 hides the selected row,
    // which the song list needs.
    match song_list("{ 901 }") {
        Err(ImportError::Essential {
            part: EssentialPart::SelectedRow,
            ..
        }) => {}
        other => panic!("{:?}", other.map(|i| i.imported.report.notes)),
    }
    let i = song_list("901").unwrap_or_else(|e| panic!("{e:?}"));
    let notes: Vec<_> = i
        .imported
        .report
        .notes
        .iter()
        .filter(|n| n.kind == NoteKind::Ignored { field: "op" })
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", i.imported.report.notes);
}
