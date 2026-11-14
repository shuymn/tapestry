//! `cargo xtask check-fmt`: `cargo fmt -p <package> --check` for each
//! workspace package, then `cargo fmt --check` on the fuzz crate, which is
//! outside the workspace. Together they check what
//! `cargo fmt --all --check` and the fuzz crate's own check do, but each run
//! passes rustfmt only one package's root files: `--all` passes every
//! target's at once, and under a long checkout path Windows refuses that
//! command line (OS error 206).

use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::Context as _;
use cargo_metadata::MetadataCommand;

/// The fuzz crate's manifest, relative to the workspace root.
const FUZZ_MANIFEST: &str = "fuzz/Cargo.toml";

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
        checks.push((format!("`{name}`"), spawn(&mut cmd)?));
    }
    let mut cmd = Command::new(crate::cargo());
    cmd.arg("fmt")
        .arg("--manifest-path")
        .arg(root.join(FUZZ_MANIFEST))
        .arg("--check");
    checks.push((format!("`fuzz` ({FUZZ_MANIFEST})"), spawn(&mut cmd)?));

    let mut diagnostics = Vec::new();
    for (package, child) in checks {
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

fn spawn(cmd: &mut Command) -> anyhow::Result<Child> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {cmd:?}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::run;
    use crate::fixture::{lib, write_workspace};

    const FORMATTED: &str = "fn f() {}
";

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

    #[test]
    fn names_each_unformatted_package_and_the_fuzz_crate() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        let manifest = dir.path().join("Cargo.toml");
        assert_eq!(run(&manifest).unwrap(), Vec::<String>::new());

        let misformatted = "fn  f( ){}\n";
        fs::write(dir.path().join("crates/two/src/lib.rs"), misformatted).unwrap();
        fs::write(dir.path().join("fuzz/src/lib.rs"), misformatted).unwrap();
        assert_eq!(
            run(&manifest).unwrap(),
            [
                "`two` is not formatted",
                "`fuzz` (fuzz/Cargo.toml) is not formatted"
            ]
        );
    }
}
