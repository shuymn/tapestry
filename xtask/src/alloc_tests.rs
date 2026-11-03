//! `cargo xtask check-alloc-tests`: every allocation guard also runs in a
//! release step of CI (design.md "Scope of No Allocation").
//!
//! An allocation guard is a workspace package's integration test named
//! `alloc` or `*_alloc` (`tests/alloc.rs`, `tests/rearm_alloc.rs`). Tests
//! that bound allocation sizes or order rather than count take other names
//! (`alloc_order`, `memory`, `reserve`). Each guard must appear in the
//! release command of a step in [`WORKFLOW`]: a `cargo test --release`
//! whose `-p` list names the guard's package and whose `--test` list names
//! the guard. The portable guards go in [`ALLOC_STEP`]; a Windows-only
//! crate's guard may have a step of its own on a Windows job (render's).
//!
//! The player's counting allocator exists only in its development builds,
//! so its allocation guards are unit tests of its binary: every `#[test]`
//! under the package's `src` whose body reads the counter ([`COUNTER`]) is
//! one, and must be named `*_alloc` too. Each must be selected by the
//! release command of a step (on a Windows job, [`UNIT_STEP`]): a
//! `cargo test --release` naming the package, its binary and its
//! [`UNIT_FEATURE`] feature, whose libtest filters after `--` select the
//! test by its path (`module::tests::name`) as libtest does: a substring,
//! or the whole path with `--exact`, and no `--skip` matching it.
//!
//! The workflow is read line by line, enough for its own form: the list
//! items with a `name:` and a `run:` (inline or a `|` block; a folded `>`
//! block is refused), with a trailing `\` joining lines, comments dropped
//! and the shell text split into commands. It is not a YAML parser.

use std::path::Path;

use anyhow::{Context as _, bail};
use cargo_metadata::MetadataCommand;
use quote::ToTokens as _;

/// The workflow that runs the release steps, relative to the workspace root.
pub const WORKFLOW: &str = ".github/workflows/ci.yml";
/// The release allocation step of the portable crates.
pub const ALLOC_STEP: &str = "Allocation tests (release)";
/// The package whose unit tests may be allocation guards.
pub const UNIT_PACKAGE: &str = "player";
/// The feature that builds the package's counting allocator.
pub const UNIT_FEATURE: &str = "dev-tools";
/// What a unit test's body names when it counts allocations: the
/// counting allocator's module.
pub const COUNTER: &str = "alloc_count";
/// The release step of the player's allocation guards.
pub const UNIT_STEP: &str = "Player allocation guards (release)";

/// A unit test of [`UNIT_PACKAGE`]'s binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitTest {
    /// Its path as libtest names it, `module::tests::name`.
    pub path: String,
    /// Its source file, for the message.
    pub file: String,
    /// Whether its body reads the counting allocator ([`COUNTER`]).
    pub counts: bool,
}

/// The binary whose unit tests are checked, and its tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitTests {
    pub bin: String,
    pub tests: Vec<UnitTest>,
}

/// An allocation guard: a package and its integration test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guard {
    pub package: String,
    pub test: String,
    /// Its source, for the message.
    pub path: String,
}

/// A workflow step with a `run:` command.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub run: String,
}

/// One `cargo test --release` command of a step.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReleaseTest {
    packages: Vec<String>,
    tests: Vec<String>,
    bins: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    /// libtest's filters, skips and `--exact`, after `--`.
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
}

impl ReleaseTest {
    /// Whether it runs unit test `path` of binary `bin` of `package` with
    /// [`UNIT_FEATURE`].
    fn runs_unit(&self, package: &str, bin: &str, path: &str) -> bool {
        let feature = format!("{package}/{UNIT_FEATURE}");
        let built = self.packages.iter().any(|p| p == package)
            && self.bins.iter().any(|b| b == bin)
            && (self.all_features
                || self
                    .features
                    .iter()
                    .any(|f| f == UNIT_FEATURE || *f == feature));
        let matches = |pattern: &String| {
            if self.exact {
                pattern == path
            } else {
                path.contains(pattern.as_str())
            }
        };
        built
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(matches)
    }
}

pub fn run(manifest: &Path) -> anyhow::Result<Vec<String>> {
    let metadata = crate::metadata(MetadataCommand::new().manifest_path(manifest).no_deps())?;
    let root = metadata.workspace_root.as_std_path();
    let mut guards = Vec::new();
    for package in metadata.workspace_packages() {
        for target in &package.targets {
            if target.is_test() && is_guard(&target.name) {
                let path = target
                    .src_path
                    .as_std_path()
                    .strip_prefix(root)
                    .unwrap_or(target.src_path.as_std_path());
                guards.push(Guard {
                    package: package.name.to_string(),
                    test: target.name.clone(),
                    path: path.to_string_lossy().replace('\\', "/"),
                });
            }
        }
    }
    if guards.is_empty() {
        bail!("no allocation guard found (tests named `alloc` or `*_alloc`)");
    }
    let unit = match metadata
        .workspace_packages()
        .into_iter()
        .find(|p| p.name.as_str() == UNIT_PACKAGE)
    {
        Some(package) => {
            let Some(bin) = package.targets.iter().find(|t| t.is_bin()) else {
                bail!("package `{UNIT_PACKAGE}` has no binary");
            };
            let src = bin
                .src_path
                .as_std_path()
                .parent()
                .context("a binary's source has a directory")?;
            Some(UnitTests {
                bin: bin.name.clone(),
                tests: unit_tests(root, src)?,
            })
        }
        None => None,
    };
    let path = root.join(WORKFLOW);
    let workflow = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    check(&guards, unit.as_ref(), &workflow)
}

/// The unit tests of the crate whose root directory is `src`, read from
/// every `.rs` file under it, its files' paths relative to `root`.
fn unit_tests(root: &Path, src: &Path) -> anyhow::Result<Vec<UnitTest>> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> anyhow::Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(src, &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let relative = path.strip_prefix(src).unwrap_or(&path);
        let module = module_path(&relative.to_string_lossy().replace('\\', "/"));
        let file = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        out.extend(tests_in(&text, &module, &file)?);
    }
    Ok(out)
}

/// The module path of a crate's file from its path under `src`: the root
/// (`main.rs`, `lib.rs`) is empty, and `x/mod.rs` is `x`.
fn module_path(relative: &str) -> String {
    let stem = relative.strip_suffix(".rs").unwrap_or(relative);
    let parts: Vec<&str> = stem.split('/').collect();
    let parts = match parts.as_slice() {
        ["main" | "lib"] => &[][..],
        [rest @ .., "mod"] => rest,
        all => all,
    };
    parts.join("::")
}

/// The `#[test]` functions of one file whose module path is `module`,
/// with those of its inline modules.
fn tests_in(text: &str, module: &str, file: &str) -> anyhow::Result<Vec<UnitTest>> {
    fn collect(items: &[syn::Item], module: &str, file: &str, out: &mut Vec<UnitTest>) {
        for item in items {
            match item {
                syn::Item::Fn(f) if f.attrs.iter().any(|a| a.path().is_ident("test")) => {
                    let name = f.sig.ident.to_string();
                    let counts = f.block.to_token_stream().to_string().contains(COUNTER);
                    out.push(UnitTest {
                        path: join(module, &name),
                        file: file.to_owned(),
                        counts,
                    });
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content {
                        collect(inner, &join(module, &m.ident.to_string()), file, out);
                    }
                }
                _ => {}
            }
        }
    }
    fn join(module: &str, name: &str) -> String {
        if module.is_empty() {
            name.to_owned()
        } else {
            format!("{module}::{name}")
        }
    }
    let parsed = syn::parse_file(text).with_context(|| format!("parsing {file}"))?;
    let mut out = Vec::new();
    collect(&parsed.items, module, file, &mut out);
    Ok(out)
}

/// Whether an integration test is an allocation guard by its name.
pub fn is_guard(test: &str) -> bool {
    test == "alloc" || test.ends_with("_alloc")
}

/// The diagnostics for `guards` and `unit`'s tests against the workflow's
/// text: one per guard that runs in no release step, naming the guard and
/// the step, and one per unit test that counts allocations under another
/// name than `*_alloc`.
pub fn check(
    guards: &[Guard],
    unit: Option<&UnitTests>,
    workflow: &str,
) -> anyhow::Result<Vec<String>> {
    let steps = steps(workflow)?;
    if !steps.iter().any(|s| s.name == ALLOC_STEP) {
        bail!("{WORKFLOW} has no step named `{ALLOC_STEP}`");
    }
    let mut commands: Vec<(&str, ReleaseTest)> = Vec::new();
    for s in &steps {
        for c in release_tests(&s.name, &s.run)? {
            commands.push((s.name.as_str(), c));
        }
    }
    let mut diagnostics = Vec::new();
    for g in guards {
        let names = |c: &ReleaseTest| c.tests.contains(&g.test);
        let builds = |c: &ReleaseTest| c.packages.contains(&g.package);
        if commands.iter().any(|(_, c)| names(c) && builds(c)) {
            continue;
        }
        let without_package: Vec<&str> = commands
            .iter()
            .filter(|(_, c)| names(c))
            .map(|(step, _)| *step)
            .collect();
        diagnostics.push(if without_package.is_empty() {
            format!(
                "allocation guard `{}` ({}) runs in no release step of {WORKFLOW}: \
                 add `--test {}` and `-p {}` to `{ALLOC_STEP}`",
                g.test, g.path, g.test, g.package
            )
        } else {
            format!(
                "allocation guard `{}` ({}) is in the `--test` list of `{}` in {WORKFLOW} \
                 without `-p {}`: add `-p {}` to `{ALLOC_STEP}`",
                g.test,
                g.path,
                without_package.join("`, `"),
                g.package,
                g.package
            )
        });
    }
    if let Some(u) = unit {
        for t in &u.tests {
            let name = t.path.rsplit("::").next().unwrap_or(&t.path);
            if t.counts && !is_guard(name) {
                diagnostics.push(format!(
                    "unit test `{}` ({}) counts allocations ({COUNTER}) but is not named \
                     `*_alloc`: rename it so `{UNIT_STEP}` runs it in release",
                    t.path, t.file
                ));
                continue;
            }
            if !is_guard(name)
                || commands
                    .iter()
                    .any(|(_, c)| c.runs_unit(UNIT_PACKAGE, &u.bin, &t.path))
            {
                continue;
            }
            diagnostics.push(format!(
                "allocation guard `{}` ({}) runs in no release step of {WORKFLOW}: select it in \
                 `{UNIT_STEP}` (`cargo test --release -p {UNIT_PACKAGE} --features \
                 {UNIT_FEATURE} --bin {} -- _alloc`)",
                t.path, t.file, u.bin
            ));
        }
    }
    Ok(diagnostics)
}

/// The steps of a workflow that have a `run:` command, in order.
pub fn steps(workflow: &str) -> anyhow::Result<Vec<Step>> {
    struct Item {
        key_indent: usize,
        name: Option<String>,
        run: Option<String>,
    }
    fn finish(item: Option<Item>, out: &mut Vec<Step>) {
        if let Some(Item {
            run: Some(run),
            name,
            ..
        }) = item
        {
            out.push(Step {
                name: name.unwrap_or_default(),
                run,
            });
        }
    }

    let mut out = Vec::new();
    let mut item: Option<Item> = None;
    // A block scalar in progress: its key's indent and whether it is the
    // item's `run:`.
    let mut block: Option<(usize, bool)> = None;
    for line in workflow.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some((key_indent, is_run)) = block {
            if trimmed.is_empty() || indent > key_indent {
                if is_run && let Some(run) = item.as_mut().and_then(|i| i.run.as_mut()) {
                    run.push_str(trimmed);
                    run.push('\n');
                }
                continue;
            }
            block = None;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key_indent, rest) = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                finish(item.take(), &mut out);
                let key_indent = indent + 2;
                item = Some(Item {
                    key_indent,
                    name: None,
                    run: None,
                });
                (key_indent, rest.trim_start())
            }
            None => (indent, trimmed),
        };
        if item.as_ref().is_some_and(|i| key_indent < i.key_indent) {
            finish(item.take(), &mut out);
        }
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let is_block = value.starts_with('|') || value.starts_with('>');
        let own = item.as_mut().filter(|i| i.key_indent == key_indent);
        let is_run = key == "run" && own.is_some();
        match (key, own) {
            ("name", Some(i)) => i.name = Some(unquote(value).to_owned()),
            // Folding would join the command's lines; refused rather than
            // read differently from the shell.
            ("run", Some(i)) if value.starts_with('>') => bail!(
                "{WORKFLOW}: step `{}` has a folded `run: >`, which this check does not \
                 read; use `run: |`",
                i.name.as_deref().unwrap_or("(unnamed)")
            ),
            ("run", Some(i)) if is_block => i.run = Some(String::new()),
            ("run", Some(i)) => i.run = Some(format!("{}\n", unquote(value))),
            _ => {}
        }
        if is_block {
            block = Some((key_indent, is_run));
        }
    }
    finish(item, &mut out);
    Ok(out)
}

fn unquote(value: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    value
}

/// Shell syntax a release test command may not hold: redirections,
/// background jobs, substitutions and quotes, which would need a shell to
/// read.
const NOT_PLAIN: &[&str] = &["&", "<", ">", "$(", "`", "\"", "'"];

/// The `cargo test --release` commands of step `step`'s shell text, with
/// their `-p` and `--test` lists. The text is split into commands at line
/// ends, `;`, `&&`, `||` and `|`, so one command's flags never join
/// another's, and comments are dropped. A command that only builds
/// (`--no-run`) or lists (`--list` after `--`) runs nothing. A release
/// command with any other shell syntax ([`NOT_PLAIN`]) is an error rather
/// than read differently from the shell.
fn release_tests(step: &str, run: &str) -> anyhow::Result<Vec<ReleaseTest>> {
    let mut out = Vec::new();
    for line in run.replace("\\\n", " ").lines() {
        for command in commands(line) {
            let tokens: Vec<&str> = command.split_whitespace().collect();
            let is_release =
                tokens.windows(2).any(|w| w == ["cargo", "test"]) && command.contains("--release");
            if !is_release {
                continue;
            }
            if NOT_PLAIN.iter().any(|m| command.contains(m)) {
                bail!(
                    "{WORKFLOW}: step `{step}` runs `{}` with shell syntax; this check reads \
                     only plain cargo commands",
                    command.trim()
                );
            }
            out.extend(release_test(&tokens));
        }
    }
    Ok(out)
}

/// One line's commands, split at `;`, `&&`, `||` and `|`, up to a comment:
/// a `#` at the start of a word or right after one of those operators.
fn commands(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut word_start = true;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let operator = match c {
            ';' => true,
            '|' => {
                chars.next_if(|&(_, n)| n == '|');
                true
            }
            '&' => chars.next_if(|&(_, n)| n == '&').is_some(),
            _ => false,
        };
        if operator {
            out.push(&line[start..at]);
            start = chars.peek().map_or(line.len(), |&(next, _)| next);
            word_start = true;
            continue;
        }
        if c == '#' && word_start {
            out.push(&line[start..at]);
            return out;
        }
        word_start = c.is_whitespace();
    }
    out.push(&line[start..]);
    out
}

/// One command's `cargo test --release` lists, if it is one that runs.
fn release_test(tokens: &[&str]) -> Option<ReleaseTest> {
    let start = tokens.windows(2).position(|w| w == ["cargo", "test"])? + 2;
    let mut command = ReleaseTest::default();
    let mut release = false;
    let mut args = tokens.iter().skip(start).copied();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str, short: Option<&str>| {
            if arg == flag || short == Some(arg) {
                args.next().map(str::to_owned)
            } else {
                arg.strip_prefix(flag)
                    .and_then(|v| v.strip_prefix('='))
                    .map(str::to_owned)
            }
        };
        match arg {
            "--" => {
                let rest: Vec<&str> = args.collect();
                if rest.contains(&"--list") {
                    return None;
                }
                libtest_args(&rest, &mut command);
                break;
            }
            "--release" => release = true,
            "--no-run" => return None,
            "--all-features" => command.all_features = true,
            _ => {
                if let Some(p) = value("--package", Some("-p")) {
                    command.packages.push(p);
                } else if let Some(t) = value("--test", None) {
                    command.tests.push(t);
                } else if let Some(b) = value("--bin", None) {
                    command.bins.push(b);
                } else if let Some(f) = value("--features", Some("-F")) {
                    command
                        .features
                        .extend(f.split(',').filter(|f| !f.is_empty()).map(str::to_owned));
                }
            }
        }
    }
    release.then_some(command)
}

/// libtest's arguments after `--`: filters, `--skip` patterns and
/// `--exact`; the options that take a value skip it.
fn libtest_args(args: &[&str], command: &mut ReleaseTest) {
    const VALUED: &[&str] = &[
        "--test-threads",
        "--logfile",
        "--format",
        "--color",
        "-Z",
        "--shuffle-seed",
    ];
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        if arg == "--exact" {
            command.exact = true;
        } else if arg == "--skip" {
            command.skips.extend(args.next().map(str::to_owned));
        } else if let Some(skip) = arg.strip_prefix("--skip=") {
            command.skips.push(skip.to_owned());
        } else if VALUED.contains(&arg) {
            args.next();
        } else if !arg.starts_with('-') {
            command.filters.push(arg.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(package: &str, test: &str) -> Guard {
        Guard {
            package: package.to_owned(),
            test: test.to_owned(),
            path: format!("crates/{package}/tests/{test}.rs"),
        }
    }

    /// A workflow in the real one's form: the portable step with a block
    /// command and a continuation, a Windows step of its own, and steps the
    /// check must not mistake for release commands.
    fn workflow(alloc_tests: &str) -> String {
        format!(
            "\
jobs:
  windows-release:
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      # A comment.
      - name: Render staging allocation test (release)
        run: mise exec -- cargo test --release --locked -p render --test alloc
  linux:
    steps:
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: |
            . -> target
      - name: Test
        run: mise exec -- cargo test --locked -p game --test tempo_alloc
      - name: {ALLOC_STEP}
        run: |
          mise exec -- cargo test --release --locked -p eval -p game \\
            {alloc_tests} -- --test not_a_target
      - name: \"Quoted\"
        run: echo done
"
        )
    }

    fn guards() -> Vec<Guard> {
        vec![
            guard("eval", "alloc"),
            guard("game", "alloc"),
            guard("game", "tempo_alloc"),
            guard("render", "alloc"),
        ]
    }

    #[test]
    fn the_guard_rule_takes_alloc_names_only() {
        for name in ["alloc", "tempo_alloc", "audio_burst_alloc"] {
            assert!(is_guard(name), "{name}");
        }
        for name in ["alloc_order", "allocator", "memory", "reserve", "malloc"] {
            assert!(!is_guard(name), "{name}");
        }
    }

    #[test]
    fn steps_are_read_with_their_commands() {
        let steps = steps(&workflow("--test alloc")).unwrap();
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Render staging allocation test (release)",
                "Test",
                ALLOC_STEP,
                "Quoted"
            ]
        );
        assert_eq!(
            steps[2].run,
            "mise exec -- cargo test --release --locked -p eval -p game \\\n\
             --test alloc -- --test not_a_target\n"
        );
        assert_eq!(
            release_tests("", &steps[2].run).unwrap(),
            [ReleaseTest {
                packages: vec!["eval".into(), "game".into()],
                tests: vec!["alloc".into()],
                filters: vec!["not_a_target".into()],
                ..ReleaseTest::default()
            }]
        );
        assert!(release_tests("", &steps[1].run).unwrap().is_empty());
    }

    #[test]
    fn every_guard_in_a_release_step_passes() {
        let diags = check(
            &guards(),
            None,
            &workflow("--test alloc --test tempo_alloc"),
        )
        .unwrap();
        assert_eq!(diags, Vec::<String>::new());
    }

    #[test]
    fn a_guard_missing_from_the_release_step_fails_with_its_name_and_the_step() {
        // Only in the dev-profile step and after `--`.
        let diags = check(
            &guards(),
            None,
            &workflow("--test alloc --package=eval -- --test tempo_alloc"),
        )
        .unwrap();
        assert_eq!(
            diags,
            [format!(
                "allocation guard `tempo_alloc` (crates/game/tests/tempo_alloc.rs) runs in no \
                 release step of {WORKFLOW}: add `--test tempo_alloc` and `-p game` to \
                 `{ALLOC_STEP}`"
            )]
        );
    }

    #[test]
    fn a_guard_whose_package_the_step_does_not_build_fails() {
        let mut guards = guards();
        guards.push(guard("audio", "ring_out_alloc"));
        let diags = check(
            &guards,
            None,
            &workflow("--test alloc --test tempo_alloc --test=ring_out_alloc"),
        )
        .unwrap();
        assert_eq!(
            diags,
            [format!(
                "allocation guard `ring_out_alloc` (crates/audio/tests/ring_out_alloc.rs) is in \
                 the `--test` list of `{ALLOC_STEP}` in {WORKFLOW} without `-p audio`: add \
                 `-p audio` to `{ALLOC_STEP}`"
            )]
        );
    }

    /// The fixture workflow with `tempo_alloc` left out of [`ALLOC_STEP`]
    /// and a step `Extra` whose block command is `lines`.
    fn with_extra(lines: &[&str]) -> String {
        let mut text = workflow("--test alloc");
        text.push_str("      - name: Extra\n        run: |\n");
        for line in lines {
            text.push_str("          ");
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    fn missing_tempo_alloc() -> String {
        format!(
            "allocation guard `tempo_alloc` (crates/game/tests/tempo_alloc.rs) runs in no \
             release step of {WORKFLOW}: add `--test tempo_alloc` and `-p game` to \
             `{ALLOC_STEP}`"
        )
    }

    #[test]
    fn a_release_command_elsewhere_in_a_step_counts() {
        let text = with_extra(&[
            "cargo test --locked -p eval &&",
            "  cargo test --release --locked -p game --test tempo_alloc || exit 1",
        ]);
        assert_eq!(check(&guards(), None, &text).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn commands_that_run_no_release_test_do_not_count() {
        for lines in [
            // Commented out.
            &["# cargo test --release --locked -p game --test tempo_alloc"][..],
            // A dev command with a comment that names the flag.
            &["cargo test --locked -p game --test tempo_alloc # --release"],
            // A comment right after an operator.
            &["echo done;# cargo test --release --locked -p game --test tempo_alloc"],
            // A dev command whose flags must not join the next command's,
            // one case per separator.
            &["cargo test -p game --test tempo_alloc --locked; cargo test --release -p eval"],
            &["cargo test -p game --test tempo_alloc --locked|cargo test --release -p eval"],
            &["cargo test -p game --test tempo_alloc --locked&&cargo test --release -p eval"],
            &["cargo test -p game --test tempo_alloc --locked||cargo test --release -p eval"],
            &[
                "cargo test -p game --test tempo_alloc --locked",
                "cargo test --release -p eval",
            ],
            // Built only, or listed.
            &["cargo test --release --locked -p game --test tempo_alloc --no-run"],
            &["cargo test --release --locked -p game --test tempo_alloc -- --list"],
        ] {
            let diags = check(&guards(), None, &with_extra(lines)).unwrap();
            assert_eq!(diags, [missing_tempo_alloc()], "{lines:?}");
        }
    }

    #[test]
    fn a_release_command_with_other_shell_syntax_is_refused() {
        for command in [
            "cargo test --release -p game --test tempo_alloc 2>&1 --no-run",
            "cargo test --release -p game --test tempo_alloc -- 2>&1 --list",
            "cargo test --release -p game --test tempo_alloc > log",
            "cargo test --release -p game --test \"tempo_alloc\"",
            "cargo test --release -p game --test $(echo tempo_alloc)",
        ] {
            let e = check(&guards(), None, &with_extra(&[command])).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "{WORKFLOW}: step `Extra` runs `{command}` with shell syntax; this check \
                     reads only plain cargo commands"
                )
            );
        }
    }

    #[test]
    fn a_package_and_a_test_from_different_steps_do_not_count() {
        let text = with_extra(&["cargo test --release --locked -p eval --test tempo_alloc"]);
        assert_eq!(
            check(&guards(), None, &text).unwrap(),
            [format!(
                "allocation guard `tempo_alloc` (crates/game/tests/tempo_alloc.rs) is in the \
                 `--test` list of `Extra` in {WORKFLOW} without `-p game`: add `-p game` to \
                 `{ALLOC_STEP}`"
            )]
        );
    }

    #[test]
    fn a_folded_run_is_refused() {
        let text = workflow("--test alloc").replace("run: |", "run: >");
        let e = check(&guards(), None, &text).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "{WORKFLOW}: step `{ALLOC_STEP}` has a folded `run: >`, which this check does \
                 not read; use `run: |`"
            )
        );
    }

    #[test]
    fn a_workflow_without_the_release_step_is_an_error() {
        let text = workflow("--test alloc").replace(ALLOC_STEP, "Renamed");
        let e = check(&guards(), None, &text).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("{WORKFLOW} has no step named `{ALLOC_STEP}`")
        );
    }

    #[test]
    fn module_paths_follow_the_files() {
        assert_eq!(module_path("main.rs"), "");
        assert_eq!(module_path("input_side.rs"), "input_side");
        assert_eq!(module_path("dev/scenario.rs"), "dev::scenario");
        assert_eq!(module_path("render_side/mod.rs"), "render_side");
    }

    /// Unit tests are found in inline modules with their paths, and a
    /// test reading the counting allocator is told apart from one that
    /// does not; a helper reading it is not a test.
    #[test]
    fn unit_tests_are_found_with_their_paths_and_whether_they_count() {
        let text = "\
fn helper() { let _ = crate::alloc_count::this_thread(); }
#[test]
fn top() {}
#[cfg(test)]
mod tests {
    #[cfg(feature = \"dev-tools\")]
    #[test]
    fn counted_alloc() {
        use crate::alloc_count::this_thread;
        assert_eq!(this_thread(), 0);
    }
    mod inner {
        #[test]
        fn plain() {}
    }
}
";
        let tests = tests_in(text, "measure", "crates/player/src/measure.rs").unwrap();
        let found: Vec<(&str, bool)> = tests.iter().map(|t| (t.path.as_str(), t.counts)).collect();
        assert_eq!(
            found,
            [
                ("measure::top", false),
                ("measure::tests::counted_alloc", true),
                ("measure::tests::inner::plain", false),
            ]
        );
    }

    fn unit(tests: &[(&str, bool)]) -> UnitTests {
        UnitTests {
            bin: "tapestry".to_owned(),
            tests: tests
                .iter()
                .map(|&(path, counts)| UnitTest {
                    path: path.to_owned(),
                    file: "crates/player/src/input_side.rs".to_owned(),
                    counts,
                })
                .collect(),
        }
    }

    const GUARD: &str = "input_side::gate_tests::abort_close_alloc";

    fn unselected() -> String {
        format!(
            "allocation guard `{GUARD}` (crates/player/src/input_side.rs) runs in no release \
             step of {WORKFLOW}: select it in `{UNIT_STEP}` (`cargo test --release -p player \
             --features dev-tools --bin tapestry -- _alloc`)"
        )
    }

    /// The fixture workflow, every integration guard in its step, and a
    /// step `Extra` whose block command is `lines`.
    fn with_unit_step(lines: &[&str]) -> String {
        with_extra(lines).replace(
            "--test alloc -- --test not_a_target",
            "--test alloc --test tempo_alloc -- --test not_a_target",
        )
    }

    #[test]
    fn a_unit_guard_selected_by_a_release_step_passes() {
        let unit = unit(&[(GUARD, true), ("input_side::tests::plain", false)]);
        for command in [
            "cargo test --release --locked -p player --features dev-tools --bin tapestry -- _alloc",
            "cargo test --release -p player -F player/dev-tools,game/dev-tools --bin=tapestry \
             -- --test-threads 1 close_alloc",
            "cargo test --release -p player --all-features --bin tapestry -- --exact \
             input_side::gate_tests::abort_close_alloc",
            "cargo test --release -p player --features=dev-tools --bin tapestry",
        ] {
            let diags = check(&guards(), Some(&unit), &with_unit_step(&[command])).unwrap();
            assert_eq!(diags, Vec::<String>::new(), "{command}");
        }
    }

    #[test]
    fn a_unit_guard_no_release_step_selects_fails() {
        let unit = unit(&[(GUARD, true)]);
        for command in [
            // Dev profile.
            "cargo test --locked -p player --features dev-tools --bin tapestry -- _alloc",
            // Without the counting allocator, the binary or the package.
            "cargo test --release -p player --bin tapestry -- _alloc",
            "cargo test --release -p player --features dev-tools -- _alloc",
            "cargo test --release -p game --features dev-tools --bin tapestry -- _alloc",
            // Filtered out, skipped, or not the whole path with `--exact`.
            "cargo test --release -p player --features dev-tools --bin tapestry -- menu_",
            "cargo test --release -p player --features dev-tools --bin tapestry -- _alloc \
             --skip gate_tests",
            "cargo test --release -p player --features dev-tools --bin tapestry -- --exact _alloc",
            // Listed only.
            "cargo test --release -p player --features dev-tools --bin tapestry -- --list",
        ] {
            let diags = check(&guards(), Some(&unit), &with_unit_step(&[command])).unwrap();
            assert_eq!(diags, [unselected()], "{command}");
        }
    }

    #[test]
    fn a_unit_test_that_counts_under_another_name_fails() {
        let unit = unit(&[("input_side::gate_tests::a_handoff_keeps_the_sources", true)]);
        let command =
            "cargo test --release -p player --features dev-tools --bin tapestry -- handoff";
        let diags = check(&guards(), Some(&unit), &with_unit_step(&[command])).unwrap();
        assert_eq!(
            diags,
            [format!(
                "unit test `input_side::gate_tests::a_handoff_keeps_the_sources` \
                 (crates/player/src/input_side.rs) counts allocations (alloc_count) but is not \
                 named `*_alloc`: rename it so `{UNIT_STEP}` runs it in release"
            )]
        );
    }
}
