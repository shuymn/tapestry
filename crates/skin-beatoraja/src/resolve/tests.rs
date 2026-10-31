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
