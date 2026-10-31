//! Resolution of the header's wrapper-enabled fields: choices, conditions,
//! conditional items, includes, and what is never read.

mod common;

use std::collections::BTreeSet;

use common::{header, names, play, skin};
use skin_beatoraja::{Definition, ImportError, Syntax, read_header, resolve};
use skin_model::RootError;

/// The option names `body` resolves to with `enabled`.
fn options(body: &str, extra: &[(&str, &str)], enabled: &[i32]) -> Vec<String> {
    let mut files = vec![("s.json", play(body))];
    files.extend(extra.iter().map(|&(n, t)| (n, t.to_owned())));
    let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
    let d = skin(&files);
    let mut def = Definition::read(d.path(), "s.json").unwrap();
    let enabled: BTreeSet<i32> = enabled.iter().copied().collect();
    let h = resolve(&mut def, &enabled).unwrap().header().unwrap();
    names(&h)
}

#[test]
fn a_choice_takes_its_first_branch_that_holds() {
    let body = r#""property": [[
        {"if": 1, "value": {"name": "one"}},
        {"value": {"name": "any"}},
        {"value": {"name": "later"}}
    ]]"#;
    assert_eq!(options(body, &[], &[]), ["any"]);
    assert_eq!(options(body, &[], &[1]), ["one"]);
    // No branch holding, or one with no value, gives no record; an item
    // that is no object holds with no value.
    assert!(
        options(
            r#""property": [[{"if": 1, "value": {"name": "a"}}]]"#,
            &[],
            &[]
        )
        .is_empty()
    );
    assert!(
        options(
            r#""property": [[{"if": 1}, {"value": {"name": "a"}}]]"#,
            &[],
            &[1]
        )
        .is_empty()
    );
    assert!(options(r#""property": [[5, {"value": {"name": "a"}}]]"#, &[], &[]).is_empty());
    // A choice's value may be a choice.
    let nested = r#""property": [[{"value": [{"if": 2, "value": {"name": "b"}}, {"value": {"name": "c"}}]}]]"#;
    assert_eq!(options(nested, &[], &[2]), ["b"]);
}

#[test]
fn conditions_take_four_forms() {
    let cond = |c: &str, enabled: &[i32]| {
        let body = format!(r#""property": [{{"if": {c}, "value": {{"name": "x"}}}}]"#);
        !options(&body, &[], enabled).is_empty()
    };
    assert!(cond("901", &[901]));
    assert!(!cond("901", &[]));
    assert!(cond("-901", &[]));
    assert!(!cond("-901", &[901]));
    assert!(cond("[901, 911]", &[901, 911]));
    assert!(!cond("[901, 911]", &[901]));
    assert!(cond("[[901, 902], 911]", &[902, 911]));
    assert!(!cond("[[901, 902], 911]", &[911]));
    assert!(cond("[[901, -902], -911]", &[]));
    assert!(!cond("[[901, -902], -911]", &[902]));
    // Only number tokens are option numbers.
    assert!(!cond(r#""901""#, &[901]));
    assert!(!cond(r#"[901, "911"]"#, &[901, 911]));
    assert!(!cond(r#"[["901"]]"#, &[901]));
    assert!(!cond("{}", &[]));
    assert!(cond("[]", &[]));
    // A fraction truncates; a number outside i32 is refused.
    assert!(cond("901.7", &[901]));
    let d = skin(&[(
        "s.json",
        &play(r#""property": [{"if": 4294967297, "value": {}}]"#),
    )]);
    assert!(matches!(
        header(&d),
        Err(ImportError::Range { field: "if", .. })
    ));
}

#[test]
fn an_option_number_without_a_negation_refuses() {
    for c in ["-2147483648", "[-2147483648, 1]", "[[1, -2147483648]]"] {
        let d = skin(&[(
            "s.json",
            &play(&format!(r#""property": [{{"if": {c}, "value": {{}}}}]"#)),
        )]);
        assert!(
            matches!(header(&d), Err(ImportError::Range { field: "if", .. })),
            "{c}"
        );
    }
    let d = skin(&[(
        "s.json",
        &play(r#""property": [{"if": -2147483647, "value": {"name": "x"}}]"#),
    )]);
    assert!(header(&d).is_ok());
}

#[test]
fn a_conditional_item_adds_its_value_then_its_values() {
    let body = r#""property": [
        {"name": "first"},
        {"if": 1, "values": [{"name": "v1"}, {"name": "v2"}], "value": {"name": "single"}},
        {"if": -1, "value": {"name": "not 1"}},
        {"if": 1},
        {"name": "last"}
    ]"#;
    assert_eq!(
        options(body, &[], &[1]),
        ["first", "single", "v1", "v2", "", "last"]
    );
    assert_eq!(options(body, &[], &[]), ["first", "not 1", "", "last"]);
}

#[test]
fn wrapper_names_ignore_ascii_case_and_the_first_wins() {
    let a = r#"[{"name": "from a"}]"#;
    assert_eq!(
        options(
            r#""property": {"INCLUDE": "a.json", "include": "b.json"}"#,
            &[("a.json", a)],
            &[]
        ),
        ["from a"]
    );
    let body =
        r#""property": [{"If": 1, "IF": 2, "Value": {"name": "x"}, "value": {"name": "y"}}]"#;
    assert_eq!(options(body, &[], &[1]), ["x"]);
    assert!(options(body, &[], &[2]).is_empty());
}

#[test]
fn includes_supply_arrays_and_records() {
    let list = r#"[{"name": "a"}, {"if": 1, "value": {"name": "b"}}]"#;
    let one = r#"{"name": "single"}"#;
    let extra = [("list.json", list), ("one.json", one)];
    // In place of the array.
    assert_eq!(
        options(r#""property": {"include": "list.json"}"#, &extra, &[1]),
        ["a", "b"]
    );
    // As items.
    assert_eq!(
        options(
            r#""property": [{"include": "list.json"}, {"name": "c"}, {"include": "one.json"}]"#,
            &extra,
            &[]
        ),
        ["a", "c", "single"]
    );
    // As a record.
    assert_eq!(
        options(
            r#""property": [[{"value": {"include": "one.json"}}]]"#,
            &extra,
            &[]
        ),
        ["single"]
    );
    // An array included as a record is a choice.
    assert_eq!(
        options(
            r#""property": [[{"value": {"include": "choice.json"}}]]"#,
            &[(
                "choice.json",
                r#"[{"if": 1, "value": {"name": "p"}}, {"value": {"name": "q"}}]"#
            )],
            &[]
        ),
        ["q"]
    );
}

#[test]
fn an_include_resolves_from_the_folder_of_the_file_naming_it() {
    let d = skin(&[
        (
            "play/s.json",
            &play(r#""property": {"include": "../parts/a.json"}"#),
        ),
        ("parts/a.json", r#"[{"include": "sub/b.json"}]"#),
        (
            "parts/sub/b.json",
            r#"[{"name": "deep"}, {"include": "../../play/c.json"}]"#,
        ),
        ("play/c.json", r#"{"name": "back"}"#),
    ]);
    assert_eq!(
        names(&common::header_of(d.path(), "play/s.json").unwrap()),
        ["deep", "back"]
    );
}

#[test]
fn nothing_is_read_where_wrappers_are_not_enabled() {
    // Unknown fields, option items and unselected branches never read the
    // files they name.
    let body = r#""image": {"include": "missing.json"},
        "destination": [{"include": "missing.json"}],
        "property": [
            {"name": "o", "item": [{"include": "missing.json", "name": "i", "op": 3}]},
            {"if": 5, "values": {"include": "missing.json"}},
            [{"if": 5, "value": {"include": "missing.json"}}]
        ]"#;
    let d = skin(&[("s.json", &play(body))]);
    let h = header(&d).unwrap();
    assert_eq!(names(&h), ["o"]);
    assert_eq!(h.options[0].items[0].name.as_deref(), Some("i"));
    assert_eq!(h.options[0].items[0].op, 3);
    // Where they are, a missing file is an error.
    let d = skin(&[(
        "s.json",
        &play(r#""property": {"include": "missing.json"}"#),
    )]);
    let err = header(&d).map(|_| ()).unwrap_err();
    // The message is this step; the cause is only its source.
    assert!(
        err.to_string().ends_with(": cannot read missing.json"),
        "{err}"
    );
    assert!(std::error::Error::source(&err).is_some());
    match err {
        ImportError::Read {
            at: Some(at),
            name,
            source: RootError::Missing,
        } => {
            assert_eq!(name, "missing.json");
            assert_eq!((at.file.as_str(), at.line), ("s.json", 1));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_string_is_validated_even_where_it_is_never_read() {
    for body in [
        r#""image": [{"id": "a\x"}]"#,
        r#""property": [{"if": 5, "value": {"name": "a\x"}}]"#,
    ] {
        let d = skin(&[("s.json", &play(body))]);
        match header(&d) {
            Err(ImportError::Syntax {
                what: Syntax::InvalidEscape,
                ..
            }) => {}
            other => panic!("{body}: {other:?}"),
        }
    }
}

#[test]
fn include_cycles_are_refused_before_parsing() {
    let d = skin(&[("s.json", &play(r#""property": {"include": "s.json"}"#))]);
    match header(&d) {
        Err(ImportError::IncludeCycle { chain, .. }) => assert_eq!(chain, ["s.json", "s.json"]),
        other => panic!("{other:?}"),
    }
    let d = skin(&[
        ("s.json", &play(r#""property": [{"include": "a.json"}]"#)),
        ("a.json", r#"[{"include": "dir/b.json"}]"#),
        ("dir/b.json", r#"{"include": "../a.json"}"#),
    ]);
    match header(&d) {
        Err(ImportError::IncludeCycle { chain, .. }) => {
            assert_eq!(chain, ["s.json", "a.json", "dir/b.json", "a.json"]);
        }
        other => panic!("{other:?}"),
    }
    // The same file twice, not nested, is no cycle.
    let d = skin(&[
        (
            "s.json",
            &play(r#""property": [{"include": "a.json"}, {"include": "./a.json"}]"#),
        ),
        ("a.json", r#"{"name": "a"}"#),
    ]);
    assert_eq!(names(&header(&d).unwrap()), ["a", "a"]);
}

#[cfg(unix)]
#[test]
fn a_cycle_through_another_spelling_is_found_by_identity() {
    let d = skin(&[
        ("s.json", &play(r#""property": {"include": "a.json"}"#)),
        ("a.json", r#"{"include": "alias.json"}"#),
    ]);
    std::os::unix::fs::symlink(d.path().join("s.json"), d.path().join("alias.json")).unwrap();
    match header(&d) {
        Err(ImportError::IncludeCycle { chain, .. }) => {
            assert_eq!(chain, ["s.json", "a.json", "alias.json"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn include_names_leaving_the_folder_are_refused() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("skin");
    common::write(&outer.path().join("away"), "x.json", b"{}");
    common::write(&root, "in.json", br#"{"name": "in"}"#);
    common::link_dir(&root.join("out"), &outer.path().join("away"));
    common::link_dir(&root.join("loop"), &root);
    let outside = outer.path().join("away").join("x.json");
    let check = |name: &str| {
        let text = play(&format!(r#""property": {{"include": {name:?}}}"#));
        common::write(&root, "s.json", text.as_bytes());
        common::header_of(&root, "s.json")
    };
    for (name, want) in [
        (outside.to_str().unwrap(), "rejected"),
        ("/x.json", "rejected"),
        ("\\x.json", "rejected"),
        ("\\\\server\\s\\x.json", "rejected"),
        ("//server/s/x.json", "rejected"),
        ("\\\\?\\C:\\x.json", "rejected"),
        ("C:\\x.json", "rejected"),
        ("c:x.json", "rejected"),
        ("../away/x.json", "underflow"),
        ("a/../../x.json", "underflow"),
        ("out/x.json", "escapes"),
    ] {
        match (want, check(name)) {
            (
                "rejected",
                Err(ImportError::Read {
                    source: RootError::Rejected,
                    ..
                }),
            )
            | (
                "underflow",
                Err(ImportError::Read {
                    source: RootError::Underflow,
                    ..
                }),
            )
            | (
                "escapes",
                Err(ImportError::Read {
                    source: RootError::Escapes,
                    ..
                }),
            ) => {}
            (_, other) => panic!("{name}: {other:?}"),
        }
    }
    assert_eq!(names(&check("loop/in.json").unwrap()), ["in"]);
    assert!(matches!(
        check("*.json"),
        Err(ImportError::IncludeWildcard { .. })
    ));
}

/// Opening a FIFO with no writer blocks: the include must be refused from
/// its type first. The read runs on a thread with a deadline.
#[cfg(unix)]
#[test]
fn a_fifo_include_is_refused_without_opening_it() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = skin(&[("s.json", &play(r#""property": {"include": "p.json"}"#))]);
    let status = std::process::Command::new("mkfifo")
        .arg(d.path().join("p.json"))
        .status()
        .expect("this test needs `mkfifo`");
    assert!(status.success());
    let root = d.path().to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(common::header_of(&root, "s.json").map(|_| ()));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => assert!(
            matches!(
                result,
                Err(ImportError::Read {
                    source: RootError::NotAFile,
                    ..
                })
            ),
            "{result:?}"
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the FIFO was opened"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the reading thread panicked"),
    }
}

#[test]
fn a_small_entry_file_may_include_a_much_larger_fragment() {
    let items: Vec<String> = (0..16)
        .map(|i| format!(r#"{{"name": "i{i}", "op": {i}}}"#))
        .collect();
    let record = format!(r#"{{"name": "o", "item": [{}]}}"#, items.join(","));
    let fragment = format!("[{}]", vec![record; 1_000].join(",\n"));
    let d = skin(&[
        ("s.json", &play(r#""property": {"include": "big.json"}"#)),
        ("big.json", &fragment),
    ]);
    let h = header(&d).unwrap();
    assert_eq!(h.options.len(), 1_000);
    assert_eq!(h.options[999].items[15].op, 15);
}

#[test]
fn errors_name_the_included_file_and_place() {
    let d = skin(&[
        (
            "s.json",
            &play(r#""property": {"include": "parts/a.json"}"#),
        ),
        (
            "parts/a.json",
            "[\n  {\"name\": 1}\n  {\"name\": 2} {\"name\": 3}\n]",
        ),
    ]);
    match header(&d) {
        Err(ImportError::Syntax { at, what }) => {
            assert_eq!(what, Syntax::ExpectedSeparator);
            assert_eq!((at.file.as_str(), at.line, at.col), ("parts/a.json", 3, 15));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn resolving_again_reads_the_includes_again() {
    let d = skin(&[
        (
            "s.json",
            &play(
                r#""property": [{"if": 1, "values": {"include": "a.json"}}, {"if": -1, "values": {"include": "b.json"}}]"#,
            ),
        ),
        ("a.json", r#"[{"name": "a"}]"#),
        ("b.json", r#"[{"name": "b"}]"#),
    ]);
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    assert_eq!(names(&h), ["b"]);
    std::fs::write(d.path().join("a.json"), r#"[{"name": "a2"}]"#).unwrap();
    let on = resolve(&mut def, &BTreeSet::from([1]))
        .unwrap()
        .header()
        .unwrap();
    assert_eq!(names(&on), ["a2"]);
}
