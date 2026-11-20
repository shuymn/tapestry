//! The media worker links tapestry's FFmpeg build (design.md "Libraries",
//! the FFmpeg row): `cargo xtask ffmpeg fetch` unpacks the release
//! ffmpeg.toml pins into `target/ffmpeg/` (`FFMPEG_DIR`, .cargo/config.toml),
//! where ffmpeg-sys-next finds its headers and import libraries. This script
//! fails the build, naming that command, unless the release there is the
//! pinned one, copies its DLLs beside the binaries it builds, where
//! `tapestry-media.exe` loads them, and names them to the launcher, which
//! stages the worker with them (`media_host::stage`). Cargo runs ffmpeg-sys-next's build script
//! before this one (its `links` metadata reaches this one), so with no
//! release there at all the build fails first in ffmpeg-sys-next's header
//! check, which finds no headers in `target/ffmpeg/include`, and this
//! script's message does not show (docs/setup.md "FFmpeg").
//!
//! With `dev-tools`, the script also names the commit the binary is built
//! from as `TAPESTRY_COMMIT` (docs/test-device.md "Verdicts", provenance):
//! 12 hex digits, `-dirty` for a build of uncommitted changes, `nogit`
//! without Git. A production build gets nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    stamp_commit();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if let Err(e) = run() {
        panic!("{e}");
    }
}

fn run() -> Result<(), String> {
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
    let root = manifest
        .ancestors()
        .nth(2)
        .ok_or("the player crate is not two levels below the workspace root")?;
    let pin = root.join("ffmpeg.toml");
    println!("cargo:rerun-if-changed={}", pin.display());
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let text = fs::read_to_string(&pin).map_err(|e| format!("{}: {e}", pin.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", pin.display()))?;
    let tag = table
        .get("release")
        .and_then(|r| r.get("tag"))
        .and_then(toml::Value::as_str)
        .ok_or("ffmpeg.toml pins no release ([release] tag)")?;
    let dlls: Vec<&str> = table
        .get("build")
        .and_then(|b| b.get("dlls"))
        .and_then(toml::Value::as_array)
        .ok_or("ffmpeg.toml lists no DLLs ([build] dlls)")?
        .iter()
        .map(|d| {
            d.as_str()
                .ok_or("a DLL name in ffmpeg.toml is not a string")
        })
        .collect::<Result<_, _>>()?;

    let dir = PathBuf::from(env("FFMPEG_DIR")?);
    let tag_file = dir.join("TAG");
    println!("cargo:rerun-if-changed={}", tag_file.display());
    let fetch = "run `cargo xtask ffmpeg fetch` (docs/setup.md \"FFmpeg\")";
    let found = fs::read_to_string(&tag_file)
        .map_err(|_| format!("{} holds no FFmpeg release: {fetch}", dir.display()))?;
    if found.trim() != tag {
        return Err(format!(
            "{} holds FFmpeg release {}, ffmpeg.toml pins {tag}: {fetch}",
            dir.display(),
            found.trim()
        ));
    }

    let out = PathBuf::from(env("OUT_DIR")?);
    // OUT_DIR is <profile>/build/player-<hash>/out: the binaries are in
    // <profile>.
    let bin_dir = out
        .ancestors()
        .nth(3)
        .ok_or("OUT_DIR is not inside a profile directory")?;
    // The launcher stages the worker with the same DLLs.
    println!("cargo:rustc-env=TAPESTRY_FFMPEG_DLLS={}", dlls.join(","));
    for dll in dlls {
        let name = format!("{dll}.dll");
        let from = dir.join("bin").join(&name);
        println!("cargo:rerun-if-changed={}", from.display());
        copy_if_changed(&from, &bin_dir.join(&name))?;
    }
    Ok(())
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// Copies `from` to `to` unless `to` already holds the same bytes: a DLL a
/// running worker has loaded cannot be overwritten.
fn copy_if_changed(from: &Path, to: &Path) -> Result<(), String> {
    let bytes = fs::read(from).map_err(|e| format!("{}: {e}", from.display()))?;
    if fs::read(to).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    fs::write(to, &bytes).map_err(|e| format!("{}: {e}", to.display()))
}

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

/// With `dev-tools`, sets `TAPESTRY_COMMIT` and reruns when the commit or
/// the tree changes.
fn stamp_commit() {
    if std::env::var_os("CARGO_FEATURE_DEV_TOOLS").is_none() {
        return;
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
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
