//! The header: the skin type and size, typed fields as the source reads
//! them, records read literally, file choices listed, and the header pass
//! resolved with no option enabled.

mod common;

use std::collections::BTreeSet;

use common::{header, names, play, skin};
use skin_beatoraja::{
    FileSpec, ImportError, OffsetSpec, OptionItem, OptionSpec, SIZES, read_header, resolve,
};
use skin_model::RootError;

#[test]
fn only_the_7_key_play_type_is_accepted() {
    for (text, refused) in [
        ("{}", true),
        (r#"{"type": 1}"#, true),
        (r#"{"type": -1}"#, true),
        (r#"{"type": 0}"#, false),
        (r#"{"type": "0"}"#, false),
        // A fraction truncates toward zero.
        (r#"{"type": 0.9}"#, false),
        (r#"{"type": -0.5}"#, false),
        (r#"{"type": 1, "type": 0}"#, false),
    ] {
        let result = header(&skin(&[("s.json", text)]));
        assert_eq!(
            matches!(result, Err(ImportError::NotPlay7 { .. })),
            refused,
            "{text}: {result:?}"
        );
        if !refused {
            result.unwrap();
        }
    }
    // A field named in another case is another field.
    let result = header(&skin(&[("s.json", r#"{"Type": 0}"#)]));
    assert!(matches!(result, Err(ImportError::NotPlay7 { at: None })));
    let result = header(&skin(&[("s.json", "[]")]));
    assert!(
        matches!(
            result,
            Err(ImportError::Type {
                field: "(root)",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn only_the_listed_sizes_are_accepted() {
    assert_eq!(
        SIZES,
        [
            [1280, 720],
            [1600, 900],
            [1920, 1080],
            [2560, 1440],
            [3840, 2160]
        ]
    );
    for [w, h] in SIZES {
        let text = play(&format!(r#""w": {w}, "h": "{h}""#));
        assert_eq!(header(&skin(&[("s.json", &text)])).unwrap().size, [w, h]);
    }
    assert_eq!(
        header(&skin(&[("s.json", &play(r#""name": "x""#))]))
            .unwrap()
            .size,
        [1280, 720]
    );
    for (w, h) in [
        (1366, 768),
        (1280, 960),
        (1920, 720),
        (640, 480),
        (-1280, 720),
    ] {
        let text = play(&format!(r#""w": {w}, "h": {h}"#));
        match header(&skin(&[("s.json", &text)])) {
            Err(ImportError::Resolution { w: rw, h: rh, at }) => {
                assert_eq!((rw, rh), (w, h));
                assert!(at.is_some());
            }
            other => panic!("{w}x{h}: {other:?}"),
        }
    }
    // An omitted height is 720.
    let text = play(r#""w": 1920"#);
    assert!(matches!(
        header(&skin(&[("s.json", &text)])),
        Err(ImportError::Resolution {
            w: 1920,
            h: 720,
            ..
        })
    ));
}

fn offset_id(value: &str) -> Result<i32, ImportError> {
    let text = play(&format!(r#""offset": [{{"id": {value}}}]"#));
    header(&skin(&[("s.json", &text)])).map(|h| h.offsets[0].id)
}

#[test]
fn integer_fields_truncate_and_stay_inside_i32() {
    for (value, id) in [
        ("10.7", 10),
        ("-10.7", -10),
        ("1e9", 1_000_000_000),
        ("2147483647", i32::MAX),
        ("-2147483648", i32::MIN),
        ("2147483647.9", i32::MAX),
        (r#""42""#, 42),
        (r#""+42""#, 42),
        (r#""-042""#, -42),
        ("+7", 7),
    ] {
        assert_eq!(offset_id(value).unwrap(), id, "{value}");
    }
    for value in [
        "2147483648",
        "-2147483649",
        "2147483648.0",
        "1e10",
        r#""2147483648""#,
        "99999999999999999999",
    ] {
        assert!(
            matches!(
                offset_id(value),
                Err(ImportError::Range { field: "id", .. })
            ),
            "{value}: {:?}",
            offset_id(value)
        );
    }
    for value in [
        "abc",
        r#""10.7""#,
        "true",
        "null",
        "[]",
        "{}",
        "1-2",
        r#""""#,
    ] {
        assert!(
            matches!(offset_id(value), Err(ImportError::Type { field: "id", .. })),
            "{value}: {:?}",
            offset_id(value)
        );
    }
}

#[test]
fn strings_keep_their_spelling() {
    let text = play(
        r#""name": 007, "author": true, "property": [
            {"name": 1.50, "def": -0, "item": [{"name": 1e3, "op": "12"}, {"name": "a b", "op": 3.9}]},
            {"name": unquoted with spaces  , "def": null}
        ]"#,
    );
    let h = header(&skin(&[("s.json", &text)])).unwrap();
    assert_eq!(h.name.as_deref(), Some("007"));
    assert_eq!(h.author.as_deref(), Some("true"));
    assert_eq!(
        h.options,
        [
            OptionSpec {
                name: Some("1.50".into()),
                items: vec![
                    OptionItem {
                        name: Some("1e3".into()),
                        op: 12
                    },
                    OptionItem {
                        name: Some("a b".into()),
                        op: 3
                    },
                ],
                def: Some("-0".into()),
            },
            OptionSpec {
                name: Some("unquoted with spaces".into()),
                items: Vec::new(),
                def: None,
            },
        ]
    );
    let text = play(r#""name": {"value": "x"}"#);
    assert!(matches!(
        header(&skin(&[("s.json", &text)])),
        Err(ImportError::Type { field: "name", .. })
    ));
}

#[test]
fn ordinary_fields_match_exactly_and_the_last_wins() {
    let text = play(
        r#""name": "a", "name": "b", "Name": "c",
        "property": [{"name": "x", "NAME": "z", "name": "y"}],
        "offset": [{"id": 1}], "offset": [{"id": 2}, {"id": 3}]"#,
    );
    let h = header(&skin(&[("s.json", &text)])).unwrap();
    assert_eq!(h.name.as_deref(), Some("b"));
    assert_eq!(names(&h), ["y"]);
    let ids: Vec<i32> = h.offsets.iter().map(|o| o.id).collect();
    assert_eq!(ids, [2, 3]);
}

#[test]
fn records_that_are_not_objects_take_their_defaults() {
    let text = play(r#""property": [5, "x", null], "offset": [true]"#);
    let h = header(&skin(&[("s.json", &text)])).unwrap();
    assert_eq!(h.options.len(), 3);
    assert!(
        h.options
            .iter()
            .all(|o| o.name.is_none() && o.items.is_empty())
    );
    assert_eq!(
        h.offsets,
        [OffsetSpec {
            name: None,
            id: 0,
            x: false,
            y: false,
            w: false,
            h: false,
            r: false,
            a: false
        }]
    );
    // A field holding a string or number in place of its array is empty.
    let text = play(r#""property": "x", "filepath": 3"#);
    let h = header(&skin(&[("s.json", &text)])).unwrap();
    assert!(h.options.is_empty() && h.files.is_empty());
}

#[test]
fn option_items_are_an_array_of_objects_read_literally() {
    for (items, field) in [
        (r#"{"name": "a"}"#, "item"),
        (r#""a""#, "item"),
        ("[1]", "item"),
        (r#"[{"name": "a", "op": "x"}]"#, "op"),
    ] {
        let text = play(&format!(r#""property": [{{"item": {items}}}]"#));
        match header(&skin(&[("s.json", &text)])) {
            Err(ImportError::Type { field: f, .. }) => assert_eq!(f, field, "{items}"),
            other => panic!("{items}: {other:?}"),
        }
    }
}

#[test]
fn offset_flags_read_true_or_a_true_string() {
    let text = play(
        r#""offset": [{"name": "o", "id": 3, "x": true, "y": "TRUE", "w": 1, "h": "yes", "r": false, "a": tRuE}]"#,
    );
    let h = header(&skin(&[("s.json", &text)])).unwrap();
    assert_eq!(
        h.offsets,
        [OffsetSpec {
            name: Some("o".into()),
            id: 3,
            x: true,
            y: true,
            w: false,
            h: false,
            r: false,
            a: true
        }]
    );
    let text = play(r#""offset": [{"x": null}]"#);
    assert!(matches!(
        header(&skin(&[("s.json", &text)])),
        Err(ImportError::Type { field: "x", .. })
    ));
}

#[test]
fn the_header_pass_resolves_with_no_option_enabled() {
    // The option defaults to its first item, which enables 901, but the
    // header pass enables nothing: it sees the branch for 901 off.
    let text = play(
        r#""property": [
            {"name": "mode", "item": [{"name": "on", "op": 901}, {"name": "off", "op": 902}], "def": "on"},
            {"if": 901, "values": [{"name": "only when on"}]},
            {"if": -901, "values": [{"name": "only when off"}]}
        ]"#,
    );
    let d = skin(&[("s.json", &text)]);
    let (mut def, h) = read_header(d.path(), "s.json").unwrap();
    assert_eq!(names(&h), ["mode", "only when off"]);
    // The kept definition resolves again for the picked options, and
    // back.
    let on = resolve(&mut def, &BTreeSet::from([901]))
        .unwrap()
        .header()
        .unwrap();
    assert_eq!(names(&on), ["mode", "only when on"]);
    let off = resolve(&mut def, &BTreeSet::new())
        .unwrap()
        .header()
        .unwrap();
    assert_eq!(off, h);
}

#[test]
fn categories_are_not_read() {
    let text = play(
        r#""category": [{"name": {"include": "missing.json"}, "item": 5}],
        "property": [{"name": "a", "category": {"include": "missing.json"}}]"#,
    );
    assert_eq!(names(&header(&skin(&[("s.json", &text)])).unwrap()), ["a"]);
}

fn files(root: &std::path::Path, file: &str) -> Result<Vec<FileSpec>, ImportError> {
    common::header_of(root, file).map(|h| h.files)
}

fn candidates(path: &str) -> Vec<String> {
    let d = skin(&[
        ("bg/b.png", ""),
        ("bg/a.png", ""),
        ("bg/C.PNG", ""),
        ("bg/d.Png", ""),
        ("bg/x.txt", ""),
        ("bg/dir.png/f", ""),
        ("custom/one/main.png", ""),
        ("custom/two/main.png", ""),
        ("custom/f.json", ""),
    ]);
    let text = play(&format!(
        r#""filepath": [{{"name": "n", "path": "{path}", "def": "a"}}]"#
    ));
    common::write(d.path(), "s.json", text.as_bytes());
    let f = files(d.path(), "s.json").unwrap();
    assert_eq!(f[0].name.as_deref(), Some("n"));
    assert_eq!(f[0].def.as_deref(), Some("a"));
    assert_eq!(f[0].path.as_deref(), Some(path));
    f[0].candidates.clone()
}

#[test]
fn file_choices_list_matching_entries_sorted() {
    assert_eq!(
        candidates("bg/*.png"),
        ["C.PNG", "a.png", "b.png", "d.Png", "dir.png"]
    );
    // Case is ignored in every letter of the pattern.
    assert_eq!(candidates("bg/*.pNg"), candidates("bg/*.png"));
    assert_eq!(candidates("bg/a*"), ["a.png"]);
    // A folder choice.
    assert_eq!(candidates("custom/*"), ["f.json", "one", "two"]);
    // A folder that does not exist, or is a file, offers nothing.
    assert!(candidates("none/*.png").is_empty());
    assert!(candidates("bg/x.txt/*").is_empty());
}

#[test]
fn file_choices_resolve_from_the_definition_folder() {
    let d = skin(&[
        ("_font/x.ttf", ""),
        ("_font/y.ttf", ""),
        (
            "play/s.json",
            &play(r#""filepath": [{"path": "../_font/*.ttf"}]"#),
        ),
        (
            "play/t.json",
            &play(r#""filepath": [{"path": "../../x/*.ttf"}]"#),
        ),
        (
            "play/u.json",
            &play(r#""filepath": [{"path": "/x/*.ttf"}]"#),
        ),
    ]);
    assert_eq!(
        files(d.path(), "play/s.json").unwrap()[0].candidates,
        ["x.ttf", "y.ttf"]
    );
    // A file choice declared in an included fragment resolves from the
    // entry file's folder, not the fragment's.
    let fragment = skin(&[
        ("play/z.ttf", ""),
        ("play/parts/w.ttf", ""),
        (
            "play/s.json",
            &play(r#""filepath": {"include": "parts/f.json"}"#),
        ),
        ("play/parts/f.json", r#"[{"path": "*.ttf"}]"#),
    ]);
    assert_eq!(
        files(fragment.path(), "play/s.json").unwrap()[0].candidates,
        ["z.ttf"]
    );
    for (file, want) in [("play/t.json", "underflow"), ("play/u.json", "rejected")] {
        match files(d.path(), file) {
            Err(ImportError::Read {
                at: Some(_),
                source,
                ..
            }) => match (want, source) {
                ("underflow", RootError::Underflow) | ("rejected", RootError::Rejected) => {}
                (_, other) => panic!("{file}: {other:?}"),
            },
            other => panic!("{file}: {other:?}"),
        }
    }
}

#[test]
fn file_choices_allow_wildcards_in_the_last_component_only() {
    for path in [
        "*/main.png",
        "a/*/b.png",
        "bg/a?.png",
        "bg/[ab].png",
        "bg/{a,b}",
        "bg/*.png|x|",
    ] {
        let text = play(&format!(r#""filepath": [{{"path": "{path}"}}]"#));
        assert!(
            matches!(
                header(&skin(&[("s.json", &text)])),
                Err(ImportError::Pattern { .. })
            ),
            "{path}"
        );
    }
}

#[test]
fn file_choice_names_are_checked_whole_before_listing() {
    let long_last = format!("bg/{}*.png", "a".repeat(skin_model::MAX_PATH_BYTES));
    let joined_long = format!("{}*", "a".repeat(skin_model::MAX_PATH_BYTES - 1));
    for (file, path) in [
        ("s.json", "C:*.png"),
        ("s.json", "c:bg/*.png"),
        ("s.json", "bg/x:*.png"),
        ("s.json", long_last.as_str()),
        // Within the bound as written, over it joined to `play`.
        ("play/s.json", joined_long.as_str()),
    ] {
        let text = play(&format!(r#""filepath": [{{"path": "{path}"}}]"#));
        let d = skin(&[(file, &text), ("bg/a.png", ""), ("play/a.png", "")]);
        match common::header_of(d.path(), file) {
            Err(ImportError::Read {
                source: RootError::Rejected,
                ..
            }) => {}
            other => panic!("{path}: {other:?}"),
        }
    }
}

#[test]
fn file_choices_do_not_list_folders_outside_the_skin() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("skin");
    common::write(&outer.path().join("away"), "x.png", b"");
    common::write(
        &root,
        "s.json",
        play(r#""filepath": [{"path": "out/*.png"}]"#).as_bytes(),
    );
    common::link_dir(&root.join("out"), &outer.path().join("away"));
    match common::header_of(&root, "s.json") {
        Err(ImportError::Read {
            source: RootError::Escapes,
            ..
        }) => {}
        other => panic!("{other:?}"),
    }
}
