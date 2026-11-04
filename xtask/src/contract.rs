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
//! A tag is a line `// Covers: <ID>`, one threat ID per line, in the run of
//! comments and attributes directly above a `#[test]` fn of a `.rs` file
//! under `crates/` (`xtask/`, whose fixtures are strings, is not scanned).
//! It must name a threat ID and sit above a test that is not `#[ignore]`d.
//! The files are not lexed: a line whose text starts with `// Covers:` is
//! a tag even inside a block comment or a multiline string, and is
//! reported there when it is not directly above a `#[test]` fn.
//! The `### Verification` table, under the next heading at level 3 or
//! above after the threat section (deeper headings may come between), has
//! one row per threat ID in ID order: its `Tagged tests` cell is generated
//! from the tags (grouped by file, files and tests in order, or
//! `Unverified`), and its `Note` cell is kept as written. A table that
//! differs from the generated one fails; `--update` rewrites the table's
//! lines alone, in the file's line ending.
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

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Range;
use std::path::Path;

use anyhow::Context as _;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// The design document, relative to the workspace root.
pub const DESIGN: &str = "docs/design.md";
/// The level-2 heading of the clauses.
pub const CONTRACT: &str = "Behavioral Contract";
/// The level-3 heading over the threat table.
pub const THREATS: &str = "Threats and Mitigations";
/// The level-3 heading over the generated table, right after [`THREATS`].
pub const VERIFICATION: &str = "Verification";
/// The directory scanned for tags, relative to the workspace root.
pub const TAGGED: &str = "crates";
/// What starts a tag line, after its indentation.
const TAG: &str = "// Covers:";

/// Checks the workspace of `manifest`; with `update`, first rewrites the
/// Verification table of design.md when it is not the generated one.
pub fn run(manifest: &Path, update: bool) -> anyhow::Result<Vec<String>> {
    let root = manifest
        .parent()
        .context("the workspace manifest has no directory")?;
    let path = root.join(DESIGN);
    let design = std::fs::read_to_string(&path).with_context(|| format!("cannot read {DESIGN}"))?;
    let sources = sources(root)?;
    let read = |file: &str| std::fs::read_to_string(root.join(file));
    if !update {
        return Ok(check(&design, &sources, read));
    }
    let (updated, diags) = self::update(&design, &sources, read);
    if updated != design {
        replace(&path, &updated).with_context(|| format!("cannot write {DESIGN}"))?;
        eprintln!("check-contract: rewrote the {VERIFICATION} table of {DESIGN}");
    }
    Ok(diags)
}

/// The file [`replace`] writes beside `path` before renaming it over.
fn temporary(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".update");
    path.with_file_name(name)
}

/// Replaces the contents of `path` with `text` by writing a file beside it
/// and renaming it over `path`, so a failed write leaves `path` whole.
fn replace(path: &Path, text: &str) -> io::Result<()> {
    let tmp = temporary(path);
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The `.rs` files under [`TAGGED`] that hold a tag, as (path relative to
/// `root` with `/` separators, contents), in path order.
pub fn sources(root: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut dirs = vec![(root.join(TAGGED), TAGGED.to_owned())];
    while let Some((dir, rel)) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("cannot read `{rel}`"))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read `{rel}`"))?;
            let kind = entry
                .file_type()
                .with_context(|| format!("cannot read `{rel}`"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{rel}/{name}");
            if kind.is_dir() {
                dirs.push((entry.path(), path));
            } else if kind.is_file() && name.ends_with(".rs") {
                let text = std::fs::read_to_string(entry.path())
                    .with_context(|| format!("cannot read `{path}`"))?;
                if text.contains(TAG) {
                    out.push((path, text));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The diagnostics for `design`, reading each cited file through `read`
/// and the tags from `sources` (path, contents).
pub fn check(
    design: &str,
    sources: &[(String, String)],
    read: impl Fn(&str) -> io::Result<String>,
) -> Vec<String> {
    let (mut diags, generated) = analyze(design, sources, &read);
    if let Some(Generated { range, line, text }) = generated
        && let Some(current) = design.get(range)
        && current != text
    {
        // The first line that differs, a missing or extra one included.
        let (old, new): (Vec<&str>, Vec<&str>) =
            (current.split('\n').collect(), text.split('\n').collect());
        let differs = (0..old.len().max(new.len()))
            .find(|&i| old.get(i) != new.get(i))
            .unwrap_or(0);
        diags.push(format!(
            "{}: the {VERIFICATION} table is not the generated one; run `cargo xtask check-contract --update`",
            at(line + differs)
        ));
    }
    diags
}

/// `design` with its Verification table replaced by the generated one, and
/// the diagnostics other than a stale table.
pub fn update(
    design: &str,
    sources: &[(String, String)],
    read: impl Fn(&str) -> io::Result<String>,
) -> (String, Vec<String>) {
    let (diags, generated) = analyze(design, sources, &read);
    let mut out = design.to_owned();
    if let Some(Generated { range, text, .. }) = generated {
        out.replace_range(range, &text);
    }
    (out, diags)
}

/// The generated Verification table, for the byte range it replaces.
struct Generated {
    range: Range<usize>,
    /// The line index of the range's start.
    line: usize,
    text: String,
}

/// Every diagnostic but a stale Verification table, and the generated table
/// when design.md has one to replace.
fn analyze(
    design: &str,
    sources: &[(String, String)],
    read: &impl Fn(&str) -> io::Result<String>,
) -> (Vec<String>, Option<Generated>) {
    let doc = Doc::parse(design);
    let mut diags = Vec::new();
    contract(&doc, read, &mut diags);
    let ids = threats(&doc, &mut diags);
    let tagged = tags(sources, ids.as_ref(), &mut diags);
    let generated = ids.and_then(|ids| verification(&doc, design, &ids, &tagged, &mut diags));
    (diags, generated)
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

/// A top-level table: its line index and byte range, its header's first
/// cell as plain text, its header's cells as written, and its body rows.
struct Table {
    line: usize,
    range: Range<usize>,
    header: String,
    columns: Vec<String>,
    rows: Vec<Row>,
}

/// A body row of a table: its line index, its first cell as plain text and
/// its cells as written, trimmed.
struct Row {
    line: usize,
    id: String,
    cells: Vec<String>,
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
                    range: range.clone(),
                    header: String::new(),
                    columns: Vec::new(),
                    rows: Vec::new(),
                }),
                Event::Start(Tag::TableHead) if containers == 0 => column = 0,
                Event::Start(Tag::TableRow) if containers == 0 => {
                    column = 0;
                    if let Some(t) = doc.tables.last_mut() {
                        t.rows.push(Row {
                            line,
                            id: String::new(),
                            cells: Vec::new(),
                        });
                    }
                }
                Event::Start(Tag::TableCell) if containers == 0 => {
                    into = Into::Cell(column == 0);
                    text.clear();
                    column += 1;
                    if let Some(t) = doc.tables.last_mut() {
                        let raw = source.get(range.clone()).unwrap_or("").trim().to_owned();
                        match t.rows.last_mut() {
                            Some(row) => row.cells.push(raw),
                            None => t.columns.push(raw),
                        }
                    }
                }
                Event::End(TagEnd::TableCell) => {
                    if let (Into::Cell(true), Some(t)) = (&into, doc.tables.last_mut()) {
                        let cell = text.trim().to_owned();
                        match t.rows.last_mut() {
                            Some(row) => row.id = cell,
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
    section_at(headings, i)
}

/// The line range of the section under `headings[i]`, up to the next
/// heading at its level or above.
fn section_at(headings: &[Heading], i: usize) -> Option<(usize, usize)> {
    let heading = headings.get(i)?;
    let stop = headings
        .iter()
        .skip(i + 1)
        .find(|h| h.level <= heading.level)
        .map_or(usize::MAX, |h| h.line);
    Some((heading.line + 1, stop))
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
    let (_, attrs) = attributes(&lines, def);
    let place = format!("`{file}:{}`", def + 1);
    if !is_test(&attrs) {
        diags.push(format!("{at}: {place} `{test}` is not a #[test]"));
    }
    if is_ignored(&attrs) {
        diags.push(format!("{at}: {place} `{test}` is #[ignore]d"));
    }
}

fn is_test(attrs: &[String]) -> bool {
    attrs.iter().any(|a| a == "#[test]")
}

fn is_ignored(attrs: &[String]) -> bool {
    attrs.iter().any(|a| {
        a.strip_prefix("#[ignore")
            .is_some_and(|r| r.starts_with([']', ' ', '=']))
    })
}

/// Whether `line` starts the definition of `fn name`.
fn defines(line: &str, name: &str) -> bool {
    fn_name(line) == Some(name)
}

/// The name of the fn whose definition `line` starts.
fn fn_name(line: &str) -> Option<&str> {
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
    let rest = rest.strip_prefix("fn ")?;
    let end = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let (name, after) = rest.split_at(end);
    (!name.is_empty() && after.starts_with(['(', '<'])).then_some(name)
}

/// The first line index of the run of comments and attributes directly
/// above line `def`, and its attributes, one string each. An attribute over
/// several lines ends with a line ending in `]` and starts at the nearest
/// `#[` above it.
fn attributes(lines: &[&str], def: usize) -> (usize, Vec<String>) {
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
    (i, out)
}

/// Whether `id` is `T` and two digits.
fn well_formed(id: &str) -> bool {
    id.strip_prefix('T')
        .is_some_and(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Checks the threat table's IDs, and returns them; `None` when there is
/// no table to read them from or one of its IDs is reported, since the
/// Verification table is generated from a valid threat table only.
fn threats(doc: &Doc, diags: &mut Vec<String>) -> Option<BTreeSet<String>> {
    let Some((start, stop)) = section(&doc.headings, 3, THREATS) else {
        diags.push(format!("{DESIGN}: no \"### {THREATS}\" section"));
        return None;
    };
    let Some(table) = doc.tables.iter().find(|t| (start..stop).contains(&t.line)) else {
        diags.push(format!("{DESIGN}: no table under \"### {THREATS}\""));
        return None;
    };
    if table.header != "ID" {
        diags.push(format!(
            "{}: the threat table's first column is not \"ID\"",
            at(table.line)
        ));
        return None;
    }
    let reported = diags.len();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &table.rows {
        let (line, id) = (row.line, row.id.as_str());
        if id.is_empty() {
            diags.push(format!("{}: the threat row has no ID", at(line)));
        } else if !well_formed(id) {
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
    (diags.len() == reported).then(|| seen.into_keys().map(str::to_owned).collect())
}

/// The tagged tests: threat ID, then file, then test names.
type Tagged = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// Checks the tags of `sources` and returns the tests they tag. A tag's ID
/// is checked against `ids` when the threat table could be read.
fn tags(
    sources: &[(String, String)],
    ids: Option<&BTreeSet<String>>,
    diags: &mut Vec<String>,
) -> Tagged {
    let mut tagged = Tagged::new();
    for (file, source) in sources {
        let lines: Vec<&str> = source.lines().collect();
        // Each fn: the start of its run, its line, its name.
        let fns: Vec<(usize, usize, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| fn_name(l).map(|name| (attributes(&lines, i).0, i, name)))
            .collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim().strip_prefix(TAG) else {
                continue;
            };
            let place = format!("{file}:{}", i + 1);
            let id = rest.trim();
            if !well_formed(id) || !rest.starts_with(' ') {
                diags.push(format!(
                    "{place}: a tag is `{TAG} <ID>` with one threat ID, not `{}`",
                    line.trim()
                ));
                continue;
            }
            let known = ids.is_none_or(|ids| ids.contains(id));
            if !known {
                diags.push(format!("{place}: `{id}` is not a threat ID"));
            }
            // The first fn below the tag, if the tag is in its run.
            let below = fns.iter().find(|&&(_, def, _)| def > i);
            let test = below.and_then(|&(start, def, name)| {
                let (_, attrs) = attributes(&lines, def);
                (start <= i && is_test(&attrs)).then_some((name, is_ignored(&attrs)))
            });
            match test {
                None => diags.push(format!(
                    "{place}: the tag is not in the comments and attributes directly above a #[test] fn"
                )),
                Some((name, true)) => {
                    diags.push(format!("{place}: the tag is on `{name}`, which is #[ignore]d"));
                }
                Some((name, false)) if known => {
                    tagged
                        .entry(id.to_owned())
                        .or_default()
                        .entry(file.clone())
                        .or_default()
                        .insert(name.to_owned());
                }
                Some(_) => {}
            }
        }
    }
    tagged
}

/// Checks that the Verification table follows the threat section with its
/// columns, and returns its generated text for its byte range.
fn verification(
    doc: &Doc,
    design: &str,
    ids: &BTreeSet<String>,
    tagged: &Tagged,
    diags: &mut Vec<String>,
) -> Option<Generated> {
    let threats = doc
        .headings
        .iter()
        .position(|h| h.level == 3 && h.text == THREATS)?;
    // The next heading at level 3 or above; deeper ones in the notes are
    // part of the threat section.
    let next = doc
        .headings
        .iter()
        .enumerate()
        .skip(threats + 1)
        .find(|(_, h)| h.level <= 3)
        .filter(|(_, h)| h.level == 3 && h.text == VERIFICATION);
    let Some((start, stop)) = next.and_then(|(i, _)| section_at(&doc.headings, i)) else {
        diags.push(format!(
            "{DESIGN}: no \"### {VERIFICATION}\" section right after \"### {THREATS}\""
        ));
        return None;
    };
    let Some(table) = doc.tables.iter().find(|t| (start..stop).contains(&t.line)) else {
        diags.push(format!("{DESIGN}: no table under \"### {VERIFICATION}\""));
        return None;
    };
    if table.columns != ["ID", "Tagged tests", "Note"] {
        diags.push(format!(
            "{}: the {VERIFICATION} table's columns are not \"ID\", \"Tagged tests\" and \"Note\"",
            at(table.line)
        ));
        return None;
    }
    let current = design.get(table.range.clone())?;
    let eol = if current.split('\n').next()?.ends_with('\r') {
        "\r\n"
    } else {
        "\n"
    };
    let mut notes: BTreeMap<&str, &str> = BTreeMap::new();
    for row in &table.rows {
        if let Some(note) = row.cells.get(2) {
            notes.entry(row.id.as_str()).or_insert(note.as_str());
        }
    }
    let mut lines = vec![
        "| ID | Tagged tests | Note |".to_owned(),
        "| --- | --- | --- |".to_owned(),
    ];
    for id in ids {
        let cell = match tagged.get(id) {
            None => "Unverified".to_owned(),
            Some(files) => files
                .iter()
                .map(|(file, tests)| {
                    let tests: Vec<String> = tests.iter().map(|t| format!("`{t}`")).collect();
                    format!("`{file}`: {}", tests.join(", "))
                })
                .collect::<Vec<_>>()
                .join("; "),
        };
        let note = notes.get(id.as_str()).copied().unwrap_or("");
        lines.push(if note.is_empty() {
            format!("| {id} | {cell} | |")
        } else {
            format!("| {id} | {cell} | {note} |")
        });
    }
    let mut text = lines.join(eol);
    if current.ends_with('\n') {
        text.push_str(eol);
    }
    Some(Generated {
        range: table.range.clone(),
        line: table.line,
        text,
    })
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{check, update};

    const TESTS: &str = "crates/a/tests/a.rs";
    const TAGGED: &str = "crates/b/tests/b.rs";

    /// A design.md excerpt that passes, citing two tests of [`TESTS`] and
    /// listing the tagged tests of [`TAGGED_SOURCE`].
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

### Verification

| ID | Tagged tests | Note |
| --- | --- | --- |
| T01 | `crates/b/tests/b.rs`: `bounded`, `generated` | |
| T02 | Unverified | CI step \"Archives\" |

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

    /// The tagged test file: two tagged tests, one in a proptest body, and
    /// a helper, a const and an ignored test, all untagged.
    const TAGGED_SOURCE: &str = "\
/// A test of T01.
// Covers: T01
#[test]
fn bounded() {}

fn helper() {}

const LIMIT: u8 = 1;

proptest! {
    // Covers: T01
    #[test]
    fn generated(x in 0..1) {}
}

#[test]
#[ignore = \"slow\"]
fn ignored() {}
";

    fn read(source: &str) -> impl Fn(&str) -> io::Result<String> {
        move |file| {
            if file == TESTS {
                Ok(source.to_owned())
            } else {
                Err(io::ErrorKind::NotFound.into())
            }
        }
    }

    /// The tagged sources, [`TAGGED`] holding `tagged`.
    fn sources(tagged: &str) -> Vec<(String, String)> {
        vec![(TAGGED.to_owned(), tagged.to_owned())]
    }

    /// The check of `design` with [`TESTS`] holding `source`.
    fn run(design: &str, source: &str) -> Vec<String> {
        check(design, &sources(TAGGED_SOURCE), read(source))
    }

    /// The check of `DESIGN` with [`TAGGED`] holding `tagged`.
    fn run_tagged(tagged: &str) -> Vec<String> {
        check(DESIGN, &sources(tagged), read(SOURCE))
    }

    /// `--update` of `design` with [`TAGGED`] holding `tagged`.
    fn update_tagged(design: &str, tagged: &str) -> (String, Vec<String>) {
        update(design, &sources(tagged), read(SOURCE))
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

    const FIRST: &str = "docs/design.md:31";

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
                "docs/design.md:32: \"Watermark\" is not a design.md heading".to_owned(),
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
                "docs/design.md:32: \"Threats and Mitigations\" is not a design.md heading",
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
            run(&design_with("| T02 | Archives", "|  | Archives"), SOURCE),
            ["docs/design.md:16: the threat row has no ID"]
        );
        assert_eq!(
            run(&design_with("| T02 | Archives", "| T2 | Archives"), SOURCE),
            ["docs/design.md:16: threat ID `T2` is not `T` and two digits"]
        );
        assert_eq!(
            run(&design_with("| T02 | Archives", "| T01 | Archives"), SOURCE),
            ["docs/design.md:16: threat ID `T01` is already used on line 15"]
        );
        assert_eq!(
            run(&design_with("| ID | Attack", "| Key | Attack"), SOURCE),
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
            ["docs/design.md:37: \"Hidden\" is not a design.md heading"]
        );
    }

    #[test]
    fn an_indented_threat_row_is_still_checked() {
        assert_eq!(
            run(&design_with("| T02 | Archives", "  |  | Archives"), SOURCE),
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
            ["docs/design.md:37: \"Hidden\" is not a design.md heading"]
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
            ["docs/design.md:33: the clause has no `Spec:`"]
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
                "docs/design.md:33: \"Watermark\" is not a design.md heading",
                "docs/design.md:34: \"Watermark\" is not a design.md heading",
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
                "docs/design.md:31: \"Watermark\" is not a design.md heading",
                "docs/design.md:32: \"Watermark\" is not a design.md heading",
            ]
        );
    }

    #[test]
    fn a_setext_heading_starting_with_a_hash_keeps_it() {
        assert_eq!(
            run(&heading("#Watermark\n---\n"), SOURCE),
            [
                "docs/design.md:32: \"Watermark\" is not a design.md heading",
                "docs/design.md:33: \"Watermark\" is not a design.md heading",
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
            .replace("| T02 | Archives", "|  | Archives")
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

    /// `TAGGED_SOURCE` with `from` replaced by `to`, which must occur once.
    fn tagged_with(from: &str, to: &str) -> String {
        assert_eq!(TAGGED_SOURCE.matches(from).count(), 1, "{from}");
        TAGGED_SOURCE.replace(from, to)
    }

    /// The tag above `bounded`.
    const BOUNDED: &str = "// Covers: T01\n#[test]\nfn bounded";
    /// The fixture's generated row of T01.
    const T01_ROW: &str = "| T01 | `crates/b/tests/b.rs`: `bounded`, `generated` | |";
    const STALE: &str = "the Verification table is not the generated one; run `cargo xtask check-contract --update`";

    #[test]
    fn an_unknown_or_malformed_tag_is_reported() {
        let unknown = tagged_with(BOUNDED, &format!("// Covers: T09\n{BOUNDED}"));
        assert_eq!(
            run_tagged(&unknown),
            ["crates/b/tests/b.rs:2: `T09` is not a threat ID"]
        );
        // One threat ID per tag line.
        let two = tagged_with(BOUNDED, &format!("// Covers: T01, T02\n{BOUNDED}"));
        assert_eq!(
            run_tagged(&two),
            [
                "crates/b/tests/b.rs:2: a tag is `// Covers: <ID>` with one threat ID, not `// Covers: T01, T02`"
            ]
        );
        // A space after the colon.
        let tight = tagged_with(BOUNDED, &format!("// Covers:T01\n{BOUNDED}"));
        assert_eq!(
            run_tagged(&tight),
            [
                "crates/b/tests/b.rs:2: a tag is `// Covers: <ID>` with one threat ID, not `// Covers:T01`"
            ]
        );
    }

    #[test]
    fn a_tag_separated_from_its_test_by_a_blank_line_is_stray() {
        let spaced = format!("{TAGGED_SOURCE}// Covers: T01\n\n#[test]\nfn spaced() {{}}\n");
        assert_eq!(
            run_tagged(&spaced),
            [
                "crates/b/tests/b.rs:19: the tag is not in the comments and attributes directly above a #[test] fn"
            ]
        );
    }

    #[test]
    fn a_tag_line_in_a_block_comment_or_string_is_still_read() {
        // A known limitation: the lines are not lexed, so a line starting
        // with the tag text counts as a tag wherever it is.
        let commented = tagged_with("const LIMIT", "/*\n// Covers: T01\n*/\nconst LIMIT");
        let stray = "the tag is not in the comments and attributes directly above a #[test] fn";
        assert_eq!(
            run_tagged(&commented),
            [format!("crates/b/tests/b.rs:9: {stray}")]
        );
        let string = tagged_with(
            "const LIMIT: u8 = 1;",
            "const TEXT: &str = \"\n// Covers: T01\n\";",
        );
        assert_eq!(
            run_tagged(&string),
            [format!("crates/b/tests/b.rs:9: {stray}")]
        );
    }

    #[test]
    fn a_stray_tag_is_reported() {
        let stray = "the tag is not in the comments and attributes directly above a #[test] fn";
        let helper = tagged_with("fn helper", "// Covers: T01\nfn helper");
        assert_eq!(
            run_tagged(&helper),
            [format!("crates/b/tests/b.rs:6: {stray}")]
        );
        let constant = tagged_with("const LIMIT", "// Covers: T01\nconst LIMIT");
        assert_eq!(
            run_tagged(&constant),
            [format!("crates/b/tests/b.rs:8: {stray}")]
        );
        let last = format!("{TAGGED_SOURCE}// Covers: T01\n");
        assert_eq!(
            run_tagged(&last),
            [format!("crates/b/tests/b.rs:19: {stray}")]
        );
    }

    #[test]
    fn a_tag_on_an_ignored_test_is_reported() {
        let ignored = tagged_with("#[test]\n#[ignore", "// Covers: T01\n#[test]\n#[ignore");
        assert_eq!(
            run_tagged(&ignored),
            ["crates/b/tests/b.rs:16: the tag is on `ignored`, which is #[ignore]d"]
        );
    }

    #[test]
    fn only_rust_files_under_crates_are_scanned_for_tags() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let tag = "// Covers: T01\n#[test]\nfn t() {}\n";
        write("crates/a/tests/t.rs", tag);
        write("crates/a/src/lib.rs", "fn untagged() {}\n");
        write("crates/a/notes.txt", tag);
        write("xtask/src/fixture.rs", tag);
        assert_eq!(
            super::sources(root.path()).unwrap(),
            [("crates/a/tests/t.rs".to_owned(), tag.to_owned())]
        );
    }

    #[test]
    fn a_threat_without_tags_is_unverified_and_passes() {
        let design = design_with(T01_ROW, "| T01 | Unverified | |");
        assert_eq!(check(&design, &[], read(SOURCE)), Vec::<String>::new());
        assert_eq!(
            update(&design, &[], read(SOURCE)),
            (design.clone(), Vec::new())
        );
    }

    #[test]
    fn a_stale_table_is_reported_and_update_regenerates_it() {
        // A new tagged test of T02.
        let added = format!("{TAGGED_SOURCE}\n// Covers: T02\n#[test]\nfn extracted() {{}}\n");
        assert_eq!(run_tagged(&added), [format!("docs/design.md:25: {STALE}")]);
        let expected = design_with(
            "| T02 | Unverified | CI step \"Archives\" |",
            "| T02 | `crates/b/tests/b.rs`: `extracted` | CI step \"Archives\" |",
        );
        assert_eq!(
            update_tagged(DESIGN, &added),
            (expected.clone(), Vec::new())
        );
        // A second update changes nothing, and the result passes.
        assert_eq!(
            update_tagged(&expected, &added),
            (expected.clone(), Vec::new())
        );
        assert_eq!(
            check(&expected, &sources(&added), read(SOURCE)),
            Vec::<String>::new()
        );
        // A removed tag and a renamed test leave the table stale too.
        let removed = tagged_with(BOUNDED, "#[test]\nfn bounded");
        assert_eq!(
            run_tagged(&removed),
            [format!("docs/design.md:24: {STALE}")]
        );
        let renamed = tagged_with("fn bounded", "fn bounded_by_the_cap");
        assert_eq!(
            run_tagged(&renamed),
            [format!("docs/design.md:24: {STALE}")]
        );
        assert_eq!(
            update_tagged(DESIGN, &renamed).0,
            design_with(
                T01_ROW,
                "| T01 | `crates/b/tests/b.rs`: `bounded_by_the_cap`, `generated` | |"
            )
        );
    }

    #[test]
    fn update_keeps_a_note_as_written() {
        // A stale cell beside a note with markup and an escaped pipe.
        let noted = design_with(T01_ROW, "| T01 | old | `a \\| b` and *c* |");
        assert_eq!(run(&noted, SOURCE), [format!("docs/design.md:24: {STALE}")]);
        let expected = design_with(
            T01_ROW,
            "| T01 | `crates/b/tests/b.rs`: `bounded`, `generated` | `a \\| b` and *c* |",
        );
        assert_eq!(update_tagged(&noted, TAGGED_SOURCE), (expected, Vec::new()));
    }

    #[test]
    fn update_keeps_crlf_line_endings() {
        let crlf = DESIGN.replace('\n', "\r\n");
        let stale = design_with(T01_ROW, "| T01 | old | |").replace('\n', "\r\n");
        assert_eq!(run(&stale, SOURCE), [format!("docs/design.md:24: {STALE}")]);
        assert_eq!(update_tagged(&stale, TAGGED_SOURCE), (crlf, Vec::new()));
    }

    #[test]
    fn tagged_tests_are_ordered_by_id_file_and_name() {
        // Files out of path order, tests out of name order, IDs mixed.
        let z = "// Covers: T02\n#[test]\nfn zeta() {}\n\n// Covers: T02\n// Covers: T01\n#[test]\nfn alpha() {}\n";
        let a = "mod tests {\n    // Covers: T02\n    #[test]\n    fn mid() {}\n    // Covers: T02\n    #[test]\n    fn beta() {}\n}\n";
        let shuffled = [
            ("crates/z/tests/z.rs".to_owned(), z.to_owned()),
            ("crates/a/src/lib.rs".to_owned(), a.to_owned()),
        ];
        let expected = design_with(T01_ROW, "| T01 | `crates/z/tests/z.rs`: `alpha` | |").replace(
            "| T02 | Unverified |",
            "| T02 | `crates/a/src/lib.rs`: `beta`, `mid`; `crates/z/tests/z.rs`: `alpha`, `zeta` |",
        );
        assert_eq!(
            update(DESIGN, &shuffled, read(SOURCE)),
            (expected, Vec::new())
        );
    }

    #[test]
    fn a_missing_or_misshapen_verification_table_is_reported() {
        let missing = design_with("### Verification", "### Evidence");
        let reported = [
            "docs/design.md: no \"### Verification\" section right after \"### Threats and Mitigations\"",
        ];
        assert_eq!(run(&missing, SOURCE), reported);
        // `--update` leaves it as it is and still reports it.
        assert_eq!(
            update_tagged(&missing, TAGGED_SOURCE),
            (missing.clone(), reported.map(str::to_owned).to_vec())
        );
        let misshapen = design_with("| Tagged tests |", "| Tests |");
        let reported = [
            "docs/design.md:22: the Verification table's columns are not \"ID\", \"Tagged tests\" and \"Note\"",
        ];
        assert_eq!(run(&misshapen, SOURCE), reported);
        assert_eq!(
            update_tagged(&misshapen, TAGGED_SOURCE),
            (misshapen.clone(), reported.map(str::to_owned).to_vec())
        );
    }

    #[test]
    fn a_deeper_heading_in_the_threat_notes_is_allowed() {
        let nested = design_with(
            "Notes on the table.\n",
            "Notes on the table.\n\n#### Loading notes\n\nMore notes.\n",
        );
        assert_eq!(run(&nested, SOURCE), Vec::<String>::new());
    }

    #[test]
    fn a_missing_or_extra_last_row_is_reported_on_its_line() {
        let t02 = "| T02 | Unverified | CI step \"Archives\" |\n";
        let missing = design_with(t02, "");
        assert_eq!(
            run(&missing, SOURCE),
            [format!("docs/design.md:25: {STALE}")]
        );
        let extra = design_with(t02, &format!("{t02}| T03 | Unverified | |\n"));
        assert_eq!(run(&extra, SOURCE), [format!("docs/design.md:26: {STALE}")]);
        // The file ending after the T01 row, with no line ending.
        let cut = DESIGN.get(..DESIGN.find(t02).unwrap() - 1).unwrap();
        assert_eq!(
            run(cut, SOURCE),
            [
                "docs/design.md: no \"## Behavioral Contract\" section".to_owned(),
                format!("docs/design.md:25: {STALE}"),
            ]
        );
    }

    #[test]
    fn a_failed_write_leaves_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("design.md");
        std::fs::write(&path, "old").unwrap();
        super::replace(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // The temporary file cannot be written: a directory is in its way.
        std::fs::create_dir(super::temporary(&path)).unwrap();
        assert!(super::replace(&path, "newer").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }
}
