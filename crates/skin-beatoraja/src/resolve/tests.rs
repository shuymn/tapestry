//! The bounds the input caps make unreachable at their standard values,
//! at lowered values: every expanded record is a node of its own, since a
//! repeated file is parsed again, and every decoded byte comes from a
//! retained input byte, so the node and input caps fire first. Then the
//! structure of the bar line groups, checked as they resolve.

use std::collections::BTreeSet;

use crate::error::{ImportError, InputLimit};
use crate::limits::Limits;
use crate::resolve::{Definition, resolve};

fn skin(src: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("s.json"), src).unwrap();
    d
}

fn header(d: &tempfile::TempDir, limits: Limits) -> Result<usize, ImportError> {
    let mut def = Definition::read_with(d.path(), "s.json", limits)?;
    let header = resolve(&mut def, &BTreeSet::new())?.header()?;
    Ok(header.options.len())
}

#[test]
fn expanded_records_are_counted_before_they_are_added() {
    let limits = Limits {
        expanded: 3,
        ..Limits::STANDARD
    };
    let at_cap = skin(r#"{"type":0,"property":[{},{"if":-1,"values":[{},{}]}]}"#);
    assert_eq!(header(&at_cap, limits).unwrap(), 3);
    let over = skin(
        r#"{"type":0,"property":[{},{"if":-1,"values":[{},{}]},
        {}]}"#,
    );
    match header(&over, limits) {
        Err(ImportError::Input {
            at: Some(at),
            limit: InputLimit::Expanded,
        }) => assert_eq!((at.line, at.col), (2, 9)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn decoded_bytes_are_counted_before_they_are_allocated() {
    let limits = Limits {
        decoded_bytes: 6,
        ..Limits::STANDARD
    };
    // Six bytes decoded: `é` is two.
    let at_cap = skin("{\"type\":0,\"name\":\"ab\\u00e9\",\"author\":\"cd\"}");
    header(&at_cap, limits).unwrap();
    let over = skin("{\"type\":0,\"name\":\"ab\\u00e9\",\"author\":\"cde\"}");
    match header(&over, limits) {
        Err(ImportError::Input {
            at: Some(at),
            limit: InputLimit::DecodedBytes,
        }) => assert_eq!(at.col, 39),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_arena_grows_with_the_nodes_not_the_file_size() {
    let d = skin(&format!(r#"{}{{"type":0}}"#, " ".repeat(1 << 20)));
    let def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
    assert_eq!(def.arena.nodes.len(), 2);
    assert!(
        def.arena.nodes.capacity() <= 1_024,
        "{}",
        def.arena.nodes.capacity()
    );
}

#[test]
fn successful_resolution_builds_no_location() {
    use crate::tree::RESCANS;
    let d = skin(
        r#"{"type":0,
        "property":[{"name":"p","item":[{"name":"i","op":901}]},
          {"include":"p.json"},
          {"if":901,"values":{"include":"p.json"}},
          [{"if":-901,"value":{"include":"q.json"}}]],
        "filepath":[{"include":"f.json"}, {"name":"g","path":"*.json"}],
        "offset":{"include":"o.json"}}"#,
    );
    for (name, src) in [
        ("p.json", r#"[{"name":"q"}]"#),
        ("q.json", r#"{"name":"r"}"#),
        ("f.json", r#"[{"name":"f","path":"*.JSON"}]"#),
        ("o.json", r#"[{"id":1,"x":true}]"#),
    ] {
        std::fs::write(d.path().join(name), src).unwrap();
    }
    RESCANS.with(|n| n.set(0));
    let mut def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
    let first = resolve(&mut def, &BTreeSet::new())
        .unwrap()
        .header()
        .unwrap();
    let configured = resolve(&mut def, &BTreeSet::from([901]))
        .unwrap()
        .header()
        .unwrap();
    assert_eq!(RESCANS.with(std::cell::Cell::get), 0);
    assert_eq!(first.options.len(), 3);
    assert_eq!(configured.options.len(), 3);
    assert_eq!(first.files[0].candidates.len(), 5);
    // The counter sees a location when one is built.
    let _ = def.loc(def.top);
    assert_eq!(RESCANS.with(std::cell::Cell::get), 1);
}

/// The `note` record's groups of a skin whose `group` field is `group`,
/// beside the files `files`, with option 901 enabled: each group's
/// record and its `dst` keys, or what resolution refuses as a type error.
fn groups(group: &str, files: &[(&str, &str)]) -> Result<Vec<usize>, (&'static str, &'static str)> {
    let d = skin(&format!(r#"{{"type": 0, "note": {{"group": {group}}}}}"#));
    for (name, text) in files {
        std::fs::write(d.path().join(name), text).unwrap();
    }
    let mut def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
    let enabled = BTreeSet::from([901]);
    let tables = resolve(&mut def, &enabled).and_then(|mut r| r.tables(&enabled));
    match tables {
        Ok(t) => {
            let note = t.records[3].unwrap();
            let [field] = def.fields(note, ["group"]);
            let list = field
                .and_then(|f| t.nested.get(&f))
                .cloned()
                .unwrap_or_default();
            Ok(list
                .iter()
                .map(|&g| {
                    let [dst] = def.fields(g, ["dst"]);
                    dst.and_then(|f| t.nested.get(&f)).map_or(0, Vec::len)
                })
                .collect())
        }
        Err(ImportError::Type {
            field, expected, ..
        }) => Err((field, expected)),
        Err(e) => panic!("{group}: {e:?}"),
    }
}

#[test]
fn groups_resolve_through_their_wrappers() {
    let bar = r#"{"id": "bar", "dst": [{"x": 0}, {"x": 1}]}"#;
    assert_eq!(groups(&format!("[{bar}, {bar}]"), &[]), Ok(vec![2, 2]));
    assert_eq!(
        groups(
            &format!(r#"[{{"if": 901, "value": {bar}}}, {{"if": -901, "values": 42}}]"#),
            &[]
        ),
        Ok(vec![2])
    );
    assert_eq!(
        groups(
            r#"{"include": "g.json"}"#,
            &[("g.json", &format!("[{bar}]"))]
        ),
        Ok(vec![2])
    );
    // A wrapper choosing nothing, or a null field, gives no group.
    assert_eq!(groups(r#"[{"if": -901, "value": 42}]"#, &[]), Ok(vec![]));
    assert_eq!(groups("null", &[]), Ok(vec![]));
    assert_eq!(groups(r#"[{"id": "bar", "dst": null}]"#, &[]), Ok(vec![0]));
}

#[test]
fn malformed_groups_are_refused() {
    let array = Err(("group", "an array"));
    let object = Err(("group", "an object"));
    assert_eq!(groups("42", &[]), array);
    assert_eq!(groups(r#""bar""#, &[]), array);
    assert_eq!(
        groups(r#"{"include": "g.json"}"#, &[("g.json", "42")]),
        array
    );
    assert_eq!(groups(r#"[{"if": 901, "values": true}]"#, &[]), array);
    // A single record where the array stands, written there or included.
    let bar = r#"{"id": "bar", "dst": [{"x": 0}]}"#;
    assert_eq!(groups(bar, &[]), array);
    assert_eq!(
        groups(r#"{"include": "g.json"}"#, &[("g.json", bar)]),
        array
    );
    assert_eq!(
        groups(&format!(r#"[{{"if": 901, "values": {bar}}}]"#), &[]),
        array
    );
    assert_eq!(groups("[42]", &[]), object);
    assert_eq!(groups(r#"[{"if": 901, "value": 42}]"#, &[]), object);
    assert_eq!(
        groups(r#"[{"include": "g.json"}]"#, &[("g.json", "[true]")]),
        object
    );
    assert_eq!(
        groups(r#"[{"id": "bar", "dst": 5}]"#, &[]),
        Err(("dst", "an array"))
    );
    for dst in [r#"{"x": 0}"#, r#"{"include": "d.json"}"#] {
        assert_eq!(
            groups(
                &format!(r#"[{{"id": "bar", "dst": {dst}}}]"#),
                &[("d.json", r#"{"x": 0}"#)]
            ),
            Err(("dst", "an array"))
        );
    }
    assert_eq!(
        groups(r#"[{"id": "bar", "dst": [{"x": 0}, 5]}]"#, &[]),
        Err(("dst", "an object"))
    );
}

/// The `songlist` record of a selection skin `src` resolved with option
/// 901 enabled under `limits`: each of its `liston` destinations' `dst`
/// key counts.
fn song_list(src: &str, limits: Limits) -> Result<Vec<usize>, ImportError> {
    let d = skin(src);
    let mut def = Definition::read_with(d.path(), "s.json", limits)?;
    let enabled = BTreeSet::from([901]);
    let t = resolve(&mut def, &enabled).and_then(|mut r| r.tables(&enabled))?;
    let list = t.records[4].unwrap();
    let [on] = def.fields(list, ["liston"]);
    let on = on
        .and_then(|f| t.nested.get(&f))
        .cloned()
        .unwrap_or_default();
    Ok(on
        .iter()
        .map(|&p| {
            let [dst] = def.fields(p, ["dst"]);
            dst.and_then(|f| t.nested.get(&f)).map_or(0, Vec::len)
        })
        .collect())
}

/// The song list resolves through a conditional wrapper at the record, at
/// its `liston` field, at a placement and at a placement's `dst`, as the
/// source's serializers read them (D11); a play skin leaves it unread.
#[test]
fn the_song_list_resolves_through_wrappers_at_each_level() {
    let src = r#"{"type": 5, "songlist": [{"if": 902}, {"if": 901, "value": {"id": "l",
        "liston": [{"if": 901, "values": [
            {"if": 901, "value": {"id": "p", "dst": [{"if": 901, "values": [{}, {}]}]}},
            {"if": 902, "value": {"id": "q", "dst": [{}]}},
            {"id": "r", "dst": [{"if": 901, "value": {}}, {"if": 902, "value": {}}]}
        ]}]}}]}"#;
    assert_eq!(song_list(src, Limits::STANDARD).unwrap(), [2, 1]);
    let d = skin(&src.replace("\"type\": 5", "\"type\": 0"));
    let mut def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
    let enabled = BTreeSet::from([901]);
    let t = resolve(&mut def, &enabled)
        .and_then(|mut r| r.tables(&enabled))
        .unwrap();
    assert_eq!(t.records[4], None);
}

/// The song list's records and keys count against the expanded values, at
/// the cap and one past it.
#[test]
fn the_song_list_s_values_count_against_the_expanded_cap() {
    let src = |keys: &str| {
        format!(
            r#"{{"type": 5, "songlist": {{"id": "l", "liston": [{{"id": "p", "dst": [{keys}]}}]}}}}"#
        )
    };
    // Its placement and two keys: three values.
    let limits = Limits {
        expanded: 3,
        ..Limits::STANDARD
    };
    assert_eq!(song_list(&src("{}, {}"), limits).unwrap(), [2]);
    match song_list(&src("{}, {}, {}"), limits) {
        Err(ImportError::Input {
            limit: InputLimit::Expanded,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// Each of the song list's nine `Destination[]` fields, a destination's
/// `dst` in it, and `customEvents` are read on a selection skin only: an
/// include of a missing file there refuses a selection skin's tables and
/// leaves a play skin's untouched.
#[test]
fn song_list_fields_and_custom_events_are_read_on_selection_only() {
    use crate::resolve::SONG_LIST_FIELDS;
    let missing = r#"{"include": "none.json"}"#;
    let mut cases: Vec<String> = SONG_LIST_FIELDS
        .iter()
        .map(|f| format!(r#""songlist": {{"id": "l", "{f}": [{missing}]}}"#))
        .collect();
    cases.push(format!(
        r#""songlist": {{"id": "l", "liston": [{{"id": "p", "dst": [{missing}]}}]}}"#
    ));
    cases.push(format!(r#""customEvents": [{missing}]"#));
    for body in &cases {
        for (ty, read) in [(5, true), (0, false)] {
            let d = skin(&format!(r#"{{"type": {ty}, {body}}}"#));
            let mut def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
            let enabled = BTreeSet::new();
            let r = resolve(&mut def, &enabled).and_then(|mut r| r.tables(&enabled));
            if read {
                assert!(
                    matches!(r, Err(ImportError::Read { .. })),
                    "{ty} {body}: {r:?}"
                );
            } else {
                assert!(r.is_ok(), "{ty} {body}: {r:?}");
            }
        }
    }
}

/// A JSON string in a float field reads as the same text written as a
/// number token does: its decimal numeral rounded to the nearest `f64`,
/// then to the nearest `f32`. `1.0000000596046448` is the `f64` 1 + 2^-24,
/// a tie that rounds to 1, where rounding the text straight to `f32` gives
/// the next float up (`0x3f80_0001`).
#[test]
fn a_string_float_rounds_as_a_number_token() {
    for src in [
        r#"{"a": "1.0000000596046448"}"#,
        r#"{"a": 1.0000000596046448}"#,
    ] {
        let d = skin(src);
        let mut def = Definition::read_with(d.path(), "s.json", Limits::STANDARD).unwrap();
        let [a] = def.fields(def.top(), ["a"]);
        let v = def.float(a.unwrap(), "alpha").unwrap();
        assert_eq!(v.to_bits(), 0x3f80_0000, "{src}");
    }
}
