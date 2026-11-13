//! The ledger, tag and lint checks on fixtures, and the command on a file
//! tree.

use super::{Mode, check, init, run, update};

const SITE_A: &str = "crates/a/src/lib.rs";
const SITE_B: &str = "crates/b/src/lib.rs";
const TESTS: &str = "crates/a/tests/t.rs";
const LINTED: &str = "crates/c/src/lib.rs";

/// Two entries: SF-001 on one site and one test, SF-002 on two sites and
/// one test.
const LEDGER: &str = r##"
[[entry]]
id = "SF-001"
summary = "A copy"
class = "compatibility"
reason = "Skins rely on it."
source = ["src/A.java:10-20 @beatoraja-0.8.9"]
sites = ["crates/a/src/lib.rs"]
tests = ["crates/a/tests/t.rs::pinned"]

[[entry]]
id = "SF-002"
summary = "A kept byproduct"
class = "kept-byproduct"
decision = "#9"
reason = "The owner keeps it."
source = ["src/B.java:5 @libgdx-1.9.9", "src/C.java:7 @libgdx-1.9.9"]
sites = ["crates/a/src/lib.rs", "crates/b/src/lib.rs"]
tests = ["crates/a/tests/t.rs::also_pinned"]
"##;

/// [`SITE_A`]: a tagged fn, and a tagged statement in a fn.
const SITE_A_TEXT: &str = "\
//! A crate.

/// Reads a value.
// Source: SF-001
fn reads() {}

fn other() {
    // Source: SF-002
    let x = 1;
}
";

const SITE_B_TEXT: &str = "\
// Source: SF-002
pub fn kept() {}
";

/// [`TESTS`]: two tagged tests, an untagged one, an ignored one and a
/// helper.
const TESTS_TEXT: &str = "\
// Source: SF-001
#[test]
fn pinned() {}

/// Another.
// Source: SF-002
#[test]
fn also_pinned() {}

#[test]
fn unlisted() {}

#[test]
#[ignore = \"slow\"]
fn ignored() {}

fn helper() {}
";

/// [`LINTED`]: one hit of each lint, on lines 1, 3, 5 and 9.
const LINTED_TEXT: &str = "\
mod java_int;

fn rounds_as_the_source() {}

#[expect(clippy::cast_possible_truncation, reason = \"Java's (int)\")]
fn cast() {}

fn utf() -> usize {
    \"a\".encode_utf16().count()
}
";

const BASELINE: &str = "\
# A comment.
expect-reason 1 crates/c/src/lib.rs
java-name 1 crates/c/src/lib.rs
source-name 1 crates/c/src/lib.rs
utf16 1 crates/c/src/lib.rs
";

/// The fixture's files, in path order.
fn files() -> Vec<(String, String)> {
    [
        (SITE_A, SITE_A_TEXT),
        (TESTS, TESTS_TEXT),
        (SITE_B, SITE_B_TEXT),
        (LINTED, LINTED_TEXT),
    ]
    .iter()
    .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
    .collect()
}

/// The fixture's files with `from` replaced by `to` in `file`, where it
/// must occur once.
fn files_with(file: &str, from: &str, to: &str) -> Vec<(String, String)> {
    let mut out = files();
    let (_, text) = out.iter_mut().find(|(p, _)| p == file).unwrap();
    assert_eq!(text.matches(from).count(), 1, "{from}");
    *text = text.replace(from, to);
    out
}

/// [`files`] with `file` added holding `text`.
fn files_and(file: &str, text: &str) -> Vec<(String, String)> {
    let mut out = files();
    out.push((file.to_owned(), text.to_owned()));
    out
}

/// [`LEDGER`] with `from` replaced by `to`, which must occur once.
fn ledger_with(from: &str, to: &str) -> String {
    assert_eq!(LEDGER.matches(from).count(), 1, "{from}");
    LEDGER.replace(from, to)
}

fn run_files(files: &[(String, String)]) -> Vec<String> {
    check(LEDGER, BASELINE, files)
}

fn run_ledger(ledger: &str) -> Vec<String> {
    check(ledger, BASELINE, &files())
}

const NONE: [String; 0] = [];

#[test]
fn the_valid_fixture_passes() {
    assert_eq!(run_files(&files()), NONE);
}

#[test]
fn a_malformed_tag_is_reported() {
    let say = |line: &str| {
        format!("{SITE_A}:4: a tag is `// Source: SF-ddd` with one entry ID, not `{line}`")
    };
    for bad in [
        "// Source:SF-001",
        "// Source: SF-12",
        "// Source:  SF-001",
        "// Source: SF-001 and more",
        "// Source: SF-001, SF-002",
        "// Source: SFX",
    ] {
        let tagged = files_with(SITE_A, "// Source: SF-001", bad);
        // The site has lost its tag too.
        assert_eq!(
            run_files(&tagged),
            [
                say(bad),
                format!(
                    "docs/fidelity.toml: SF-001: site `{SITE_A}` has no `// Source: SF-001` above code other than a test"
                ),
            ],
            "{bad}"
        );
    }
}

#[test]
fn provenance_comments_are_not_tags_or_hits() {
    let provenance = "\
// Source: the selection loader (`Loader.java:12-30` at `0.8.9`).
//Source: SF-001 is not read either.
/// Source: SF-001
fn plain() {} // As `Foo.java:12` does.
";
    assert_eq!(
        run_files(&files_and("crates/d/src/lib.rs", provenance)),
        NONE
    );
}

#[test]
fn a_tag_without_an_entry_is_reported() {
    let unknown = files_with(
        SITE_B,
        "// Source: SF-002\n",
        "// Source: SF-002\n// Source: SF-009\n",
    );
    assert_eq!(
        run_files(&unknown),
        [format!(
            "{SITE_B}:2: `SF-009` is not an entry of docs/fidelity.toml"
        )]
    );
}

#[test]
fn a_tag_outside_its_entry_s_files_is_reported() {
    // Above code other than a test in a file that is not a site.
    let outside = files_with(
        SITE_B,
        "// Source: SF-002\n",
        "// Source: SF-002\n// Source: SF-001\n",
    );
    assert_eq!(
        run_files(&outside),
        [format!("{SITE_B}:2: `{SITE_B}` is not a site of SF-001")]
    );
    // A helper of a test file is code other than a test.
    let helper = files_with(TESTS, "fn helper", "// Source: SF-001\nfn helper");
    assert_eq!(
        run_files(&helper),
        [format!("{TESTS}:17: `{TESTS}` is not a site of SF-001")]
    );
}

#[test]
fn a_tag_above_an_unlisted_test_is_reported() {
    // A listed test file, an extra test in it carrying a listed entry's tag.
    let extra = files_with(
        TESTS,
        "#[test]\nfn unlisted",
        "// Source: SF-001\n#[test]\nfn unlisted",
    );
    assert_eq!(
        run_files(&extra),
        [format!(
            "{TESTS}:10: test `unlisted` is not listed in `tests` of SF-001"
        )]
    );
    // A listed test carrying another entry's tag.
    let swapped = files_with(
        TESTS,
        "// Source: SF-001\n#[test]\nfn pinned",
        "// Source: SF-002\n#[test]\nfn pinned",
    );
    assert_eq!(
        run_files(&swapped),
        [
            format!("{TESTS}:1: test `pinned` is not listed in `tests` of SF-002"),
            format!(
                "docs/fidelity.toml: SF-001: test `{TESTS}::pinned` does not carry `// Source: SF-001`"
            ),
        ]
    );
}

#[test]
fn a_tag_not_above_code_is_reported() {
    let stray = "the tag is not in the comments and attributes directly above code";
    let spaced = files_with(SITE_B, "// Source: SF-002\n", "// Source: SF-002\n\n");
    assert_eq!(
        run_files(&spaced),
        [
            format!("{SITE_B}:1: {stray}"),
            format!(
                "docs/fidelity.toml: SF-002: site `{SITE_B}` has no `// Source: SF-002` above code other than a test"
            ),
        ]
    );
    let last = files_and("crates/d/src/lib.rs", "fn f() {}\n// Source: SF-001\n");
    assert_eq!(
        run_files(&last),
        [format!("crates/d/src/lib.rs:2: {stray}")]
    );
}

#[test]
fn a_tag_above_a_multi_line_attribute_reaches_its_item() {
    let attributed = files_with(
        SITE_B,
        "// Source: SF-002\npub fn kept",
        "// Source: SF-002\n#[expect(\n    clippy::x,\n    reason = \"a ] in [a reason\"\n)]\n// A note.\npub fn kept",
    );
    assert_eq!(run_files(&attributed), NONE);
}

#[test]
fn an_untagged_or_missing_site_is_reported() {
    let untagged = files_with(SITE_B, "// Source: SF-002\n", "");
    assert_eq!(
        run_files(&untagged),
        [format!(
            "docs/fidelity.toml: SF-002: site `{SITE_B}` has no `// Source: SF-002` above code other than a test"
        )]
    );
    let missing = ledger_with(
        "\"crates/b/src/lib.rs\"]",
        "\"crates/b/src/lib.rs\", \"crates/b/src/gone.rs\"]",
    );
    assert_eq!(
        run_ledger(&missing),
        ["docs/fidelity.toml: SF-002: site `crates/b/src/gone.rs` does not exist"]
    );
    let outside = ledger_with(
        "\"crates/b/src/lib.rs\"]",
        "\"crates/b/src/lib.rs\", \"xtask/src/main.rs\"]",
    );
    assert_eq!(
        run_ledger(&outside),
        ["docs/fidelity.toml: SF-002: site `xtask/src/main.rs` is not a .rs file under crates/"]
    );
    // A tag above a test does not make its file a site.
    let only_test = ledger_with(
        "sites = [\"crates/a/src/lib.rs\"]",
        "sites = [\"crates/a/src/lib.rs\", \"crates/a/tests/t.rs\"]",
    );
    assert_eq!(
        run_ledger(&only_test),
        [format!(
            "docs/fidelity.toml: SF-001: site `{TESTS}` has no `// Source: SF-001` above code other than a test"
        )]
    );
}

#[test]
fn a_cited_test_must_exist_run_and_carry_the_tag() {
    let at = "docs/fidelity.toml: SF-001";
    let citing = |test: &str| ledger_with("t.rs::pinned", test);
    assert_eq!(
        run_ledger(&citing("t.rs::gone")),
        [
            format!("{TESTS}:1: test `pinned` is not listed in `tests` of SF-001"),
            format!("{at}: `{TESTS}` has no fn `gone`"),
        ]
    );
    assert_eq!(
        run_ledger(&citing("t.rs::helper")),
        [
            format!("{TESTS}:1: test `pinned` is not listed in `tests` of SF-001"),
            format!("{at}: `{TESTS}:17` `helper` is not a #[test]"),
            format!("{at}: test `{TESTS}::helper` does not carry `// Source: SF-001`"),
        ]
    );
    assert_eq!(
        run_ledger(&ledger_with(
            "crates/a/tests/t.rs::pinned",
            "crates/a/tests/u.rs::pinned"
        )),
        [
            format!("{TESTS}:1: test `pinned` is not listed in `tests` of SF-001"),
            format!("{at}: `crates/a/tests/u.rs` does not exist"),
        ]
    );
    // The seed test #[ignore]d.
    let ignored = files_with(TESTS, "#[test]\nfn pinned", "#[test]\n#[ignore]\nfn pinned");
    assert_eq!(
        run_files(&ignored),
        [format!("{at}: `{TESTS}:4` `pinned` is #[ignore]d")]
    );
    // Its tag removed, or a second fn of its name.
    let untagged = files_with(TESTS, "// Source: SF-001\n", "");
    assert_eq!(
        run_files(&untagged),
        [format!(
            "{at}: test `{TESTS}::pinned` does not carry `// Source: SF-001`"
        )]
    );
    let twice = files_with(TESTS, "fn helper() {}", "fn helper() {}\nfn pinned() {}");
    assert_eq!(
        run_files(&twice),
        [format!(
            "{at}: `{TESTS}` defines `pinned` more than once (lines 3, 18)"
        )]
    );
}

#[test]
fn entry_fields_are_checked() {
    let at = "docs/fidelity.toml: SF-001";
    let cases: [(&str, &str, &str); 12] = [
        ("summary = \"A copy\"\n", "", "no `summary`"),
        (
            "summary = \"A copy\"",
            "summary = \"A\\ncopy\"",
            "`summary` is more than one line",
        ),
        (
            "reason = \"Skins rely on it.\"",
            "reason = \" \"",
            "`reason` is empty",
        ),
        ("reason = \"Skins rely on it.\"\n", "", "no `reason`"),
        (
            "reason = \"Skins rely on it.\"",
            "reason = 1",
            "`reason` is not a string",
        ),
        (
            "class = \"compatibility\"",
            "class = \"byproduct\"",
            "`class` is not one of `rule`, `compatibility`, `kept-byproduct`",
        ),
        (
            "class = \"compatibility\"",
            "class = \"compatibility\"\nrecorded = []",
            "unknown field `recorded`",
        ),
        (
            "class = \"compatibility\"",
            "class = \"rule\"\ndecision = \"9\"",
            "`decision` is not `#` and an issue number",
        ),
        (
            "class = \"compatibility\"",
            "class = \"rule\"\ndecision = \"#09\"",
            "`decision` is not `#` and an issue number",
        ),
        (
            "\"src/A.java:10-20 @beatoraja-0.8.9\"",
            "\"src/A.java:20-10 @beatoraja-0.8.9\"",
            "source `src/A.java:20-10 @beatoraja-0.8.9` is not `<path>:<line>[-<line>] @<pin>`",
        ),
        (
            "\"src/A.java:10-20 @beatoraja-0.8.9\"",
            "\"src/A.java @beatoraja-0.8.9\"",
            "source `src/A.java @beatoraja-0.8.9` is not `<path>:<line>[-<line>] @<pin>`",
        ),
        (
            "tests = [\"crates/a/tests/t.rs::pinned\"]",
            "tests = [\"crates/a/tests/t.rs::pinned\", \"crates/a/tests/t.rs\"]",
            "test `crates/a/tests/t.rs` is not `<file>::<fn>`",
        ),
    ];
    for (from, to, message) in cases {
        assert_eq!(
            run_ledger(&ledger_with(from, to)),
            [format!("{at}: {message}")],
            "{to}"
        );
    }
    let no_decision = ledger_with("decision = \"#9\"\n", "");
    assert_eq!(
        run_ledger(&no_decision),
        ["docs/fidelity.toml: SF-002: a kept byproduct needs its `decision`"]
    );
}

#[test]
fn entry_ids_are_well_formed_unique_and_in_order() {
    let bad = ledger_with("id = \"SF-001\"", "id = \"SF-1\"");
    let reported = run_ledger(&bad);
    assert_eq!(
        reported.first().map(String::as_str),
        Some("docs/fidelity.toml: entry 1: `id` is not `SF-` and three digits")
    );
    // Its tags name no entry then.
    assert!(reported.contains(&format!(
        "{SITE_A}:4: `SF-001` is not an entry of docs/fidelity.toml"
    )));
    let twice = ledger_with("id = \"SF-002\"", "id = \"SF-001\"");
    assert_eq!(
        run_ledger(&twice).first().map(String::as_str),
        Some("docs/fidelity.toml: SF-001: `SF-001` is used by an earlier entry")
    );
    let swapped = LEDGER
        .replace("id = \"SF-001\"", "id = \"SF-00X\"")
        .replace("id = \"SF-002\"", "id = \"SF-001\"")
        .replace("id = \"SF-00X\"", "id = \"SF-002\"");
    assert!(
        check(&swapped, BASELINE, &files()).contains(
            &"docs/fidelity.toml: SF-001: `SF-001` comes after `SF-002`; entries are in id order"
                .to_owned()
        )
    );
}

#[test]
fn each_list_is_non_empty_distinct_strings() {
    let at = "docs/fidelity.toml: SF-001";
    for (key, value) in [
        ("source", "\"src/A.java:10-20 @beatoraja-0.8.9\""),
        ("sites", "\"crates/a/src/lib.rs\""),
        ("tests", "\"crates/a/tests/t.rs::pinned\""),
    ] {
        let line = format!("{key} = [{value}]");
        let with = |to: &str| run_ledger(&ledger_with(&line, &format!("{key} = {to}")));
        // The diagnostic each makes is the first; an emptied site or test
        // list also leaves its tags unlisted.
        let first = |to: &str| with(to).into_iter().next();
        assert_eq!(
            first("[]"),
            Some(format!("{at}: `{key}` is empty")),
            "{key}"
        );
        assert_eq!(
            first(&format!("[{value}, 1]")),
            Some(format!("{at}: `{key}` item 2 is not a string")),
            "{key}"
        );
        assert_eq!(
            first(&format!("[{value}, \"\"]")),
            Some(format!("{at}: `{key}` item 2 is empty")),
            "{key}"
        );
        assert_eq!(
            first(&format!("[{value}, {value}]")),
            Some(format!(
                "{at}: `{key}` lists `{}` twice",
                value.trim_matches('"')
            )),
            "{key}"
        );
        assert_eq!(
            first(value),
            Some(format!("{at}: `{key}` is not an array")),
            "{key}"
        );
        assert_eq!(
            run_ledger(&ledger_with(&format!("{line}\n"), "")).first(),
            Some(&format!("{at}: no `{key}`")),
            "{key}"
        );
    }
    // An emptied site list leaves the site's tag outside its files.
    let no_sites = ledger_with("sites = [\"crates/a/src/lib.rs\"]", "sites = []");
    assert_eq!(
        run_ledger(&no_sites),
        [
            "docs/fidelity.toml: SF-001: `sites` is empty".to_owned(),
            format!("{SITE_A}:4: `{SITE_A}` is not a site of SF-001"),
        ]
    );
}

#[test]
fn a_ledger_that_is_not_toml_is_reported() {
    let broken = format!("{LEDGER}\n[[entry]\n");
    let reported = run_ledger(&broken);
    // The unclosed `[[entry]` is line 21.
    assert_eq!(
        reported,
        ["docs/fidelity.toml:21: not TOML: unclosed array table, expected `]`"]
    );
    assert_eq!(
        run_ledger(&format!("title = \"x\"\n{LEDGER}")),
        [
            "docs/fidelity.toml: unknown top-level key `title`; the ledger holds `[[entry]]` tables only"
        ]
    );
    assert_eq!(
        run_ledger("entry = 1\n").first().map(String::as_str),
        Some("docs/fidelity.toml: `entry` is not an array of tables")
    );
    assert_eq!(
        run_ledger("entry = [1]\n").first().map(String::as_str),
        Some("docs/fidelity.toml: entry 1 is not a table")
    );
}

/// The diagnostics of [`LEDGER`] and the fixture's files with `path`
/// holding `text`, against an empty baseline.
fn hits_in(path: &str, text: &str) -> Vec<String> {
    check(LEDGER, "", &files_and(path, text))
        .into_iter()
        .filter(|d| !d.starts_with(LINTED))
        .collect()
}

/// The growth diagnostic of `lint` with `lines` in `file`, `allowed` by
/// the baseline.
fn grew(file: &str, lint: &str, lines: &[usize], allowed: usize) -> String {
    let numbers: Vec<String> = lines.iter().map(ToString::to_string).collect();
    format!(
        "{file}: {lint}: {} hit(s) (lines {}) where xtask/fidelity-baseline.txt allows {allowed}; tag a copy kept on purpose `// Source: SF-ddd`, mark other code `// Not a source copy: <reason>`, or remove what the hit names",
        lines.len(),
        numbers.join(", ")
    )
}

#[test]
fn each_lint_finds_its_pattern() {
    const F: &str = "crates/d/src/lib.rs";
    let hits = "\
pub(crate) mod java_int;
pub const LUAJ_MAX: u8 = 1;
struct GdxBatch;
static mut LIBGDX_STATE: u8 = 0;
pub(crate) const fn java_s_floor() {}
fn rounds_as_the_source() {}
fn the_source_s_timer() {}
fn as_java_does() {}
fn matches_the_source() {}
fn f() -> usize { \"a\".encode_utf16().count() + 'a'.len_utf16() }
#[expect(clippy::x, reason = \"LuaJ holds a double\")]
#[allow(clippy::y, reason = \"libGDX's batch\")]
#![expect(clippy::z, reason = \"the source's cast\")]
#[expect(clippy::w, reason = \"rounds as the source does\")]
fn g() {}
";
    assert_eq!(
        hits_in(F, hits),
        [
            grew(F, "expect-reason", &[11, 12, 13, 14], 0),
            grew(F, "java-name", &[1, 2, 4, 5], 0),
            grew(F, "source-name", &[5, 6, 7, 8, 9], 0),
            grew(F, "utf16", &[10], 0),
        ]
    );
}

#[test]
fn a_reason_continued_on_following_lines_is_read() {
    const F: &str = "crates/d/src/lib.rs";
    let continued = "\
#[expect(
    clippy::cast_precision_loss,
    reason = \"every Int is within 2^53, so exact; a wider one rounds once, \\
              as the source's `l2d` of its `long` does\"
)]
fn float() {}
";
    assert_eq!(hits_in(F, continued), [grew(F, "expect-reason", &[1], 0)]);
    // A continuation inside the phrase joins its words with no backslash.
    let split = "\
#[expect(clippy::cast_precision_loss, reason = \"rounds as the \\
    source does\")]
fn float() {}
";
    assert_eq!(hits_in(F, split), [grew(F, "expect-reason", &[1], 0)]);
}

#[test]
fn near_misses_are_not_hits() {
    let misses = "\
// mod java_int;
/// fn rounds_as_the_source() {}
/* \"a\".encode_utf16() */
// See `Loader.java:12` and `JsonReader.rl:216-268`.
fn takes_the_sources_branch() {}
fn what_the_source_shows() {}
fn javascript_s() {}
let java_x = 1;
fn javanese() {}
const JAVANESE: u8 = 1;
#[expect(clippy::x, reason = \"JavaScript has the source's twin\")]
#[expect(clippy::x, reason = \"as the sources say\")]
#[expect(clippy::x, reason = \"has the source\")]
#[expect(clippy::x)]
#[cfg(test)]
fn g() {} // \"a\".encode_utf16()
";
    // Line 11 names "the source's"; its JavaScript, and lines 12 and 13,
    // are no hits.
    assert_eq!(
        hits_in("crates/d/src/lib.rs", misses),
        [grew("crates/d/src/lib.rs", "expect-reason", &[11], 0)]
    );
    let without = misses.replace(" has the source's twin", "");
    assert_eq!(hits_in("crates/d/src/lib.rs", &without), NONE);
    // UTF-16 in a Windows-only crate is its system calls' encoding.
    let wide = "fn f() -> Vec<u16> { \"a\".encode_utf16().collect() }\n";
    assert_eq!(hits_in("crates/input/src/hid.rs", wide), NONE);
    assert_eq!(
        hits_in("crates/library/src/a.rs", wide),
        [grew("crates/library/src/a.rs", "utf16", &[1], 0)]
    );
}

#[test]
fn the_reference_executor_is_not_linted() {
    for oracle in [
        "crates/skin-beatoraja/src/bin/lua_replay/reference.rs",
        "crates/skin-beatoraja/src/lua/replay.rs",
    ] {
        assert_eq!(hits_in(oracle, LINTED_TEXT), NONE, "{oracle}");
    }
    assert_eq!(
        hits_in(
            "crates/skin-beatoraja/src/lua/replays.rs",
            "mod java_int;\n"
        ),
        [grew(
            "crates/skin-beatoraja/src/lua/replays.rs",
            "java-name",
            &[1],
            0
        )]
    );
}

#[test]
fn a_tag_or_marker_above_a_hit_clears_it() {
    let marked = "\
/// A cast.
// Not a source copy: the cast saturates as the format says.
#[expect(clippy::x, reason = \"Java's (int)\")]
#[test]
fn rounds_as_the_source() {}
";
    assert_eq!(hits_in("crates/d/src/lib.rs", marked), NONE);
    // A marker clears only the hits of the item below its run.
    let next = format!("{marked}\nfn rounds_like_the_source() {{}}\n");
    assert_eq!(
        hits_in("crates/d/src/lib.rs", &next),
        [grew("crates/d/src/lib.rs", "source-name", &[7], 0)]
    );
    // A tag clears the hits of its site's item.
    let tagged = files_with(SITE_A, "fn reads() {}", "fn reads_as_the_source() {}");
    assert_eq!(check(LEDGER, BASELINE, &tagged), NONE);
    // An empty marker is reported and clears nothing.
    let empty = marked.replace("copy: the cast saturates as the format says.", "copy:");
    assert_eq!(
        hits_in("crates/d/src/lib.rs", &empty),
        [
            "crates/d/src/lib.rs:2: `// Not a source copy:` needs its reason".to_owned(),
            grew("crates/d/src/lib.rs", "expect-reason", &[3], 0),
            grew("crates/d/src/lib.rs", "source-name", &[5], 0),
        ]
    );
}

#[test]
fn a_marker_clears_nothing_past_a_code_line_ending_in_a_bracket() {
    // The line above the hit ends in `]` but is code, not an attribute:
    // the marker above the first fn stays with that fn.
    let indexed = "\
// Not a source copy: this helper describes the format.
#[must_use]
fn helper_as_the_source() -> usize {
    0
}

fn unrelated(values: &[&str], i: usize) -> usize {
    values[i]
        .encode_utf16()
        .count()
}
";
    assert_eq!(
        hits_in("crates/d/src/lib.rs", indexed),
        [grew("crates/d/src/lib.rs", "utf16", &[9], 0)]
    );
}

#[test]
fn a_new_hit_beyond_the_baseline_is_reported() {
    let added = files_with(
        LINTED,
        "fn cast() {}",
        "fn cast() {}\nfn floor_as_the_source() {}",
    );
    assert_eq!(run_files(&added), [grew(LINTED, "source-name", &[3, 7], 1)]);
    let renamed = files_with(LINTED, "fn cast() {}", "fn java_cast() {}");
    assert_eq!(run_files(&renamed), [grew(LINTED, "java-name", &[1, 6], 1)]);
}

#[test]
fn a_removed_hit_must_leave_the_baseline() {
    let renamed = files_with(LINTED, "mod java_int;", "mod int;");
    assert_eq!(
        run_files(&renamed),
        [
            "xtask/fidelity-baseline.txt: `java-name 1 crates/c/src/lib.rs` has 0 hit(s) now; run `cargo xtask check-fidelity --update`"
        ]
    );
}

#[test]
fn update_lowers_but_never_raises() {
    let header = super::HEADER;
    // One hit gone, another added in another lint: the first count falls
    // and its line goes, the second stays and its growth is reported.
    let shifted = files_with(LINTED, "mod java_int;", "mod int;\nfn f_as_the_source() {}");
    let (text, diags) = update(LEDGER, BASELINE, &shifted);
    assert_eq!(
        text.as_deref(),
        Some(
            format!(
                "{header}expect-reason 1 crates/c/src/lib.rs\nsource-name 1 crates/c/src/lib.rs\nutf16 1 crates/c/src/lib.rs\n"
            )
            .as_str()
        )
    );
    assert_eq!(diags, [grew(LINTED, "source-name", &[2, 4], 1)]);
    // A count of 2 with 3 hits stays 2.
    let two = BASELINE.replace("source-name 1", "source-name 2");
    let three = files_with(
        LINTED,
        "fn cast() {}",
        "fn cast() {}\nfn a_as_the_source() {}\nfn b_as_the_source() {}",
    );
    let (text, diags) = update(LEDGER, &two, &three);
    assert_eq!(
        text.as_deref(),
        Some(
            format!(
                "{header}expect-reason 1 crates/c/src/lib.rs\njava-name 1 crates/c/src/lib.rs\nsource-name 2 crates/c/src/lib.rs\nutf16 1 crates/c/src/lib.rs\n"
            )
            .as_str()
        )
    );
    assert_eq!(diags, [grew(LINTED, "source-name", &[3, 7, 8], 2)]);
    // A file with hits but no line is not added.
    let (text, diags) = update(LEDGER, "", &files());
    assert_eq!(text.as_deref(), Some(header));
    assert_eq!(diags.len(), 4, "{diags:?}");
    // Once lowered, the check passes.
    let (text, _) = update(
        LEDGER,
        &format!("{BASELINE}java-name 3 crates/b/src/lib.rs\n"),
        &files(),
    );
    let text = text.unwrap();
    assert_eq!(
        text,
        format!("{header}{}", BASELINE.replace("# A comment.\n", ""))
    );
    assert_eq!(check(LEDGER, &text, &files()), NONE);
}

#[test]
fn init_writes_the_current_counts() {
    let (text, diags) = init(LEDGER, &files());
    assert_eq!(diags, NONE);
    assert_eq!(
        text,
        format!(
            "{}{}",
            super::HEADER,
            BASELINE.replace("# A comment.\n", "")
        )
    );
    assert_eq!(check(LEDGER, &text, &files()), NONE);
}

#[test]
fn a_malformed_baseline_is_reported() {
    let at = "xtask/fidelity-baseline.txt";
    for (line, message) in [
        (
            "java-name crates/c/src/lib.rs",
            "a line is `<lint> <count> <file>`",
        ),
        (
            "java-name 1 crates/c/src/lib.rs extra",
            "a line is `<lint> <count> <file>`",
        ),
        ("java 1 crates/c/src/lib.rs", "`java` is not a lint"),
        ("utf16 0 crates/c/src/lib.rs", "`0` is not a positive count"),
        (
            "utf16 -1 crates/c/src/lib.rs",
            "`-1` is not a positive count",
        ),
        (
            "utf16 1 crates/c/src/lib.rs",
            "`utf16 crates/c/src/lib.rs` is listed twice",
        ),
    ] {
        let baseline = format!("{BASELINE}{line}\n");
        assert_eq!(
            check(LEDGER, &baseline, &files()),
            [format!("{at}:6: {message}")],
            "{line}"
        );
        // `--update` writes nothing then.
        let (text, diags) = update(LEDGER, &baseline, &files());
        assert_eq!((text, diags.len()), (None, 1), "{line}");
    }
}

/// The command on a tree: every `.rs` file under `crates/` is read,
/// whether or not it holds a `// Covers:` tag or a `// Source:` tag.
#[test]
fn the_command_reads_every_rust_file_and_bootstraps_the_baseline() {
    let root = tempfile::tempdir().unwrap();
    let write = |path: &str, text: &str| {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("Cargo.toml", "");
    write("docs/fidelity.toml", LEDGER);
    for (path, text) in files() {
        write(&path, &text);
    }
    write("crates/c/notes.txt", "mod java_int;\n");
    write("xtask/src/fixture.rs", "mod java_int;\n");
    let manifest = root.path().join("Cargo.toml");
    let baseline = root.path().join("xtask/fidelity-baseline.txt");
    // No baseline: the check and `--update` fail, `--init` writes it.
    for mode in [Mode::Check, Mode::Update] {
        let e = run(&manifest, mode).unwrap_err();
        assert!(e.to_string().contains("does not exist"), "{e}");
    }
    assert_eq!(run(&manifest, Mode::Init).unwrap(), NONE);
    let written = std::fs::read_to_string(&baseline).unwrap();
    assert_eq!(
        written,
        format!(
            "{}{}",
            super::HEADER,
            BASELINE.replace("# A comment.\n", "")
        )
    );
    // Then it passes, and `--init` refuses to write again.
    assert_eq!(run(&manifest, Mode::Check).unwrap(), NONE);
    let e = run(&manifest, Mode::Init).unwrap_err();
    assert!(e.to_string().contains("exists"), "{e}");
    // A hit added in a file with no tag of either kind is counted.
    write("crates/e/src/lib.rs", "fn rounds_as_the_source() {}\n");
    assert_eq!(
        run(&manifest, Mode::Check).unwrap(),
        [grew("crates/e/src/lib.rs", "source-name", &[1], 0)]
    );
    // `--update` lowers after a hit goes, and leaves the new one reported.
    write(LINTED, &LINTED_TEXT.replace("mod java_int;", "mod int;"));
    assert_eq!(
        run(&manifest, Mode::Update).unwrap(),
        [grew("crates/e/src/lib.rs", "source-name", &[1], 0)]
    );
    let lowered = std::fs::read_to_string(&baseline).unwrap();
    assert!(!lowered.contains("java-name"), "{lowered}");
    assert!(!lowered.contains("crates/e/"), "{lowered}");
}
