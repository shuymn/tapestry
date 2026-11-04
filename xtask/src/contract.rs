//! `cargo xtask check-contract`: every clause of design.md "Behavioral
//! Contract" names design.md headings and tests that exist, and every row
//! of design.md "Threats and Mitigations" has its own threat ID.
//!
//! A clause is a list item of the contract section, in the form
//! `- <clause with SHALL> ("<heading>"[, ...]). Spec: `<file>::<test>`[, ...].`
//! Each quoted heading must be a design.md heading, so renaming a cited
//! heading fails the check, as does renaming the contract or threat
//! section itself. `<file>` is relative to the repository root and must
//! define `fn <test>` exactly once, with `#[test]` and without `#[ignore]`
//! in the run of comments and attributes directly above it. The threat
//! table's first column is `ID`, and each row's ID is `T` and two digits,
//! used once.
//!
//! design.md is read with a CommonMark parser (`pulldown-cmark`, with
//! tables): its headings, the contract section's list items and the threat
//! table are what that parser returns, so code blocks, inline code and
//! other markdown elsewhere cannot change them. Only top-level blocks
//! count, outside block quotes, list items and footnotes. A heading is
//! keyed by its plain text (inline markup dropped, a code span by its
//! content), and a clause is a top-level list item's own text, without
//! the lists nested in it. The cited files are read
//! as text, line by line. A cited `fn` is not proven to be compiled: one
//! inside a raw string, a macro never invoked or a `cfg` that removes it
//! still counts. A citation names a file, not a module path, and an
//! attribute spelled unusually (`#[ignore/* c */]`) is not recognized. CI
//! building and running the tests, and review, cover those.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use anyhow::Context as _;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

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
    let doc = Doc::parse(design);
    let mut diags = Vec::new();
    contract(&doc, &read, &mut diags);
    threats(&doc, &mut diags);
    diags
}

/// A top-level heading: its line index, level and plain text.
struct Heading {
    line: usize,
    level: usize,
    text: String,
}

/// An item of a top-level list: its line index and its own inline text,
/// without the blocks nested in it.
struct Item {
    line: usize,
    text: String,
}

/// A top-level table: its line index, its header's first cell and each
/// body row's line index and first cell.
struct Table {
    line: usize,
    header: String,
    rows: Vec<(usize, String)>,
}

/// What the checker reads from design.md, as the CommonMark parser
/// returns it. Headings, list items and tables count only outside block
/// quotes, list items and footnotes.
struct Doc {
    headings: Vec<Heading>,
    items: Vec<Item>,
    tables: Vec<Table>,
}

/// Where the inline text of the current event goes.
enum Into {
    Nothing,
    Heading(usize, usize),
    Item,
    /// A table cell, and whether it is its row's first.
    Cell(bool),
}

impl Doc {
    fn parse(source: &str) -> Self {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let line_of = |offset: usize| starts.partition_point(|&s| s <= offset).saturating_sub(1);
        let mut doc = Doc {
            headings: Vec::new(),
            items: Vec::new(),
            tables: Vec::new(),
        };
        // Open block quotes, list items and footnotes.
        let mut containers = 0_usize;
        // Inside a top-level item: the depth of the blocks nested in it.
        let mut nested: Option<usize> = None;
        let mut into = Into::Nothing;
        let mut text = String::new();
        let mut column = 0_usize;
        for (event, range) in Parser::new_ext(source, Options::ENABLE_TABLES).into_offset_iter() {
            let line = line_of(range.start);
            match event {
                Event::Start(Tag::Heading { level, .. }) if containers == 0 => {
                    into = Into::Heading(line, level as usize);
                    text.clear();
                }
                Event::End(TagEnd::Heading(_)) => {
                    if let Into::Heading(line, level) = into {
                        doc.headings.push(Heading {
                            line,
                            level,
                            text: text.trim().to_owned(),
                        });
                        into = Into::Nothing;
                    }
                }
                Event::Start(Tag::Item) => {
                    if containers == 0 {
                        doc.items.push(Item {
                            line,
                            text: String::new(),
                        });
                        nested = Some(0);
                        into = Into::Item;
                    } else if let Some(depth) = nested.as_mut() {
                        *depth += 1;
                    }
                    containers += 1;
                }
                Event::End(TagEnd::Item) => {
                    containers = containers.saturating_sub(1);
                    match nested {
                        Some(0) => {
                            if let Some(item) = doc.items.last_mut() {
                                item.text = text.trim().to_owned();
                            }
                            text.clear();
                            nested = None;
                            into = Into::Nothing;
                        }
                        Some(depth) => nested = Some(depth - 1),
                        None => {}
                    }
                }
                Event::Start(Tag::List(_)) => {
                    if let Some(depth) = nested.as_mut() {
                        *depth += 1;
                    }
                }
                Event::End(TagEnd::List(_)) => {
                    if let Some(depth) = nested.as_mut()
                        && *depth > 0
                    {
                        *depth -= 1;
                    }
                }
                Event::Start(Tag::BlockQuote(_) | Tag::FootnoteDefinition(_)) => containers += 1,
                Event::End(TagEnd::BlockQuote(_) | TagEnd::FootnoteDefinition) => {
                    containers = containers.saturating_sub(1);
                }
                Event::Start(Tag::Table(_)) if containers == 0 => doc.tables.push(Table {
                    line,
                    header: String::new(),
                    rows: Vec::new(),
                }),
                Event::Start(Tag::TableHead) if containers == 0 => column = 0,
                Event::Start(Tag::TableRow) if containers == 0 => {
                    column = 0;
                    if let Some(t) = doc.tables.last_mut() {
                        t.rows.push((line, String::new()));
                    }
                }
                Event::Start(Tag::TableCell) if containers == 0 => {
                    into = Into::Cell(column == 0);
                    text.clear();
                    column += 1;
                }
                Event::End(TagEnd::TableCell) => {
                    if let (Into::Cell(true), Some(t)) = (&into, doc.tables.last_mut()) {
                        let cell = text.trim().to_owned();
                        match t.rows.last_mut() {
                            Some(row) => row.1 = cell,
                            None => t.header = cell,
                        }
                    }
                    if let Into::Cell(_) = into {
                        into = Into::Nothing;
                    }
                }
                Event::Text(s) => push(&into, nested, &mut text, &s),
                // A heading is keyed by its plain text; a clause keeps its
                // code spans' backticks, which mark its citations.
                Event::Code(s) => match into {
                    Into::Item => push(&into, nested, &mut text, &format!("`{s}`")),
                    _ => push(&into, nested, &mut text, &s),
                },
                Event::SoftBreak | Event::HardBreak | Event::End(TagEnd::Paragraph) => {
                    push(&into, nested, &mut text, " ");
                }
                _ => {}
            }
        }
        doc
    }
}

/// Appends `s` to `text` when it belongs to a heading, a table cell, or a
/// top-level item's own text (not a block nested in it).
fn push(into: &Into, nested: Option<usize>, text: &mut String, s: &str) {
    let keep = match into {
        Into::Nothing => false,
        Into::Item => nested == Some(0),
        Into::Heading(..) | Into::Cell(_) => true,
    };
    if keep {
        text.push_str(s);
    }
}

/// The line range of the section under the heading `text` at `level`, up
/// to the next heading at that level or above.
fn section(headings: &[Heading], level: usize, text: &str) -> Option<(usize, usize)> {
    let i = headings
        .iter()
        .position(|h| h.level == level && h.text == text)?;
    let start = headings.get(i)?.line + 1;
    let stop = headings
        .iter()
        .skip(i + 1)
        .find(|h| h.level <= level)
        .map_or(usize::MAX, |h| h.line);
    Some((start, stop))
}

fn at(line: usize) -> String {
    format!("{DESIGN}:{}", line + 1)
}

fn contract(doc: &Doc, read: &impl Fn(&str) -> io::Result<String>, diags: &mut Vec<String>) {
    let Some((start, stop)) = section(&doc.headings, 2, CONTRACT) else {
        diags.push(format!("{DESIGN}: no \"## {CONTRACT}\" section"));
        return;
    };
    let mut files = BTreeMap::new();
    for item in doc.items.iter().filter(|i| (start..stop).contains(&i.line)) {
        check_clause(
            item.line,
            &item.text,
            &doc.headings,
            read,
            &mut files,
            diags,
        );
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

fn threats(doc: &Doc, diags: &mut Vec<String>) {
    let Some((start, stop)) = section(&doc.headings, 3, THREATS) else {
        diags.push(format!("{DESIGN}: no \"### {THREATS}\" section"));
        return;
    };
    let Some(table) = doc.tables.iter().find(|t| (start..stop).contains(&t.line)) else {
        diags.push(format!("{DESIGN}: no table under \"### {THREATS}\""));
        return;
    };
    if table.header != "ID" {
        diags.push(format!(
            "{}: the threat table's first column is not \"ID\"",
            at(table.line)
        ));
        return;
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (line, id) in &table.rows {
        let (line, id) = (*line, id.as_str());
        let well_formed = id
            .strip_prefix('T')
            .is_some_and(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()));
        if id.is_empty() {
            diags.push(format!("{}: the threat row has no ID", at(line)));
        } else if !well_formed {
            diags.push(format!(
                "{}: threat ID `{id}` is not `T` and two digits",
                at(line)
            ));
        } else if let Some(&first) = seen.get(id) {
            diags.push(format!(
                "{}: threat ID `{id}` is already used on line {}",
                at(line),
                first + 1
            ));
        } else {
            seen.insert(id, line);
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
    fn a_fence_inside_a_list_item_closes_there() {
        let listed = design_with("Notes on the table.", "- ```text\n  code\n  ```");
        assert_eq!(run(&listed, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_line_of_inline_code_opens_no_fence() {
        let inline = design_with("Notes on the table.", "``` inline ```");
        assert_eq!(run(&inline, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_no_break_space_does_not_close_a_fence() {
        // `~~~` and a no-break space is a line of the code block.
        let hidden = design_with(
            "The watermark prose.",
            "The watermark prose.\n\n~~~\n~~~\u{a0}\n## Hidden\n~~~\u{a0}\n~~~",
        );
        let hidden = hidden.replace("once (\"Watermark\")", "once (\"Hidden\")");
        assert_eq!(
            run(&hidden, SOURCE),
            ["docs/design.md:30: \"Hidden\" is not a design.md heading"]
        );
    }

    #[test]
    fn a_heading_in_a_block_quote_does_not_end_a_section() {
        let quoted = design_with(
            "### Threats and Mitigations\n",
            "### Threats and Mitigations\n\n> ## Example\n",
        );
        assert_eq!(run(&quoted, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_heading_in_a_block_quote_does_not_end_the_contract() {
        let quoted = design_with(
            "the section it names.\n",
            "the section it names.\n\n> ## Roadmap\n",
        );
        let quoted = quoted.replace(
            ". Spec: `crates/a/tests/a.rs::judged_once`",
            ". Evidence: `crates/a/tests/a.rs::judged_once`",
        );
        assert_eq!(
            run(&quoted, SOURCE),
            ["docs/design.md:26: the clause has no `Spec:`"]
        );
    }

    /// `DESIGN` with its "## Watermark" heading replaced by `heading`.
    fn heading(heading: &str) -> String {
        design_with("## Watermark\n", heading)
    }

    #[test]
    fn a_setext_heading_is_keyed_by_all_its_lines() {
        let two_lines = heading("Watermark\ncontinued\n---------\n");
        assert_eq!(
            run(&two_lines, SOURCE),
            [
                "docs/design.md:26: \"Watermark\" is not a design.md heading",
                "docs/design.md:27: \"Watermark\" is not a design.md heading",
            ]
        );
        let cited = two_lines.replace("\"Watermark\"", "\"Watermark continued\"");
        assert_eq!(run(&cited, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_closing_sequence_is_not_heading_text() {
        // The parser strips one after a space; after a tab (pulldown-cmark
        // 0.13) it keeps it as text, and the check takes what it returns.
        assert_eq!(
            run(&heading("## Watermark ###\n"), SOURCE),
            Vec::<String>::new()
        );
        assert_eq!(
            run(&heading("## Watermark\t###\n"), SOURCE),
            [
                "docs/design.md:24: \"Watermark\" is not a design.md heading",
                "docs/design.md:25: \"Watermark\" is not a design.md heading",
            ]
        );
    }

    #[test]
    fn a_setext_heading_starting_with_a_hash_keeps_it() {
        assert_eq!(
            run(&heading("#Watermark\n---\n"), SOURCE),
            [
                "docs/design.md:25: \"Watermark\" is not a design.md heading",
                "docs/design.md:26: \"Watermark\" is not a design.md heading",
            ]
        );
    }

    #[test]
    fn a_heading_is_keyed_by_its_plain_text() {
        assert_eq!(
            run(&heading("## *Watermark*\n"), SOURCE),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_nested_list_is_not_part_of_its_clause() {
        // The citations in a nested list leave the clause without them.
        let nested_spec = design_with(
            "Spec: `crates/a/tests/a.rs::judged_once`, `crates/a/tests/a.rs::in_a_proptest`.",
            "Spec:\n  - `crates/a/tests/a.rs::judged_once`, `crates/a/tests/a.rs::in_a_proptest`.",
        );
        assert_eq!(
            run(&nested_spec, SOURCE),
            [format!(
                "{FIRST}: `Spec:` is not a list of `<file>::<test>` separated by \", \" and ending in \".\""
            )]
        );
        // A note nested under a whole clause leaves the clause whole.
        let nested_note = design_with(
            "`crates/a/tests/a.rs::in_a_proptest`.\n",
            "`crates/a/tests/a.rs::in_a_proptest`.\n  - A note under the clause.\n",
        );
        assert_eq!(run(&nested_note, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn crlf_line_endings_keep_the_line_numbers() {
        let crlf = design_with("SHALL judge", "judges")
            .replace("| T02 |", "|  |")
            .replace('\n', "\r\n");
        assert_eq!(
            run(&crlf, SOURCE),
            [
                format!("{FIRST}: the clause has no SHALL"),
                "docs/design.md:16: the threat row has no ID".to_owned(),
            ]
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
