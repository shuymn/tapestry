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

/// One case: its name, the sibling's code, and the error code it must
/// fail with (`None`: it compiles).
const CASES: &[(&str, &str, Option<&str>)] = &[
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
    let scratch = root.join("target").join("privacy-harness");
    let screens = root.join(SCREENS);
    if !screens.join("mod.rs").is_file() {
        bail!("{SCREENS}/mod.rs not found");
    }
    let widened = scratch.join("widened");
    if widened.exists() {
        fs::remove_dir_all(&widened).context("clearing the widened tree")?;
    }
    copy_widened(&screens, &widened)?;
    let mut members = Vec::new();
    for (tree, variant) in [(&screens, "real"), (&widened, "widened")] {
        for (case, code, _) in CASES {
            let name = format!("{variant}-{case}").replace('_', "-");
            write_case(root, &scratch.join(&name), &name, tree, code)?;
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
        for (case, _, expected) in CASES {
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

/// Copies the tree at `from` to `to`, its restricted items widened.
fn copy_widened(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_widened(&path, &dest)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path)?;
            fs::write(&dest, text.replace(RESTRICTED, WIDENED))?;
        }
    }
    Ok(())
}

/// Writes case crate `name` in `dir`: the tree at `tree` as `screens`,
/// beside `sibling` holding `code`.
fn write_case(root: &Path, dir: &Path, name: &str, tree: &Path, code: &str) -> anyhow::Result<()> {
    let src = dir.join("src");
    fs::create_dir_all(&src)?;
    let dep = |c: &str| {
        format!(
            "{c} = {{ path = \"{}\" }}\n",
            slashes(&root.join("crates").join(c))
        )
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
             [dependencies]\n{}{}{}",
            dep("bms-model"),
            dep("skin-beatoraja"),
            dep("skin-model"),
        ),
    )?;
    fs::write(
        src.join("lib.rs"),
        format!(
            "#![allow(warnings)]\n#[path = \"{}\"]\nmod screens;\nmod sibling;\n",
            slashes(&tree.join("mod.rs"))
        ),
    )?;
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
