//! `cargo xtask portable <cargo-command> [args...]`: runs
//! `cargo <cargo-command> --workspace --exclude <each Windows-only package> [args...]`.
//! The excludes come from `[package.metadata.tapestry] windows-only`, so they
//! cannot drift from the workspace.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::bail;
use cargo_metadata::MetadataCommand;

use crate::arch::is_windows_only;

/// Packages that must stay in the portable set (built and tested on Linux).
const REQUIRED_PORTABLE: &[&str] = &[
    "bms-model",
    "bms-parser",
    "rules",
    "rules-toml",
    "eval",
    "replay",
    "input",
    "audio",
    "game",
    "library",
    "skin-model",
    "skin-beatoraja",
    "eval-cli",
    "xtask",
];

pub fn run(manifest: &Path, args: &[OsString]) -> ExitCode {
    let Some((command, rest)) = args.split_first() else {
        eprintln!("xtask portable: missing cargo command (e.g. `cargo xtask portable test`)");
        return ExitCode::from(2);
    };
    let excludes = match windows_only_packages(manifest) {
        Ok(excludes) => excludes,
        Err(e) => {
            eprintln!("xtask portable: {e:#}");
            return ExitCode::from(2);
        }
    };
    let mut cmd = Command::new(crate::cargo());
    cmd.arg(command).arg("--workspace");
    for name in &excludes {
        cmd.args(["--exclude", name]);
    }
    cmd.args(rest);
    eprintln!("xtask portable: excluding {excludes:?}");
    match cmd.status() {
        Ok(status) => match status.code() {
            Some(0) => ExitCode::SUCCESS,
            Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            None => ExitCode::from(1),
        },
        Err(e) => {
            eprintln!("xtask portable: failed to run cargo: {e}");
            ExitCode::from(2)
        }
    }
}

/// Names of the Windows-only workspace packages. Fails if a package in
/// [`REQUIRED_PORTABLE`] is missing from the portable set.
fn windows_only_packages(manifest: &Path) -> anyhow::Result<Vec<String>> {
    let metadata = crate::metadata(MetadataCommand::new().manifest_path(manifest).no_deps())?;
    let members = metadata.workspace_packages();
    let mut excludes = Vec::new();
    for pkg in &members {
        if is_windows_only(pkg) {
            excludes.push(pkg.name.to_string());
        }
    }
    for required in REQUIRED_PORTABLE {
        let portable = members
            .iter()
            .any(|p| p.name.as_str() == *required && !is_windows_only(p));
        if !portable {
            bail!("R2: `{required}` must be a portable (non-Windows-only) workspace package");
        }
    }
    Ok(excludes)
}

#[cfg(test)]
mod tests {
    use super::windows_only_packages;
    use crate::fixture::{Crate, baseline, set, write_workspace};

    fn excludes(crates: &[Crate]) -> anyhow::Result<Vec<String>> {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path(), crates);
        windows_only_packages(&dir.path().join("Cargo.toml"))
    }

    #[test]
    fn excludes_windows_only_packages() {
        assert_eq!(excludes(&baseline()).unwrap(), ["player"]);
    }

    #[test]
    fn rejects_required_package_marked_windows_only() {
        let mut crates = baseline();
        set(
            &mut crates,
            "audio",
            "[package.metadata.tapestry]\nwindows-only = true\n",
        );
        let err = format!("{:#}", excludes(&crates).unwrap_err());
        assert!(err.starts_with("R2: `audio` must be a portable"), "{err}");
    }
}
