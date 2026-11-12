//! `cargo xtask check-arch`'s privacy harness (design.md "beatoraja Lua
//! Skins", Unreachable from play): `player` has no library target, so no
//! doctest of it runs, and an external crate cannot tell
//! `pub(in crate::screens)` from `pub(crate)`. The harness instead builds
//! a scratch workspace whose crates each hold the real
//! `crates/player/src/screens/` tree, by path, beside a sibling module that
//! plays a caller outside `screens`:
//!
//! - the passing case holds a `ScriptSlot` and passes it to
//!   `selection_visit`, and compiles;
//! - each failing case (opening the slot, opening the cell's handle,
//!   calling a restricted method) fails with its privacy error in the
//!   sibling;
//! - the harness checks itself: with the tree's `pub(in crate::screens)`
//!   widened to `pub(crate)`, every failing case compiles, so the negative
//!   cases tell the boundary from the weaker one.
//!
//! Every case is checked in one `cargo check --keep-going` run, in its own
//! target directory under the workspace's `target/`.
//!
//! The same harness holds `lua_replay`'s production tree
//! (`crates/skin-beatoraja/src/bin/lua_replay/production/`, design.md
//! "beatoraja Lua Skins", the reference executor) beside a sibling that
//! plays the reference: naming the opaque `ProductionRun` and calling its
//! public method compiles; reading its fields or naming the per-frame
//! trace fails with a privacy error; with `pub(in crate::production)`
//! widened to `pub(crate)`, every failing case compiles.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use cargo_metadata::Message;

/// The tree the harness holds, relative to the workspace root.
const SCREENS: &str = "crates/player/src/screens";
/// The visibility the tree's restricted items carry, and what the
/// self-check widens it to.
const RESTRICTED: &str = "pub(in crate::screens)";
const WIDENED: &str = "pub(crate)";

/// `lua_replay`'s source folder and its production tree, relative to the
/// workspace root, and the tree's restricted visibility.
const REPLAY: &str = "crates/skin-beatoraja/src/bin/lua_replay";
const PRODUCTION: &str = "crates/skin-beatoraja/src/bin/lua_replay/production";
const PRODUCTION_RESTRICTED: &str = "pub(in crate::production)";
/// The modules of `lua_replay` the production tree names, beside it.
const REPLAY_MODULES: [&str; 6] = [
    "hints",
    "inputs",
    "isolation",
    "observed",
    "report",
    "script",
];

/// One case: its name, the sibling's code, and the error code it must
/// fail with (`None`: it compiles).
type Case = (&'static str, &'static str, Option<&'static str>);

/// The production tree's cases: the reference may name the handle and
/// call its public method, never read what production did.
const REPLAY_CASES: &[Case] = &[
    (
        "names_the_run",
        "pub(crate) fn caller(
            run: &crate::production::ProductionRun,
            other: &skin_beatoraja::lua::replay::Recorded,
        ) -> bool {
            run.same_import(other, \"\")
        }",
        None,
    ),
    (
        "reads_the_frames",
        "pub(crate) fn caller(run: &crate::production::ProductionRun) -> usize {
            run.frames.len()
        }",
        Some("E0616"),
    ),
    (
        "reads_the_records",
        "pub(crate) fn caller(run: &crate::production::ProductionRun) -> usize {
            run.recorded.occurrences.len()
        }",
        Some("E0616"),
    ),
    (
        "names_the_trace",
        "pub(crate) fn caller(f: &crate::production::FrameTrace) -> usize {
            f.events.len()
        }",
        Some("E0603"),
    ),
];

const CASES: &[Case] = &[
    (
        "holds_a_slot",
        "pub(crate) fn caller(
            slot: &mut crate::screens::ScriptSlot,
            skin: &skin_model::Skin,
            sel: &mut skin_model::Selection,
            timers: &skin_model::SelectionTimers,
        ) {
            let _ = crate::screens::selection_visit(slot, skin, sel, bms_model::TimeUs(0), timers);
        }",
        None,
    ),
    (
        "opens_the_slot",
        "pub(crate) fn caller(slot: &crate::screens::ScriptSlot) {
            let _ = &slot.0;
        }",
        Some("E0616"),
    ),
    (
        "opens_the_cell",
        "pub(crate) fn caller(scripts: &crate::screens::Scripts) {
            let _ = &scripts.0;
        }",
        Some("E0616"),
    ),
    (
        "calls_a_restricted_method",
        "pub(crate) fn caller(slot: &mut crate::screens::ScriptSlot) {
            let _ = slot.running();
        }",
        Some("E0624"),
    ),
];

/// Runs the harness for the workspace at `root`; one diagnostic per broken
/// expectation.
pub fn run(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut d = run_tree(root, &screens_tree(root))?;
    d.extend(run_tree(root, &replay_tree(root))?);
    Ok(d)
}

/// One tree the harness holds.
struct Tree {
    /// The scratch workspace's folder name under `target/`.
    scratch: &'static str,
    /// The tree, relative to the workspace root.
    dir: &'static str,
    restricted: &'static str,
    /// The case crate's `lib.rs` for a tree at a path.
    lib: fn(&Path, &Path) -> String,
    /// The crates a case depends on, each with its features.
    deps: &'static [(&'static str, &'static str)],
    cases: &'static [Case],
}

fn screens_tree(_root: &Path) -> Tree {
    Tree {
        scratch: "privacy-harness",
        dir: SCREENS,
        restricted: RESTRICTED,
        lib: |_, tree| {
            format!(
                "#![allow(warnings)]\n#[path = \"{}\"]\nmod screens;\nmod sibling;\n",
                slashes(&tree.join("mod.rs"))
            )
        },
        deps: &[
            ("bms-model", ""),
            ("skin-beatoraja", ""),
            ("skin-model", ""),
        ],
        cases: CASES,
    }
}

fn replay_tree(_root: &Path) -> Tree {
    Tree {
        scratch: "privacy-harness-replay",
        dir: PRODUCTION,
        restricted: PRODUCTION_RESTRICTED,
        lib: |root, tree| {
            let mut lib = String::from("#![allow(warnings)]\n");
            for m in REPLAY_MODULES {
                lib.push_str(&format!(
                    "#[path = \"{}\"]\nmod {m};\n",
                    slashes(&root.join(REPLAY).join(format!("{m}.rs")))
                ));
            }
            lib.push_str(&format!(
                "#[path = \"{}\"]\nmod production;\nmod sibling;\n",
                slashes(&tree.join("mod.rs"))
            ));
            lib
        },
        deps: &[
            ("bms-model", ""),
            ("eval", ""),
            ("game", ""),
            ("rules", ""),
            ("skin-beatoraja", "dev-tools"),
            ("skin-model", "dev-tools"),
        ],
        cases: REPLAY_CASES,
    }
}

/// The harness over `t`: its real and widened trees, every case.
fn run_tree(root: &Path, t: &Tree) -> anyhow::Result<Vec<String>> {
    let scratch = root.join("target").join(t.scratch);
    let real = root.join(t.dir);
    if !real.join("mod.rs").is_file() {
        bail!("{}/mod.rs not found", t.dir);
    }
    let widened = scratch.join("widened");
    if widened.exists() {
        fs::remove_dir_all(&widened).context("clearing the widened tree")?;
    }
    copy_widened(&real, &widened, t.restricted)?;
    let mut members = Vec::new();
    for (tree, variant) in [(&real, "real"), (&widened, "widened")] {
        for (case, code, _) in t.cases {
            let name = format!("{variant}-{case}").replace('_', "-");
            write_case(root, &scratch.join(&name), &name, t, tree, code)?;
            members.push(name);
        }
    }
    let quoted: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
    fs::write(
        scratch.join("Cargo.toml"),
        format!(
            "[workspace]\nresolver = \"3\"\nmembers = [{}]\n",
            quoted.join(", ")
        ),
    )?;
    fs::copy(root.join("Cargo.lock"), scratch.join("Cargo.lock")).context("copying Cargo.lock")?;
    let errors = check(&scratch)?;
    let mut diagnostics = Vec::new();
    for (variant, widened) in [("real", false), ("widened", true)] {
        for (case, _, expected) in t.cases {
            let name = format!("{variant}-{case}").replace('_', "-");
            let got = errors.get(&name).cloned().unwrap_or_default();
            let expected = if widened { None } else { *expected };
            match expected {
                None if !got.is_empty() => diagnostics.push(format!(
                    "privacy harness: `{case}` ({variant}) should compile, but: {}",
                    got.join("; ")
                )),
                Some(code) if !got.iter().any(|e| e.starts_with(code)) => {
                    diagnostics.push(format!(
                        "privacy harness: `{case}` ({variant}) should fail with {code} in its \
                         sibling, but got: {}",
                        if got.is_empty() {
                            "no error".to_owned()
                        } else {
                            got.join("; ")
                        }
                    ));
                }
                _ => {}
            }
        }
    }
    // A widened tree whose failing cases still fail means the negative
    // expectations do not tell the boundary from `pub(crate)`.
    Ok(diagnostics)
}

/// Copies the tree at `from` to `to`, its `restricted` items widened.
fn copy_widened(from: &Path, to: &Path, restricted: &str) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_widened(&path, &dest, restricted)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path)?;
            fs::write(&dest, text.replace(restricted, WIDENED))?;
        }
    }
    Ok(())
}

/// Writes case crate `name` in `dir`: the tree at `tree` as `t` holds it,
/// beside `sibling` holding `code`.
fn write_case(
    root: &Path,
    dir: &Path,
    name: &str,
    t: &Tree,
    tree: &Path,
    code: &str,
) -> anyhow::Result<()> {
    let src = dir.join("src");
    fs::create_dir_all(&src)?;
    let deps: String = t
        .deps
        .iter()
        .map(|(c, features)| {
            let features = if features.is_empty() {
                String::new()
            } else {
                format!(", features = [\"{features}\"]")
            };
            format!(
                "{c} = {{ path = \"{}\"{features} }}\n",
                slashes(&root.join("crates").join(c))
            )
        })
        .collect();
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
             [dependencies]\n{deps}",
        ),
    )?;
    fs::write(src.join("lib.rs"), (t.lib)(root, tree))?;
    fs::write(src.join("sibling.rs"), format!("{code}\n"))?;
    Ok(())
}

fn slashes(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Checks every case crate of the scratch workspace at `scratch`: per
/// package, the error codes (with their messages) reported in its
/// sibling module.
fn check(scratch: &Path) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let target: PathBuf = scratch.join("target");
    let output = Command::new(crate::cargo())
        .arg("check")
        .arg("--manifest-path")
        .arg(scratch.join("Cargo.toml"))
        .args(["--workspace", "--keep-going", "--message-format=json"])
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .context("failed to run cargo check")?;
    let mut errors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut finished = false;
    for message in Message::parse_stream(output.stdout.as_slice()) {
        match message.context("reading cargo check output")? {
            Message::CompilerMessage(m) => {
                let d = &m.message;
                if !matches!(d.level, cargo_metadata::diagnostic::DiagnosticLevel::Error) {
                    continue;
                }
                let in_sibling = d.spans.iter().any(|s| {
                    s.is_primary && s.file_name.replace('\\', "/").ends_with("src/sibling.rs")
                });
                let code = d.code.as_ref().map_or("", |c| c.code.as_str());
                let what = if in_sibling {
                    format!("{code} {}", d.message)
                } else {
                    format!("outside the sibling: {code} {}", d.message)
                };
                errors
                    .entry(m.target.name.replace('_', "-"))
                    .or_default()
                    .push(what);
            }
            Message::BuildFinished(_) => finished = true,
            _ => {}
        }
    }
    if !finished {
        bail!(
            "cargo check did not finish a build:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(errors)
}
