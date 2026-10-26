//! `cargo xtask check-lints`: the lints that enforce the design rules stay
//! configured and still fire.
//!
//! - Fixture run: `cargo clippy -p eval -p xtask --keep-going` with
//!   `--cfg tapestry_lint_fixtures`, which compiles one fixture file per
//!   lint ([`LINTS`]) inside the real crates, under their real
//!   configuration. Each fixture must produce error-level diagnostics of
//!   exactly its own lint (`dead_code` aside), so removing a lint from its
//!   configuration, or lowering it to a warning, fails the check. The cfg
//!   is set only here, so no build ever contains the fixtures.
//! - Site check (text, so it also covers the Windows-only files): every
//!   workspace member inherits `[workspace.lints]`; the cast and
//!   disallowed-API lines stand at the crate roots in [`CRATE_ROOT_LINES`];
//!   every in-play site in [`IN_PLAY`] carries the attribute denying
//!   `unwrap`, `expect` and indexing; the test allowances of the two
//!   `clippy.toml` files match; `crates/eval/clippy.toml` lists every path
//!   in [`REQUIRED_DISALLOWED`]; and the fixture files are exactly
//!   [`LINTS`], with no attribute in them.
//!
//! The site check is partial, like R1's allowlist: it checks the listed
//! sites, and the in-play list is a reviewed table (design.md "Scope of No
//! Allocation").

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::diagnostic::DiagnosticLevel;
use cargo_metadata::{Message, MetadataCommand};

/// The cfg that compiles the fixtures.
const FIXTURE_CFG: &str = "tapestry_lint_fixtures";
/// The packages that hold the fixtures.
const FIXTURE_PACKAGES: &[&str] = &["eval", "xtask"];

/// Every lint and the directory of its fixture, `<dir>/<lint>.rs`, relative
/// to the workspace root.
pub const LINTS: &[(&str, &str)] = &[
    ("undocumented_unsafe_blocks", WORKSPACE_FIXTURES),
    ("dbg_macro", WORKSPACE_FIXTURES),
    ("todo", WORKSPACE_FIXTURES),
    ("unimplemented", WORKSPACE_FIXTURES),
    ("cast_possible_truncation", CRATE_FIXTURES),
    ("cast_possible_wrap", CRATE_FIXTURES),
    ("cast_sign_loss", CRATE_FIXTURES),
    ("disallowed_methods", CRATE_FIXTURES),
    ("disallowed_types", CRATE_FIXTURES),
    ("disallowed_macros", CRATE_FIXTURES),
    ("unwrap_used", IN_PLAY_FIXTURES),
    ("expect_used", IN_PLAY_FIXTURES),
    ("indexing_slicing", IN_PLAY_FIXTURES),
];
/// Fixtures of the `[workspace.lints]` entries, compiled in `xtask` (the
/// other crates forbid `unsafe` code).
const WORKSPACE_FIXTURES: &str = "xtask/lint-fixtures";
/// Fixtures of `eval`'s crate-root lints and `clippy.toml`.
const CRATE_FIXTURES: &str = "crates/eval/lint-fixtures/crate";
/// Fixtures of the in-play lints, compiled inside `eval`'s evaluator module.
const IN_PLAY_FIXTURES: &str = "crates/eval/lint-fixtures/in_play";

/// The cast lints at the root of each crate that takes them.
const CAST_LINE: &str = "#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]";
/// The disallowed-API lists in `clippy.toml` only name paths; this makes
/// them errors.
const DISALLOWED_LINE: &str =
    "#![deny(clippy::disallowed_macros, clippy::disallowed_methods, clippy::disallowed_types)]";
/// Crate roots and the inner attributes each must hold.
pub const CRATE_ROOT_LINES: &[(&str, &[&str])] = &[
    ("crates/eval/src/lib.rs", &[CAST_LINE, DISALLOWED_LINE]),
    ("crates/rules/src/lib.rs", &[CAST_LINE]),
    ("crates/bms-parser/src/lib.rs", &[CAST_LINE]),
];

/// The attribute of an in-play module file, before its first item.
pub const MODULE_ATTR: &str =
    "#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]";
/// The attribute of an in-play function in a file that is not in play as a
/// whole.
pub const FN_ATTR: &str =
    "#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]";

/// An in-play site: a whole module file, or functions of a file, each
/// named `Type::name` inside an `impl Type` or `trait Type` block and
/// `name` outside one. Every definition of a named function outside the
/// file's test modules must carry [`FN_ATTR`].
#[derive(Clone, Copy, Debug)]
pub enum Site {
    File(&'static str),
    Fns(&'static str, &'static [&'static str]),
}

/// The code the in-play threads run (design.md "Scope of No Allocation"):
/// the input thread's loop and device arrival, the logic step, the audio
/// callback, and per-frame rendering, with what they call in other crates.
/// The pull request that changes this table lists the call paths.
pub const IN_PLAY: &[Site] = &[
    // The logic step's evaluator.
    Site::File("crates/eval/src/evaluator.rs"),
    Site::File("crates/eval/src/convert.rs"),
    Site::File("crates/eval/src/gauge.rs"),
    Site::File("crates/eval/src/input.rs"),
    Site::File("crates/eval/src/merge.rs"),
    Site::Fns(
        "crates/rules/src/preset.rs",
        &[
            "PerJudgment::get",
            "JudgeWindows::table",
            "LowDamage::applies_at",
            "LowDamage::reduce",
            "MineRule::change",
        ],
    ),
    Site::Fns(
        "crates/bms-model/src/lib.rs",
        &["TimeUs::checked_add", "TimeUs::checked_sub"],
    ),
    Site::Fns(
        "crates/bms-model/src/chart.rs",
        &["Lane::new", "Lane::index", "Lane::all", "Note::end"],
    ),
    Site::Fns(
        "crates/bms-model/src/scroll.rs",
        &[
            "ScrollChange::at",
            "Segment::position",
            "Segment::earliest_at_or_past",
            "ScrollMap::initial_bpm_micro",
            "ScrollMap::segment_at",
            "ScrollMap::position_at",
            "ScrollMap::time_range",
            "ScrollMap::earliest_at_or_past",
            "ceil_div",
            "saturate",
        ],
    ),
    // The input thread.
    Site::File("crates/input/src/capture.rs"),
    Site::File("crates/input/src/gate.rs"),
    Site::File("crates/input/src/hid.rs"),
    Site::File("crates/input/src/queue.rs"),
    Site::File("crates/input/src/raw.rs"),
    Site::File("crates/input/src/sources.rs"),
    Site::File("crates/input/src/time.rs"),
    Site::File("crates/input/src/win.rs"),
    Site::File("crates/player/src/input_side.rs"),
    // The logic step and its sinks.
    Site::File("crates/game/src/logic.rs"),
    Site::File("crates/game/src/render.rs"),
    Site::File("crates/game/src/sound.rs"),
    Site::Fns(
        "crates/game/src/live.rs",
        &["play_and_next", "Rig::stepped"],
    ),
    Site::Fns(
        "crates/game/src/session.rs",
        &["Loaded::ends_play", "Command::cause"],
    ),
    Site::Fns(
        "crates/game/src/policy.rs",
        &["DefaultPolicy::on_survival_failure"],
    ),
    Site::Fns(
        "crates/game/src/tuning.rs",
        &[
            "Adjust::is_input_offset",
            "Tuning::adjust",
            "Tuning::volume",
            "Tuning::visible_time",
        ],
    ),
    Site::Fns(
        "crates/player/src/play.rs",
        &[
            "PlayerRig::sync",
            "PlayerRig::send_end",
            "PlayerRig::now",
            "PlayerRig::wait",
            "PlayerRig::take",
            "PlayerRig::quitting",
            "PlayerRig::measured",
            "PlayerRig::focus_lost",
            "PlayerRig::connected",
            "PlayerRig::audio_failure",
            "PlayerRig::screen_failed",
            "PlayerRig::cancel_audio",
            "PlayerRig::seal_audio",
            "PlayerRig::audio_done",
        ],
    ),
    Site::Fns(
        "crates/player/src/controls.rs",
        &[
            "Controls::tuning",
            "Controls::take",
            "Controls::retry_at",
            "Controls::quitting",
            "Controls::focused",
        ],
    ),
    Site::File("crates/player/src/tuning.rs"),
    // The audio thread.
    Site::File("crates/game/src/audio_loop.rs"),
    Site::File("crates/audio/src/clock.rs"),
    Site::File("crates/audio/src/engine.rs"),
    Site::File("crates/audio/src/mixer.rs"),
    Site::File("crates/audio/src/run.rs"),
    Site::File("crates/audio/src/wasapi.rs"),
    Site::Fns(
        "crates/audio/src/bank.rs",
        &[
            "Sound::frame",
            "Sound::frames",
            "Sound::channels",
            "SoundBank::slot",
            "SoundBank::stored",
        ],
    ),
    Site::File("crates/player/src/audio_side.rs"),
    // The render thread's frame.
    Site::File("crates/render/src/measure.rs"),
    Site::File("crates/render/src/renderer.rs"),
    Site::File("crates/render/src/simple.rs"),
    Site::File("crates/render/src/skin.rs"),
    Site::File("crates/render/src/sprite.rs"),
    Site::Fns(
        "crates/render/src/atlas.rs",
        &["glyph_uv", "solid_uv", "texels", "cell_of"],
    ),
    Site::Fns("crates/render/src/gpu.rs", &["Gpu::device", "Gpu::queue"]),
    Site::File("crates/player/src/measure.rs"),
    Site::File("crates/player/src/render_side.rs"),
];

/// The workspace `clippy.toml` and `eval`'s, which must agree on
/// [`TEST_ALLOWANCES`]: clippy reads only the nearest file.
const ROOT_CLIPPY_TOML: &str = "clippy.toml";
const EVAL_CLIPPY_TOML: &str = "crates/eval/clippy.toml";
/// Test code may unwrap, expect and index.
const TEST_ALLOWANCES: &[&str] = &[
    "allow-unwrap-in-tests",
    "allow-expect-in-tests",
    "allow-indexing-slicing-in-tests",
];

/// Every path `crates/eval/clippy.toml` must list, per list.
pub const REQUIRED_DISALLOWED: &[(&str, &[&str])] = &[
    (
        "disallowed-methods",
        &[
            "std::time::Instant::now",
            "std::time::SystemTime::now",
            "std::fs::read",
            "std::fs::read_to_string",
            "std::fs::write",
            "std::fs::read_dir",
            "std::fs::metadata",
            "std::fs::create_dir",
            "std::fs::create_dir_all",
            "std::fs::remove_file",
            "std::fs::remove_dir",
            "std::fs::remove_dir_all",
            "std::fs::rename",
            "std::fs::copy",
            "std::fs::canonicalize",
            "std::fs::File::open",
            "std::fs::File::create",
            "std::env::var",
            "std::env::var_os",
            "std::env::vars",
            "std::env::vars_os",
            "std::env::args",
            "std::env::args_os",
            "std::env::current_dir",
            "std::env::current_exe",
            "std::env::temp_dir",
            "std::thread::spawn",
            "std::thread::scope",
            "std::thread::sleep",
            "std::thread::yield_now",
            "std::thread::park",
            "std::thread::park_timeout",
            "std::thread::current",
            "std::thread::available_parallelism",
            "std::io::stdin",
            "std::io::stdout",
            "std::io::stderr",
        ],
    ),
    (
        "disallowed-types",
        &[
            "std::time::Instant",
            "std::time::SystemTime",
            "std::fs::File",
            "std::fs::OpenOptions",
            "std::fs::ReadDir",
            "std::thread::Builder",
            "std::thread::JoinHandle",
            "std::collections::HashMap",
            "std::collections::HashSet",
            "std::hash::RandomState",
        ],
    ),
    (
        "disallowed-macros",
        &[
            "std::print",
            "std::println",
            "std::eprint",
            "std::eprintln",
            "std::dbg",
        ],
    ),
];

/// Runs the site check, then the fixture run. Returns one diagnostic per
/// violation; `Err` when the check could not run.
pub fn run(manifest: &Path) -> Result<Vec<String>, String> {
    let metadata = MetadataCommand::new()
        .cargo_path(crate::cargo())
        .manifest_path(manifest)
        .no_deps()
        .other_options(vec!["--locked".to_owned()])
        .exec()
        .map_err(|e| format!("cargo metadata failed: {e}"))?;
    let root = metadata.workspace_root.clone().into_std_path_buf();
    let members: Vec<PathBuf> = metadata
        .workspace_packages()
        .iter()
        .map(|p| p.manifest_path.clone().into_std_path_buf())
        .collect();
    let mut diagnostics = check_sites(&root, &members)?;
    let messages = fixture_run(manifest, &root)?;
    diagnostics.extend(classify(&messages));
    Ok(diagnostics)
}

/// One compiler diagnostic of the fixture run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diag {
    /// The primary span's file, relative to the workspace root, with `/`.
    pub file: Option<String>,
    pub code: Option<String>,
    pub error: bool,
    pub message: String,
}

/// Runs clippy on the fixture packages with the fixture cfg.
fn fixture_run(manifest: &Path, root: &Path) -> Result<Vec<Diag>, String> {
    let mut cmd = Command::new(crate::cargo());
    cmd.arg("clippy").arg("--manifest-path").arg(manifest);
    for p in FIXTURE_PACKAGES {
        cmd.args(["-p", p]);
    }
    // Every fixture package is compiled even though each fails.
    cmd.args(["--locked", "--keep-going", "--message-format=json", "--"])
        .args(["--cfg", FIXTURE_CFG]);
    let (messages, stderr) = json_messages(&mut cmd)?;
    let mut diags = Vec::new();
    let mut finished = false;
    for message in messages {
        match message {
            Message::CompilerMessage(m) => {
                let d = &m.message;
                let file = d
                    .spans
                    .iter()
                    .find(|s| s.is_primary)
                    .map(|s| workspace_relative(root, &s.file_name));
                diags.push(Diag {
                    file,
                    code: d.code.as_ref().map(|c| c.code.clone()),
                    error: matches!(d.level, DiagnosticLevel::Error | DiagnosticLevel::Ice),
                    message: d.message.clone(),
                });
            }
            Message::BuildFinished(_) => finished = true,
            _ => {}
        }
    }
    if !finished {
        return Err(format!(
            "cargo clippy did not finish a build:\n{}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    Ok(diags)
}

/// Runs `cmd` to its end and parses its standard output as cargo's JSON
/// messages; also returns its standard error. `Command::output` drains both
/// pipes at once, so a child that fills one while the other is read does
/// not block.
fn json_messages(cmd: &mut Command) -> Result<(Vec<Message>, Vec<u8>), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run cargo clippy: {e}"))?;
    let messages = Message::parse_stream(output.stdout.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("reading cargo clippy output: {e}"))?;
    Ok((messages, output.stderr))
}

/// Checks the fixture run's diagnostics: each fixture in [`LINTS`] gives
/// errors of its own lint and nothing else, and nothing else is reported.
pub fn classify(diags: &[Diag]) -> Vec<String> {
    let fixture_of = |file: &str| {
        LINTS
            .iter()
            .find(|(lint, dir)| file == format!("{dir}/{lint}.rs"))
            .map(|&(lint, _)| lint)
    };
    let mut out = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for d in diags {
        let Some(file) = &d.file else {
            if is_summary(d) {
                continue;
            }
            out.push(format!("fixture run: unexpected diagnostic: {}", d.message));
            continue;
        };
        let Some(lint) = fixture_of(file) else {
            out.push(format!(
                "fixture run: unexpected diagnostic in {file}: {} ({})",
                d.message,
                d.code.as_deref().unwrap_or("no code"),
            ));
            continue;
        };
        if d.code.as_deref() == Some("dead_code") {
            continue;
        }
        let expected = format!("clippy::{lint}");
        if d.code.as_deref() == Some(expected.as_str()) {
            seen.insert(lint);
            if !d.error {
                out.push(format!(
                    "fixture {file}: `{expected}` is reported as a warning, not an error"
                ));
            }
        } else {
            out.push(format!(
                "fixture {file}: unexpected {} `{}`: {}",
                if d.error { "error" } else { "warning" },
                d.code.as_deref().unwrap_or("no code"),
                d.message,
            ));
        }
    }
    for &(lint, dir) in LINTS {
        if !seen.contains(lint) {
            out.push(format!(
                "fixture {dir}/{lint}.rs: no `clippy::{lint}` diagnostic; is the lint still configured?"
            ));
        }
    }
    out
}

/// `path` with `/` separators and its `.` and `..` components resolved
/// lexically (a module loaded through `#[path]` is reported as
/// `src/../lint-fixtures/...`).
fn lexical(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "." => {}
            ".." if parts.last().is_some_and(|p| !p.is_empty() && *p != "..") => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

/// A diagnostic's `file`, relative to the workspace `root` with `/`
/// separators. Cargo reports workspace files relative to the root, but a
/// path may also come absolute: POSIX, drive letter, UNC, or verbatim
/// (`\\?\`). Such a path below `root` loses the root's prefix; any other
/// path stays absolute (and so matches no fixture).
fn workspace_relative(root: &Path, file: &str) -> String {
    /// Drops the verbatim prefix, so `\\?\C:\x` reads `C:/x` and
    /// `\\?\UNC\srv\x` reads `//srv/x`.
    fn plain(path: String) -> String {
        if let Some(rest) = path.strip_prefix("//?/UNC/") {
            format!("//{rest}")
        } else if let Some(rest) = path.strip_prefix("//?/") {
            rest.to_owned()
        } else {
            path
        }
    }
    let file = plain(lexical(file));
    let root = plain(lexical(&root.to_string_lossy()));
    let root = root.trim_end_matches('/');
    // Windows paths compare without case; POSIX ones exactly.
    let windows = root.starts_with("//") || root.get(1..2) == Some(":");
    let head = file.get(..root.len());
    let below = head.is_some_and(|h| {
        if windows {
            h.eq_ignore_ascii_case(root)
        } else {
            h == root
        }
    });
    match file.get(root.len()..) {
        Some(rest) if below && !root.is_empty() && rest.starts_with('/') => {
            rest.trim_start_matches('/').to_owned()
        }
        _ => file,
    }
}

/// The compiler's closing summary ("aborting due to ...", "... warnings
/// emitted"), which has no span and no code.
fn is_summary(d: &Diag) -> bool {
    d.code.is_none()
        && (d.message.starts_with("aborting due to")
            || d.message.ends_with("warning emitted")
            || d.message.ends_with("warnings emitted"))
}

/// The text checks of the module docs.
pub fn check_sites(root: &Path, members: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for manifest in members {
        let table = read_toml(manifest)?;
        let inherits = table
            .get("lints")
            .and_then(|l| l.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true);
        if !inherits {
            out.push(format!(
                "{}: `[lints] workspace = true` is missing",
                relative(root, manifest)
            ));
        }
    }
    for &(file, lines) in CRATE_ROOT_LINES {
        let text = read(&root.join(file))?;
        let attrs = inner_attributes(&text);
        for line in lines {
            if !attrs.contains(&normalize(line)) {
                out.push(format!("{file}: crate root lacks `{line}`"));
            }
        }
    }
    for site in IN_PLAY {
        out.extend(check_site(root, *site)?);
    }
    out.extend(check_clippy_toml(root)?);
    out.extend(check_fixture_files(root)?);
    Ok(out)
}

/// One [`IN_PLAY`] entry.
fn check_site(root: &Path, site: Site) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    match site {
        Site::File(file) => {
            let text = read(&root.join(file))?;
            if !inner_attributes(&text).contains(&normalize(MODULE_ATTR)) {
                out.push(format!(
                    "{file}: in-play module lacks `{MODULE_ATTR}` before its first item"
                ));
            }
        }
        Site::Fns(file, names) => {
            let text = read(&root.join(file))?;
            let fns = fn_attributes(&text);
            let fn_attr = normalize(FN_ATTR);
            for name in names {
                let defs: Vec<_> = fns.iter().filter(|(n, _)| n == name).collect();
                if defs.is_empty() {
                    out.push(format!("{file}: in-play function `{name}` not found"));
                }
                for (_, attrs) in defs {
                    if !attrs.contains(&fn_attr) {
                        out.push(format!(
                            "{file}: in-play function `{name}` lacks `{FN_ATTR}`"
                        ));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The two `clippy.toml` files: the same test allowances, all on, and
/// `eval`'s lists hold every [`REQUIRED_DISALLOWED`] path.
fn check_clippy_toml(root: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let workspace = read_toml(&root.join(ROOT_CLIPPY_TOML))?;
    let eval = read_toml(&root.join(EVAL_CLIPPY_TOML))?;
    for key in TEST_ALLOWANCES {
        for (file, table) in [(ROOT_CLIPPY_TOML, &workspace), (EVAL_CLIPPY_TOML, &eval)] {
            if table.get(*key).and_then(toml::Value::as_bool) != Some(true) {
                out.push(format!("{file}: `{key} = true` is missing"));
            }
        }
    }
    for &(list, paths) in REQUIRED_DISALLOWED {
        let listed: BTreeSet<&str> = eval
            .get(list)
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| match entry {
                toml::Value::String(s) => Some(s.as_str()),
                toml::Value::Table(t) => t.get("path").and_then(toml::Value::as_str),
                _ => None,
            })
            .collect();
        for path in paths {
            if !listed.contains(path) {
                out.push(format!("{EVAL_CLIPPY_TOML}: `{list}` lacks `{path}`"));
            }
        }
    }
    Ok(out)
}

/// The fixture directories hold exactly a `mod.rs` and one file per lint of
/// [`LINTS`], and no fixture file carries an attribute.
fn check_fixture_files(root: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let dirs: BTreeSet<&str> = LINTS.iter().map(|&(_, dir)| dir).collect();
    for dir in dirs {
        let expected: BTreeSet<String> = LINTS
            .iter()
            .filter(|&&(_, d)| d == dir)
            .map(|&(lint, _)| format!("{lint}.rs"))
            .chain(["mod.rs".to_owned()])
            .collect();
        let entries = fs::read_dir(root.join(dir)).map_err(|e| format!("reading {dir}: {e}"))?;
        let mut found = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {dir}: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let text = read(&entry.path())?;
            if let Some(attr) = any_attribute(&text) {
                out.push(format!(
                    "{dir}/{name}: fixtures carry no attribute, found `{attr}`"
                ));
            }
            found.insert(name);
        }
        for name in expected.difference(&found) {
            out.push(format!("{dir}/{name}: fixture missing"));
        }
        for name in found.difference(&expected) {
            out.push(format!("{dir}/{name}: not the fixture of a lint in LINTS"));
        }
    }
    Ok(out)
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn read_toml(path: &Path) -> Result<toml::Table, String> {
    read(path)?
        .parse()
        .map_err(|e| format!("parsing {}: {e}", path.display()))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// An attribute without whitespace and without a trailing comma before its
/// closing parenthesis, so rustfmt's wrapping does not matter.
fn normalize(attr: &str) -> String {
    let compact: String = attr.chars().filter(|c| !c.is_whitespace()).collect();
    compact.replace(",)", ")")
}

/// Whether `line` starts an outer or inner attribute.
fn is_attribute(line: &str) -> bool {
    line.starts_with("#[") || line.starts_with("#![")
}

/// Splits `text` into lines and joins each attribute (from `#[` or `#![`
/// at the start of a line to its closing bracket) into one entry. Other
/// lines stay as they are. Brackets inside string literals are skipped.
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if !is_attribute(trimmed) {
            out.push(line.to_owned());
            continue;
        }
        let mut attr = trimmed.to_owned();
        let mut depth = bracket_depth(trimmed);
        while depth > 0 {
            let Some(next) = lines.next() else { break };
            attr.push('\n');
            attr.push_str(next);
            depth += bracket_depth(next);
        }
        out.push(attr);
    }
    out
}

/// The change in bracket depth over `s`, outside string literals.
fn bracket_depth(s: &str) -> i32 {
    let mut depth = 0;
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// The inner attributes before the first item, normalized. Comments and
/// blank lines are skipped; anything else ends them.
fn inner_attributes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in logical_lines(text) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with("#![") {
            out.push(normalize(trimmed));
            continue;
        }
        break;
    }
    out
}

/// Each function definition outside `#[cfg(test)]` modules, named as
/// [`Site::Fns`] names it (`Type::name` inside an `impl` or `trait` block,
/// `name` elsewhere), with its outer attributes, normalized. A test
/// module's body is skipped and the items after it are read.
fn fn_attributes(text: &str) -> Vec<(String, Vec<String>)> {
    let masked = mask(text);
    let lines: Vec<&str> = text.lines().collect();
    let code: Vec<&str> = masked.lines().collect();
    let mut out = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    // Open `impl`/`trait` blocks: the type and the depth outside the block.
    let mut blocks: Vec<(String, i32)> = Vec::new();
    // A block header seen whose `{` has not opened yet.
    let mut header: Option<String> = None;
    // The depth a skipped test module's body returns to.
    let mut skip_to: Option<i32> = None;
    let mut depth = 0;
    let mut i = 0;
    while let (Some(&line), Some(&masked_line)) = (lines.get(i), code.get(i)) {
        i += 1;
        let before = depth;
        depth += brace_depth(masked_line);
        if let Some(target) = skip_to {
            if depth <= target {
                skip_to = None;
            }
            continue;
        }
        while blocks.last().is_some_and(|&(_, outside)| depth <= outside) {
            blocks.pop();
        }
        let trimmed = line.trim();
        if masked_line.trim().is_empty() {
            // A comment or a blank line.
            continue;
        }
        if is_attribute(trimmed) {
            // Join a wrapped attribute; its brackets are balanced in code.
            let mut attr = trimmed.to_owned();
            let mut open = bracket_depth(masked_line);
            while open > 0 {
                let (Some(&next), Some(&next_code)) = (lines.get(i), code.get(i)) else {
                    break;
                };
                i += 1;
                depth += brace_depth(next_code);
                attr.push_str(next);
                open += bracket_depth(next_code);
            }
            pending.push(normalize(&attr));
            continue;
        }
        let attrs = std::mem::take(&mut pending);
        if let Some(name) = block_type(trimmed) {
            header = Some(name);
        }
        // A `{` ends the header: its block opens here, or opens and closes.
        if masked_line.contains('{')
            && let Some(name) = header.take()
            && depth > before
        {
            blocks.push((name, before));
        }
        if trimmed.starts_with("mod ") && attrs.iter().any(|a| a == "#[cfg(test)]") {
            if depth > before {
                skip_to = Some(before);
            }
            continue;
        }
        if let Some(name) = fn_name(trimmed) {
            let name = match blocks.last() {
                Some((ty, _)) => format!("{ty}::{name}"),
                None => name.to_owned(),
            };
            out.push((name, attrs));
        }
    }
    out
}

/// `text` with the contents of comments, string literals and character
/// literals replaced by spaces (line breaks kept), so braces and brackets
/// in it are code. Raw strings are handled; a lifetime is not a literal.
fn mask(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        // The end (exclusive) of a comment or literal starting at `i`.
        let end = if c == '/' && next == Some('/') {
            (i..chars.len())
                .find(|&k| chars.get(k) == Some(&'\n'))
                .unwrap_or(chars.len())
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            let mut k = i;
            loop {
                match (chars.get(k), chars.get(k + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        k += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        k += 2;
                        if depth == 0 {
                            break k;
                        }
                    }
                    (Some(_), _) => k += 1,
                    (None, _) => break k,
                }
            }
        } else if c == '"' {
            let mut k = i + 1;
            loop {
                match chars.get(k) {
                    Some('\\') => k += 2,
                    Some('"') => break k + 1,
                    Some(_) => k += 1,
                    None => break k,
                }
            }
        } else if c == 'r'
            && (next == Some('"') || next == Some('#'))
            && !chars
                .get(i.wrapping_sub(1))
                .is_some_and(|p| p.is_alphanumeric() || *p == '_')
        {
            let hashes = chars
                .get(i + 1..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&h| h == '#')
                .count();
            if chars.get(i + 1 + hashes) == Some(&'"') {
                let mut k = i + 2 + hashes;
                loop {
                    if chars.get(k).is_none() {
                        break k;
                    }
                    if chars.get(k) == Some(&'"')
                        && (1..=hashes).all(|h| chars.get(k + h) == Some(&'#'))
                    {
                        break k + 1 + hashes;
                    }
                    k += 1;
                }
            } else {
                i + 1
            }
        } else if c == '\'' {
            // `'x'` or `'\n'`-style: a literal; anything else is a lifetime.
            match (next, chars.get(i + 2)) {
                // The escape takes at least `i + 2`, so `'\''` ends at `i + 3`.
                (Some('\\'), _) => (i + 3..chars.len())
                    .find(|&k| chars.get(k) == Some(&'\''))
                    .map_or(chars.len(), |k| k + 1),
                (Some(_), Some('\'')) => i + 3,
                _ => i + 1,
            }
        } else {
            i + 1
        };
        if end == i + 1 {
            out.push(c);
        } else {
            out.extend(
                chars
                    .get(i..end)
                    .unwrap_or_default()
                    .iter()
                    .map(|&c| blank(c)),
            );
        }
        i = end;
    }
    out
}

/// The change in brace depth over code `s` (masked by [`mask`]).
fn brace_depth(s: &str) -> i32 {
    s.chars()
        .map(|c| match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        })
        .sum()
}

/// The type an `impl` or `trait` header line names: `S` for `impl S`,
/// `impl<T> S<T>`, `impl Trait for S<'_>` and `unsafe impl Send for S`;
/// `T` for `pub trait T: Send`.
fn block_type(line: &str) -> Option<String> {
    let mut rest = line;
    for prefix in ["pub(crate) ", "pub(super) ", "pub ", "unsafe "] {
        if let Some(r) = rest.strip_prefix(prefix) {
            rest = r;
        }
    }
    let rest = if let Some(r) = rest.strip_prefix("trait ") {
        r
    } else {
        let r = rest.strip_prefix("impl")?;
        let r = if r.starts_with('<') {
            // Skip the generics, which may nest.
            let mut depth = 0;
            let end = r.char_indices().find_map(|(k, c)| {
                match c {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    _ => {}
                }
                (depth == 0).then_some(k + 1)
            })?;
            r.get(end..)?
        } else if r.starts_with(' ') {
            r
        } else {
            return None;
        };
        let r = r.trim_start();
        r.split_once(" for ").map_or(r, |(_, ty)| ty)
    };
    let path = rest
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .next()?;
    let name = path.trim_end_matches(':').rsplit("::").next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// The name of the function `line` defines, if it starts a definition.
fn fn_name(line: &str) -> Option<&str> {
    let mut rest = line;
    if let Some(r) = rest.strip_prefix("pub") {
        rest = r.trim_start();
        if rest.starts_with('(') {
            let close = rest.find(')')?;
            rest = rest.get(close + 1..)?.trim_start();
        }
    }
    for qualifier in [
        "const ",
        "async ",
        "unsafe ",
        "extern \"C\" ",
        "extern \"system\" ",
    ] {
        if let Some(r) = rest.strip_prefix(qualifier) {
            rest = r.trim_start();
        }
    }
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    let next = rest.get(end..)?;
    (next.starts_with('(') || next.starts_with('<')).then(|| rest.get(..end))?
}

/// The first line of `text` holding `#[` or `#![` anywhere, if any.
/// Fixtures need no attribute, and an attribute anywhere (in a nested
/// module, after code on a line, kept by `#[rustfmt::skip]`) could set a
/// lint level, so any one is rejected without parsing.
fn any_attribute(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.contains("#[") || line.contains("#!["))
        .map(|line| line.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::path::Path;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{
        Diag, FN_ATTR, LINTS, MODULE_ATTR, Site, any_attribute, check_site, classify,
        fn_attributes, inner_attributes, json_messages, normalize, workspace_relative,
    };

    fn diag(file: &str, code: &str, error: bool) -> Diag {
        Diag {
            file: Some(file.to_owned()),
            code: Some(code.to_owned()),
            error,
            message: "m".to_owned(),
        }
    }

    /// One error of its own lint per fixture, as a healthy run gives.
    fn healthy() -> Vec<Diag> {
        LINTS
            .iter()
            .map(|(lint, dir)| {
                diag(
                    &format!("{dir}/{lint}.rs"),
                    &format!("clippy::{lint}"),
                    true,
                )
            })
            .collect()
    }

    #[test]
    fn a_healthy_run_passes_with_dead_code_and_summaries_ignored() {
        let mut diags = healthy();
        diags.push(diag(
            "crates/eval/lint-fixtures/crate/cast_sign_loss.rs",
            "dead_code",
            false,
        ));
        diags.push(Diag {
            file: None,
            code: None,
            error: true,
            message: "aborting due to 9 previous errors; 3 warnings emitted".to_owned(),
        });
        assert_eq!(classify(&diags), Vec::<String>::new());
    }

    #[test]
    fn paths_through_a_module_path_attribute_resolve() {
        assert_eq!(
            super::lexical("crates/eval/src/../lint-fixtures/crate/cast_sign_loss.rs"),
            "crates/eval/lint-fixtures/crate/cast_sign_loss.rs"
        );
        assert_eq!(
            super::lexical("xtask\\src\\..\\lint-fixtures\\todo.rs"),
            "xtask/lint-fixtures/todo.rs"
        );
    }

    #[test]
    fn a_missing_lint_is_reported_by_fixture() {
        let mut diags = healthy();
        diags.retain(|d| d.code.as_deref() != Some("clippy::todo"));
        let out = classify(&diags);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("xtask/lint-fixtures/todo.rs"), "{out:?}");
    }

    #[test]
    fn a_warning_instead_of_an_error_fails() {
        let mut diags = healthy();
        for d in &mut diags {
            if d.code.as_deref() == Some("clippy::disallowed_types") {
                d.error = false;
            }
        }
        let out = classify(&diags);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("warning, not an error"), "{out:?}");
    }

    #[test]
    fn an_extra_lint_in_a_fixture_fails() {
        let mut diags = healthy();
        diags.push(diag(
            "crates/eval/lint-fixtures/in_play/unwrap_used.rs",
            "clippy::expect_used",
            true,
        ));
        let out = classify(&diags);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("clippy::expect_used"), "{out:?}");
    }

    #[test]
    fn uncoded_errors_and_diagnostics_outside_fixtures_fail() {
        let mut diags = healthy();
        diags.push(Diag {
            file: Some("crates/eval/lint-fixtures/crate/mod.rs".to_owned()),
            code: None,
            error: true,
            message: "file not found for module".to_owned(),
        });
        diags.push(Diag {
            file: None,
            code: None,
            error: true,
            message: "something broke".to_owned(),
        });
        assert_eq!(classify(&diags).len(), 2);
    }

    #[test]
    fn module_attribute_is_found_only_before_the_first_item() {
        let ok = format!("//! Docs.\n\n{MODULE_ATTR}\n\nuse std::fmt;\n");
        assert!(inner_attributes(&ok).contains(&normalize(MODULE_ATTR)));
        let wrapped = "//! Docs.\n#![deny(\n    clippy::expect_used,\n    clippy::indexing_slicing,\n    clippy::unwrap_used\n)]\nuse std::fmt;\n";
        assert!(inner_attributes(wrapped).contains(&normalize(MODULE_ATTR)));
        let late = format!("//! Docs.\nuse std::fmt;\n{MODULE_ATTR}\n");
        assert!(!inner_attributes(&late).contains(&normalize(MODULE_ATTR)));
        let commented = format!("//! Docs.\n// {MODULE_ATTR}\nuse std::fmt;\n");
        assert!(!inner_attributes(&commented).contains(&normalize(MODULE_ATTR)));
        let cfg_attr = "#![cfg_attr(any(), deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used))]\nuse std::fmt;\n";
        assert!(!inner_attributes(cfg_attr).contains(&normalize(MODULE_ATTR)));
    }

    #[test]
    fn site_check_reads_files_and_functions() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| fs::write(dir.path().join(name), text).unwrap();
        file("ok.rs", &format!("{MODULE_ATTR}\nfn f() {{}}\n"));
        file("missing.rs", "fn f() {}\n");
        file(
            "fns.rs",
            &format!(
                "/// Doc.\n#[must_use]\n{FN_ATTR}\npub(crate) const fn hot() -> u8 {{ 0 }}\n\
                 impl S {{\n    {FN_ATTR}\n    fn twice(&self) {{}}\n}}\n\
                 impl<T: Copy> fmt::Debug for U<T>\nwhere\n    T: Send,\n{{\n    fn twice(&self) {{}}\n}}\n\
                 fn cold() {{}}\n\
                 #[cfg(test)]\nmod tests {{\n    fn hot() {{ let _ = \"}}\"; let _ = '}}'; }}\n}}\n\
                 impl Late {{\n    // }}\n    fn after(&self) {{}}\n}}\n"
            ),
        );
        let check = |site| check_site(dir.path(), site).unwrap();
        assert!(check(Site::File("ok.rs")).is_empty());
        assert_eq!(check(Site::File("missing.rs")).len(), 1);
        assert!(check(Site::Fns("fns.rs", &["hot"])).is_empty());
        // Each name binds to its own block: one carries it, one does not,
        // and neither is a free function.
        assert!(check(Site::Fns("fns.rs", &["S::twice"])).is_empty());
        assert_eq!(check(Site::Fns("fns.rs", &["U::twice"])).len(), 1);
        assert_eq!(check(Site::Fns("fns.rs", &["twice"])).len(), 1);
        assert_eq!(check(Site::Fns("fns.rs", &["cold"])).len(), 1);
        assert_eq!(check(Site::Fns("fns.rs", &["absent"])).len(), 1);
        // A production item after the test module is read, and lacks it.
        let late = check(Site::Fns("fns.rs", &["Late::after"]));
        assert_eq!(late.len(), 1, "{late:?}");
        assert!(late[0].contains("lacks"), "{late:?}");
    }

    #[test]
    fn test_modules_are_skipped_and_scanning_resumes() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {\n        \
                    let _ = r#\"}\"#;\n        /* } */\n    }\n}\n\
                    pub trait R: Send {\n    fn c(&self) {}\n}\n\
                    #[cfg(test)]\nmod more;\nunsafe impl Send for V {}\nfn d<'a>(x: &'a u8) {}\n\
                    impl W {\n    const C: [(char, char); 2] = [('\\'','{'), ('\\\\','}')];\n    \
                    fn e() {}\n}\nfn f() {}\n";
        let fns = fn_attributes(text);
        let names: Vec<_> = fns.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "R::c", "d", "W::e", "f"]);
    }

    #[test]
    fn wrapped_function_attributes_are_joined() {
        let text = "impl S {\n    #[deny(\n        clippy::expect_used,\n        clippy::indexing_slicing,\n        clippy::unwrap_used,\n    )]\n    #[expect(\n        clippy::indexing_slicing,\n        reason = \"[x]\"\n    )]\n    fn f(&self) {}\n}\n";
        let fns = fn_attributes(text);
        assert_eq!(fns.len(), 1);
        let (name, attrs) = &fns[0];
        assert_eq!(name, "S::f");
        assert!(attrs.contains(&normalize(FN_ATTR)), "{attrs:?}");
    }

    #[test]
    fn fixture_files_reject_any_attribute() {
        assert_eq!(any_attribute("fn f() {}\n"), None);
        assert_eq!(any_attribute("//! Docs that allow(x).\nmod a;\n"), None);
        assert!(any_attribute("#[allow(dead_code)]\nfn f() {}\n").is_some());
        assert!(any_attribute("#![expect(clippy::todo)]\n").is_some());
        assert!(any_attribute("#[inline]\nfn f() {}\n").is_some());
        // Kept on one line by `#[rustfmt::skip]`: a lint level in a nested
        // module, not at the start of a line.
        let nested =
            "#[rustfmt::skip]\nmod inner { #[deny(clippy::todo)] fn f() -> u8 { todo!() } }\n";
        assert!(any_attribute(nested).is_some());
        let inline = "mod inner { #[deny(clippy::todo)] fn f() -> u8 { todo!() } }\n";
        assert_eq!(any_attribute(inline).as_deref(), Some(inline.trim()));
    }

    #[test]
    fn absolute_diagnostic_paths_lose_the_workspace_root() {
        let rel = "xtask/lint-fixtures/todo.rs";
        let cases = [
            ("/home/u/ws", "/home/u/ws/xtask/lint-fixtures/todo.rs"),
            (
                "/home/u/ws/",
                "/home/u/ws/xtask/src/../lint-fixtures/todo.rs",
            ),
            ("C:\\ws", "C:\\ws\\xtask\\lint-fixtures\\todo.rs"),
            ("C:\\ws", "c:/ws/xtask/lint-fixtures/todo.rs"),
            ("\\\\?\\C:\\ws", "C:\\ws\\xtask\\lint-fixtures\\todo.rs"),
            ("C:\\ws", "\\\\?\\C:\\ws\\xtask\\lint-fixtures\\todo.rs"),
            (
                "\\\\srv\\share\\ws",
                "\\\\srv\\share\\ws\\xtask\\lint-fixtures\\todo.rs",
            ),
            (
                "\\\\srv\\share\\ws",
                "\\\\?\\UNC\\srv\\share\\ws\\xtask\\lint-fixtures\\todo.rs",
            ),
            ("/home/u/ws", rel),
        ];
        for (root, file) in cases {
            assert_eq!(
                workspace_relative(Path::new(root), file),
                rel,
                "{root} {file}"
            );
        }
        // Outside the root, or sharing only a prefix of its last name.
        for file in [
            "/home/u/wsx/a.rs",
            "/HOME/u/ws/a.rs",
            "/rustc/abc/library/core/src/lib.rs",
        ] {
            assert_eq!(workspace_relative(Path::new("/home/u/ws"), file), file);
        }
    }

    /// Set by [`a_child_flooding_stderr_does_not_block`] for the child.
    const FLOOD: &str = "XTASK_LINTS_FLOOD";

    /// The child of [`a_child_flooding_stderr_does_not_block`]: more than
    /// any pipe buffer on stderr, then a message on stdout. Does nothing in
    /// a normal test run.
    #[test]
    fn flood_child() {
        if std::env::var_os(FLOOD).is_none() {
            return;
        }
        // Written to the handles, not through the test harness's capture.
        std::io::stderr().write_all(&vec![b'x'; 4 << 20]).unwrap();
        let mut out = std::io::stdout();
        out.write_all(b"{\"reason\":\"build-finished\",\"success\":false}\n")
            .unwrap();
        out.flush().unwrap();
    }

    #[test]
    fn a_child_flooding_stderr_does_not_block() {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", "lints::tests::flood_child", "--nocapture", "-q"])
                .env(FLOOD, "1");
            let _ = tx.send(json_messages(&mut cmd));
        });
        let (messages, stderr) = rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the child's pipes were not drained")
            .unwrap();
        assert!(stderr.len() > 1 << 20, "{}", stderr.len());
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, cargo_metadata::Message::BuildFinished(_))),
            "{messages:?}"
        );
    }
}
