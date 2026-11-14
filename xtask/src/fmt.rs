//! `cargo xtask check-fmt`: `cargo fmt -p <package> --check` for each
//! workspace package, then `cargo fmt --check` on the fuzz crate, which is
//! outside the workspace. Each run passes rustfmt only one package's root
//! files; `cargo fmt --all` passes every target's at once, and under a long
//! checkout path Windows refuses that command line (OS error 206).
//!
//! `--all` also checks local path dependencies outside the workspace. Every
//! local path dependency is a workspace member today, so checking the
//! members and the fuzz crate covers what `cargo fmt --all --check` and the
//! fuzz crate's own check did.

use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};

use anyhow::Context as _;
use cargo_metadata::MetadataCommand;

/// The fuzz crate's manifest, relative to the workspace root.
const FUZZ_MANIFEST: &str = "fuzz/Cargo.toml";

/// The command: [`run`] on the workspace of `manifest`, reported like the
/// other checks, each report line given to `line`.
pub fn main(manifest: &Path, line: impl FnMut(String)) -> ExitCode {
    crate::report_with("check-fmt", run(manifest), line)
}

/// One diagnostic per package that is not formatted, after rustfmt's own
/// output. The checks run at once; their output is printed in package order.
pub fn run(manifest: &Path) -> anyhow::Result<Vec<String>> {
    let metadata = crate::metadata(MetadataCommand::new().manifest_path(manifest).no_deps())?;
    let mut names: Vec<String> = metadata
        .workspace_packages()
        .iter()
        .map(|p| p.name.to_string())
        .collect();
    names.sort();
    let root = manifest.parent().context("the manifest has no folder")?;
    let mut checks = Vec::new();
    for name in &names {
        let mut cmd = Command::new(crate::cargo());
        cmd.arg("fmt")
            .arg("--manifest-path")
            .arg(manifest)
            .args(["-p", name, "--check"]);
        checks.push((format!("`{name}`"), cmd));
    }
    let mut cmd = Command::new(crate::cargo());
    cmd.arg("fmt")
        .arg("--manifest-path")
        .arg(root.join(FUZZ_MANIFEST))
        .arg("--check");
    checks.push((format!("`fuzz` ({FUZZ_MANIFEST})"), cmd));

    let mut diagnostics = Vec::new();
    for (package, child) in spawn_all(checks)? {
        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for the check of {package}"))?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            diagnostics.push(format!("{package} is not formatted"));
        }
    }
    Ok(diagnostics)
}

/// Starts every command, its output piped. When one cannot start, waits for
/// those already started, then returns that error.
fn spawn_all<T>(commands: Vec<(T, Command)>) -> anyhow::Result<Vec<(T, Child)>> {
    let mut started = Vec::with_capacity(commands.len());
    for (key, mut cmd) in commands {
        let spawned = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {cmd:?}"));
        match spawned {
            Ok(child) => started.push((key, child)),
            Err(e) => {
                for (_, child) in started {
                    // The start failure is the error reported; a failed wait
                    // here would only hide it.
                    drop(child.wait_with_output());
                }
                return Err(e);
            }
        }
    }
    Ok(started)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::{Command, ExitCode};
    use std::time::Duration;

    use super::{main, run, spawn_all};
    use crate::fixture::{lib, write_workspace};

    const FORMATTED: &str = "fn f() {}\n";
    const MISFORMATTED: &str = "fn  f( ){}\n";

    /// Names the file [`slow_child`] writes when it finishes.
    const MARKER: &str = "XTASK_FMT_TEST_MARKER";

    /// A workspace of `one` and `two` with the fuzz crate beside them.
    fn workspace(dir: &Path) {
        write_workspace(dir, &[lib("one", ""), lib("two", "")]);
        for name in ["one", "two"] {
            fs::write(dir.join("crates").join(name).join("src/lib.rs"), FORMATTED).unwrap();
        }
        let fuzz = dir.join("fuzz");
        fs::create_dir_all(fuzz.join("src")).unwrap();
        fs::write(
            fuzz.join("Cargo.toml"),
            "[package]\nname = \"fuzz\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             publish = false\n[workspace]\n",
        )
        .unwrap();
        fs::write(fuzz.join("src/lib.rs"), FORMATTED).unwrap();
    }

    fn misformat(dir: &Path) {
        fs::write(dir.join("crates/two/src/lib.rs"), MISFORMATTED).unwrap();
        fs::write(dir.join("fuzz/src/lib.rs"), MISFORMATTED).unwrap();
    }

    #[test]
    fn names_each_unformatted_package_and_the_fuzz_crate() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        let manifest = dir.path().join("Cargo.toml");
        assert_eq!(run(&manifest).unwrap(), Vec::<String>::new());

        misformat(dir.path());
        assert_eq!(
            run(&manifest).unwrap(),
            [
                "`two` is not formatted",
                "`fuzz` (fuzz/Cargo.toml) is not formatted"
            ]
        );
    }

    #[test]
    fn the_command_exits_1_naming_unformatted_packages_and_0_when_formatted() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        let manifest = dir.path().join("Cargo.toml");
        let mut lines = Vec::new();
        assert_eq!(main(&manifest, |l| lines.push(l)), ExitCode::SUCCESS);
        assert_eq!(lines, ["check-fmt: ok"]);

        misformat(dir.path());
        let mut lines = Vec::new();
        assert_eq!(main(&manifest, |l| lines.push(l)), ExitCode::from(1));
        assert_eq!(
            lines,
            [
                "check-fmt: `two` is not formatted",
                "check-fmt: `fuzz` (fuzz/Cargo.toml) is not formatted",
                "check-fmt: 2 violation(s)"
            ]
        );
    }

    /// Run by [`a_failed_start_waits_for_the_started_checks`] as its slow
    /// child: sleeps, then writes the marker file.
    #[test]
    fn slow_child() {
        let Some(marker) = std::env::var_os(MARKER) else {
            return;
        };
        std::thread::sleep(Duration::from_secs(2));
        fs::write(marker, "done").unwrap();
    }

    #[test]
    fn a_failed_start_waits_for_the_started_checks() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let mut slow = Command::new(std::env::current_exe().unwrap());
        slow.args(["--exact", "fmt::tests::slow_child", "--nocapture", "-q"])
            .env(MARKER, &marker);
        let missing = Command::new(dir.path().join("no-such-program"));

        let err = spawn_all(vec![("slow", slow), ("missing", missing)]).unwrap_err();
        assert!(format!("{err:#}").starts_with("failed to run "), "{err:#}");
        assert!(
            marker.exists(),
            "returned before the started child finished"
        );
    }
}
