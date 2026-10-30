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
//! - Site check (parsed source, so it also covers the Windows-only
//!   files, which are not compiled here): every
//!   workspace member inherits `[workspace.lints]`; the cast and
//!   disallowed-API lines stand at the crate roots in [`CRATE_ROOT_LINES`];
//!   every in-play site in [`IN_PLAY`] carries the attribute denying
//!   `unwrap`, `expect` and indexing, no attribute in its file lowers
//!   them to `allow` or `warn`, and its file names neither `anyhow` nor
//!   the `log` facade (both scans partial, see [`names_log`]); the test
//!   allowances of the two `clippy.toml` files match;
//!   `crates/eval/clippy.toml` lists every path in [`REQUIRED_DISALLOWED`];
//!   and the fixture files are exactly
//!   [`LINTS`], with no attribute (doc comments aside) and no macro
//!   definition in them.
//!
//! The site check is partial, like R1's allowlist: it checks the listed
//! sites, and the in-play list is a reviewed table (design.md "Scope of No
//! Allocation").

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr as _;

use anyhow::{Context as _, bail};
use cargo_metadata::diagnostic::DiagnosticLevel;
use cargo_metadata::{Message, MetadataCommand};
use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};
use quote::ToTokens as _;
use syn::ext::IdentExt as _;
use syn::punctuated::Punctuated;

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
    ("crates/skin-model/src/lib.rs", &[CAST_LINE]),
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
/// Each listed file, a [`Site::Fns`] file whole, names neither `anyhow`
/// nor the `log` facade: in-play code keeps typed errors and never logs.
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
            "LnEndWindows::table",
            "LnEndWindows::judgment_of",
            "LowDamage::applies_at",
            "MineRule::change",
        ],
    ),
    Site::Fns(
        "crates/rules/src/windows.rs",
        &[
            "LaneClass::of",
            "PerLaneClass::get",
            "PerLaneClass::of_lane",
        ],
    ),
    Site::Fns(
        "crates/bms-model/src/lib.rs",
        &["TimeUs::checked_add", "TimeUs::checked_sub"],
    ),
    Site::Fns(
        "crates/bms-model/src/chart.rs",
        &[
            "Lane::new",
            "Lane::index",
            "Lane::all",
            "Note::end",
            "Note::end_pos",
        ],
    ),
    Site::Fns(
        "crates/bms-model/src/scroll.rs",
        &[
            "Position::minus",
            "ScrollSegment::value",
            "ScrollMap::initial_rate",
            "ScrollMap::position_at",
        ],
    ),
    // The input thread.
    Site::File("crates/input/src/autoplay.rs"),
    Site::File("crates/input/src/cadence.rs"),
    Site::File("crates/input/src/capture.rs"),
    Site::File("crates/input/src/gate.rs"),
    Site::File("crates/input/src/hid.rs"),
    Site::File("crates/input/src/menu.rs"),
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
        &["play_and_next", "Rig::stepped", "Rig::input_failure"],
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
            "PlayerRig::input_failure",
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
            "Controls::wake_game",
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
    // A seek plan is made off the audio thread; the engine reads it there.
    Site::Fns(
        "crates/audio/src/seek.rs",
        &[
            "FrameLine::frame",
            "SeekPlan::time",
            "SeekPlan::line",
            "SeekPlan::engine",
            "SeekPlan::chart",
            "BgmSchedule::into_parts",
            "BgmSchedule::from_parts",
            "SeekPlan::at",
            "SeekPlan::split",
            "SeekPlan::budget",
            "SeekPlan::seq",
            "SeekPlan::slots",
        ],
    ),
    Site::File("crates/player/src/audio_side.rs"),
    // The render thread's frame.
    Site::File("crates/render/src/measure.rs"),
    Site::File("crates/render/src/renderer.rs"),
    Site::File("crates/render/src/simple.rs"),
    Site::File("crates/render/src/skin.rs"),
    Site::File("crates/render/src/sprite.rs"),
    // A skin's per-frame expressions and quad batch.
    Site::File("crates/skin-model/src/expr/arith.rs"),
    Site::File("crates/skin-model/src/expr/program.rs"),
    Site::File("crates/skin-model/src/quad.rs"),
    Site::Fns(
        "crates/render/src/atlas.rs",
        &["glyph_uv", "solid_uv", "texels", "cell_of"],
    ),
    Site::Fns("crates/render/src/gpu.rs", &["Gpu::device", "Gpu::queue"]),
    Site::File("crates/player/src/measure.rs"),
    Site::File("crates/player/src/render_side.rs"),
    // The play frame's calls through the screen the render thread keeps;
    // its menu preparation runs on menus only.
    Site::Fns(
        "crates/player/src/menu_screen.rs",
        &[
            "MenuScreen::wait",
            "MenuScreen::paced",
            "MenuScreen::build",
            "MenuScreen::draw",
            "MenuScreen::statistics",
            "MenuScreen::presentation",
        ],
    ),
    // The development builds' counting allocator, which every allocation
    // calls, and the render thread's app region marks.
    Site::File("crates/player/src/alloc_count.rs"),
    // A failed frame's report.
    Site::Fns("crates/game/src/error.rs", &["Chain::fmt"]),
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
pub fn run(manifest: &Path) -> anyhow::Result<Vec<String>> {
    let metadata = crate::metadata(
        MetadataCommand::new()
            .manifest_path(manifest)
            .no_deps()
            .other_options(vec!["--locked".to_owned()]),
    )?;
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
fn fixture_run(manifest: &Path, root: &Path) -> anyhow::Result<Vec<Diag>> {
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
        bail!(
            "cargo clippy did not finish a build:\n{}",
            String::from_utf8_lossy(&stderr)
        );
    }
    Ok(diags)
}

/// Runs `cmd` to its end and parses its standard output as cargo's JSON
/// messages; also returns its standard error. `Command::output` drains both
/// pipes at once, so a child that fills one while the other is read does
/// not block.
fn json_messages(cmd: &mut Command) -> anyhow::Result<(Vec<Message>, Vec<u8>)> {
    let output = cmd.output().context("failed to run cargo clippy")?;
    let messages = Message::parse_stream(output.stdout.as_slice())
        .collect::<Result<_, _>>()
        .context("reading cargo clippy output")?;
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

/// The parsed-source site checks of the module docs.
pub fn check_sites(root: &Path, members: &[PathBuf]) -> anyhow::Result<Vec<String>> {
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
        let attrs = inner_attributes(&parse_source(root, file)?);
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
fn check_site(root: &Path, site: Site) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let (Site::File(file) | Site::Fns(file, _)) = site;
    let (source, tokens) = parse_and_lex(root, file)?;
    if names_anyhow(tokens.clone()) {
        out.push(format!(
            "{file}: in-play code names `anyhow`; it keeps typed errors (design.md \"Errors\")"
        ));
    }
    if names_log(tokens.clone()) {
        out.push(format!(
            "{file}: in-play code names the `log` facade; only loading and lifecycle code logs (design.md \"Log File\")"
        ));
    }
    for attr in lowering_attributes(tokens) {
        out.push(format!(
            "{file}: `{attr}` lowers an in-play lint; state the invariant in an `#[expect]`"
        ));
    }
    match site {
        Site::File(_) => {
            if !has_module_attr(&source) {
                out.push(format!(
                    "{file}: in-play module lacks `{MODULE_ATTR}` before its first item"
                ));
            }
        }
        Site::Fns(_, names) => {
            let whole = has_module_attr(&source);
            let fns = fn_attributes(&source);
            let fn_attr = normalize(FN_ATTR);
            for name in names {
                let defs: Vec<_> = fns.iter().filter(|(n, _)| n == name).collect();
                if defs.is_empty() {
                    out.push(format!("{file}: in-play function `{name}` not found"));
                }
                for (_, attrs) in defs {
                    if !whole && !attrs.contains(&fn_attr) {
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
fn check_clippy_toml(root: &Path) -> anyhow::Result<Vec<String>> {
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
/// [`LINTS`], and no fixture file holds what [`fixture_violation`] rejects.
fn check_fixture_files(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let dirs: BTreeSet<&str> = LINTS.iter().map(|&(_, dir)| dir).collect();
    for dir in dirs {
        let expected: BTreeSet<String> = LINTS
            .iter()
            .filter(|&&(_, d)| d == dir)
            .map(|&(lint, _)| format!("{lint}.rs"))
            .chain(["mod.rs".to_owned()])
            .collect();
        let entries = fs::read_dir(root.join(dir)).with_context(|| format!("reading {dir}"))?;
        let mut found = BTreeSet::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {dir}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let text = read(&entry.path())?;
            let violation =
                fixture_violation(&text).with_context(|| format!("parsing {dir}/{name}"))?;
            if let Some(violation) = violation {
                out.push(format!("{dir}/{name}: {violation}"));
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

fn read(path: &Path) -> anyhow::Result<String> {
    fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn read_toml(path: &Path) -> anyhow::Result<toml::Table> {
    read(path)?
        .parse()
        .with_context(|| format!("parsing {}", path.display()))
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

/// `attr` as source text, normalized.
fn attr_text(attr: &syn::Attribute) -> String {
    normalize(&attr.to_token_stream().to_string())
}

/// Parses the Rust source file `path`. A file that does not parse fails
/// the check: it cannot be shown to carry its attributes.
fn parse_source(root: &Path, path: &str) -> anyhow::Result<syn::File> {
    Ok(parse_and_lex(root, path)?.0)
}

/// [`parse_source`], and the file's tokens.
fn parse_and_lex(root: &Path, path: &str) -> anyhow::Result<(syn::File, TokenStream)> {
    let text = read(&root.join(path))?;
    let file = syn::parse_file(&text).with_context(|| format!("parsing {path}"))?;
    let tokens = lex(&text).with_context(|| format!("parsing {path}"))?;
    Ok((file, tokens))
}

/// The file's inner attributes (those before its first item), normalized.
fn inner_attributes(file: &syn::File) -> Vec<String> {
    file.attrs.iter().map(attr_text).collect()
}

/// Whether the file carries [`MODULE_ATTR`], which covers all its code.
fn has_module_attr(file: &syn::File) -> bool {
    inner_attributes(file).contains(&normalize(MODULE_ATTR))
}

/// Each function definition outside `#[cfg(test)]` modules, named as
/// [`Site::Fns`] names it (`Type::name` inside an `impl` or `trait` block,
/// `name` elsewhere), with its outer attributes, normalized. Functions in
/// nested modules are named the same way; functions inside function
/// bodies are not items of the file and are left out.
fn fn_attributes(file: &syn::File) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    collect_fns(&file.items, &mut out);
    out
}

fn collect_fns(items: &[syn::Item], out: &mut Vec<(String, Vec<String>)>) {
    let texts = |attrs: &[syn::Attribute]| attrs.iter().map(attr_text).collect::<Vec<_>>();
    for item in items {
        match item {
            syn::Item::Fn(f) => out.push((f.sig.ident.to_string(), texts(&f.attrs))),
            syn::Item::Impl(block) => {
                let ty = type_name(&block.self_ty);
                for item in &block.items {
                    if let syn::ImplItem::Fn(f) = item {
                        out.push((format!("{ty}::{}", f.sig.ident), texts(&f.attrs)));
                    }
                }
            }
            syn::Item::Trait(block) => {
                for item in &block.items {
                    if let syn::TraitItem::Fn(f) = item {
                        out.push((format!("{}::{}", block.ident, f.sig.ident), texts(&f.attrs)));
                    }
                }
            }
            syn::Item::Mod(m) if !is_test_module(&m.attrs) => {
                if let Some((_, items)) = &m.content {
                    collect_fns(items, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether a module's attributes include `#[cfg(test)]`.
fn is_test_module(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| attr_text(a) == "#[cfg(test)]")
}

/// The name an `impl` block's type gives its functions: the last path
/// segment (`S` for `S<T>`, `&S` or `crate::m::S`), or the whole type,
/// normalized, for any other type.
fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or_else(String::new, |s| s.ident.to_string()),
        syn::Type::Reference(r) => type_name(&r.elem),
        syn::Type::Paren(p) => type_name(&p.elem),
        syn::Type::Group(g) => type_name(&g.elem),
        other => normalize(&other.to_token_stream().to_string()),
    }
}

/// Lexes Rust source `text` into tokens. Comments are dropped, doc comments
/// become `#[doc = "..."]` attributes, and every literal is one token, so
/// nothing inside a string or comment is seen as code.
fn lex(text: &str) -> anyhow::Result<TokenStream> {
    // `LexError` is neither `Send` nor `Sync`, so `anyhow` cannot hold it as
    // a source; its text becomes the error.
    TokenStream::from_str(text).map_err(|e| anyhow::Error::msg(e.to_string()))
}

/// Calls `f` on every token tree of `tokens`, at any depth, with the trees
/// that follow it in its own group.
fn for_each_tree(tokens: TokenStream, f: &mut impl FnMut(&TokenTree, &[TokenTree])) {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    for (k, tree) in trees.iter().enumerate() {
        f(tree, trees.get(k + 1..).unwrap_or_default());
        if let TokenTree::Group(g) = tree {
            for_each_tree(g.stream(), f);
        }
    }
}

/// The attribute a `#` starts, given the trees after it: `!` for an inner
/// one, and its bracket group.
fn attribute_after(rest: &[TokenTree]) -> Option<(&'static str, &Group)> {
    match rest {
        [TokenTree::Punct(b), TokenTree::Group(g), ..]
            if b.as_char() == '!' && g.delimiter() == Delimiter::Bracket =>
        {
            Some(("!", g))
        }
        [TokenTree::Group(g), ..] if g.delimiter() == Delimiter::Bracket => Some(("", g)),
        _ => None,
    }
}

/// The attribute `#`, `bang` and `group` form, normalized.
fn attribute_text(bang: &str, group: &Group) -> String {
    normalize(&format!("#{bang}{group}"))
}

/// Whether an attribute's bracket group is a doc comment's, exactly
/// `doc = "..."`.
fn is_doc(group: &Group) -> bool {
    let body: Vec<TokenTree> = group.stream().into_iter().collect();
    matches!(
        body.as_slice(),
        [TokenTree::Ident(i), TokenTree::Punct(eq), TokenTree::Literal(l)]
            if *i == "doc" && eq.as_char() == '=' && l.to_string().starts_with('"')
    )
}

/// What a fixture may not hold, if `text` holds any: a `#` anywhere (an
/// attribute, or one being built from pieces) other than a doc comment, or
/// a `macro_rules!` definition, whose expansion could build one. `Err` when
/// `text` does not lex.
fn fixture_violation(text: &str) -> anyhow::Result<Option<String>> {
    let mut found = None;
    for_each_tree(lex(text)?, &mut |tree, rest| {
        if found.is_some() {
            return;
        }
        match tree {
            TokenTree::Punct(p) if p.as_char() == '#' => match attribute_after(rest) {
                Some((_, g)) if is_doc(g) => {}
                Some((bang, g)) => {
                    let attr = attribute_text(bang, g);
                    found = Some(format!("fixtures carry no attribute, found `{attr}`"));
                }
                None => found = Some("fixtures carry no attribute, found a `#`".to_owned()),
            },
            TokenTree::Ident(i)
                if *i == "macro_rules"
                    && matches!(rest.first(), Some(TokenTree::Punct(b)) if b.as_char() == '!') =>
            {
                found = Some("fixtures define no macro, found `macro_rules!`".to_owned());
            }
            _ => {}
        }
    });
    Ok(found)
}

/// The lints the in-play attributes deny, and the groups that hold them.
const IN_PLAY_LINTS: &[&str] = &[
    "clippy::expect_used",
    "clippy::indexing_slicing",
    "clippy::unwrap_used",
    "clippy::restriction",
    "clippy::all",
];

/// Every attribute in source `tokens`, at any depth (statements,
/// expressions and macro input included), that lowers an in-play lint to
/// `allow` or `warn`, directly or through `cfg_attr`, normalized. Such an
/// attribute would silently undo the file's or function's deny; an
/// in-play exception is an `#[expect]` instead, which states its reason
/// and fails when unfulfilled.
fn lowering_attributes(tokens: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    for_each_tree(tokens, &mut |tree, rest| {
        if let TokenTree::Punct(p) = tree
            && p.as_char() == '#'
            && let Some((bang, g)) = attribute_after(rest)
            && syn::parse2::<syn::Meta>(g.stream()).is_ok_and(|meta| lowers(&meta))
        {
            out.push(attribute_text(bang, g));
        }
    });
    out
}

/// Whether source `tokens` name `anyhow` anywhere, test modules included:
/// no in-play file needs it, so a [`Site::Fns`] file is scanned whole too.
/// Comments are dropped and each literal is one token, so only code counts.
fn names_anyhow(tokens: TokenStream) -> bool {
    let mut found = false;
    for_each_tree(tokens, &mut |tree, _| {
        if let TokenTree::Ident(i) = tree
            && i.unraw() == "anyhow"
        {
            found = true;
        }
    });
    found
}

/// The `log` facade's macros, which `#[macro_use]` can bring in unnamed.
const LOG_MACROS: &[&str] = &[
    "error",
    "warn",
    "info",
    "debug",
    "trace",
    "log",
    "log_enabled",
];

/// Whether source `tokens` name the `log` facade anywhere, test modules
/// included, as [`names_anyhow`] scans: the identifier `log` before `::`,
/// `use log` or `crate log` (`extern crate`), or a call of one of
/// [`LOG_MACROS`] (the name, `!`, then a group). Raw identifiers count as
/// their names.
///
/// The check is partial. It flags an unrelated macro of one of those names
/// and a local `log` module named with `::`. It misses an alias made by a
/// grouped or absolute import (`use {log as l};`, `use ::log as l;`) and
/// used only through the alias, an alias re-exported from another module, a
/// wrapper defined outside the file, and generated code.
fn names_log(tokens: TokenStream) -> bool {
    let mut found = false;
    for_each_tree(tokens, &mut |tree, rest| {
        let TokenTree::Ident(i) = tree else {
            return;
        };
        let name = i.unraw();
        let path = name == "log"
            && matches!(rest, [TokenTree::Punct(a), TokenTree::Punct(b), ..]
                if a.as_char() == ':' && a.spacing() == Spacing::Joint && b.as_char() == ':');
        let import = (name == "use" || name == "crate")
            && matches!(rest.first(), Some(TokenTree::Ident(n)) if n.unraw() == "log");
        let call = LOG_MACROS.iter().any(|m| name == m)
            && matches!(rest, [TokenTree::Punct(bang), TokenTree::Group(_), ..]
                if bang.as_char() == '!');
        if path || import || call {
            found = true;
        }
    });
    found
}

/// Whether `meta` is `allow(..)` or `warn(..)` naming an in-play lint, or a
/// `cfg_attr` holding one. Arguments that do not parse count as lowering.
fn lowers(meta: &syn::Meta) -> bool {
    let syn::Meta::List(list) = meta else {
        return false;
    };
    let args = || list.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated);
    if list.path.is_ident("allow") || list.path.is_ident("warn") {
        return args().map_or(true, |lints| {
            lints.iter().any(|lint| {
                matches!(lint, syn::Meta::Path(path)
                    if IN_PLAY_LINTS.contains(&normalize(&path.to_token_stream().to_string()).as_str()))
            })
        });
    }
    if list.path.is_ident("cfg_attr") {
        // The predicate, then the attributes it applies.
        return args().map_or(true, |metas| metas.iter().skip(1).any(lowers));
    }
    false
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
        Diag, FN_ATTR, IN_PLAY_LINTS, LINTS, MODULE_ATTR, Site, check_site, classify,
        fixture_violation, fn_attributes, has_module_attr, json_messages, lowering_attributes,
        normalize, workspace_relative,
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

    fn parsed(text: &str) -> syn::File {
        syn::parse_file(text).unwrap()
    }

    fn fn_names(text: &str) -> Vec<String> {
        fn_attributes(&parsed(text))
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    }

    #[test]
    fn module_attribute_is_found_only_before_the_first_item() {
        let has = |text: &str| has_module_attr(&parsed(text));
        assert!(has(&format!(
            "//! Docs.\n\n{MODULE_ATTR}\n\nuse std::fmt;\n"
        )));
        let wrapped = "//! Docs.\n#![deny(\n    clippy::expect_used,\n    clippy::indexing_slicing,\n    clippy::unwrap_used,\n)]\nuse std::fmt;\n";
        assert!(has(wrapped));
        assert!(!has(&format!(
            "//! Docs.\n// {MODULE_ATTR}\nuse std::fmt;\n"
        )));
        let cfg_attr = "#![cfg_attr(any(), deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used))]\nuse std::fmt;\n";
        assert!(!has(cfg_attr));
        // Not before the first item: the file does not parse, and fails
        // the check.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("late.rs"),
            format!("use std::fmt;\n{MODULE_ATTR}\n"),
        )
        .unwrap();
        assert!(check_site(dir.path(), Site::File("late.rs")).is_err());
    }

    #[test]
    fn site_check_reads_files_and_functions() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| fs::write(dir.path().join(name), text).unwrap();
        file("ok.rs", &format!("{MODULE_ATTR}\nfn f() {{}}\n"));
        file("missing.rs", "fn f() {}\n");
        file(
            "whole.rs",
            &format!("{MODULE_ATTR}\nimpl S {{\n    fn hot() {{}}\n}}\n"),
        );
        file("broken.rs", "fn f( {}\n");
        file(
            "fns.rs",
            &format!(
                "/// Doc.\n#[must_use]\n{FN_ATTR}\npub(crate) const fn hot() -> u8 {{ 0 }}\n\
                 impl S {{\n    {FN_ATTR}\n    fn twice(&self) {{}}\n}}\n\
                 impl<T: Copy> fmt::Debug for U<T>\nwhere\n    T: Send,\n{{\n    fn twice(&self) {{}}\n}}\n\
                 fn cold() {{}}\n\
                 #[cfg(test)]\nmod tests {{\n    fn hot() {{}}\n}}\n\
                 impl Late {{\n    fn after(&self) {{}}\n}}\n"
            ),
        );
        let check = |site| check_site(dir.path(), site).unwrap();
        assert!(check(Site::File("ok.rs")).is_empty());
        assert_eq!(check(Site::File("missing.rs")).len(), 1);
        assert!(check(Site::Fns("whole.rs", &["S::hot"])).is_empty());
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
        // A file that does not parse fails closed.
        assert!(check_site(dir.path(), Site::Fns("broken.rs", &["f"])).is_err());
    }

    #[test]
    fn site_errors_keep_their_cause_as_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("broken.rs"), "fn f( {}\n").unwrap();
        let e = check_site(dir.path(), Site::File("broken.rs")).unwrap_err();
        let syn = e.root_cause().downcast_ref::<syn::Error>();
        assert!(syn.is_some(), "{e:?}");
        assert_eq!(
            format!("{e:#}"),
            format!("parsing broken.rs: {}", syn.unwrap())
        );

        let e = check_site(dir.path(), Site::File("absent.rs")).unwrap_err();
        let io = e.root_cause().downcast_ref::<std::io::Error>();
        assert_eq!(
            io.map(std::io::Error::kind),
            Some(std::io::ErrorKind::NotFound),
            "{e:?}"
        );
        let path = dir.path().join("absent.rs");
        assert_eq!(
            format!("{e:#}"),
            format!("reading {}: {}", path.display(), io.unwrap())
        );
    }

    #[test]
    fn test_modules_are_skipped_and_scanning_resumes() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n\
                    pub trait R: Send {\n    fn c(&self) {}\n    fn decl(&self);\n}\n\
                    #[cfg(test)]\nmod more;\nunsafe impl Send for V {}\nfn d<'a>(x: &'a u8) {}\n\
                    mod nested {\n    impl W {\n        fn e() {}\n    }\n}\nfn f() {}\n";
        assert_eq!(fn_names(text), ["a", "R::c", "R::decl", "d", "W::e", "f"]);
    }

    #[test]
    fn wrapped_function_attributes_are_joined() {
        let text = "impl S {\n    #[deny(\n        clippy::expect_used,\n        clippy::indexing_slicing,\n        clippy::unwrap_used,\n    )]\n    #[expect(\n        clippy::indexing_slicing,\n        reason = \"[x]\"\n    )]\n    fn f(&self) {}\n}\n";
        let fns = fn_attributes(&parsed(text));
        assert_eq!(fns.len(), 1);
        let (name, attrs) = &fns[0];
        assert_eq!(name, "S::f");
        assert!(attrs.contains(&normalize(FN_ATTR)), "{attrs:?}");
    }

    /// Writes `text` as `x.rs` and checks the in-play functions `names`.
    fn check_fns(text: &str, names: &'static [&'static str]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.rs"), text).unwrap();
        check_site(dir.path(), Site::Fns("x.rs", names)).unwrap()
    }

    #[test]
    fn a_raw_byte_string_hides_no_definition() {
        // An annotated copy inside the literal is not a definition, and the
        // real one after it is read. The literal holds a `"`, which ends an
        // ordinary string.
        let text = format!(
            "const FAKE: &[u8] = br#\"\n\"\nimpl S {{\n    {FN_ATTR}\n    fn hot() {{}}\n}}\n\"#;\n\
             const FAKE_C: &core::ffi::CStr = cr#\"\nimpl S {{ fn hot() {{}} }}\n\"#;\n\
             impl S {{\n    fn hot() {{}}\n}}\n"
        );
        let out = check_fns(&text, &["S::hot"]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("`S::hot` lacks"), "{out:?}");
    }

    #[test]
    fn an_arrow_in_impl_generics_keeps_the_type() {
        // `F: Fn() -> u8` does not end the generics: both blocks are `S`'s,
        // and the unprotected one fails.
        let text = format!(
            "impl<F: Fn() -> u8> S<F> {{\n    fn hot() {{}}\n}}\n\
             impl S<fn() -> u8> {{\n    {FN_ATTR}\n    fn hot() {{}}\n}}\n"
        );
        let out = check_fns(&text, &["S::hot"]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("`S::hot` lacks"), "{out:?}");
    }

    #[test]
    fn a_test_module_of_any_visibility_is_skipped() {
        let text = format!(
            "{FN_ATTR}\nfn hot() {{}}\n\
             #[cfg(test)]\npub(crate) mod tests {{\n    fn hot() {{}}\n}}\n\
             #[cfg(test)]\npub mod more_tests {{\n    fn hot() {{}}\n}}\n\
             fn cold() {{}}\n"
        );
        // The test copies of `hot` do not count; `cold` after them lacks it.
        assert!(check_fns(&text, &["hot"]).is_empty());
        let out = check_fns(&text, &["cold"]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("`cold` lacks"), "{out:?}");
        // `cfg(not(test))` is production code.
        let not_test = "#[cfg(not(test))]\nmod prod {\n    fn cold() {}\n}\n";
        assert_eq!(check_fns(not_test, &["cold"]).len(), 1);
    }

    #[test]
    fn fixture_files_reject_attributes_and_macro_definitions() {
        let found = |text: &str| fixture_violation(text).unwrap();
        let attr = |found: &str| format!("fixtures carry no attribute, found `{found}`");
        assert_eq!(found("fn f() {}\n"), None);
        assert_eq!(
            found(
                "//! Docs that allow(x).\n/// Doc.\nfn f() {}\nconst S: &str = \"#[deny(x)]\";\n"
            ),
            None
        );
        assert!(found("#[allow(dead_code)]\nfn f() {}\n").is_some());
        assert!(found("#![expect(clippy::todo)]\n").is_some());
        assert!(found("#[inline]\nfn f() {}\n").is_some());
        assert!(found("#[doc(hidden)]\nfn f() {}\n").is_some());
        // Kept on one line by `#[rustfmt::skip]`: a lint level in a nested
        // module.
        let nested =
            "#[rustfmt::skip]\nmod inner { #[deny(clippy::todo)] fn f() -> u8 { todo!() } }\n";
        assert_eq!(found(nested), Some(attr("#[rustfmt::skip]")));
        let inline = "mod inner { #[deny(clippy::todo)] fn f() -> u8 { todo!() } }\n";
        assert_eq!(found(inline), Some(attr("#[deny(clippy::todo)]")));
        // Attribute tokens split by whitespace and comments.
        let split = "fn g() -> u8 {\n    0\n}\n# /* c */ [deny(clippy::todo)]\nfn f() -> u8 {\n    todo!()\n}\n";
        assert_eq!(found(split), Some(attr("#[deny(clippy::todo)]")));
        // On a statement, an expression and a field, and in macro input.
        assert!(found("fn f() { #[deny(clippy::todo)] let x = 1; }\n").is_some());
        assert!(found("fn f() -> u8 { (#[allow(unused)] 1) }\n").is_some());
        assert!(found("struct S { #[allow(dead_code)] x: u8 }\n").is_some());
        assert!(found("m! { #![deny(clippy::todo)] }\n").is_some());
        // An attribute on a foreign item, which the token walk finds like any other.
        let foreign = "unsafe extern \"C\" {\n    #[deny(clippy::todo)]\n    safe fn f();\n}\n";
        assert_eq!(found(foreign), Some(attr("#[deny(clippy::todo)]")));
        // A `#` handed to a macro that builds the attribute at expansion.
        let built = "macro_rules! m {\n    ($h:tt, $i:item) => {\n        $h[deny(clippy::todo)] $i\n    };\n}\n\
                     m!(#, fn fixture() -> u8 { todo!() });\n";
        assert_eq!(
            found(built).as_deref(),
            Some("fixtures define no macro, found `macro_rules!`")
        );
        let handed = "m!(#, fn fixture() -> u8 { todo!() });\n";
        assert_eq!(
            found(handed).as_deref(),
            Some("fixtures carry no attribute, found a `#`")
        );
        // A fixture that does not lex fails closed.
        assert!(fixture_violation("fn f( {}\n").is_err());
    }

    #[test]
    fn the_lowering_check_covers_every_denied_lint() {
        for attr in [MODULE_ATTR, FN_ATTR] {
            let lints = attr
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(lints, _)| lints)
                .unwrap();
            for lint in lints.split(',') {
                assert!(IN_PLAY_LINTS.contains(&lint.trim()), "{lint}");
            }
        }
    }

    #[test]
    fn in_play_files_lower_no_in_play_lint() {
        let lowered = |text: &str| lowering_attributes(text.parse().unwrap());
        // A file-level deny with an impl-level and a statement-level allow.
        let text = format!(
            "{MODULE_ATTR}\n\
             #[allow(clippy::unwrap_used)]\nimpl S {{\n    fn hot(x: Option<u8>) -> u8 {{ x.unwrap() }}\n}}\n\
             fn cold(v: &[u8]) -> u8 {{\n    #[allow(clippy::indexing_slicing)]\n    let x = v[0];\n    x\n}}\n"
        );
        assert_eq!(
            lowered(&text),
            [
                "#[allow(clippy::unwrap_used)]",
                "#[allow(clippy::indexing_slicing)]"
            ]
        );
        // Each way to lower one: `warn`, a group, inner, `cfg_attr`, in an
        // expression, among other lints, and in macro input.
        for attr in [
            "#[warn(clippy::expect_used)]",
            "#[allow(clippy::restriction)]",
            "#[allow(clippy::all)]",
            "#![allow(dead_code, clippy::unwrap_used)]",
            "#[cfg_attr(test, allow(clippy::unwrap_used))]",
            "#[cfg_attr(all(), inline, cfg_attr(any(), warn(clippy::indexing_slicing)))]",
            "#[allow(clippy::unwrap_used, reason = \"x\")]",
        ] {
            assert_eq!(
                lowered(&format!("fn f() {{ m!({attr} 1); }}\n")).len(),
                1,
                "{attr}"
            );
        }
        // `expect` (checked for fulfillment) and other lints stay allowed,
        // as do attributes in literals.
        let fine = "#[expect(clippy::indexing_slicing, reason = \"bounded\")]\nfn f() {}\n\
                    #[allow(dead_code, clippy::cast_sign_loss)]\nfn g() {}\n\
                    #[deny(clippy::unwrap_used)]\nfn h() {}\nconst S: &str = \"#[allow(clippy::unwrap_used)]\";\n";
        assert!(lowered(fine).is_empty());
        // Through the site check, for files and functions alike.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.rs"), &text).unwrap();
        let out = check_site(dir.path(), Site::File("x.rs")).unwrap();
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].contains("lowers an in-play lint"), "{out:?}");
        let out = check_site(dir.path(), Site::Fns("x.rs", &["S::hot"])).unwrap();
        assert_eq!(out.len(), 2, "{out:?}");
    }

    #[test]
    fn in_play_files_name_no_anyhow() {
        let dir = tempfile::tempdir().unwrap();
        let check = |text: &str, site: Site| {
            fs::write(dir.path().join("x.rs"), text).unwrap();
            check_site(dir.path(), site).unwrap()
        };
        let file = Site::File("x.rs");
        let fns = Site::Fns("x.rs", &["hot"]);
        // A module-level import, used inside a listed function: both a
        // whole file and a file of listed functions fail, once each.
        let imported = format!(
            "{MODULE_ATTR}\nuse anyhow::Error as Report;\n\
             {FN_ATTR}\nfn hot() -> Result<(), Report> {{ Ok(()) }}\n"
        );
        for site in [file, fns] {
            let out = check(&imported, site);
            assert_eq!(out.len(), 1, "{out:?}");
            assert!(out[0].contains("`anyhow`"), "{out:?}");
        }
        // The whole file is scanned: outside the listed functions, in a
        // test module, and as a raw identifier, it fails too.
        for text in [
            format!("{FN_ATTR}\nfn hot() {{}}\nfn cold() -> anyhow::Result<()> {{ Ok(()) }}\n"),
            format!(
                "{FN_ATTR}\nfn hot() {{}}\n#[cfg(test)]\nmod tests {{\n    use anyhow::Context;\n}}\n"
            ),
            format!("{FN_ATTR}\nfn hot() {{ let _ = r#anyhow::anyhow!(\"x\"); }}\n"),
        ] {
            assert_eq!(check(&text, fns).len(), 1, "{text}");
        }
        // Comments, doc comments and strings name nothing.
        let fine = format!(
            "{MODULE_ATTR}\n// anyhow\n/// Not `anyhow`.\n\
             {FN_ATTR}\nfn hot() -> &'static str {{ /* anyhow */ \"anyhow\" }}\n"
        );
        assert!(check(&fine, file).is_empty());
        assert!(check(&fine, fns).is_empty());
    }

    #[test]
    fn in_play_files_name_no_log() {
        let dir = tempfile::tempdir().unwrap();
        let check = |text: &str, site: Site| {
            fs::write(dir.path().join("x.rs"), text).unwrap();
            check_site(dir.path(), site).unwrap()
        };
        let file = Site::File("x.rs");
        let fns = Site::Fns("x.rs", &["hot"]);
        let named = "x.rs: in-play code names the `log` facade; only loading and lifecycle code logs (design.md \"Log File\")";
        // Each form, in a whole file and in a file of listed functions,
        // outside the listed function and in a test module too: exactly the
        // one diagnostic.
        for form in [
            "use log::warn;",
            "fn f() { log::info!(\"x\"); }",
            "fn f() { warn!(\"x\"); }",
            "fn f() { r#warn!(\"x\"); }",
            "fn f(_: log::Level) {}",
            "fn f() { r#log::logger().flush(); }",
            "extern crate log;",
            "use log as l;",
            "fn f() { log_enabled!(log::Level::Info); }",
            "fn f() { error![\"x\"]; trace!{\"x\"} }",
        ] {
            for text in [
                format!("{MODULE_ATTR}\n{FN_ATTR}\nfn hot() {{}}\n{form}\n"),
                format!(
                    "{MODULE_ATTR}\n{FN_ATTR}\nfn hot() {{}}\n#[cfg(test)]\nmod tests {{\n    {form}\n}}\n"
                ),
            ] {
                for site in [file, fns] {
                    assert_eq!(check(&text, site), [named], "{text}");
                }
            }
        }
        // Other uses of the names, comments and strings name nothing.
        let fine = format!(
            "{MODULE_ATTR}\n// log::warn!(\"x\")\n/// `use log;`\n\
             {FN_ATTR}\nfn hot() -> &'static str {{ \"warn!\" }}\n\
             fn log() {{}}\nstruct InputLog {{ log: u8 }}\n\
             fn f(s: &InputLog) -> u8 {{ let log = 1; let _ = s.log; log + 1 }}\n\
             fn g(a: bool) -> bool {{ let warn = a; warn != a }}\n\
             #[warn(unused)]\nfn h() {{}}\n"
        );
        assert_eq!(check(&fine, file), Vec::<String>::new());
        assert_eq!(check(&fine, fns), Vec::<String>::new());
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
