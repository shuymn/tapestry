//! A destination's `op` that is not a table, through the import path:
//! a Lua skin's reads as no condition, as the source's Lua loader reads
//! any array field (`skin/lua/LuaSkinLoader.java:171-186` at `0.8.9`),
//! with an ignored note at the value; a JSON skin's, and a Lua table
//! with string keys, still fail the skin (`skin/json/JSONSkinLoader.java:109`).

mod common;

use skin_beatoraja::{Cancel, Config, ImportError, NoteKind, Severity, read_lua_header};
use tempfile::TempDir;

/// A Lua play skin whose option `opt` has items 900 (the default) and
/// 901, drawing a 64×64 image under `op`, written as Lua.
fn lua_skin(op: &str) -> TempDir {
    let text = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080,\n\
         property = {{ {{ name = \"opt\", item = {{ {{ name = \"a\", op = 900 }}, {{ name = \"b\", op = 901 }} }}, def = \"a\" }} }} }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         skin.destination = {{ {{ id = \"i\", op = {op},\n\
         dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}\n\
         return skin\n"
    );
    let d = common::skin(&[("s.luaskin", &text)]);
    common::png(d.path(), "a.png", 64, 64);
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
        "true",
        "function() return false end",
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
