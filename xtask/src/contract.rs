//! `cargo xtask check-contract`: every clause of design.md "Behavioral
//! Contract" names design.md headings and tests that exist, and every row
//! of design.md "Threats and Mitigations" has its own threat ID.
//!
//! A clause is a bullet of the contract section, one line in the form
//! `- <clause with SHALL> ("<heading>"[, ...]). Spec: `<file>::<test>`[, ...].`
//! Each quoted heading must be a design.md heading, so renaming a cited
//! heading fails the check, as does renaming the contract or threat
//! section itself. `<file>` is relative to the repository root and must
//! define `fn <test>` exactly once, with `#[test]` and without `#[ignore]`
//! in the run of comments and attributes directly above it. The threat
//! table's first column is `ID`, and each row's ID is `T` and two digits,
//! used once.
//!
//! Everything is read as text, line by line: design.md's headings (outside
//! code fences, each closed by its own marker), the contract section's
//! bullets, the threat table (rows indented up to three spaces), and the
//! cited files. A cited `fn` is not proven to be compiled: one inside a
//! raw string, a macro never invoked or a `cfg` that removes it still
//! counts. A citation names a file, not a module path, and an attribute
//! spelled unusually (`#[ignore/* c */]`) is not recognized. CI building
//! and running the tests, and review, cover those.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use anyhow::Context as _;

/// The design document, relative to the workspace root.
pub const DESIGN: &str = "docs/design.md";
/// The level-2 heading of the clauses.
pub const CONTRACT: &str = "Behavioral Contract";
/// The level-3 heading over the threat table.
pub const THREATS: &str = "Threats and Mitigations";

pub fn run(manifest: &Path) -> anyhow::Result<Vec<String>> {
    let root = manifest
        .parent()
        .context("the workspace manifest has no directory")?;
    let design = std::fs::read_to_string(root.join(DESIGN))
        .with_context(|| format!("cannot read {DESIGN}"))?;
    Ok(check(&design, |file| {
        std::fs::read_to_string(root.join(file))
    }))
}

/// The diagnostics for `design`, reading each cited file through `read`.
pub fn check(design: &str, read: impl Fn(&str) -> io::Result<String>) -> Vec<String> {
    let lines: Vec<&str> = design.lines().collect();
    let headings = headings(&lines);
    let mut diags = Vec::new();
    contract(&lines, &headings, &read, &mut diags);
    threats(&lines, &headings, &mut diags);
    diags
}

/// A heading: its line index, level and text.
struct Heading<'a> {
    line: usize,
    level: usize,
    text: &'a str,
}

/// `line` without the up to three spaces of indentation a block may have.
fn unindent(line: &str) -> &str {
    let spaces = line.bytes().take(4).take_while(|&b| b == b' ').count();
    line.get(spaces.min(3)..).unwrap_or(line)
}

/// A code fence line's marker and its length: three or more backticks or
/// tildes.
fn fence(line: &str) -> Option<(u8, usize)> {
    let line = unindent(line);
    let marker = *line
        .as_bytes()
        .first()
        .filter(|&&b| b == b'`' || b == b'~')?;
    let len = line.bytes().take_while(|&b| b == marker).count();
    (len >= 3).then_some((marker, len))
}

/// The ATX headings outside code fences. A fence closes on a line of its
/// own marker, at least as long as the opening one and nothing after it.
fn headings<'a>(lines: &[&'a str]) -> Vec<Heading<'a>> {
    let mut out = Vec::new();
    let mut open: Option<(u8, usize)> = None;
    for (line, text) in lines.iter().enumerate() {
        if let Some((marker, len)) = open {
            let closes = fence(text).is_some_and(|(m, l)| {
                m == marker
                    && l >= len
                    && unindent(text).get(l..).is_some_and(|r| r.trim().is_empty())
            });
            if closes {
                open = None;
            }
            continue;
        }
        if let Some(f) = fence(text) {
            open = Some(f);
            continue;
        }
        let level = text.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&level)
            && let Some(rest) = text[level..].strip_prefix(' ')
        {
            out.push(Heading {
                line,
                level,
                text: rest.trim(),
            });
        }
    }
    out
}

/// The line range of the section under the heading `text` at `level`, up
/// to the next heading at that level or above.
fn section(headings: &[Heading], level: usize, text: &str, end: usize) -> Option<(usize, usize)> {
    let i = headings
        .iter()
        .position(|h| h.level == level && h.text == text)?;
    let start = headings.get(i)?.line + 1;
    let stop = headings
        .iter()
        .skip(i + 1)
        .find(|h| h.level <= level)
        .map_or(end, |h| h.line);
    Some((start, stop))
}

fn at(line: usize) -> String {
    format!("{DESIGN}:{}", line + 1)
}

fn contract(
    lines: &[&str],
    headings: &[Heading],
    read: &impl Fn(&str) -> io::Result<String>,
    diags: &mut Vec<String>,
) {
    let Some((start, stop)) = section(headings, 2, CONTRACT, lines.len()) else {
        diags.push(format!("{DESIGN}: no \"## {CONTRACT}\" section"));
        return;
    };
    let mut files = BTreeMap::new();
    for (i, line) in lines.iter().enumerate().take(stop).skip(start) {
        if let Some(clause) = line.strip_prefix("- ") {
            check_clause(i, clause, headings, read, &mut files, diags);
        }
    }
}

fn check_clause(
    line: usize,
    clause: &str,
    headings: &[Heading],
    read: &impl Fn(&str) -> io::Result<String>,
    files: &mut BTreeMap<String, io::Result<String>>,
    diags: &mut Vec<String>,
) {
    let at = at(line);
    if !clause
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "SHALL")
    {
        diags.push(format!("{at}: the clause has no SHALL"));
    }
    let Some((claim, spec)) = clause.split_once("Spec:") else {
        diags.push(format!("{at}: the clause has no `Spec:`"));
        return;
    };
    match sections(claim.trim_end()) {
        Some(names) => {
            for name in names {
                if !headings.iter().any(|h| h.text == name) {
                    diags.push(format!("{at}: \"{name}\" is not a design.md heading"));
                }
            }
        }
        None => diags.push(format!(
            "{at}: the clause does not end with its headings, (\"<heading>\", ...), before `Spec:`"
        )),
    }
    let Some(citations) = citations(spec.trim()) else {
        diags.push(format!(
            "{at}: `Spec:` is not a list of `<file>::<test>` separated by \", \" and ending in \".\""
        ));
        return;
    };
    if citations.is_empty() {
        diags.push(format!("{at}: the clause cites no test"));
    }
    for (file, test) in citations {
        let source = files.entry(file.to_owned()).or_insert_with(|| read(file));
        match source {
            Ok(source) => check_test(&at, file, test, source, diags),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                diags.push(format!("{at}: `{file}` does not exist"));
            }
            Err(e) => diags.push(format!("{at}: cannot read `{file}`: {e}")),
        }
    }
}

/// The quoted headings of `("<heading>", ...).` at the end of `claim`.
fn sections(claim: &str) -> Option<Vec<&str>> {
    let open = claim.rfind("(\"")?;
    let inner = claim.strip_suffix(").")?.get(open + 1..)?;
    // `"a", "b"` splits into "", a, ", ", b, "".
    let parts: Vec<&str> = inner.split('"').collect();
    let n = parts.len();
    let shaped = n >= 3
        && n % 2 == 1
        && parts.iter().enumerate().all(|(i, part)| {
            if i == 0 || i == n - 1 {
                part.is_empty()
            } else if i % 2 == 0 {
                *part == ", "
            } else {
                !part.is_empty()
            }
        });
    shaped.then(|| parts.iter().skip(1).step_by(2).copied().collect())
}

/// The `(file, test)` pairs of `` `file::test`, ... . ``, or `None` when
/// malformed. An empty list (`.` alone) gives `Some([])`.
fn citations(spec: &str) -> Option<Vec<(&str, &str)>> {
    let list = spec.strip_suffix('.')?;
    if list.is_empty() {
        return Some(Vec::new());
    }
    list.split(", ")
        .map(|c| {
            let (file, test) = c.strip_prefix('`')?.strip_suffix('`')?.rsplit_once("::")?;
            let word = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
            (word(file) && word(test) && !test.contains('`')).then_some((file, test))
        })
        .collect()
}

/// Checks that `source` defines `fn test` once, as a test that is not
/// ignored.
fn check_test(at: &str, file: &str, test: &str, source: &str, diags: &mut Vec<String>) {
    let lines: Vec<&str> = source.lines().collect();
    let defs: Vec<usize> = (0..lines.len())
        .filter(|&i| lines.get(i).is_some_and(|l| defines(l, test)))
        .collect();
    let [def] = defs[..] else {
        if defs.is_empty() {
            diags.push(format!("{at}: `{file}` has no fn `{test}`"));
        } else {
            let numbers: Vec<String> = defs.iter().map(|i| (i + 1).to_string()).collect();
            diags.push(format!(
                "{at}: `{file}` defines `{test}` more than once (lines {})",
                numbers.join(", ")
            ));
        }
        return;
    };
    let attrs = attributes(&lines, def);
    let place = format!("`{file}:{}`", def + 1);
    if !attrs.iter().any(|a| a == "#[test]") {
        diags.push(format!("{at}: {place} `{test}` is not a #[test]"));
    }
    if attrs.iter().any(|a| {
        a.strip_prefix("#[ignore")
            .is_some_and(|r| r.starts_with([']', ' ', '=']))
    }) {
        diags.push(format!("{at}: {place} `{test}` is #[ignore]d"));
    }
}

/// Whether `line` starts the definition of `fn name`.
fn defines(line: &str, name: &str) -> bool {
    let mut rest = line.trim_start();
    'prefixes: loop {
        for prefix in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "async ",
            "const ",
            "unsafe ",
        ] {
            if let Some(r) = rest.strip_prefix(prefix) {
                rest = r;
                continue 'prefixes;
            }
        }
        break;
    }
    rest.strip_prefix("fn ")
        .and_then(|r| r.strip_prefix(name))
        .is_some_and(|r| r.starts_with(['(', '<']))
}

/// The attributes, one string each, in the run of comments and attributes
/// directly above line `def`. An attribute over several lines ends with a
/// line ending in `]` and starts at the nearest `#[` above it.
fn attributes(lines: &[&str], def: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = def;
    while let Some(prev) = i.checked_sub(1) {
        let Some(line) = lines.get(prev).map(|l| l.trim()) else {
            break;
        };
        if line.starts_with("//") {
            i = prev;
        } else if line.starts_with("#[") {
            out.push(line.to_owned());
            i = prev;
        } else if line.ends_with(']') {
            // The last line of a multi-line attribute.
            let Some(open) = (0..prev)
                .rev()
                .find(|&j| lines.get(j).is_some_and(|l| l.trim().starts_with("#[")))
            else {
                break;
            };
            let joined: Vec<&str> = (open..=prev)
                .filter_map(|j| lines.get(j).map(|l| l.trim()))
                .collect();
            out.push(joined.concat());
            i = open;
        } else {
            break;
        }
    }
    out
}

fn threats(lines: &[&str], headings: &[Heading], diags: &mut Vec<String>) {
    let Some((start, stop)) = section(headings, 3, THREATS, lines.len()) else {
        diags.push(format!("{DESIGN}: no \"### {THREATS}\" section"));
        return;
    };
    let is_row = |i: usize| lines.get(i).is_some_and(|l| unindent(l).starts_with('|'));
    let Some(table) = (start..stop).find(|&i| is_row(i)) else {
        diags.push(format!("{DESIGN}: no table under \"### {THREATS}\""));
        return;
    };
    let first_cell = |i: usize| {
        lines
            .get(i)
            .and_then(|l| l.trim().strip_prefix('|'))
            .and_then(|l| l.split('|').next())
            .map(str::trim)
    };
    if first_cell(table) != Some("ID") {
        diags.push(format!(
            "{}: the threat table's first column is not \"ID\"",
            at(table)
        ));
        return;
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    // The header and its separator row come first.
    let rows = (table + 2..stop).take_while(|&i| is_row(i));
    for i in rows {
        let id = first_cell(i).unwrap_or_default();
        let well_formed = id
            .strip_prefix('T')
            .is_some_and(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()));
        if id.is_empty() {
            diags.push(format!("{}: the threat row has no ID", at(i)));
        } else if !well_formed {
            diags.push(format!(
                "{}: threat ID `{id}` is not `T` and two digits",
                at(i)
            ));
        } else if let Some(&first) = seen.get(id) {
            diags.push(format!(
                "{}: threat ID `{id}` is already used on line {}",
                at(i),
                first + 1
            ));
        } else {
            seen.insert(id, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::check;

    const TESTS: &str = "crates/a/tests/a.rs";

    /// A design.md excerpt that passes, citing two tests of [`TESTS`].
    const DESIGN: &str = "\
# Design

## Watermark

The watermark prose.

```toml
# Fenced
```

### Threats and Mitigations

| ID | Attack vector | Threat |
| --- | --- | --- |
| T01 | Chart files | DoS |
| T02 | Archives | zip slip |

Notes on the table.

## Behavioral Contract

Each clause restates a guarantee from the section it names.

- WHEN a play is evaluated, the evaluator SHALL judge each note once (\"Watermark\"). Spec: `crates/a/tests/a.rs::judged_once`, `crates/a/tests/a.rs::in_a_proptest`.
- IF a record is newer THEN decoding SHALL refuse it (\"Watermark\", \"Threats and Mitigations\"). Spec: `crates/a/tests/a.rs::with_a_long_attribute`.

## Roadmap
";

    /// The cited test file: a unit test, a helper, a proptest body, an
    /// ignored test and a test under a multi-line attribute.
    const SOURCE: &str = "\
mod tests {
    #[test]
    fn judged_once() {}

    fn helper() {}
}

proptest! {
    /// Generated.
    #[test]
    fn in_a_proptest(x in 0..1) {}
}

#[test]
#[ignore = \"slow\"]
fn ignored() {}

#[expect(
    clippy::unwrap_used,
    reason = \"a fixture\"
)]
#[test]
fn with_a_long_attribute() {}
";

    fn run(design: &str, source: &str) -> Vec<String> {
        check(design, |file| {
            if file == TESTS {
                Ok(source.to_owned())
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        })
    }

    /// `DESIGN` with `from` replaced by `to`, which must occur once.
    fn design_with(from: &str, to: &str) -> String {
        assert_eq!(DESIGN.matches(from).count(), 1, "{from}");
        DESIGN.replace(from, to)
    }

    /// The first clause's citation of `judged_once` replaced by `to`.
    fn citing(to: &str) -> String {
        design_with("`crates/a/tests/a.rs::judged_once`", to)
    }

    const FIRST: &str = "docs/design.md:24";

    #[test]
    fn the_valid_fixture_passes() {
        assert_eq!(run(DESIGN, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_missing_file_and_an_absent_name_are_reported() {
        assert_eq!(
            run(&citing("`crates/a/tests/b.rs::judged_once`"), SOURCE),
            [format!("{FIRST}: `crates/a/tests/b.rs` does not exist")]
        );
        assert_eq!(
            run(&citing("`crates/a/tests/a.rs::judged_twice`"), SOURCE),
            [format!(
                "{FIRST}: `crates/a/tests/a.rs` has no fn `judged_twice`"
            )]
        );
    }

    #[test]
    fn a_fn_without_test_and_an_ignored_test_are_reported() {
        assert_eq!(
            run(&citing("`crates/a/tests/a.rs::helper`"), SOURCE),
            [format!(
                "{FIRST}: `crates/a/tests/a.rs:5` `helper` is not a #[test]"
            )]
        );
        assert_eq!(
            run(&citing("`crates/a/tests/a.rs::ignored`"), SOURCE),
            [format!(
                "{FIRST}: `crates/a/tests/a.rs:16` `ignored` is #[ignore]d"
            )]
        );
    }

    #[test]
    fn a_name_defined_twice_is_ambiguous() {
        let source = format!("{SOURCE}\nmod more {{\n    fn judged_once() {{}}\n}}\n");
        assert_eq!(
            run(DESIGN, &source),
            [format!(
                "{FIRST}: `crates/a/tests/a.rs` defines `judged_once` more than once (lines 3, 26)"
            )]
        );
    }

    #[test]
    fn a_clause_without_spec_or_shall_is_reported() {
        let no_spec = design_with(
            ". Spec: `crates/a/tests/a.rs::judged_once`, `crates/a/tests/a.rs::in_a_proptest`.",
            ".",
        );
        assert_eq!(
            run(&no_spec, SOURCE),
            [format!("{FIRST}: the clause has no `Spec:`")]
        );
        let no_shall = design_with("SHALL judge", "judges");
        assert_eq!(
            run(&no_shall, SOURCE),
            [format!("{FIRST}: the clause has no SHALL")]
        );
    }

    #[test]
    fn a_clause_without_headings_or_tests_is_reported() {
        let no_headings = design_with("once (\"Watermark\").", "once.");
        assert_eq!(
            run(&no_headings, SOURCE),
            [format!(
                "{FIRST}: the clause does not end with its headings, (\"<heading>\", ...), before `Spec:`"
            )]
        );
        let no_tests = design_with(
            "Spec: `crates/a/tests/a.rs::judged_once`, `crates/a/tests/a.rs::in_a_proptest`.",
            "Spec: .",
        );
        assert_eq!(
            run(&no_tests, SOURCE),
            [format!("{FIRST}: the clause cites no test")]
        );
        assert_eq!(
            run(&citing("crates/a/tests/a.rs::judged_once"), SOURCE),
            [format!(
                "{FIRST}: `Spec:` is not a list of `<file>::<test>` separated by \", \" and ending in \".\""
            )]
        );
    }

    #[test]
    fn a_quoted_section_that_is_not_a_heading_is_reported() {
        // A renamed heading.
        let renamed = design_with("## Watermark\n", "## The Watermark\n");
        assert_eq!(
            run(&renamed, SOURCE),
            [
                format!("{FIRST}: \"Watermark\" is not a design.md heading"),
                "docs/design.md:25: \"Watermark\" is not a design.md heading".to_owned(),
            ]
        );
        // The headings the check reads under.
        let contract = design_with("## Behavioral Contract", "## Contract");
        assert_eq!(
            run(&contract, SOURCE),
            ["docs/design.md: no \"## Behavioral Contract\" section"]
        );
        let threats = design_with("### Threats and Mitigations", "### Threats");
        assert_eq!(
            run(&threats, SOURCE),
            [
                "docs/design.md:25: \"Threats and Mitigations\" is not a design.md heading",
                "docs/design.md: no \"### Threats and Mitigations\" section",
            ]
        );
        // A `#` line in a code fence is not a heading.
        let fenced = design_with("once (\"Watermark\")", "once (\"Fenced\")");
        assert_eq!(
            run(&fenced, SOURCE),
            [format!("{FIRST}: \"Fenced\" is not a design.md heading")]
        );
    }

    #[test]
    fn missing_malformed_and_duplicate_threat_ids_are_reported() {
        assert_eq!(
            run(&design_with("| T02 |", "|  |"), SOURCE),
            ["docs/design.md:16: the threat row has no ID"]
        );
        assert_eq!(
            run(&design_with("| T02 |", "| T2 |"), SOURCE),
            ["docs/design.md:16: threat ID `T2` is not `T` and two digits"]
        );
        assert_eq!(
            run(&design_with("| T02 |", "| T01 |"), SOURCE),
            ["docs/design.md:16: threat ID `T01` is already used on line 15"]
        );
        assert_eq!(
            run(&design_with("| ID |", "| Key |"), SOURCE),
            ["docs/design.md:13: the threat table's first column is not \"ID\""]
        );
    }

    #[test]
    fn prose_outside_the_parsed_regions_does_not_matter() {
        let edited = design_with(
            "The watermark prose.",
            "Other prose, a \"Quoted\" name, `a.rs::b` and SHALL.\n\n## A New Section",
        );
        let edited = edited.replace("Notes on the table.", "- A bullet with no Spec.");
        let edited = edited.replace(
            "Each clause restates a guarantee from the section it names.",
            "Changed prose with Spec: `x.rs::y`.",
        );
        assert_eq!(run(&edited, SOURCE), Vec::<String>::new());
        let source = SOURCE.replace("fn helper() {}", "fn helper() {}\n    fn other() {}");
        assert_eq!(run(DESIGN, &source), Vec::<String>::new());
    }

    #[test]
    fn a_fence_closes_only_on_its_own_marker() {
        // A tilde fence holding a backtick line: the fence ends at the tilde line.
        let tilde = design_with(
            "The watermark prose.",
            "The watermark prose.\n\n~~~md\n```\n~~~",
        );
        assert_eq!(run(&tilde, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_heading_inside_a_longer_fence_is_not_a_heading() {
        let hidden = design_with(
            "The watermark prose.",
            "The watermark prose.\n\n````md\n```\n## Hidden\n```\n````",
        );
        let hidden = hidden.replace("once (\"Watermark\")", "once (\"Hidden\")");
        assert_eq!(
            run(&hidden, SOURCE),
            ["docs/design.md:30: \"Hidden\" is not a design.md heading"]
        );
    }

    #[test]
    fn an_indented_threat_row_is_still_checked() {
        assert_eq!(
            run(&design_with("| T02 |", "  |  |"), SOURCE),
            ["docs/design.md:16: the threat row has no ID"]
        );
    }

    #[test]
    fn shall_counts_only_as_a_whole_word() {
        assert_eq!(
            run(&design_with("SHALL judge", "SHALLOW judges"), SOURCE),
            [format!("{FIRST}: the clause has no SHALL")]
        );
    }
}
