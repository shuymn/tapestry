//! `cargo xtask check-fidelity`: the ledger of source behaviour kept on
//! purpose agrees with the code that carries it, and no new code points at
//! a reference player's internals unnoticed (design.md "Design
//! Principles", "Reproduce a source's rules, not its byproducts").
//!
//! The ledger, `docs/fidelity.toml`, is a list of `[[entry]]` tables and
//! nothing else. Each entry has these fields, all required but `decision`,
//! and no others:
//!
//! - `id`: `SF-` and three digits, used once, the entries in id order;
//! - `summary`: one line;
//! - `class`: `rule`, `compatibility` or `kept-byproduct`;
//! - `decision`: `#` and an issue number, required for `kept-byproduct`;
//! - `reason`: why the copy stays, naming the rule a player can see or
//!   the compatibility it keeps;
//! - `source`: the source lines followed, each `<path>:<line>[-<line>]
//!   @<pin>` (only the shape is checked);
//! - `sites`: the `.rs` files under `crates/` that copy it;
//! - `tests`: the tests that pin it, each `<file>::<fn>`.
//!
//! `source`, `sites` and `tests` are non-empty arrays of distinct,
//! non-empty strings.
//!
//! A tag is a line that is exactly `// Source: SF-ddd`. A line whose text
//! starts with `// Source:` and then, after any spaces, `SF` is read as a
//! tag and reported when it is not one; other `// Source:` lines are
//! provenance and are ignored, as are `.java:NNN` citations. A tag sits in
//! the run of comments and attributes directly above code and names an
//! entry. Above a `#[test]` fn, that test is one of the entry's `tests`;
//! above other code, the file is one of its `sites`. Each site holds a tag
//! of its entry above non-test code, and each cited test is defined once
//! in its file, is a `#[test]`, is not `#[ignore]`d and carries the tag.
//!
//! The heuristic lints read every `.rs` file under `crates/` but the
//! reference executor's ([`ORACLES`]), whatever else the file holds:
//!
//! - `java-name`: an item named `java_*`, `luaj_*`, `gdx_*` or
//!   `libgdx_*`;
//! - `utf16`: `encode_utf16` or `len_utf16` on a code line, outside the
//!   Windows-only crates (whose system calls take UTF-16);
//! - `source-name`: a fn named with `as_the_source`, `like_the_source`,
//!   `match_the_source`, `matches_the_source`, `follow_the_source`,
//!   `follows_the_source`, `the_source_s`, `java_s`, `luaj_s`, `libgdx_s`,
//!   `gdx_s` or `as_java` between underscores. `the_sources` is not one
//!   (input sources), so a name such as
//!   `stretch_five_takes_the_sources_branch_for_any_sign` passes: an
//!   accepted miss;
//! - `expect-reason`: an `#[expect(...)]` or `#[allow(...)]` (inner ones
//!   too) whose `reason` string, over as many lines as it takes, holds the
//!   word Java, LuaJ or libGDX (`'s` allowed), or "the source's" or "as the
//!   source".
//!
//! A hit is cleared when the run of comments and attributes directly above
//! its item or attribute holds a tag or a `// Not a source copy: <reason>`
//! marker (an empty reason is reported). Each clears only the hits of the
//! code directly below its run, never a copy elsewhere in the file.
//!
//! The hits left are counted per lint and file against the baseline,
//! `xtask/fidelity-baseline.txt` (`<lint> <count> <file>` per line, `#`
//! comments): more hits than the baseline allows are reported with their
//! lines, and fewer ask for `--update`. `--update` lowers each count to the
//! current one and drops a zero; it never raises a count or adds a line,
//! and without a baseline it fails. `--init` writes the baseline from the
//! current counts when it does not exist, and fails when it does: it is
//! for the first PR only, and a baseline raised by hand is left to review.
//! Counting per file keeps line moves harmless and misses a hit swapped
//! for another in one file.
//!
//! The files are not lexed: a tag or marker line inside a block comment or
//! a string still counts, and an attribute spelled unusually is not
//! recognized. Review covers those.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use anyhow::{Context as _, bail};

use crate::arch::WINDOWS_ONLY_ALLOWED;
use crate::contract::{attributes, check_test, fn_name, is_test, replace};

/// The ledger, relative to the workspace root.
pub const LEDGER: &str = "docs/fidelity.toml";
/// The lint baseline, relative to the workspace root.
pub const BASELINE: &str = "xtask/fidelity-baseline.txt";
/// The directory scanned for tags and lint hits.
const SCANNED: &str = "crates";
/// The reference executor, which mirrors the source on purpose: its files
/// are not linted.
const ORACLES: &[&str] = &[
    "crates/skin-beatoraja/src/bin/lua_replay/",
    "crates/skin-beatoraja/src/lua/replay.rs",
];
/// What starts a tag line, after its indentation.
const TAG: &str = "// Source:";
/// What starts a marker line, after its indentation.
const MARKER: &str = "// Not a source copy:";
/// The lints, in baseline order.
const LINTS: &[&str] = &["expect-reason", "java-name", "source-name", "utf16"];
/// The prefixes of a `java-name` hit.
const JAVA_PREFIXES: &[&str] = &["java_", "luaj_", "gdx_", "libgdx_"];
/// The words of a `source-name` hit, between underscores.
const SOURCE_NAMES: &[&str] = &[
    "as_the_source",
    "like_the_source",
    "match_the_source",
    "matches_the_source",
    "follow_the_source",
    "follows_the_source",
    "the_source_s",
    "java_s",
    "luaj_s",
    "libgdx_s",
    "gdx_s",
    "as_java",
];
/// The words of an `expect-reason` hit, lowercased.
const REASON_WORDS: &[&str] = &["java", "luaj", "libgdx"];
/// The phrases of an `expect-reason` hit, lowercased.
const REASON_PHRASES: &[&str] = &["the source's", "as the source"];
const CLASSES: &[&str] = &["rule", "compatibility", "kept-byproduct"];
const FIELDS: &[&str] = &[
    "id", "summary", "class", "decision", "reason", "source", "sites", "tests",
];
const HEADER: &str = "\
# The heuristic lint hits of `cargo xtask check-fidelity`, per lint and file:
# `<lint> <count> <file>`. `--update` lowers a count and drops a zero; a
# count never rises.
";

/// What `cargo xtask check-fidelity` does with the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Compare the hits with it.
    Check,
    /// Lower it to the current hits.
    Update,
    /// Write it from the current hits when it is absent.
    Init,
}

/// Checks the workspace of `manifest`, reading or writing the baseline as
/// `mode` says.
pub fn run(manifest: &Path, mode: Mode) -> anyhow::Result<Vec<String>> {
    let root = manifest
        .parent()
        .context("the workspace manifest has no directory")?;
    let ledger = std::fs::read_to_string(root.join(LEDGER))
        .with_context(|| format!("cannot read {LEDGER}"))?;
    let sources = sources(root)?;
    let path = root.join(BASELINE);
    let baseline = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("cannot read {BASELINE}")),
    };
    match (mode, baseline) {
        (Mode::Init, Some(_)) => bail!(
            "{BASELINE} exists; `--init` writes the first baseline only, and `--update` lowers it"
        ),
        (Mode::Init, None) => {
            let (text, diags) = init(&ledger, &sources);
            replace(&path, &text).with_context(|| format!("cannot write {BASELINE}"))?;
            eprintln!("check-fidelity: wrote {BASELINE}");
            Ok(diags)
        }
        (Mode::Check | Mode::Update, None) => bail!(
            "{BASELINE} does not exist; the first PR creates it with `cargo xtask check-fidelity --init`"
        ),
        (Mode::Check, Some(old)) => Ok(check(&ledger, &old, &sources)),
        (Mode::Update, Some(old)) => {
            let (text, diags) = update(&ledger, &old, &sources);
            if let Some(text) = text
                && text != old
            {
                replace(&path, &text).with_context(|| format!("cannot write {BASELINE}"))?;
                eprintln!("check-fidelity: lowered {BASELINE}");
            }
            Ok(diags)
        }
    }
}

/// Every `.rs` file under [`SCANNED`], as (path relative to `root` with
/// `/` separators, contents), in path order.
pub fn sources(root: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut dirs = vec![(root.join(SCANNED), SCANNED.to_owned())];
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
                out.push((path, text));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The diagnostics of `ledger` and `sources` (path, contents) against the
/// baseline text `baseline`.
pub fn check(ledger: &str, baseline: &str, sources: &[(String, String)]) -> Vec<String> {
    let (mut diags, hits) = analyze(ledger, sources);
    if let Some(counts) = parse_baseline(baseline, &mut diags) {
        grown(&hits, &counts, &mut diags);
        for ((lint, file), &old) in &counts {
            let now = hits.get(&(*lint, file.clone())).map_or(0, Vec::len);
            if now < old {
                diags.push(format!(
                    "{BASELINE}: `{lint} {old} {file}` has {now} hit(s) now; run `cargo xtask check-fidelity --update`"
                ));
            }
        }
    }
    diags
}

/// The baseline `baseline` lowered to the current hits (`None` when it is
/// malformed), and the diagnostics other than a count that fell.
pub fn update(
    ledger: &str,
    baseline: &str,
    sources: &[(String, String)],
) -> (Option<String>, Vec<String>) {
    let (mut diags, hits) = analyze(ledger, sources);
    let Some(counts) = parse_baseline(baseline, &mut diags) else {
        return (None, diags);
    };
    grown(&hits, &counts, &mut diags);
    let lowered: Counts = counts
        .into_iter()
        .filter_map(|((lint, file), old)| {
            let now = hits.get(&(lint, file.clone())).map_or(0, Vec::len);
            let new = old.min(now);
            (new > 0).then_some(((lint, file), new))
        })
        .collect();
    (Some(render(&lowered)), diags)
}

/// The baseline of the current hits, and the diagnostics other than hits.
pub fn init(ledger: &str, sources: &[(String, String)]) -> (String, Vec<String>) {
    let (diags, hits) = analyze(ledger, sources);
    let counts: Counts = hits
        .into_iter()
        .map(|(key, lines)| (key, lines.len()))
        .collect();
    (render(&counts), diags)
}

/// The uncleared hits: lint and file, then their line numbers.
type Hits = BTreeMap<(&'static str, String), Vec<usize>>;
/// Baseline counts by lint and file.
type Counts = BTreeMap<(&'static str, String), usize>;

fn render(counts: &Counts) -> String {
    let mut out = HEADER.to_owned();
    for ((lint, file), n) in counts {
        out.push_str(&format!("{lint} {n} {file}\n"));
    }
    out
}

/// Reports each lint and file with more hits than `counts` allows.
fn grown(hits: &Hits, counts: &Counts, diags: &mut Vec<String>) {
    for ((lint, file), lines) in hits {
        let allowed = counts.get(&(*lint, file.clone())).copied().unwrap_or(0);
        if lines.len() > allowed {
            let numbers: Vec<String> = lines.iter().map(ToString::to_string).collect();
            diags.push(format!(
                "{file}: {lint}: {} hit(s) (lines {}) where {BASELINE} allows {allowed}; tag a copy kept on purpose `// Source: SF-ddd`, mark other code `// Not a source copy: <reason>`, or remove what the hit names",
                lines.len(),
                numbers.join(", ")
            ));
        }
    }
}

/// The counts of `text`, or `None` when a line is reported.
fn parse_baseline(text: &str, diags: &mut Vec<String>) -> Option<Counts> {
    let reported = diags.len();
    let mut counts = Counts::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let place = format!("{BASELINE}:{}", i + 1);
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [lint, count, file] = parts[..] else {
            diags.push(format!("{place}: a line is `<lint> <count> <file>`"));
            continue;
        };
        let Some(lint) = LINTS.iter().copied().find(|&l| l == lint) else {
            diags.push(format!("{place}: `{lint}` is not a lint"));
            continue;
        };
        let Some(count) = count.parse::<usize>().ok().filter(|&n| n > 0) else {
            diags.push(format!("{place}: `{count}` is not a positive count"));
            continue;
        };
        if counts.insert((lint, file.to_owned()), count).is_some() {
            diags.push(format!("{place}: `{lint} {file}` is listed twice"));
        }
    }
    (diags.len() == reported).then_some(counts)
}

/// An entry's id, sites and tests, for the cross checks.
struct Entry {
    id: String,
    sites: Vec<String>,
    tests: Vec<String>,
}

/// What a line holds.
enum Mark<'a> {
    Tag(&'a str),
    Malformed,
    Marker(&'a str),
    Nothing,
}

fn mark(line: &str) -> Mark<'_> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix(TAG) {
        if !rest.trim_start().starts_with("SF") {
            return Mark::Nothing;
        }
        return match rest.strip_prefix(' ') {
            Some(id) if well_formed(id) => Mark::Tag(id),
            _ => Mark::Malformed,
        };
    }
    match t.strip_prefix(MARKER) {
        Some(reason) => Mark::Marker(reason.trim()),
        None => Mark::Nothing,
    }
}

/// Whether `id` is `SF-` and three digits.
fn well_formed(id: &str) -> bool {
    id.strip_prefix("SF-")
        .is_some_and(|n| n.len() == 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Every diagnostic but the baseline's, and the hits left.
fn analyze(ledger: &str, sources: &[(String, String)]) -> (Vec<String>, Hits) {
    let mut diags = Vec::new();
    let entries = parse_ledger(ledger, &mut diags);
    let by_id: BTreeMap<&str, &Entry> = entries
        .iter()
        .flatten()
        .map(|e| (e.id.as_str(), e))
        .collect();
    let mut site_tags = BTreeSet::new();
    let mut test_tags = BTreeSet::new();
    let mut hits = Hits::new();
    for (file, text) in sources {
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let place = format!("{file}:{}", i + 1);
            match mark(line) {
                Mark::Nothing => {}
                Mark::Marker(reason) => {
                    if reason.is_empty() {
                        diags.push(format!("{place}: `{MARKER}` needs its reason"));
                    }
                }
                Mark::Malformed => diags.push(format!(
                    "{place}: a tag is `{TAG} SF-ddd` with one entry ID, not `{}`",
                    line.trim()
                )),
                Mark::Tag(id) => {
                    let below = code_below(&lines, i);
                    let entry = by_id.get(id);
                    if entries.is_some() && entry.is_none() {
                        diags.push(format!("{place}: `{id}` is not an entry of {LEDGER}"));
                    }
                    let Some(def) = below else {
                        diags.push(format!(
                            "{place}: the tag is not in the comments and attributes directly above code"
                        ));
                        continue;
                    };
                    let code = lines.get(def).copied().unwrap_or("");
                    let test = fn_name(code).filter(|_| is_test(&attributes(&lines, def).1));
                    match test {
                        Some(name) => {
                            let cited = format!("{file}::{name}");
                            if entry.is_some_and(|e| !e.tests.contains(&cited)) {
                                diags.push(format!(
                                    "{place}: test `{name}` is not listed in `tests` of {id}"
                                ));
                            }
                            test_tags.insert((id.to_owned(), file.clone(), name.to_owned()));
                        }
                        None => {
                            if entry.is_some_and(|e| !e.sites.contains(file)) {
                                diags.push(format!("{place}: `{file}` is not a site of {id}"));
                            }
                            site_tags.insert((id.to_owned(), file.clone()));
                        }
                    }
                }
            }
        }
        if !ORACLES.iter().any(|o| file.starts_with(o)) {
            lint(file, &lines, &mut hits);
        }
    }
    let files: BTreeMap<&str, &str> = sources
        .iter()
        .map(|(f, t)| (f.as_str(), t.as_str()))
        .collect();
    for entry in entries.iter().flatten() {
        cross_check(entry, &files, &site_tags, &test_tags, &mut diags);
    }
    (diags, hits)
}

/// Checks that each site and test of `entry` exists and carries its tag.
fn cross_check(
    entry: &Entry,
    files: &BTreeMap<&str, &str>,
    site_tags: &BTreeSet<(String, String)>,
    test_tags: &BTreeSet<(String, String, String)>,
    diags: &mut Vec<String>,
) {
    let id = &entry.id;
    let at = format!("{LEDGER}: {id}");
    let scanned = |f: &str| f.starts_with("crates/") && f.ends_with(".rs");
    for site in &entry.sites {
        if !scanned(site) {
            diags.push(format!(
                "{at}: site `{site}` is not a .rs file under crates/"
            ));
        } else if !files.contains_key(site.as_str()) {
            diags.push(format!("{at}: site `{site}` does not exist"));
        } else if !site_tags.contains(&(id.clone(), site.clone())) {
            diags.push(format!(
                "{at}: site `{site}` has no `{TAG} {id}` above code other than a test"
            ));
        }
    }
    for cited in &entry.tests {
        let Some((file, test)) = cited.rsplit_once("::") else {
            continue;
        };
        if !scanned(file) {
            diags.push(format!(
                "{at}: test `{cited}` is not in a .rs file under crates/"
            ));
            continue;
        }
        let Some(source) = files.get(file) else {
            diags.push(format!("{at}: `{file}` does not exist"));
            continue;
        };
        check_test(&at, file, test, source, diags);
        let defined = source.lines().filter(|l| fn_name(l) == Some(test)).count();
        if defined == 1 && !test_tags.contains(&(id.clone(), file.to_owned(), test.to_owned())) {
            diags.push(format!("{at}: test `{cited}` does not carry `{TAG} {id}`"));
        }
    }
}

/// The entries of `ledger` whose id is valid and new, each field checked.
/// `None` when the ledger cannot be read as a list of entries.
fn parse_ledger(ledger: &str, diags: &mut Vec<String>) -> Option<Vec<Entry>> {
    let table: toml::Table = match ledger.parse() {
        Ok(t) => t,
        Err(e) => {
            let line = e.span().map_or(1, |s| {
                ledger
                    .get(..s.start)
                    .map_or(1, |b| b.matches('\n').count() + 1)
            });
            diags.push(format!("{LEDGER}:{line}: not TOML: {}", e.message().trim()));
            return None;
        }
    };
    for key in table.keys().filter(|k| *k != "entry") {
        diags.push(format!(
            "{LEDGER}: unknown top-level key `{key}`; the ledger holds `[[entry]]` tables only"
        ));
    }
    let Some(list) = table.get("entry") else {
        return Some(Vec::new());
    };
    let Some(list) = list.as_array() else {
        diags.push(format!("{LEDGER}: `entry` is not an array of tables"));
        return None;
    };
    let mut entries: Vec<Entry> = Vec::new();
    let mut last: Option<String> = None;
    for (n, value) in list.iter().enumerate() {
        let Some(t) = value.as_table() else {
            diags.push(format!("{LEDGER}: entry {} is not a table", n + 1));
            continue;
        };
        let id = t
            .get("id")
            .and_then(toml::Value::as_str)
            .filter(|id| well_formed(id));
        let at = match id {
            Some(id) => format!("{LEDGER}: {id}"),
            None => format!("{LEDGER}: entry {}", n + 1),
        };
        if id.is_none() {
            diags.push(format!("{at}: `id` is not `SF-` and three digits"));
        }
        for key in t.keys().filter(|k| !FIELDS.contains(&k.as_str())) {
            diags.push(format!("{at}: unknown field `{key}`"));
        }
        let summary = text(t, "summary", &at, diags);
        if summary.is_some_and(|s| s.contains('\n')) {
            diags.push(format!("{at}: `summary` is more than one line"));
        }
        let class = text(t, "class", &at, diags);
        if let Some(class) = class
            && !CLASSES.contains(&class)
        {
            diags.push(format!(
                "{at}: `class` is not one of `rule`, `compatibility`, `kept-byproduct`"
            ));
        }
        match t.get("decision") {
            None if class == Some("kept-byproduct") => {
                diags.push(format!("{at}: a kept byproduct needs its `decision`"));
            }
            None => {}
            Some(d) => {
                let issue = d
                    .as_str()
                    .and_then(|d| d.strip_prefix('#'))
                    .is_some_and(|n| {
                        !n.is_empty()
                            && !n.starts_with('0')
                            && n.bytes().all(|b| b.is_ascii_digit())
                    });
                if !issue {
                    diags.push(format!("{at}: `decision` is not `#` and an issue number"));
                }
            }
        }
        text(t, "reason", &at, diags);
        for source in list_of(t, "source", &at, diags) {
            if !source_shaped(&source) {
                diags.push(format!(
                    "{at}: source `{source}` is not `<path>:<line>[-<line>] @<pin>`"
                ));
            }
        }
        let sites = list_of(t, "sites", &at, diags);
        let mut tests = list_of(t, "tests", &at, diags);
        tests.retain(|cited| {
            let word = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
            let shaped = cited
                .rsplit_once("::")
                .is_some_and(|(file, test)| word(file) && word(test));
            if !shaped {
                diags.push(format!("{at}: test `{cited}` is not `<file>::<fn>`"));
            }
            shaped
        });
        let Some(id) = id else { continue };
        if entries.iter().any(|e| e.id == id) {
            diags.push(format!("{at}: `{id}` is used by an earlier entry"));
            continue;
        }
        if let Some(prev) = &last
            && prev.as_str() > id
        {
            diags.push(format!(
                "{at}: `{id}` comes after `{prev}`; entries are in id order"
            ));
        }
        last = Some(id.to_owned());
        entries.push(Entry {
            id: id.to_owned(),
            sites,
            tests,
        });
    }
    Some(entries)
}

/// The non-empty string field `key` of `t`, reported when it is not one.
fn text<'a>(t: &'a toml::Table, key: &str, at: &str, diags: &mut Vec<String>) -> Option<&'a str> {
    match t.get(key).map(toml::Value::as_str) {
        None => diags.push(format!("{at}: no `{key}`")),
        Some(None) => diags.push(format!("{at}: `{key}` is not a string")),
        Some(Some(s)) if s.trim().is_empty() => diags.push(format!("{at}: `{key}` is empty")),
        Some(Some(s)) => return Some(s),
    }
    None
}

/// The valid items of the field `key` of `t`, which must be a non-empty
/// array of distinct, non-empty strings.
fn list_of(t: &toml::Table, key: &str, at: &str, diags: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let items = match t.get(key) {
        None => {
            diags.push(format!("{at}: no `{key}`"));
            return out;
        }
        Some(v) => match v.as_array() {
            Some(items) => items,
            None => {
                diags.push(format!("{at}: `{key}` is not an array"));
                return out;
            }
        },
    };
    if items.is_empty() {
        diags.push(format!("{at}: `{key}` is empty"));
    }
    for (i, item) in items.iter().enumerate() {
        match item.as_str() {
            None => diags.push(format!("{at}: `{key}` item {} is not a string", i + 1)),
            Some(s) if s.trim().is_empty() => {
                diags.push(format!("{at}: `{key}` item {} is empty", i + 1));
            }
            Some(s) if out.iter().any(|o| o == s) => {
                diags.push(format!("{at}: `{key}` lists `{s}` twice"));
            }
            Some(s) => out.push(s.to_owned()),
        }
    }
    out
}

/// Whether `source` is `<path>:<line>[-<line>] @<pin>`.
fn source_shaped(source: &str) -> bool {
    let word = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
    let number =
        |s: &str| !s.starts_with('0') && s.parse::<u32>().ok().filter(|&n| n > 0).is_some();
    let Some((at, pin)) = source.split_once(" @") else {
        return false;
    };
    let Some((path, lines)) = at.rsplit_once(':') else {
        return false;
    };
    let lines_ok = match lines.split_once('-') {
        None => number(lines),
        Some((a, b)) => number(a) && number(b) && a.parse::<u32>().ok() < b.parse::<u32>().ok(),
    };
    word(path) && word(pin) && lines_ok
}

/// The line of the code directly below the run of comments and
/// attributes holding line `i`, or `None` when the run ends at a blank
/// line or the end of the file.
fn code_below(lines: &[&str], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        let t = lines.get(j)?.trim();
        if t.starts_with("//") {
            j += 1;
        } else if t.starts_with("#[") || t.starts_with("#![") {
            j = attribute_end(lines, j)? + 1;
        } else if t.is_empty() {
            return None;
        } else {
            return Some(j);
        }
    }
}

/// The line where the attribute starting on line `start` closes: its
/// brackets balance, those in string literals not counted.
fn attribute_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut quoted = false;
    for (j, line) in lines.iter().enumerate().skip(start) {
        let mut escaped = false;
        for c in line.chars() {
            if quoted {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => quoted = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Adds the uncleared lint hits of `file` to `hits`.
fn lint(file: &str, lines: &[&str], hits: &mut Hits) {
    let crate_name = file
        .strip_prefix("crates/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    let windows_only = WINDOWS_ONLY_ALLOWED.contains(&crate_name);
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let comment = t.starts_with("//") || t.starts_with("/*") || t.starts_with('*');
        let code = t.split("//").next().unwrap_or("");
        let mut found: Vec<&'static str> = Vec::new();
        if !comment
            && item_name(t).is_some_and(|name| {
                let name = name.to_ascii_lowercase();
                JAVA_PREFIXES.iter().any(|p| name.starts_with(p))
            })
        {
            found.push("java-name");
        }
        if !comment
            && !windows_only
            && (code.contains("encode_utf16") || code.contains("len_utf16"))
        {
            found.push("utf16");
        }
        if fn_name(t).is_some_and(|name| {
            let name = format!("_{}_", name.to_ascii_lowercase());
            SOURCE_NAMES
                .iter()
                .any(|w| name.contains(&format!("_{w}_")))
        }) {
            found.push("source-name");
        }
        if ["#[expect(", "#[allow(", "#![expect(", "#![allow("]
            .iter()
            .any(|p| t.starts_with(p))
            && reason_hits(lines, i)
        {
            found.push("expect-reason");
        }
        if found.is_empty() || cleared(lines, i) {
            continue;
        }
        for lint in found {
            hits.entry((lint, file.to_owned())).or_default().push(i + 1);
        }
    }
}

/// Whether the run of comments and attributes directly above line `i`
/// holds a tag or a marker with its reason.
fn cleared(lines: &[&str], i: usize) -> bool {
    let (start, _) = attributes(lines, i);
    (start..i).any(|j| match lines.get(j).map(|l| mark(l)) {
        Some(Mark::Tag(_)) => true,
        Some(Mark::Marker(reason)) => !reason.is_empty(),
        _ => false,
    })
}

/// Whether the `reason` of the attribute starting on line `i` names the
/// source's internals.
fn reason_hits(lines: &[&str], i: usize) -> bool {
    let end = attribute_end(lines, i).unwrap_or(i);
    let text: Vec<&str> = (i..=end).filter_map(|j| lines.get(j).copied()).collect();
    let text = text.join("\n");
    let Some(reason) = reason(&text) else {
        return false;
    };
    let reason = reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let word = reason
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| REASON_WORDS.contains(&w));
    let phrase = REASON_PHRASES.iter().any(|p| {
        reason.match_indices(p).any(|(at, _)| {
            let before = reason.get(..at).and_then(|s| s.chars().next_back());
            let after = reason.get(at + p.len()..).and_then(|s| s.chars().next());
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
        })
    });
    word || phrase
}

/// The contents of the `reason = "..."` string of attribute text `text`.
fn reason(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("reason")?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let body = rest.strip_prefix('"')?;
    let mut escaped = false;
    for (at, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return body.get(..at),
            _ => {}
        }
    }
    None
}

/// The name of the item whose definition `line` (trimmed) starts.
fn item_name(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace().peekable();
    while let Some(w) = words.next() {
        match w {
            "pub" | "pub(crate)" | "pub(super)" | "pub(self)" | "async" | "unsafe" | "default"
            | "extern" | "\"C\"" => {}
            _ if w.starts_with("pub(in") => {
                if !w.ends_with(')') {
                    words.find(|w| w.ends_with(')'))?;
                }
            }
            "const" if matches!(words.peek(), Some(&("fn" | "unsafe" | "async" | "extern"))) => {}
            "fn" | "struct" | "enum" | "mod" | "const" | "static" | "type" | "trait" | "union"
            | "macro_rules!" => {
                let mut next = words.next()?;
                if next == "mut" {
                    next = words.next()?;
                }
                let next = next.strip_prefix("r#").unwrap_or(next);
                let end = next
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(next.len());
                let name = next.get(..end)?;
                return (!name.is_empty()).then_some(name);
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests;
