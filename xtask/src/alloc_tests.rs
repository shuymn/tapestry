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
//! so its allocation guards are unit tests of its binary, named `*_alloc`:
//! every `#[test]` reachable from the binary's root whose body names the counter's
//! module ([`COUNTER`], an identifier, not a string) must be, and one that
//! measures through a helper must be named so too, since the body check
//! does not follow calls. Each must be selected by a release command of a
//! step (on a Windows job, [`UNIT_STEP`]). Such a command names the package
//! and has exactly the form of [`UNIT_COMMAND`], followed by libtest's
//! substring filters and only `--exact` and `--skip`; it selects a test by
//! its path (`module::tests::name`) as libtest does. Any other command
//! naming the package is an error rather than read as cargo or libtest
//! might. The tests are found by walking the binary's modules from its root
//! as the compiler does, and a guard the walk cannot place for certain (an
//! attribute that may gate it out of the release build anywhere above it,
//! `#[ignore]`, `include!`, a `#[path]` it cannot follow exactly) is an
//! error ([`unit_tests`]).
//!
//! A step that selects tests by name runs nothing when a test is renamed,
//! and libtest still exits 0. So every `cargo test` or `cargo nextest run`
//! command of the workflow that passes test filters must name one package
//! and its targets (`--test`, `--bin`, `--lib`), and each filter must
//! select a test of those targets, found by the same walk of the target's
//! modules ([`selections`], [`check_selections`]).
//!
//! The check catches omissions, not adversarial edits: a step a condition
//! disables (`if: false`) still counts, and a shell variable before `--`
//! (`$DEV_TOOLS`) is taken for flags, not read.
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
/// The package's binary, whose unit tests the guards are.
pub const UNIT_BIN: &str = "tapestry";
/// The arguments after `cargo test` of a release command that runs the
/// unit guards, before libtest's `--`.
pub const UNIT_COMMAND: &[&str] = &[
    "--release",
    "--locked",
    "-p",
    UNIT_PACKAGE,
    "--features",
    UNIT_FEATURE,
    "--bin",
    UNIT_BIN,
];
/// The `cfg` predicates a unit guard and its modules may carry.
const UNIT_CFGS: &[&str] = &["test", "feature=\"dev-tools\""];

/// A unit test of [`UNIT_PACKAGE`]'s binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitTest {
    /// Its path as libtest names it, `module::tests::name`.
    pub path: String,
    /// Its source file, for the message.
    pub file: String,
    /// Whether its body reads the counting allocator ([`COUNTER`]).
    pub counts: bool,
    /// Whether it has `#[ignore]`.
    pub ignored: bool,
    /// The `cfg` predicates (whitespace removed) and `cfg_attr` attributes
    /// on it and on every module and file above it, outermost first
    /// ([`cfgs`]).
    pub cfgs: Vec<String>,
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
    /// For the unit guards' command ([`UNIT_COMMAND`]), what it selects.
    unit: Option<Selection>,
}

/// libtest's selection of a unit guards' command, after `--`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    filters: Vec<String>,
    skips: Vec<String>,
    exact: bool,
}

impl Selection {
    /// Whether it runs the unit test whose path is `path`.
    fn runs(&self, path: &str) -> bool {
        let matches = |pattern: &String| {
            if self.exact {
                pattern == path
            } else {
                path.contains(pattern.as_str())
            }
        };
        (self.filters.is_empty() || self.filters.iter().any(matches))
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
            if bin.name != UNIT_BIN {
                bail!(
                    "package `{UNIT_PACKAGE}`'s binary is `{}`, not `{UNIT_BIN}`",
                    bin.name
                );
            }
            let root_file = bin
                .src_path
                .as_std_path()
                .strip_prefix(root)
                .context("the binary's source is in the workspace")?
                .to_string_lossy()
                .replace('\\', "/");
            let read = |path: &str| -> anyhow::Result<Option<String>> {
                match std::fs::read_to_string(root.join(path)) {
                    Ok(text) => Ok(Some(text)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e).with_context(|| format!("cannot read {path}")),
                }
            };
            Some(unit_tests(&root_file, &read)?)
        }
        None => None,
    };
    let ci = root.join(WORKFLOW);
    let workflow =
        std::fs::read_to_string(&ci).with_context(|| format!("cannot read {}", ci.display()))?;
    let mut diagnostics = check(&guards, unit.as_deref(), &workflow)?;
    let tests_of = |package: &str, target: &Target| -> anyhow::Result<Option<Vec<UnitTest>>> {
        let Some(found) = metadata
            .workspace_packages()
            .into_iter()
            .find(|p| p.name.as_str() == package)
        else {
            return Ok(None);
        };
        let Some(t) = found.targets.iter().find(|t| match target {
            Target::Test(name) => t.is_test() && t.name == *name,
            Target::Bin(name) => t.is_bin() && t.name == *name,
            Target::Lib => t.is_lib(),
        }) else {
            return Ok(None);
        };
        let root_file = t
            .src_path
            .as_std_path()
            .strip_prefix(root)
            .context("the target's source is in the workspace")?
            .to_string_lossy()
            .replace('\\', "/");
        let read = |path: &str| -> anyhow::Result<Option<String>> {
            match std::fs::read_to_string(root.join(path)) {
                Ok(text) => Ok(Some(text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("cannot read {path}")),
            }
        };
        Ok(Some(unit_tests(&root_file, &read)?))
    };
    let features_of = |package: &str| -> Option<Features> {
        metadata
            .workspace_packages()
            .into_iter()
            .find(|p| p.name.as_str() == package)
            .map(|p| {
                p.features
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
    };
    let dir = root.join(WORKFLOWS);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .with_context(|| format!("cannot read {}", dir.display()))?;
    files.retain(|f| f.extension().is_some_and(|e| e == "yml" || e == "yaml"));
    files.sort();
    let mut selected = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let name = format!(
            "{WORKFLOWS}/{}",
            file.file_name().unwrap_or_default().to_string_lossy()
        );
        selected.extend(selections(&text, &name)?);
    }
    diagnostics.extend(check_selections(&selected, &tests_of, &features_of)?);
    Ok(diagnostics)
}

/// The directory of the workflows whose by-name selections are checked.
pub const WORKFLOWS: &str = ".github/workflows";

/// A target of a package a command selects tests from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Test(String),
    Bin(String),
    Lib,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Test(name) => write!(f, "--test {name}"),
            Target::Bin(name) => write!(f, "--bin {name}"),
            Target::Lib => f.write_str("--lib"),
        }
    }
}

/// Which `#[ignore]`d tests a command runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Ignored {
    /// Only the tests without `#[ignore]` (libtest's and nextest's default).
    #[default]
    Not,
    /// Only the ignored ones (`--ignored`, `--run-ignored only`).
    Only,
    /// Both (`--include-ignored`, `--run-ignored all`).
    All,
}

/// A workflow command that selects tests: by name filters, `--skip`
/// patterns, or by `#[ignore]`.
#[derive(Debug, PartialEq, Eq)]
pub struct ByName {
    workflow: String,
    step: String,
    command: String,
    package: String,
    targets: Vec<Target>,
    selection: Selection,
    ignored: Ignored,
    /// The features asked for, for the package (`f` or `package/f`).
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
}

/// A package's features and what each enables, as in its manifest.
pub type Features = std::collections::BTreeMap<String, Vec<String>>;

/// The tests of a package's target with what decides whether they run,
/// `None` when the package or the target does not exist.
type TestsOf<'a> = &'a dyn Fn(&str, &Target) -> anyhow::Result<Option<Vec<UnitTest>>>;
/// A package's features, `None` when it does not exist.
type FeaturesOf<'a> = &'a dyn Fn(&str) -> Option<Features>;

/// Flags before `--` that take a value, of `cargo test` and `cargo nextest
/// run`, so the value is not read as a filter.
const VALUE_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "-F",
    "--features",
    "--test",
    "--bin",
    "--example",
    "--bench",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--profile",
    "-P",
    "--cargo-profile",
    "--run-ignored",
    "--partition",
    "-j",
    "--jobs",
    "--test-threads",
    "--message-format",
    "--retries",
    "--no-tests",
    "--color",
    "--config",
    "-Z",
];

/// The commands of workflow `name` (its text `workflow`) that select tests:
/// a `cargo test` or `cargo nextest run` with test filters (before or after
/// `--`), `--skip` patterns, or a selection of ignored tests. Such a command
/// must name one package and its targets and hold no shell syntax
/// ([`NOT_PLAIN`], `$`); one that only builds or lists is skipped, and a
/// nextest filterset (`-E`) or a flag this check does not know is refused.
pub fn selections(workflow: &str, name: &str) -> anyhow::Result<Vec<ByName>> {
    let mut out = Vec::new();
    for step in steps_in(workflow, name)? {
        for line in step.run.replace("\\\n", " ").lines() {
            for command in commands(line) {
                if let Some(by_name) = by_name(name, &step.name, command.trim())? {
                    out.push(by_name);
                }
            }
        }
    }
    Ok(out)
}

/// `command`'s selection, if it has one.
fn by_name(workflow: &str, step: &str, command: &str) -> anyhow::Result<Option<ByName>> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let start = if let Some(i) = tokens.windows(2).position(|w| w == ["cargo", "test"]) {
        i + 2
    } else if let Some(i) = tokens
        .windows(3)
        .position(|w| w == ["cargo", "nextest", "run"])
    {
        i + 3
    } else {
        return Ok(None);
    };
    let refuse = |why: &str| {
        anyhow::anyhow!("{workflow}: step `{step}` runs `{command}`, which selects tests; {why}")
    };
    let args = tokens.get(start..).unwrap_or_default();
    let (head, tail) = match args.iter().position(|&a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    let mut packages = Vec::new();
    let mut targets = Vec::new();
    let mut features = Vec::new();
    let mut all_features = false;
    let mut no_default_features = false;
    let mut ignored = Ignored::Not;
    let mut selection = Selection::default();
    let mut head = head.iter().copied();
    while let Some(arg) = head.next() {
        match arg {
            "--no-run" => return Ok(None),
            "--lib" => {
                targets.push(Target::Lib);
                continue;
            }
            "--all-features" => {
                all_features = true;
                continue;
            }
            "--no-default-features" => {
                no_default_features = true;
                continue;
            }
            _ => {}
        }
        if ["-E", "--filterset", "--filter-expr"].contains(&arg) || arg.starts_with("-E") {
            return Err(refuse("write its filters after `--`, not as a filterset"));
        }
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg, None),
        };
        if VALUE_FLAGS.contains(&flag) {
            let value = match inline {
                Some(v) => Some(v.to_owned()),
                None => head.next().map(str::to_owned),
            };
            match (flag, value) {
                ("-p" | "--package", Some(p)) => packages.push(p),
                ("--test", Some(t)) => targets.push(Target::Test(t)),
                ("--bin", Some(b)) => targets.push(Target::Bin(b)),
                ("-F" | "--features", Some(f)) => {
                    features.extend(f.split(',').filter(|f| !f.is_empty()).map(str::to_owned))
                }
                ("--run-ignored", Some(r)) => {
                    ignored = match r.as_str() {
                        "default" => Ignored::Not,
                        "only" | "ignored-only" => Ignored::Only,
                        "all" => Ignored::All,
                        _ => return Err(refuse(&format!("this check does not read `{arg}`"))),
                    }
                }
                _ => {}
            }
        } else if let Some(p) = arg.strip_prefix("-p").filter(|p| !p.is_empty()) {
            packages.push(p.to_owned());
        } else if let Some(f) = arg.strip_prefix("-F").filter(|f| !f.is_empty()) {
            features.extend(f.split(',').filter(|f| !f.is_empty()).map(str::to_owned));
        } else if !arg.starts_with('-') && !arg.starts_with('$') {
            selection.filters.push(arg.to_owned());
        }
    }
    let mut tail = tail.iter().copied();
    while let Some(arg) = tail.next() {
        match arg {
            "--list" => return Ok(None),
            "--exact" => selection.exact = true,
            "--skip" => selection.skips.push(
                tail.next()
                    .ok_or_else(|| refuse("its `--skip` has no pattern"))?
                    .to_owned(),
            ),
            "--ignored" => ignored = Ignored::Only,
            "--include-ignored" => ignored = Ignored::All,
            "--nocapture" | "--no-capture" | "--show-output" | "--test" => {}
            _ if arg.starts_with("--test-threads=") => {}
            "--test-threads" => {
                tail.next();
            }
            _ if arg.starts_with('-') => {
                return Err(refuse(&format!("this check does not read `{arg}`")));
            }
            _ => selection.filters.push(arg.to_owned()),
        }
    }
    if selection.filters.is_empty() && selection.skips.is_empty() && ignored == Ignored::Not {
        return Ok(None);
    }
    if NOT_PLAIN.iter().any(|m| command.contains(m)) || command.contains('$') {
        return Err(refuse(
            "this check reads only plain commands, without shell syntax",
        ));
    }
    let [package] = packages.as_slice() else {
        return Err(refuse("name exactly one package with `-p`"));
    };
    if targets.is_empty() {
        return Err(refuse("name its targets (`--test`, `--bin` or `--lib`)"));
    }
    let features = features
        .into_iter()
        .filter_map(|f| match f.split_once('/') {
            Some((p, f)) => (p == package).then(|| f.to_owned()),
            None => Some(f),
        })
        .collect();
    Ok(Some(ByName {
        workflow: workflow.to_owned(),
        step: step.to_owned(),
        command: command.to_owned(),
        package: package.clone(),
        targets,
        selection,
        ignored,
        features,
        all_features,
        no_default_features,
    }))
}

/// The features of a package that `s` enables, through `default` and what
/// each enables; `None` for every feature with `--all-features`.
fn enabled(s: &ByName, manifest: &Features) -> Option<Vec<String>> {
    if s.all_features {
        return None;
    }
    let mut on: Vec<String> = s.features.clone();
    if !s.no_default_features {
        on.push("default".to_owned());
    }
    let mut i = 0;
    while let Some(f) = on.get(i).cloned() {
        for next in manifest.get(&f).into_iter().flatten() {
            if !next.contains(':') && !next.contains('/') && !on.contains(next) {
                on.push(next.clone());
            }
        }
        i += 1;
    }
    Some(on)
}

/// Whether `s` runs test `t`, if it can be told: its `#[ignore]` against the
/// command's choice, then every `cfg` above it. `cfg(test)` always holds and
/// `cfg(feature = "f")` holds when the command enables `f`; any other `cfg`
/// or a `cfg_attr` cannot be told (`Err` names it).
fn runs(s: &ByName, t: &UnitTest, on: Option<&[String]>) -> Result<(), Reason> {
    let ignored_ok = match s.ignored {
        Ignored::Not => !t.ignored,
        Ignored::Only => t.ignored,
        Ignored::All => true,
    };
    if !ignored_ok {
        return Err(Reason::Not(if t.ignored {
            "ignored".to_owned()
        } else {
            "not ignored".to_owned()
        }));
    }
    let mut unknown = None;
    for c in &t.cfgs {
        if c == "test" {
            continue;
        }
        match c
            .strip_prefix("feature=\"")
            .and_then(|f| f.strip_suffix('"'))
        {
            Some(f) => {
                if on.is_some_and(|on| !on.iter().any(|o| o == f)) {
                    return Err(Reason::Not(format!("needs feature `{f}`")));
                }
                if on.is_none() && !s.all_features {
                    unknown.get_or_insert_with(|| c.clone());
                }
            }
            None => {
                unknown.get_or_insert_with(|| c.clone());
            }
        }
    }
    unknown.map_or(Ok(()), |c| Err(Reason::Unknown(c)))
}

/// Why a command does not run a test, or cannot be told to.
enum Reason {
    Not(String),
    Unknown(String),
}

/// The diagnostics for selections: one per target that does not exist, and
/// one per filter (or, with none, per command) whose selection leaves no test
/// the command runs. A selection whose only candidates sit under a `cfg`
/// this check cannot evaluate fails too: it is rejected, not assumed to run.
pub fn check_selections(
    selected: &[ByName],
    tests_of: TestsOf,
    features_of: FeaturesOf,
) -> anyhow::Result<Vec<String>> {
    let mut diagnostics = Vec::new();
    for s in selected {
        let where_ = format!("step `{}` of {}", s.step, s.workflow);
        let mut tests = Vec::new();
        for target in &s.targets {
            match tests_of(&s.package, target)? {
                Some(found) => tests.extend(found),
                None => diagnostics.push(format!(
                    "{where_} selects tests of `-p {} {target}`, which does not exist",
                    s.package
                )),
            }
        }
        let manifest = features_of(&s.package);
        // `None`: every feature (`--all-features`) or, with no manifest, unknown.
        let on = manifest.as_ref().and_then(|m| enabled(s, m));
        let targets: Vec<String> = s.targets.iter().map(ToString::to_string).collect();
        let scope = format!("`-p {} {}`", s.package, targets.join(" "));
        let groups: Vec<(String, Selection)> = if s.selection.filters.is_empty() {
            vec![(
                if s.selection.skips.is_empty() {
                    "every test".to_owned()
                } else {
                    "every test outside its `--skip` patterns".to_owned()
                },
                Selection {
                    filters: vec![],
                    skips: s.selection.skips.clone(),
                    exact: s.selection.exact,
                },
            )]
        } else {
            s.selection
                .filters
                .iter()
                .map(|f| {
                    (
                        format!("`{f}`{}", if s.selection.exact { " (--exact)" } else { "" }),
                        Selection {
                            filters: vec![f.clone()],
                            skips: s.selection.skips.clone(),
                            exact: s.selection.exact,
                        },
                    )
                })
                .collect()
        };
        for (label, one) in groups {
            let candidates: Vec<&UnitTest> = tests.iter().filter(|t| one.runs(&t.path)).collect();
            let verdicts: Vec<(&UnitTest, Result<(), Reason>)> = candidates
                .iter()
                .map(|t| (*t, runs(s, t, on.as_deref())))
                .collect();
            if verdicts.iter().any(|(_, v)| v.is_ok()) {
                continue;
            }
            if candidates.is_empty() {
                diagnostics.push(format!(
                    "{where_} selects {label} in {scope}, which matches no test there, so the \
                     step would run nothing: `{}`",
                    s.command
                ));
                continue;
            }
            let unknown: Vec<String> = verdicts
                .iter()
                .filter_map(|(t, v)| match v {
                    Err(Reason::Unknown(c)) => Some(format!("`{}` (under `{c}`)", t.path)),
                    _ => None,
                })
                .collect();
            if !unknown.is_empty() {
                diagnostics.push(format!(
                    "{where_} selects {label} in {scope}, and this check cannot tell whether the \
                     command runs {}: rejected rather than assumed to run: `{}`",
                    unknown.join(", "),
                    s.command
                ));
                continue;
            }
            let not: Vec<String> = verdicts
                .iter()
                .filter_map(|(t, v)| match v {
                    Err(Reason::Not(why)) => Some(format!("`{}` ({why})", t.path)),
                    _ => None,
                })
                .collect();
            diagnostics.push(format!(
                "{where_} selects {label} in {scope}, but the command runs none of the tests it \
                 matches, so the step would run nothing: {}: `{}`",
                not.join(", "),
                s.command
            ));
        }
    }
    Ok(diagnostics)
}

/// Reads a file by its path relative to the workspace root, `/`-separated:
/// `None` when there is no such file.
type Read<'a> = &'a dyn Fn(&str) -> anyhow::Result<Option<String>>;

/// The unit tests of the crate whose root file is `root_file` (relative to
/// the workspace root), found by walking its modules from the root as the
/// compiler does: `mod x;` is `x.rs` or `x/mod.rs` in the declaring
/// module's directory, and a file-level `#[path]` is relative to the
/// declaring file's directory.
///
/// An attribute can gate a guard in four places: the test function, each
/// enclosing inline module, each `mod x;` line (a `#[path]` one included),
/// and each file's inner attributes, along the whole chain from the root.
/// A `cfg` other than [`UNIT_CFGS`], or any `cfg_attr`, at any of them
/// restricts everything beneath it, across files, and a restricted guard is
/// an error, as are an ignored guard, `include!` in a walked file, a
/// `#[path]` inside an inline module or not a string, a `mod x;` in a file
/// reached by `#[path]`, and an unrestricted `mod x;` with no file.
fn unit_tests(root_file: &str, read: Read) -> anyhow::Result<Vec<UnitTest>> {
    let text = read(root_file)?.with_context(|| format!("cannot read {root_file}"))?;
    let mut walk = Walk {
        read,
        out: Vec::new(),
        chain: Vec::new(),
    };
    walk.file(root_file, &text, "", &parent(root_file), false, None)?;
    Ok(walk.out)
}

struct Walk<'a> {
    read: Read<'a>,
    out: Vec<UnitTest>,
    /// The `cfg` and `cfg_attr` attributes of the modules and files being
    /// walked ([`cfgs`]).
    chain: Vec<String>,
}

impl Walk<'_> {
    /// File `file` of module `module`, holding `text`, whose `mod x;` files
    /// lie in `dir`; `followed` when a `#[path]` reached it, `restricted`
    /// the gate above it.
    fn file(
        &mut self,
        file: &str,
        text: &str,
        module: &str,
        dir: &str,
        followed: bool,
        restricted: Option<String>,
    ) -> anyhow::Result<()> {
        if has_macro(
            &text.parse().map_err(|e| anyhow::anyhow!("{file}: {e}"))?,
            "include",
        ) {
            bail!(
                "{file} uses `include!`, whose code this check cannot place; its unit guards \
                 could run unseen"
            );
        }
        let syntax = syn::parse_file(text).with_context(|| format!("parsing {file}"))?;
        let restricted = restricted.or_else(|| gate(&syntax.attrs));
        let depth = self.chain.len();
        self.chain.extend(cfgs(&syntax.attrs));
        let walked = self.items(
            &syntax.items,
            file,
            module,
            dir,
            true,
            followed,
            restricted.as_deref(),
        );
        self.chain.truncate(depth);
        walked
    }

    /// The items of module `module` in `file` (`top` when they are the
    /// file's own, not an inline module's), its `mod x;` files in `dir`.
    #[expect(clippy::too_many_arguments, reason = "the walk's state, passed down")]
    fn items(
        &mut self,
        items: &[syn::Item],
        file: &str,
        module: &str,
        dir: &str,
        top: bool,
        followed: bool,
        restricted: Option<&str>,
    ) -> anyhow::Result<()> {
        for item in items {
            match item {
                syn::Item::Fn(f) if f.attrs.iter().any(|a| a.path().is_ident("test")) => {
                    let name = f.sig.ident.to_string();
                    let path = join(module, &name);
                    let counts = has_ident(&f.block.to_token_stream(), COUNTER);
                    if counts || is_guard(&name) {
                        let gated = restricted.map(str::to_owned).or_else(|| gate(&f.attrs));
                        if let Some(gated) = gated {
                            bail!(
                                "allocation guard `{path}` ({file}) is under `{gated}`, which \
                                 the release command may not build: only `cfg(test)` and \
                                 `cfg(feature = \"{UNIT_FEATURE}\")` are allowed, and no \
                                 `cfg_attr`"
                            );
                        }
                        if f.attrs.iter().any(|a| a.path().is_ident("ignore")) {
                            bail!(
                                "allocation guard `{path}` ({file}) has `#[ignore]`, so the \
                                 release command does not run it"
                            );
                        }
                    }
                    let mut chain = self.chain.clone();
                    chain.extend(cfgs(&f.attrs));
                    self.out.push(UnitTest {
                        path,
                        file: file.to_owned(),
                        counts,
                        ignored: f.attrs.iter().any(|a| a.path().is_ident("ignore")),
                        cfgs: chain,
                    });
                }
                syn::Item::Mod(m) => {
                    let name = m.ident.to_string();
                    let child = join(module, &name);
                    let child_dir = under(dir, &name);
                    let gated = restricted.map(str::to_owned).or_else(|| gate(&m.attrs));
                    let path = m.attrs.iter().find(|a| a.path().is_ident("path"));
                    let not_followed = || {
                        anyhow::anyhow!(
                            "{file}: module `{child}` has a `#[path]`, which this check does \
                             not follow"
                        )
                    };
                    let depth = self.chain.len();
                    self.chain.extend(cfgs(&m.attrs));
                    if let Some((_, inner)) = &m.content {
                        if path.is_some() {
                            return Err(not_followed());
                        }
                        self.items(
                            inner,
                            file,
                            &child,
                            &child_dir,
                            false,
                            followed,
                            gated.as_deref(),
                        )?;
                        self.chain.truncate(depth);
                        continue;
                    }
                    if m.attrs.iter().any(|a| a.path().is_ident("cfg_attr")) {
                        bail!(
                            "{file}: module `{child}` has a `#[cfg_attr]` on its `mod` line, \
                             which may change the file it reads"
                        );
                    }
                    if followed {
                        bail!(
                            "{file}: module `{child}` is declared in a file reached by `#[path]`"
                        );
                    }
                    let target = match path.map(|a| &a.meta) {
                        Some(syn::Meta::NameValue(syn::MetaNameValue {
                            value:
                                syn::Expr::Lit(syn::ExprLit {
                                    lit: syn::Lit::Str(s),
                                    ..
                                }),
                            ..
                        })) if top => {
                            let target = normalize(&under(&parent(file), &s.value()));
                            let text = (self.read)(&target)?
                                .with_context(|| format!("cannot read {target}"))?;
                            Some((target, text, true))
                        }
                        Some(_) => return Err(not_followed()),
                        None => {
                            let own = under(dir, &format!("{name}.rs"));
                            let nested = under(&child_dir, "mod.rs");
                            match ((self.read)(&own)?, (self.read)(&nested)?) {
                                (Some(text), None) => Some((own, text, false)),
                                (None, Some(text)) => Some((nested, text, false)),
                                (None, None) if gated.is_some() => None,
                                _ => bail!(
                                    "{file}: module `{child}` has no single file ({own} or \
                                     {nested})"
                                ),
                            }
                        }
                    };
                    if let Some((target, text, by_path)) = target {
                        self.file(&target, &text, &child, &child_dir, by_path, gated)?;
                    }
                    self.chain.truncate(depth);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `path`'s directory, `/`-separated.
fn parent(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(dir, _)| dir.to_owned())
}

/// `name` in directory `dir`.
fn under(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// `path` with its `.` and `..` components resolved.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

fn join(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

/// The first attribute of `attrs` (outer or inner) that may gate what it
/// covers out of the release command: a `cfg` other than [`UNIT_CFGS`], or
/// any `cfg_attr`, as written.
fn gate(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        let syn::Meta::List(list) = &a.meta else {
            return None;
        };
        if a.path().is_ident("cfg_attr") {
            return Some(format!("cfg_attr({})", list.tokens));
        }
        if !a.path().is_ident("cfg") {
            return None;
        }
        let predicate: String = list
            .tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        (!UNIT_CFGS.contains(&predicate.as_str())).then(|| format!("cfg({})", list.tokens))
    })
}

/// Every `cfg` predicate of `attrs` (outer or inner), whitespace removed, and
/// every `cfg_attr` as written (`cfg_attr(...)`), in order.
fn cfgs(attrs: &[syn::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter_map(|a| {
            let syn::Meta::List(list) = &a.meta else {
                return None;
            };
            if a.path().is_ident("cfg_attr") {
                Some(format!("cfg_attr({})", list.tokens))
            } else if a.path().is_ident("cfg") {
                Some(
                    list.tokens
                        .to_string()
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect(),
                )
            } else {
                None
            }
        })
        .collect()
}

/// Whether `tokens` hold the identifier `name`, at any depth.
fn has_ident(tokens: &proc_macro2::TokenStream, name: &str) -> bool {
    tokens.clone().into_iter().any(|t| match t {
        proc_macro2::TokenTree::Ident(i) => i == name,
        proc_macro2::TokenTree::Group(g) => has_ident(&g.stream(), name),
        _ => false,
    })
}

/// Whether `tokens` invoke macro `name` (`name!`), at any depth.
fn has_macro(tokens: &proc_macro2::TokenStream, name: &str) -> bool {
    let trees: Vec<proc_macro2::TokenTree> = tokens.clone().into_iter().collect();
    trees.iter().enumerate().any(|(i, t)| match t {
        proc_macro2::TokenTree::Ident(id) => {
            id == name
                && matches!(trees.get(i + 1), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == '!')
        }
        proc_macro2::TokenTree::Group(g) => has_macro(&g.stream(), name),
        _ => false,
    })
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
    unit: Option<&[UnitTest]>,
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
    if let Some(unit) = unit {
        for t in unit {
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
                    .any(|(_, c)| c.unit.as_ref().is_some_and(|u| u.runs(&t.path)))
            {
                continue;
            }
            diagnostics.push(format!(
                "allocation guard `{}` ({}) runs in no release step of {WORKFLOW}: select it in \
                 `{UNIT_STEP}` (`cargo test {} -- _alloc`)",
                t.path,
                t.file,
                UNIT_COMMAND.join(" ")
            ));
        }
    }
    Ok(diagnostics)
}

/// The steps of a workflow that have a `run:` command, in order.
pub fn steps(workflow: &str) -> anyhow::Result<Vec<Step>> {
    steps_in(workflow, WORKFLOW)
}

/// The steps of workflow `name`, whose text is `workflow`, as [`steps`].
fn steps_in(workflow: &str, name: &str) -> anyhow::Result<Vec<Step>> {
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
                "{name}: step `{}` has a folded `run: >`, which this check does not \
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
            if names_package(&tokens, UNIT_PACKAGE) {
                out.push(ReleaseTest {
                    unit: Some(unit_selection(step, command.trim(), &tokens)?),
                    ..ReleaseTest::default()
                });
            } else {
                out.extend(release_test(&tokens));
            }
        }
    }
    Ok(out)
}

/// Whether a command's arguments name package `package`.
fn names_package(tokens: &[&str], package: &str) -> bool {
    tokens
        .windows(2)
        .any(|w| (w[0] == "-p" || w[0] == "--package") && w[1] == package)
        || tokens.iter().any(|t| {
            [
                format!("-p{package}"),
                format!("-p={package}"),
                format!("--package={package}"),
            ]
            .contains(&(*t).to_owned())
        })
}

/// What the unit guards' release command `command` selects: its arguments
/// must be [`UNIT_COMMAND`], then, after `--`, substring filters with only
/// `--exact` and `--skip`; any other form is an error.
fn unit_selection(step: &str, command: &str, tokens: &[&str]) -> anyhow::Result<Selection> {
    let refuse = || {
        anyhow::anyhow!(
            "{WORKFLOW}: step `{step}` runs `{command}`, which names `{UNIT_PACKAGE}` but is not \
             its allocation guards' release command: use `cargo test {} -- <filters>`, with \
             only `--exact` and `--skip <pattern>` besides the filters",
            UNIT_COMMAND.join(" ")
        )
    };
    let start = tokens
        .windows(2)
        .position(|w| w == ["cargo", "test"])
        .ok_or_else(refuse)?
        + 2;
    let args = tokens.get(start..).unwrap_or_default();
    let (head, tail) = match args.iter().position(|&a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[][..]),
    };
    if head != UNIT_COMMAND {
        return Err(refuse());
    }
    let mut selection = Selection::default();
    let mut tail = tail.iter().copied();
    while let Some(arg) = tail.next() {
        match arg {
            "--exact" => selection.exact = true,
            "--skip" => selection
                .skips
                .push(tail.next().ok_or_else(refuse)?.to_owned()),
            _ if arg.starts_with('-') => return Err(refuse()),
            _ => selection.filters.push(arg.to_owned()),
        }
    }
    Ok(selection)
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
                if args.any(|a| a == "--list") {
                    return None;
                }
                break;
            }
            "--release" => release = true,
            "--no-run" => return None,
            _ => {
                if let Some(p) = value("--package", Some("-p")) {
                    command.packages.push(p);
                } else if let Some(t) = value("--test", None) {
                    command.tests.push(t);
                }
            }
        }
    }
    release.then_some(command)
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
                unit: None,
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
        extra_step("--test alloc", lines)
    }

    /// The fixture workflow with `alloc_tests` in [`ALLOC_STEP`] and a step
    /// `Extra` whose block command is `lines`.
    fn extra_step(alloc_tests: &str, lines: &[&str]) -> String {
        let mut text = workflow(alloc_tests);
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

    /// The crate of `files`, rooted at `src/main.rs`.
    fn walk(files: &[(&str, &str)]) -> anyhow::Result<Vec<UnitTest>> {
        let read = |path: &str| -> anyhow::Result<Option<String>> {
            Ok(files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, t)| (*t).to_owned()))
        };
        unit_tests("src/main.rs", &read)
    }

    fn found(files: &[(&str, &str)]) -> Vec<(String, String, bool)> {
        walk(files)
            .unwrap()
            .into_iter()
            .map(|t| (t.path, t.file, t.counts))
            .collect()
    }

    /// A gating fixture: what it gates, the files past the root, and the
    /// error expected.
    type Row<'a> = (&'a str, Vec<(&'a str, String)>, String);

    const GUARD_FN: &str = "#[test]\nfn a_alloc() {}\n";

    /// Modules are walked from the root through their files, inline ones
    /// included, with each test's path; a test counts by the counter's
    /// identifier in its body, not a string, and a helper is not a test;
    /// `cfg(test)` and the feature restrict nothing, anywhere.
    #[test]
    fn unit_tests_are_found_through_the_module_tree() {
        let m = "\
#![cfg(feature = \"dev-tools\")]
fn helper() { let _ = crate::alloc_count::this_thread(); }
#[test]
fn top() { let _ = \"alloc_count\"; }
#[cfg(test)]
mod tests {
    #[cfg(feature = \"dev-tools\")]
    #[test]
    fn counted_alloc() {
        use crate::alloc_count::this_thread;
        assert_eq!(this_thread(), 0);
    }
    mod inner;
}
";
        let found = found(&[
            (
                "src/main.rs",
                "#[cfg(test)]\nmod m;\nmod d;\nmod unused {}\n",
            ),
            ("src/m.rs", m),
            ("src/m/tests/inner.rs", GUARD_FN),
            ("src/d/mod.rs", "#[cfg(feature = \"dev-tools\")]\nmod e;\n"),
            ("src/d/e.rs", GUARD_FN),
            ("src/orphan.rs", "#[cfg(unix)]\n#[test]\nfn b_alloc() {}\n"),
        ]);
        let expected = [
            ("m::top", "src/m.rs", false),
            ("m::tests::counted_alloc", "src/m.rs", true),
            ("m::tests::inner::a_alloc", "src/m/tests/inner.rs", false),
            ("d::e::a_alloc", "src/d/e.rs", false),
        ]
        .map(|(p, f, c)| (p.to_owned(), f.to_owned(), c));
        assert_eq!(found, expected);
    }

    /// One fixture per place an attribute can gate a guard, with a `cfg`
    /// and with a `cfg_attr`, crossing a file boundary where the place
    /// allows it; then the other refusals. Each row is the crate's files
    /// past the root and the error expected. Every mismatch is listed.
    #[test]
    fn every_place_that_can_gate_a_guard_is_checked() {
        let child = "mod c;\n";
        let off = "cfg_attr(not(debug_assertions), cfg(any()))";
        let rows: Vec<Row> = vec![
            (
                "the test function's cfg",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#[cfg(unix)]\n{GUARD_FN}")),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "the test function's cfg_attr",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#[{off}]\n{GUARD_FN}")),
                ],
                "is under `cfg_attr(".into(),
            ),
            (
                "an inline module's cfg, in its file",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    (
                        "src/m.rs",
                        format!("#[cfg(unix)]\nmod t {{\n{GUARD_FN}}}\n"),
                    ),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "an inline module's cfg, across a file",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    (
                        "src/m.rs",
                        "#[cfg(debug_assertions)]\nmod tests { mod c; }\n".into(),
                    ),
                    ("src/m/tests/c.rs", GUARD_FN.into()),
                ],
                "is under `cfg(debug_assertions)`".into(),
            ),
            (
                "an inline module's cfg_attr, across a file",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#[{off}]\nmod tests {{ mod c; }}\n")),
                    ("src/m/tests/c.rs", GUARD_FN.into()),
                ],
                "is under `cfg_attr(".into(),
            ),
            (
                "a mod line's cfg",
                vec![
                    ("src/main.rs", "#[cfg(unix)]\nmod m;".into()),
                    ("src/m.rs", GUARD_FN.into()),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "a mod line's cfg_attr",
                vec![
                    ("src/main.rs", format!("#[{off}]\nmod m;")),
                    ("src/m.rs", GUARD_FN.into()),
                ],
                "has a `#[cfg_attr]` on its `mod` line".into(),
            ),
            (
                "a mod line's cfg inside an inline module",
                vec![
                    ("src/main.rs", "mod t {\n#[cfg(unix)]\nmod m;\n}\n".into()),
                    ("src/t/m.rs", GUARD_FN.into()),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "a #[path] mod line's cfg",
                vec![
                    (
                        "src/main.rs",
                        "#[cfg(unix)]\n#[path = \"../shared/g.rs\"]\nmod s;".into(),
                    ),
                    ("shared/g.rs", GUARD_FN.into()),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "a #[path] mod line's cfg_attr",
                vec![
                    (
                        "src/main.rs",
                        format!("#[{off}]\n#[path = \"../shared/g.rs\"]\nmod s;"),
                    ),
                    ("shared/g.rs", GUARD_FN.into()),
                ],
                "has a `#[cfg_attr]` on its `mod` line".into(),
            ),
            (
                "a mod line's cfg_attr choosing another file",
                vec![
                    (
                        "src/main.rs",
                        "#[cfg_attr(test, path = \"support_tests.rs\")]\nmod support;".into(),
                    ),
                    ("src/support.rs", String::new()),
                    ("src/support_tests.rs", format!("#[cfg(unix)]\n{GUARD_FN}")),
                ],
                "has a `#[cfg_attr]` on its `mod` line".into(),
            ),
            (
                "a file's inner cfg, across a file",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#![cfg(unix)]\n{child}")),
                    ("src/m/c.rs", GUARD_FN.into()),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "a file's inner cfg_attr, across a file",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#![{off}]\n{child}")),
                    ("src/m/c.rs", GUARD_FN.into()),
                ],
                "is under `cfg_attr(".into(),
            ),
            (
                "the root's inner cfg",
                vec![
                    ("src/main.rs", "#![cfg(unix)]\nmod m;".into()),
                    ("src/m.rs", GUARD_FN.into()),
                ],
                "is under `cfg(unix)`".into(),
            ),
            (
                "an ignored guard",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", format!("#[ignore]\n{GUARD_FN}")),
                ],
                "has `#[ignore]`".into(),
            ),
            (
                "a #[path] that is not a string",
                vec![(
                    "src/main.rs",
                    "#[path = concat!(\"m\", \".rs\")]\nmod m;".into(),
                )],
                "has a `#[path]`, which this check does not follow".into(),
            ),
            (
                "a #[path] inside an inline module",
                vec![(
                    "src/main.rs",
                    "mod t {\n#[path = \"../shared/g.rs\"]\nmod s;\n}\n".into(),
                )],
                "has a `#[path]`, which this check does not follow".into(),
            ),
            (
                "a module declared in a file reached by #[path]",
                vec![
                    ("src/main.rs", "#[path = \"../shared/n.rs\"]\nmod n;".into()),
                    ("shared/n.rs", "mod deeper;\n".into()),
                ],
                "is declared in a file reached by `#[path]`".into(),
            ),
            (
                "include!",
                vec![
                    ("src/main.rs", "mod m;".into()),
                    ("src/m.rs", "include!(\"t.rs\");".into()),
                ],
                "uses `include!`".into(),
            ),
            (
                "a module with no file",
                vec![("src/main.rs", "mod m;".into())],
                "has no single file".into(),
            ),
        ];
        let mut wrong = Vec::new();
        for (row, files, expected) in &rows {
            let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
            match walk(&files) {
                Err(e) if e.to_string().contains(expected.as_str()) => {}
                other => wrong.push(format!("{row}: {:?}", other.map(|t| t.len()))),
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        // A restricted module with no file is not an error: nothing of it
        // is built.
        assert!(
            walk(&[("src/main.rs", "#[cfg(unix)]\nmod m;")])
                .unwrap()
                .is_empty()
        );
    }

    fn unit(tests: &[(&str, bool)]) -> Vec<UnitTest> {
        tests
            .iter()
            .map(|&(path, counts)| UnitTest {
                path: path.to_owned(),
                file: "crates/player/src/input_side.rs".to_owned(),
                counts,
                ignored: false,
                cfgs: vec![],
            })
            .collect()
    }

    const GUARD: &str = "input_side::gate_tests::abort_close_alloc";
    const UNIT_RUN: &str =
        "cargo test --release --locked -p player --features dev-tools --bin tapestry";

    fn unselected() -> String {
        format!(
            "allocation guard `{GUARD}` (crates/player/src/input_side.rs) runs in no release \
             step of {WORKFLOW}: select it in `{UNIT_STEP}` (`{UNIT_RUN} -- _alloc`)"
        )
    }

    /// The fixture workflow, every integration guard in its step, and a
    /// step `Extra` whose block command is `lines`.
    fn with_unit_step(lines: &[&str]) -> String {
        extra_step("--test alloc --test tempo_alloc", lines)
    }

    #[test]
    fn a_unit_guard_selected_by_the_release_command_passes() {
        let unit = unit(&[(GUARD, true), ("input_side::tests::plain", false)]);
        for tail in [
            " -- _alloc",
            " -- other close_alloc --skip menu_",
            " -- --exact input_side::gate_tests::abort_close_alloc",
            "",
        ] {
            let command = format!("{UNIT_RUN}{tail}");
            let diags = check(&guards(), Some(&unit), &with_unit_step(&[&command])).unwrap();
            assert_eq!(diags, Vec::<String>::new(), "{command}");
        }
    }

    #[test]
    fn a_unit_guard_its_command_does_not_select_fails() {
        let unit = unit(&[(GUARD, true)]);
        for command in [
            // Not a release command naming the package.
            "cargo test --locked -p player --features dev-tools --bin tapestry -- _alloc",
            "cargo test --release --workspace -- _alloc",
            // Filtered out, skipped, or not the whole path with `--exact`.
            &format!("{UNIT_RUN} -- menu_"),
            &format!("{UNIT_RUN} -- _alloc --skip gate_tests"),
            &format!("{UNIT_RUN} -- --exact _alloc"),
        ] {
            let diags = check(&guards(), Some(&unit), &with_unit_step(&[command])).unwrap();
            assert_eq!(diags, [unselected()], "{command}");
        }
    }

    /// A release command naming the package in any form but the canonical
    /// one is refused: a test name before `--`, `--ignored`,
    /// `--include-ignored`, `--bench`, `--list`, a `--skip` with no
    /// pattern, other feature spellings, other packages, and `--no-run`.
    #[test]
    fn a_unit_command_of_another_form_is_refused() {
        let unit = unit(&[(GUARD, true)]);
        for command in [
            &format!("{UNIT_RUN} abort_close_alloc -- _alloc"),
            &format!("{UNIT_RUN} -- _alloc --ignored"),
            &format!("{UNIT_RUN} -- _alloc --include-ignored"),
            &format!("{UNIT_RUN} -- --bench _alloc"),
            &format!("{UNIT_RUN} -- _alloc --skip"),
            "cargo test --release --locked -p player -F player/dev-tools --bin tapestry -- _alloc",
            "cargo test --release --locked -p player --features dev-tools -- _alloc",
            "cargo test --release --locked -p player -p game --features dev-tools --bin tapestry",
            &format!("{UNIT_RUN} --no-run"),
            &format!("{UNIT_RUN} -- --list"),
        ] {
            let e = check(&guards(), Some(&unit), &with_unit_step(&[command])).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!(
                    "{WORKFLOW}: step `Extra` runs `{command}`, which names `player` but is not \
                     its allocation guards' release command: use `cargo test --release --locked \
                     -p player --features dev-tools --bin tapestry -- <filters>`, with only \
                     `--exact` and `--skip <pattern>` besides the filters"
                ),
                "{command}"
            );
        }
    }

    const PEAKS_RUN: &str = "cargo nextest run --release --locked -p bms-parser --test memory \
                             --run-ignored only -- --exact measure_load_peaks";

    fn found_test(path: &str, ignored: bool, cfgs: &[&str]) -> UnitTest {
        UnitTest {
            path: path.to_owned(),
            file: "crates/bms-parser/tests/memory.rs".to_owned(),
            counts: false,
            ignored,
            cfgs: cfgs.iter().map(|&c| c.to_owned()).collect(),
        }
    }

    /// `bms-parser`'s `memory` test target: the ignored measurement, two
    /// ordinary tests, one behind its `dev-tools` feature and one behind a
    /// `cfg` this check cannot evaluate; `player`'s binary the unit guard;
    /// nothing else exists.
    fn tests_of(package: &str, target: &Target) -> anyhow::Result<Option<Vec<UnitTest>>> {
        Ok(match (package, target) {
            ("bms-parser", Target::Test(t)) if t == "memory" => Some(vec![
                found_test("measure_load_peaks", true, &[]),
                found_test("rows::a_row_is_counted", false, &["test"]),
                found_test("the_timeline_is_held_at_its_counted_size", false, &[]),
                found_test("dev::a_dev_only_case", false, &["feature=\"dev-tools\""]),
                found_test("unix::a_unix_case", false, &["unix"]),
            ]),
            ("player", Target::Bin(b)) if b == "tapestry" => Some(vec![found_test(
                GUARD,
                false,
                &["test", "feature=\"dev-tools\""],
            )]),
            _ => None,
        })
    }

    /// `bms-parser` has `dev-tools`, enabled by `extra`; `player` has
    /// `dev-tools`.
    fn features_of(package: &str) -> Option<Features> {
        match package {
            "bms-parser" => Some(
                [
                    ("default", vec![]),
                    ("dev-tools", vec![]),
                    ("extra", vec!["dev-tools".to_owned(), "dep:x".to_owned()]),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            ),
            "player" => Some([("dev-tools".to_owned(), vec![])].into_iter().collect()),
            _ => None,
        }
    }

    /// A workflow of one step, `Extra`, whose block command is `lines`.
    fn only_step(lines: &[&str]) -> String {
        let mut text =
            String::from("jobs:\n  j:\n    steps:\n      - name: Extra\n        run: |\n");
        for line in lines {
            text.push_str("          ");
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    fn selections_of(lines: &[&str]) -> anyhow::Result<Vec<ByName>> {
        selections(&only_step(lines), WORKFLOW)
    }

    fn selection_diagnostics(lines: &[&str]) -> anyhow::Result<Vec<String>> {
        check_selections(&selections_of(lines)?, &tests_of, &features_of)
    }

    #[test]
    fn a_by_name_command_is_read_with_its_package_targets_and_filters() {
        let selected = selections_of(&[
            PEAKS_RUN,
            "cargo test -p bms-parser --test memory --features player/dev-tools,bms-parser/extra \
             -F dev-tools --no-default-features -- --include-ignored --skip rows::",
        ])
        .unwrap();
        assert_eq!(
            selected,
            [
                ByName {
                    workflow: WORKFLOW.into(),
                    step: "Extra".into(),
                    command: PEAKS_RUN.into(),
                    package: "bms-parser".into(),
                    targets: vec![Target::Test("memory".into())],
                    selection: Selection {
                        filters: vec!["measure_load_peaks".into()],
                        skips: vec![],
                        exact: true,
                    },
                    ignored: Ignored::Only,
                    features: vec![],
                    all_features: false,
                    no_default_features: false,
                },
                ByName {
                    workflow: WORKFLOW.into(),
                    step: "Extra".into(),
                    command: "cargo test -p bms-parser --test memory --features \
                              player/dev-tools,bms-parser/extra -F dev-tools \
                              --no-default-features -- --include-ignored --skip rows::"
                        .into(),
                    package: "bms-parser".into(),
                    targets: vec![Target::Test("memory".into())],
                    selection: Selection {
                        filters: vec![],
                        skips: vec!["rows::".into()],
                        exact: false,
                    },
                    ignored: Ignored::All,
                    features: vec!["extra".into(), "dev-tools".into()],
                    all_features: false,
                    no_default_features: true,
                },
            ]
        );
    }

    #[test]
    fn a_selection_of_a_test_the_command_runs_passes() {
        for command in [
            PEAKS_RUN,
            "cargo test --release --locked -p bms-parser --test memory -- --ignored --exact \
             measure_load_peaks",
            "cargo test -p bms-parser --test memory -- --include-ignored measure_load",
            // Ignored tests only, by no name.
            "cargo test -p bms-parser --test memory -- --ignored",
            "cargo test -p bms-parser --test=memory -- row --skip other",
            "cargo nextest run -p bms-parser --test memory a_row_is",
            "cargo test -p bms-parser --test memory --features dev-tools -- a_dev_only",
            "cargo test -p bms-parser --test memory --features extra -- a_dev_only",
            "cargo test -p bms-parser --test memory --all-features -- a_dev_only",
            // Exclusions that leave one test the command runs.
            "cargo test -p bms-parser --test memory -- --skip the_ --skip dev:: --skip unix::",
            &format!("{UNIT_RUN} -- _alloc"),
        ] {
            assert_eq!(
                selection_diagnostics(&[command]).unwrap(),
                Vec::<String>::new(),
                "{command}"
            );
        }
    }

    #[test]
    fn a_filter_that_selects_no_test_fails() {
        let renamed = PEAKS_RUN.replace("measure_load_peaks", "measure_load_peak");
        let rows = [
            // Renamed: with `--exact` a prefix of the name is not the name.
            (renamed.as_str(), "`measure_load_peak` (--exact)"),
            // Every match skipped.
            (
                "cargo test -p bms-parser --test memory -- row --skip rows::",
                "`row`",
            ),
            // A filter before `--`, nextest's form.
            (
                "cargo nextest run -p bms-parser --test memory measure_peaks",
                "`measure_peaks`",
            ),
            // Exclusions only, which leave no test.
            (
                "cargo test -p bms-parser --test memory -- --skip the_ --skip a_ --skip measure_",
                "every test outside its `--skip` patterns",
            ),
        ];
        for (command, label) in rows {
            let diags = selection_diagnostics(&[command]).unwrap();
            assert_eq!(
                diags,
                [format!(
                    "step `Extra` of {WORKFLOW} selects {label} in `-p bms-parser --test \
                     memory`, which matches no test there, so the step would run nothing: \
                     `{command}`"
                )],
                "{command}"
            );
        }
    }

    /// A name that exists but that the command does not run: ignored or
    /// not against the command's choice, or behind a feature it leaves off.
    #[test]
    fn a_selection_of_a_test_the_command_does_not_run_fails() {
        let rows = [
            (
                "cargo test -p bms-parser --test memory -- --ignored --exact \
                 the_timeline_is_held_at_its_counted_size",
                "`the_timeline_is_held_at_its_counted_size` (--exact)",
                "`the_timeline_is_held_at_its_counted_size` (not ignored)",
            ),
            (
                "cargo nextest run -p bms-parser --test memory -- --exact measure_load_peaks",
                "`measure_load_peaks` (--exact)",
                "`measure_load_peaks` (ignored)",
            ),
            (
                "cargo test -p bms-parser --test memory -- a_dev_only",
                "`a_dev_only`",
                "`dev::a_dev_only_case` (needs feature `dev-tools`)",
            ),
            (
                "cargo test -p bms-parser --test memory --no-default-features --features \
                 player/dev-tools -- a_dev_only",
                "`a_dev_only`",
                "`dev::a_dev_only_case` (needs feature `dev-tools`)",
            ),
            (
                "cargo test -p bms-parser --test memory -- --skip the_ --skip a_",
                "every test outside its `--skip` patterns",
                "`measure_load_peaks` (ignored)",
            ),
        ];
        for (command, label, why) in rows {
            let diags = selection_diagnostics(&[command]).unwrap();
            assert_eq!(
                diags,
                [format!(
                    "step `Extra` of {WORKFLOW} selects {label} in `-p bms-parser --test \
                     memory`, but the command runs none of the tests it matches, so the step \
                     would run nothing: {why}: `{command}`"
                )],
                "{command}"
            );
        }
    }

    #[test]
    fn a_selection_under_a_cfg_this_check_cannot_read_is_rejected() {
        let command = "cargo test -p bms-parser --test memory -- a_unix_case";
        assert_eq!(
            selection_diagnostics(&[command]).unwrap(),
            [format!(
                "step `Extra` of {WORKFLOW} selects `a_unix_case` in `-p bms-parser --test \
                 memory`, and this check cannot tell whether the command runs \
                 `unix::a_unix_case` (under `unix`): rejected rather than assumed to run: \
                 `{command}`"
            )]
        );
    }

    #[test]
    fn a_selection_from_a_target_that_does_not_exist_fails() {
        let command = PEAKS_RUN.replace("--test memory", "--test memroy");
        let diags = selection_diagnostics(&[&command]).unwrap();
        assert_eq!(
            diags.first().map(String::as_str),
            Some(
                format!(
                    "step `Extra` of {WORKFLOW} selects tests of `-p bms-parser --test memroy`, \
                     which does not exist"
                )
                .as_str()
            )
        );
    }

    #[test]
    fn commands_that_select_nothing_by_name_are_not_read() {
        let selected = selections_of(&[
            "cargo nextest run $DEV_TOOLS --partition \"slice:$PART/$PARTS\" --no-fail-fast",
            "cargo nextest run --locked -p skin-beatoraja --features dev-tools --test lua_runtime \
             --bin lua_replay",
            "cargo test --release -p bms-parser --test memory --no-run -- measure_load_peaks",
            "cargo test -p bms-parser --test memory -- measure_load_peaks --list",
            "cargo nextest list -p bms-parser --test memory measure_load_peaks",
            "cargo bench -p eval --bench evaluate -- --test",
            "cargo nextest run -p bms-parser --test memory --run-ignored default",
        ])
        .unwrap();
        assert_eq!(selected, []);
    }

    #[test]
    fn a_by_name_command_of_another_form_is_refused() {
        let rows = [
            (
                "cargo test --workspace -- measure_load_peaks",
                "name exactly one package with `-p`",
            ),
            (
                "cargo test -p bms-parser -p eval --test memory -- measure_load_peaks",
                "name exactly one package with `-p`",
            ),
            (
                "cargo test -p bms-parser -- measure_load_peaks",
                "name its targets (`--test`, `--bin` or `--lib`)",
            ),
            (
                "cargo test -p bms-parser --test memory -- $NAME",
                "this check reads only plain commands, without shell syntax",
            ),
            (
                "cargo test -p bms-parser --test memory -- \"measure_load_peaks\"",
                "this check reads only plain commands, without shell syntax",
            ),
            (
                "cargo nextest run -p bms-parser --test memory -E test(=measure_load_peaks)",
                "write its filters after `--`, not as a filterset",
            ),
            (
                "cargo nextest run -p bms-parser --test memory --run-ignored some",
                "this check does not read `--run-ignored`",
            ),
            (
                "cargo test -p bms-parser --test memory -- --format=json measure_load_peaks",
                "this check does not read `--format=json`",
            ),
            (
                "cargo test -p bms-parser --test memory -- measure_load_peaks --skip",
                "its `--skip` has no pattern",
            ),
        ];
        for (command, why) in rows {
            let e = selections_of(&[command]).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!("{WORKFLOW}: step `Extra` runs `{command}`, which selects tests; {why}"),
                "{command}"
            );
        }
    }

    /// Every workflow file is read under its own name.
    #[test]
    fn a_selection_names_its_workflow() {
        let text = only_step(&["cargo test -p bms-parser --test memory -- measure_peaks"]);
        let selected = selections(&text, ".github/workflows/bench.yml").unwrap();
        let diags = check_selections(&selected, &tests_of, &features_of).unwrap();
        assert!(
            diags.first().is_some_and(|d| d.starts_with(
                "step `Extra` of .github/workflows/bench.yml selects `measure_peaks`"
            )),
            "{diags:?}"
        );
    }

    #[test]
    fn a_unit_test_that_counts_under_another_name_fails() {
        let unit = unit(&[("input_side::gate_tests::a_handoff_keeps_the_sources", true)]);
        let command = format!("{UNIT_RUN} -- handoff");
        let diags = check(&guards(), Some(&unit), &with_unit_step(&[&command])).unwrap();
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
