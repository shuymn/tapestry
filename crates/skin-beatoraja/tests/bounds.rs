//! Each input bound at its cap and one past it, charged before the work it
//! guards.

mod common;

use std::fs::File;

use common::{header, limit, padded, play, skin, write};
use skin_beatoraja::InputLimit;
use skin_beatoraja::limits::{
    MAX_DEPTH, MAX_FILE_BYTES, MAX_FILE_CHOICES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES,
    MAX_INPUT_BYTES, MAX_LIST_ENTRIES, MAX_OFFSETS, MAX_OPTION_ITEMS, MAX_OPTIONS,
};

const FILE: usize = MAX_FILE_BYTES as usize;

/// A sparse file of `len` zero bytes: read, it would be a syntax error.
fn sparse(root: &std::path::Path, name: &str, len: u64) {
    File::create(root.join(name)).unwrap().set_len(len).unwrap();
}

#[test]
fn a_definition_file_is_bounded_from_its_metadata() {
    let d = skin(&[]);
    write(d.path(), "s.json", &padded(&play(r#""name": "x""#), FILE));
    assert_eq!(header(&d).unwrap().name.as_deref(), Some("x"));
    sparse(d.path(), "s.json", MAX_FILE_BYTES + 1);
    assert_eq!(limit(header(&d)), InputLimit::FileBytes);
    // A terabyte is refused as fast: it is never read.
    sparse(d.path(), "s.json", 1 << 40);
    assert_eq!(limit(header(&d)), InputLimit::FileBytes);
    // An included file too.
    write(
        d.path(),
        "s.json",
        play(r#""property": {"include": "a.json"}"#).as_bytes(),
    );
    sparse(d.path(), "a.json", MAX_FILE_BYTES + 1);
    assert_eq!(limit(header(&d)), InputLimit::FileBytes);
}

#[test]
fn all_input_is_bounded_with_repeated_includes_counted_again() {
    // The entry file and one file included twice, 32 MiB in all.
    let entry = play(r#""property": [{"include": "a.json"}, {"include": "a.json"}]"#);
    let d = skin(&[]);
    write(d.path(), "s.json", &padded(&entry, 1_024));
    let half = (MAX_INPUT_BYTES as usize - 1_024) / 2;
    write(d.path(), "a.json", &padded("[]", half));
    header(&d).unwrap();
    write(d.path(), "s.json", &padded(&entry, 1_025));
    assert_eq!(limit(header(&d)), InputLimit::InputBytes);
}

#[test]
fn a_read_is_offered_only_what_remains() {
    // 16 MiB + 15 MiB read, 1 MiB remains: a file of 1 MiB + 1 byte is
    // refused from its length, before it is read.
    let entry = play(r#""property": [{"include": "a.json"}, {"include": "b.json"}]"#);
    let d = skin(&[]);
    write(d.path(), "s.json", &padded(&entry, FILE));
    write(d.path(), "a.json", &padded("[]", FILE - (1 << 20)));
    write(d.path(), "b.json", &padded("[]", 1 << 20));
    header(&d).unwrap();
    sparse(d.path(), "b.json", (1 << 20) + 1);
    assert_eq!(limit(header(&d)), InputLimit::InputBytes);
}

#[test]
fn nesting_is_bounded_across_includes() {
    // The include object is at depth 2, so its file's root is at 3.
    let entry = play(r#""property": {"include": "d.json"}"#);
    let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let at_cap = MAX_DEPTH as usize - 2;
    let d = skin(&[("s.json", &entry), ("d.json", &nested(at_cap))]);
    header(&d).unwrap();
    write(d.path(), "d.json", nested(at_cap + 1).as_bytes());
    assert_eq!(limit(header(&d)), InputLimit::Depth);
    // In the entry file alone, unknown fields included.
    let deep = |n: usize| play(&format!(r#""x": {}"#, nested(n)));
    let d = skin(&[("s.json", &deep(MAX_DEPTH as usize - 1))]);
    header(&d).unwrap();
    write(d.path(), "s.json", deep(MAX_DEPTH as usize).as_bytes());
    assert_eq!(limit(header(&d)), InputLimit::Depth);
}

/// An entry file whose options are a chain of `n` includes.
fn chain(n: u32) -> tempfile::TempDir {
    let d = skin(&[("s.json", &play(r#""property": {"include": "i1.json"}"#))]);
    for i in 1..n {
        let next = format!(r#"{{"include": "i{}.json"}}"#, i + 1);
        write(d.path(), &format!("i{i}.json"), next.as_bytes());
    }
    write(d.path(), &format!("i{n}.json"), br#"[{"name": "end"}]"#);
    d
}

#[test]
fn include_depth_is_reserved_before_reading() {
    assert_eq!(
        common::names(&header(&chain(MAX_INCLUDE_DEPTH)).unwrap()),
        ["end"]
    );
    let d = chain(MAX_INCLUDE_DEPTH + 1);
    // The 17th file is a FIFO: opening it would block.
    let last = d.path().join(format!("i{}.json", MAX_INCLUDE_DEPTH + 1));
    std::fs::remove_file(&last).unwrap();
    #[cfg(unix)]
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&last)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(limit(header(&d)), InputLimit::IncludeDepth);
}

#[test]
fn include_parses_are_reserved_before_reading() {
    let items = |n: u32| {
        let one = r#"{"include": "p.json"}"#;
        play(&format!(
            r#""property": [{}]"#,
            vec![one; n as usize].join(",")
        ))
    };
    let d = skin(&[("s.json", &items(MAX_INCLUDE_PARSES)), ("p.json", "[]")]);
    header(&d).unwrap();
    write(d.path(), "s.json", items(MAX_INCLUDE_PARSES + 1).as_bytes());
    assert_eq!(limit(header(&d)), InputLimit::IncludeParses);
    // Each resolution counts again from the entry file.
    let d = skin(&[("s.json", &items(MAX_INCLUDE_PARSES)), ("p.json", "[]")]);
    let (mut def, _) = skin_beatoraja::read_header(d.path(), "s.json").unwrap();
    skin_beatoraja::resolve(&mut def, &Default::default()).unwrap();
}

#[test]
fn folder_entries_are_counted_across_listings_before_filtering() {
    let d = skin(&[]);
    // Non-matching entries count too.
    for i in 0..MAX_LIST_ENTRIES / 2 {
        write(d.path(), &format!("a/{i}.txt"), b"");
        write(d.path(), &format!("b/{i}.txt"), b"");
    }
    let text = play(r#""filepath": [{"path": "a/*.png"}, {"path": "b/*.png"}]"#);
    write(d.path(), "s.json", text.as_bytes());
    let h = header(&d).unwrap();
    assert!(h.files.iter().all(|f| f.candidates.is_empty()));
    write(d.path(), "b/one-more.txt", b"");
    assert_eq!(limit(header(&d)), InputLimit::ListEntries);
}

/// A skin with `n` records in `field`, each `record`.
fn records(field: &str, record: &str, n: usize) -> tempfile::TempDir {
    let list = vec![record; n].join(",");
    skin(&[("s.json", &play(&format!(r#""{field}": [{list}]"#)))])
}

#[test]
fn header_records_are_counted_before_they_are_added() {
    for (field, record, cap, bound) in [
        ("property", "{}", MAX_OPTIONS, InputLimit::Options),
        (
            "filepath",
            r#"{"path": "none/*"}"#,
            MAX_FILE_CHOICES,
            InputLimit::FileChoices,
        ),
        ("offset", "{}", MAX_OFFSETS, InputLimit::Offsets),
    ] {
        header(&records(field, record, cap)).unwrap();
        assert_eq!(
            limit(header(&records(field, record, cap + 1))),
            bound,
            "{field}"
        );
    }
    let per = MAX_OPTION_ITEMS / 32;
    let items = vec![r#"{"op": 1}"#; 32].join(",");
    let option = format!(r#"{{"item": [{items}]}}"#);
    let h = header(&records("property", &option, per)).unwrap();
    assert_eq!(
        h.options.iter().map(|o| o.items.len()).sum::<usize>(),
        MAX_OPTION_ITEMS
    );
    let text = play(&format!(
        r#""property": [{}, {{"item": [{{}}]}}]"#,
        vec![option.as_str(); per].join(",")
    ));
    assert_eq!(
        limit(header(&skin(&[("s.json", &text)]))),
        InputLimit::OptionItems
    );
}
