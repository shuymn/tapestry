//! With `dev-tools`, names the commit the binary is built from as
//! `TAPESTRY_COMMIT` (docs/test-device.md "Verdicts", provenance): 12 hex
//! digits, `-dirty` for a build of uncommitted changes, `nogit` without
//! Git. A production build gets nothing.

use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_DEV_TOOLS").is_none() {
        return;
    }
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dir = Path::new(&manifest);
    let mut id = git(dir, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "nogit".into());
    if git(dir, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        id.push_str("-dirty");
    }
    println!("cargo:rustc-env=TAPESTRY_COMMIT={id}");
    // Again when the commit or the tree changes: the checkout's HEAD and
    // index, and every file of the workspace's sources.
    let branch = git(dir, &["rev-parse", "--symbolic-full-name", "HEAD"]);
    for p in ["HEAD", "index", "packed-refs"]
        .into_iter()
        .chain(branch.as_deref())
    {
        if let Some(path) = git(dir, &["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={}", dir.join(path).display());
        }
    }
    if let Some(root) = git(dir, &["rev-parse", "--show-toplevel"]) {
        let root = Path::new(&root);
        for p in ["crates", "Cargo.toml", "Cargo.lock", "docs", "xtask"] {
            println!("cargo:rerun-if-changed={}", root.join(p).display());
        }
    }
}
