//! `cargo xtask banned-words (--staged|--all) [--list <path>]`: fails when a
//! term from `banned-words.txt` appears in a path or in file contents.
//!
//! - `--staged` (pre-commit) checks the index entries that differ from
//!   `HEAD`; `--all` (CI) checks every index entry. Both read paths, contents,
//!   and the list itself from the index, never from the working copy.
//!   `--list <path>` reads the list from a file instead (tests).
//! - Contents with a UTF-8 or UTF-16 BOM are decoded by it. Otherwise,
//!   contents with a NUL byte in the first 8 KiB are binary and skipped (the
//!   path is still checked), and contents that are not UTF-8 are decoded as
//!   Shift_JIS.
//! - Diagnostics never print a term, and replace the matched part of a path
//!   with `<banned term #N>`. Columns count characters of the NFKC line.
//!
//! Exit codes: 0 clean, 1 findings, 2 usage, list, or Git error.

mod git;
mod matcher;
mod normalize;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use matcher::{Hit, Term};
use normalize::nfkc;

/// The list file (repository root).
const LIST_PATH: &str = "banned-words.txt";
/// Paths whose contents are not checked. Their paths still are.
const CONTENT_EXEMPT: &[&str] = &[
    LIST_PATH,
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "Cargo.lock",
    "mise.lock",
    "flake.lock",
];
/// Top-level directories whose contents are not checked.
const CONTENT_EXEMPT_DIRS: &[&str] = &["third_party/", "vendor/"];
/// How much of a file is searched for NUL to detect binary contents.
const BINARY_SNIFF: usize = 8 * 1024;

const USAGE: &str = "usage: cargo xtask banned-words (--staged|--all) [--list <path>]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Staged,
    All,
}

pub fn run(args: &[OsString]) -> ExitCode {
    let mut mode = None;
    let mut list = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--staged") if mode.is_none() => mode = Some(Mode::Staged),
            Some("--all") if mode.is_none() => mode = Some(Mode::All),
            Some("--list") if list.is_none() => match args.next() {
                Some(path) => list = Some(PathBuf::from(path)),
                None => return usage(),
            },
            _ => return usage(),
        }
    }
    let Some(mode) = mode else {
        return usage();
    };
    match check(mode, list.as_deref()) {
        Ok((diagnostics, files)) if diagnostics.is_empty() => {
            eprintln!("banned-words: ok ({files} file(s) checked)");
            ExitCode::SUCCESS
        }
        Ok((diagnostics, _)) => {
            for d in &diagnostics {
                eprintln!("{d}");
            }
            eprintln!("banned-words: {} finding(s)", diagnostics.len());
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("banned-words: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// Returns the diagnostics and the number of files checked.
fn check(mode: Mode, list: Option<&Path>) -> anyhow::Result<(Vec<String>, usize)> {
    let root = git::toplevel()?;
    let entries = git::index_entries(&root)?;

    let (list_name, list_bytes) = match list {
        Some(path) => (path.display().to_string(), read_list_file(path)?),
        None => {
            let entry = entries
                .iter()
                .find(|e| e.stage == 0 && e.path == LIST_PATH)
                .with_context(|| format!("{LIST_PATH} is not in the index"))?;
            let mut blobs = git::read_blobs(&root, &[entry.oid.as_str()])?;
            (LIST_PATH.to_owned(), blobs.remove(0))
        }
    };
    let list_text =
        String::from_utf8(list_bytes).with_context(|| format!("{list_name} is not UTF-8"))?;
    let terms = matcher::parse_list(&list_name, &list_text).map_err(anyhow::Error::msg)?;

    let selected: Vec<&git::Entry> = match mode {
        Mode::All => entries.iter().collect(),
        Mode::Staged => {
            let staged = git::staged_paths(&root)?;
            entries
                .iter()
                .filter(|e| staged.contains(&e.path))
                .collect()
        }
    };
    let is_readable = |e: &git::Entry| !e.is_gitlink() && !is_content_exempt(&e.path);
    let oids: Vec<&str> = selected
        .iter()
        .filter(|e| is_readable(e))
        .map(|e| e.oid.as_str())
        .collect();
    let mut blobs = git::read_blobs(&root, &oids)?.into_iter();

    let mut diagnostics = Vec::new();
    for entry in &selected {
        let contents = if is_readable(entry) {
            blobs.next()
        } else {
            None
        };
        diagnostics.extend(check_file(
            &terms,
            &list_name,
            &entry.path,
            contents.as_deref(),
        ));
    }
    Ok((diagnostics, selected.len()))
}

/// The list file `--list` names.
fn read_list_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
}

fn is_content_exempt(path: &str) -> bool {
    CONTENT_EXEMPT.contains(&path) || CONTENT_EXEMPT_DIRS.iter().any(|d| path.starts_with(d))
}

/// Diagnostics for one file: its path, then its contents unless they are
/// `None`, exempt, or binary.
fn check_file(terms: &[Term], list_name: &str, path: &str, contents: Option<&[u8]>) -> Vec<String> {
    let label = |hit: &Hit| {
        format!(
            "banned term #{} ({list_name}:{})",
            hit.term + 1,
            terms[hit.term].line
        )
    };
    let mut diagnostics = Vec::new();

    let normalized_path = nfkc(path);
    let path_hits = matcher::find(terms, &normalized_path);
    let shown_path = if path_hits.is_empty() {
        path.to_owned()
    } else {
        redact(&normalized_path, &path_hits)
    };
    for hit in &path_hits {
        diagnostics.push(format!("{shown_path}:0:0: {}", label(hit)));
    }

    let Some(bytes) = contents else {
        return diagnostics;
    };
    if is_content_exempt(path) {
        return diagnostics;
    }
    let Some(text) = decode(bytes) else {
        return diagnostics;
    };
    // The whole text at once, so a line ending between the words of a term
    // (wrapped prose) is a separator like any other.
    let text = nfkc(&text);
    for hit in matcher::find(terms, &text) {
        let before = &text[..hit.start];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let line = before.matches('\n').count() + 1;
        let col = before[line_start..].chars().count() + 1;
        diagnostics.push(format!("{shown_path}:{line}:{col}: {}", label(&hit)));
    }
    diagnostics
}

/// Text of a file: by its BOM (UTF-8, UTF-16LE, UTF-16BE) if it has one;
/// otherwise `None` if binary (a NUL in the first 8 KiB), UTF-8 if valid, and
/// Shift_JIS (lossy) if not.
fn decode(bytes: &[u8]) -> Option<std::borrow::Cow<'_, str>> {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return Some(encoding.decode_without_bom_handling(&bytes[bom..]).0);
    }
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return None;
    }
    Some(match std::str::from_utf8(bytes) {
        Ok(s) => s.into(),
        Err(_) => encoding_rs::SHIFT_JIS.decode_without_bom_handling(bytes).0,
    })
}

/// `text` with each matched range (overlaps merged) replaced by
/// `<banned term #N>`.
fn redact(text: &str, hits: &[Hit]) -> String {
    let mut out = String::new();
    let mut pos = 0;
    let mut hits = hits.iter().peekable();
    while let Some(hit) = hits.next() {
        let mut end = hit.end;
        while let Some(next) = hits.next_if(|h| h.start < end) {
            end = end.max(next.end);
        }
        out.push_str(&text[pos..hit.start]);
        out.push_str(&format!("<banned term #{}>", hit.term + 1));
        pos = end;
    }
    out.push_str(&text[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# header\nzorblax\nquux corp\nテストメイ\n";

    fn check(path: &str, contents: Option<&[u8]>) -> Vec<String> {
        let terms = matcher::parse_list(LIST_PATH, LIST).unwrap();
        check_file(&terms, LIST_PATH, path, contents)
    }

    fn assert_no_term(diagnostics: &[String]) {
        for d in diagnostics {
            let lower = d.to_lowercase();
            assert!(
                !lower.contains("zorblax") && !lower.contains("quux") && !d.contains("テストメイ"),
                "term leaked: {d}"
            );
        }
    }

    #[test]
    fn reports_content_location_without_the_term() {
        let d = check("notes.md", Some(b"ok\n  see Zorblax here\n"));
        assert_eq!(d, ["notes.md:2:7: banned term #1 (banned-words.txt:2)"]);
        assert_no_term(&d);
    }

    #[test]
    fn column_counts_characters_of_the_normalized_line() {
        let d = check("a.txt", "あい ｚｏｒｂｌａｘ".as_bytes().into());
        assert_eq!(d, ["a.txt:1:4: banned term #1 (banned-words.txt:2)"]);
    }

    #[test]
    fn redacts_the_matched_part_of_a_path() {
        let d = check("docs/newZorblaxPlayer.md", Some(b"clean\n"));
        assert_eq!(
            d,
            ["docs/new<banned term #1>Player.md:0:0: banned term #1 (banned-words.txt:2)"]
        );
        let d = check("ｄｏｃｓ/テストメイ/quux_corp.md", Some(b"zorblax\n"));
        assert_eq!(
            d,
            [
                "docs/<banned term #3>/<banned term #2>.md:0:0: banned term #3 (banned-words.txt:4)",
                "docs/<banned term #3>/<banned term #2>.md:0:0: banned term #2 (banned-words.txt:3)",
                "docs/<banned term #3>/<banned term #2>.md:1:1: banned term #1 (banned-words.txt:2)",
            ]
        );
        assert_no_term(&d);
    }

    #[test]
    fn redact_merges_overlapping_hits() {
        let hits = [
            Hit {
                term: 0,
                start: 2,
                end: 6,
            },
            Hit {
                term: 1,
                start: 4,
                end: 8,
            },
        ];
        assert_eq!(redact("ab0123456z", &hits), "ab<banned term #1>6z");
    }

    #[test]
    fn exempt_paths_skip_contents_only() {
        for path in [
            "banned-words.txt",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "Cargo.lock",
            "mise.lock",
            "flake.lock",
            "third_party/x/y.c",
            "vendor/z.rs",
        ] {
            assert_eq!(
                check(path, Some(b"zorblax\n")),
                Vec::<String>::new(),
                "{path}"
            );
        }
        assert_eq!(check("vendor/zorblax.rs", Some(b"")).len(), 1);
        assert_eq!(check("src/vendor/a.rs", Some(b"zorblax\n")).len(), 1);
        assert_eq!(check("docs/banned-words.txt", Some(b"zorblax\n")).len(), 1);
    }

    #[test]
    fn matches_across_line_endings_at_term_boundaries() {
        for text in ["x\nsee Quux\nCorp\n", "x\r\nsee Quux\r\nCorp\r\n"] {
            assert_eq!(
                check("a.md", Some(text.as_bytes())),
                ["a.md:2:5: banned term #2 (banned-words.txt:3)"],
                "{text:?}"
            );
        }
        assert_eq!(check("a.md", Some(b"zorb\nlax\n")), Vec::<String>::new());
        assert_eq!(
            check("a.md", Some(b"zorb\r\nlax\r\n")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn decodes_by_bom() {
        let mut utf16le = vec![0xFF, 0xFE];
        utf16le.extend(
            "x\r\nsee Zorblax\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let mut utf16be = vec![0xFE, 0xFF];
        utf16be.extend("see Zorblax\n".encode_utf16().flat_map(u16::to_be_bytes));
        let utf8 = b"\xEF\xBB\xBFZorblax\n";
        assert_eq!(
            check("out.txt", Some(&utf16le)),
            ["out.txt:2:5: banned term #1 (banned-words.txt:2)"]
        );
        assert_eq!(
            check("out.txt", Some(&utf16be)),
            ["out.txt:1:5: banned term #1 (banned-words.txt:2)"]
        );
        assert_eq!(
            check("out.txt", Some(utf8)),
            ["out.txt:1:1: banned term #1 (banned-words.txt:2)"]
        );
    }

    #[test]
    fn decodes_shift_jis() {
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode("#TITLE テストメイ\n");
        assert!(std::str::from_utf8(&bytes).is_err());
        let d = check("song.bms", Some(&bytes));
        assert_eq!(d, ["song.bms:1:8: banned term #3 (banned-words.txt:4)"]);
    }

    #[test]
    fn binary_contents_are_skipped_but_path_is_checked() {
        let mut bytes = b"\x89PNG\0".to_vec();
        bytes.extend_from_slice(b"zorblax");
        assert_eq!(check("a.png", Some(&bytes)), Vec::<String>::new());
        assert_eq!(check("zorblax.png", Some(&bytes)).len(), 1);
        // A NUL after the first 8 KiB does not make the file binary.
        let mut late = vec![b'a'; BINARY_SNIFF];
        late.extend_from_slice(b"\nzorblax\0\n");
        assert_eq!(check("late.txt", Some(&late)).len(), 1);
    }

    #[test]
    fn unreadable_list_file_keeps_the_io_error_as_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        let e = read_list_file(&path).unwrap_err();
        let io = e.root_cause().downcast_ref::<std::io::Error>();
        assert_eq!(
            io.map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound),
            "{e:?}"
        );
        assert_eq!(
            format!("{e:#}"),
            format!("cannot read {}: {}", path.display(), io.unwrap())
        );
    }

    #[test]
    fn gitlinks_check_only_the_path() {
        assert_eq!(check("deps/zorblax", None).len(), 1);
        assert_eq!(check("deps/other", None), Vec::<String>::new());
    }
}
