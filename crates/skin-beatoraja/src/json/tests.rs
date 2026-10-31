//! The reader against the pinned grammar, one rule per test, and its
//! bounds at the cap and one past it.

use super::parse;
use crate::error::{ImportError, InputLimit, Syntax};
use crate::limits::{MAX_DEPTH, MAX_NODES, MAX_STRING_BYTES};
use crate::tree::{Arena, KEYED, Kind, Scalar, Text};

fn arena(src: &[u8]) -> Arena {
    let mut a = Arena::with_estimate(16);
    a.files.push(Text {
        name: "t.json".into(),
        dir: String::new(),
        bytes: src.into(),
    });
    a
}

/// The tree, compactly: `{name=value,...}`, `[...]`, quoted strings in
/// quotes, tokens as written.
fn render(a: &Arena, id: u32) -> String {
    let n = a.node(id);
    let key = if n.flags & KEYED != 0 {
        let b = &a.files[0].bytes[n.key_start as usize..n.key_end as usize];
        format!("{}=", std::str::from_utf8(b).unwrap())
    } else {
        String::new()
    };
    let body = match n.kind {
        Kind::Object | Kind::Array => {
            let inner: Vec<String> = a.children(id).map(|c| render(a, c)).collect();
            if n.kind == Kind::Object {
                format!("{{{}}}", inner.join(","))
            } else {
                format!("[{}]", inner.join(","))
            }
        }
        Kind::Quoted => format!("\"{}\"", a.raw(id)),
        Kind::Bare => a.raw(id).to_owned(),
    };
    key + &body
}

fn read(src: &str) -> String {
    let mut a = arena(src.as_bytes());
    match parse(&mut a, 0, 0) {
        Ok(root) => render(&a, root),
        Err(e) => panic!("{src:?}: {e}"),
    }
}

/// The syntax error and where it is.
fn refused(src: &[u8]) -> (Syntax, u32, u32) {
    let mut a = arena(src);
    match parse(&mut a, 0, 0) {
        Err(ImportError::Syntax { at, what }) => (what, at.line, at.col),
        other => panic!("{:?}: {other:?}", String::from_utf8_lossy(src)),
    }
}

fn limit(src: &[u8], base_depth: u32) -> InputLimit {
    let mut a = arena(src);
    match parse(&mut a, 0, base_depth) {
        Err(ImportError::Input { limit, .. }) => limit,
        other => panic!("{other:?}"),
    }
}

#[test]
fn comments_are_whitespace() {
    assert_eq!(read("{/*c*/\"a\":1 // x\n,\"b\":/**/2}"), "{a=1,b=2}");
    assert_eq!(read("// lead\n[1] /* trail */"), "[1]");
    assert_eq!(refused(b"[1 /* open").0, Syntax::UnterminatedComment);
    assert_eq!(refused(b"[1, / 2]").0, Syntax::Unexpected);
    // A slash inside a token is no comment.
    assert_eq!(read("[1 / 2]"), "[1 / 2]");
}

#[test]
fn one_comma_may_open_or_close_a_container() {
    assert_eq!(read("[1,2,]"), "[1,2]");
    assert_eq!(read("{\"a\":1,}"), "{a=1}");
    assert_eq!(read("[,1]"), "[1]");
    assert_eq!(read("[,]"), "[]");
    assert_eq!(read("[1,\n]"), "[1]");
    assert_eq!(read("[\n,1]"), "[1]");
    assert_eq!(refused(b"[1,,2]"), (Syntax::Unexpected, 1, 4));
    assert_eq!(refused(b"[1,\n,2]").0, Syntax::Unexpected);
    // A comma on the line after a value must be followed by a value.
    assert_eq!(refused(b"[1\n,]"), (Syntax::Unexpected, 2, 2));
}

#[test]
fn a_line_break_separates_where_a_comma_is_missing() {
    assert_eq!(read("[{\"a\":1}\n{\"b\":2}]"), "[{a=1},{b=2}]");
    assert_eq!(read("{\"a\":1\r\n\"b\":2\n}"), "{a=1,b=2}");
    assert_eq!(read("[1\n,2]"), "[1,2]");
    assert_eq!(read("[1 // c\n2]"), "[1,2]");
    // On one line two values need a comma, and a block comment's line
    // break separates nothing.
    assert_eq!(
        refused(b"[{\"a\":1}{\"b\":2}]"),
        (Syntax::ExpectedSeparator, 1, 9)
    );
    assert_eq!(
        refused(b"[{\"a\":1} {\"b\":2}]").0,
        Syntax::ExpectedSeparator
    );
    assert_eq!(refused(b"[\"a\" \"b\"]").0, Syntax::ExpectedSeparator);
    assert_eq!(refused(b"[\"a\"/*\n*/\"b\"]").0, Syntax::ExpectedSeparator);
}

#[test]
fn unquoted_names_end_at_a_colon_line_break_or_comment() {
    assert_eq!(read("{a b:1}"), "{a b=1}");
    assert_eq!(read("{a  \t:1}"), "{a=1}");
    assert_eq!(read("{a\n:1}"), "{a=1}");
    assert_eq!(read("{a//c\n:1}"), "{a=1}");
    assert_eq!(read("{a/*c*/:1}"), "{a=1}");
    // A brace or comma does not end a name.
    assert_eq!(read("{a}b,c:1}"), "{a}b,c=1}");
    assert_eq!(refused(b"{a 1}").0, Syntax::UnexpectedEnd);
    assert_eq!(refused(b"{\"a\" 1}").0, Syntax::ExpectedColon);
}

#[test]
fn unquoted_values_end_at_a_comma_bracket_line_break_or_comment() {
    assert_eq!(read("[a b,c d]"), "[a b,c d]");
    assert_eq!(read("{x:a b}"), "{x=a b}");
    assert_eq!(read("[a b  \t]"), "[a b]");
    assert_eq!(read("[a b\n]"), "[a b]");
    assert_eq!(read("[a//c\n]"), "[a]");
    assert_eq!(read("[a /*c*/]"), "[a]");
    assert_eq!(read("[a/b{c:d\"e]"), "[a/b{c:d\"e]");
    assert_eq!(read("x y"), "x y");
    // A trimmed form feed is no whitespace between values.
    assert_eq!(refused(b"[a\x0c]").0, Syntax::ExpectedSeparator);
    // A value may not start with `}`.
    assert_eq!(refused(b"{\"a\":}"), (Syntax::Unexpected, 1, 6));
}

#[test]
fn strings_may_span_lines_and_hold_escapes() {
    assert_eq!(read("[\"a\nb\"]"), "[\"a\nb\"]");
    assert_eq!(read(r#"["a\"b"]"#), r#"["a\"b"]"#);
    assert_eq!(refused(br#"["a\x"]"#), (Syntax::InvalidEscape, 1, 4));
    assert_eq!(refused(br"[a\x]").0, Syntax::InvalidEscape);
    assert_eq!(refused(br#"{"a\q":1}"#).0, Syntax::InvalidEscape);
}

#[test]
fn errors_report_line_and_column() {
    assert_eq!(
        refused(b"{\"a\":\n  \"abc"),
        (Syntax::UnterminatedString, 2, 3)
    );
    // Columns count characters, not bytes.
    assert_eq!(
        refused("[\"\u{e9}\" x".as_bytes()),
        (Syntax::ExpectedSeparator, 1, 6)
    );
    assert_eq!(refused(b"[1] 2"), (Syntax::TrailingContent, 1, 5));
    assert_eq!(refused(b"{\"a\":1]").0, Syntax::MismatchedClose);
    assert_eq!(refused(b"[1").0, Syntax::UnexpectedEnd);
    assert_eq!(refused(b"").0, Syntax::UnexpectedEnd);
}

#[test]
fn invalid_utf8_is_refused_with_its_place() {
    assert_eq!(
        refused(b"{\n  \"a\": \"\xff\"}"),
        (Syntax::InvalidUtf8, 2, 9)
    );
    // Even where the value would never be read.
    assert_eq!(refused(b"{\"x\": 1} // \xc3").0, Syntax::InvalidUtf8);
}

#[test]
fn a_byte_order_mark_is_skipped() {
    assert_eq!(read("\u{feff}[1]"), "[1]");
    assert_eq!(
        refused("\u{feff}[x".as_bytes()),
        (Syntax::UnexpectedEnd, 1, 3)
    );
    // Only at the start.
    assert_eq!(
        refused("[1] \u{feff}".as_bytes()).0,
        Syntax::TrailingContent
    );
}

#[test]
fn members_keep_their_order_and_duplicates() {
    assert_eq!(read("{b:1,a:2,b:3}"), "{b=1,a=2,b=3}");
}

#[test]
fn tokens_are_typed_as_the_pinned_reader_types_them() {
    // The last token is `tru` and an escaped `e`: `true` once unescaped.
    let src = format!(
        r#"[1, -2, +3, 1.5, -.5e3, 2., 1-2, 1.5x, 99999999999999999999, true, false, null, "1", tru{}u0065]"#,
        '\\'
    );
    let mut a = arena(src.as_bytes());
    let root = parse(&mut a, 0, 0).unwrap();
    let kinds: Vec<String> = a
        .children(root)
        .map(|c| match a.scalar(c) {
            Scalar::Long(s) => format!("long {s}"),
            Scalar::Double(s) => format!("double {s}"),
            Scalar::Str { raw, .. } => format!("str {raw}"),
            Scalar::Bool(b) => format!("bool {b}"),
            Scalar::Null => "null".into(),
            Scalar::Object | Scalar::Array => "container".into(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "long 1",
            "long -2",
            "long +3",
            "double 1.5",
            "double -.5e3",
            "double 2.",
            "str 1-2",
            "str 1.5x",
            "str 99999999999999999999",
            "bool true",
            "bool false",
            "null",
            "str 1",
            "bool true",
        ]
    );
}

fn nested(n: usize) -> Vec<u8> {
    let mut s = vec![b'['; n];
    s.extend(std::iter::repeat_n(b']', n));
    s
}

#[test]
fn nesting_is_bounded_before_a_container_opens() {
    let mut a = arena(&nested(MAX_DEPTH as usize));
    parse(&mut a, 0, 0).unwrap();
    assert_eq!(limit(&nested(MAX_DEPTH as usize + 1), 0), InputLimit::Depth);
    // The depth the file is included at counts.
    let mut a = arena(&nested(MAX_DEPTH as usize - 10));
    parse(&mut a, 0, 10).unwrap();
    assert_eq!(
        limit(&nested(MAX_DEPTH as usize - 9), 10),
        InputLimit::Depth
    );
    // A megabyte of brackets stops at the bound, with no deep recursion.
    let deep = vec![b'['; 1 << 20];
    let mut a = arena(&deep);
    match parse(&mut a, 0, 0) {
        Err(ImportError::Input {
            at: Some(at),
            limit: InputLimit::Depth,
        }) => assert_eq!(at.col, MAX_DEPTH + 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(a.nodes.len(), MAX_DEPTH as usize);
}

#[test]
fn strings_are_bounded_before_their_node_exists() {
    let at_cap = "a".repeat(MAX_STRING_BYTES);
    let over = "a".repeat(MAX_STRING_BYTES + 1);
    for (cap, past) in [
        (format!("[\"{at_cap}\"]"), format!("[\"{over}\"]")),
        (format!("[{at_cap}]"), format!("[{over}]")),
        (format!("{{\"{at_cap}\":1}}"), format!("{{\"{over}\":1}}")),
        (format!("{{{at_cap}:1}}"), format!("{{{over}:1}}")),
    ] {
        let mut a = arena(cap.as_bytes());
        parse(&mut a, 0, 0).unwrap();
        let mut a = arena(past.as_bytes());
        match parse(&mut a, 0, 0) {
            Err(ImportError::Input {
                limit: InputLimit::StringBytes,
                ..
            }) => {}
            other => panic!("{other:?}"),
        }
        // Only the container was inserted.
        assert_eq!(a.nodes.len(), 1);
    }
}

#[test]
fn nodes_are_bounded_before_insertion_and_the_arena_never_grows_past_it() {
    // The array and its items: MAX_NODES in all.
    let mut src = b"[1".to_vec();
    src.extend(b",1".repeat(MAX_NODES - 2));
    src.push(b']');
    let mut a = arena(&src);
    parse(&mut a, 0, 0).unwrap();
    assert_eq!(a.nodes.len(), MAX_NODES);
    assert!(a.nodes.capacity() <= MAX_NODES, "{}", a.nodes.capacity());
    src.pop();
    src.extend(b",1]");
    let mut a = arena(&src);
    assert_eq!(
        match parse(&mut a, 0, 0) {
            Err(ImportError::Input { limit, .. }) => limit,
            other => panic!("{other:?}"),
        },
        InputLimit::Nodes
    );
    assert_eq!(a.nodes.len(), MAX_NODES);
    assert!(a.nodes.capacity() <= MAX_NODES);
}
