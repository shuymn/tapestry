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
    assert_eq!(
        groups(r#"[{"id": "bar", "dst": [{"x": 0}, 5]}]"#, &[]),
        Err(("dst", "an object"))
    );
}
