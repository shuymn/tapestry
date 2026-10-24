//! Build metadata for `--version` and the FFmpeg runtime DLLs.
//!
//! - `SPIKE_COMMIT`, `SPIKE_TREE` (clean/dirty, tracked files) and
//!   `SPIKE_PROFILE` for `tapestry-spike --version`.
//! - Copies `$FFMPEG_DIR/bin/*.dll` next to the executable (and into
//!   `deps/` for test binaries) so it runs without FFmpeg on `PATH`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));

    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let tree = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(s) if s.is_empty() => "clean",
        Some(_) => "dirty",
        None => "unknown",
    };
    println!("cargo:rustc-env=SPIKE_COMMIT={commit}");
    println!("cargo:rustc-env=SPIKE_TREE={tree}");
    println!(
        "cargo:rustc-env=SPIKE_PROFILE={}",
        env::var("PROFILE").expect("set by cargo")
    );
    // Re-run when HEAD moves or the index changes.
    let mut watched = vec!["HEAD".to_owned(), "index".to_owned()];
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(head_ref);
    }
    for path in watched {
        if let Some(p) = git(&["rev-parse", "--git-path", &path]) {
            println!("cargo:rerun-if-changed={}", manifest.join(p).display());
        }
    }

    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let Ok(ffmpeg) = env::var("FFMPEG_DIR") else {
        return;
    };
    // OUT_DIR is <target>/<profile>/build/<pkg>-<hash>/out.
    let out = PathBuf::from(env::var("OUT_DIR").expect("set by cargo"));
    let profile_dir = out
        .ancestors()
        .nth(3)
        .expect("OUT_DIR is inside the target directory");
    copy_dlls(&Path::new(&ffmpeg).join("bin"), profile_dir);
    copy_dlls(&Path::new(&ffmpeg).join("bin"), &profile_dir.join("deps"));
}

fn copy_dlls(from: &Path, to: &Path) {
    let Ok(entries) = fs::read_dir(from) else {
        println!("cargo:warning=no FFmpeg DLLs in {}", from.display());
        return;
    };
    fs::create_dir_all(to).expect("create target directory");
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
        {
            println!("cargo:rerun-if-changed={}", path.display());
            let dest = to.join(entry.file_name());
            fs::copy(&path, &dest)
                .unwrap_or_else(|e| panic!("copy {} to {}: {e}", path.display(), dest.display()));
        }
    }
}
