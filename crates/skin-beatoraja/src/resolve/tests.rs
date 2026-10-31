//! The bounds the input caps make unreachable at their standard values,
//! at lowered values: every expanded record is a node of its own, since a
//! repeated file is parsed again, and every decoded byte comes from a
//! retained input byte, so the node and input caps fire first.

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
