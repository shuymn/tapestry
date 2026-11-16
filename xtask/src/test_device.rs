//! `cargo xtask test-device build [--features <list>] [--picotool]`: builds
//! the test device's firmware (`test-device/firmware`, outside the
//! workspace) and writes its UF2 to `<target dir>/test-device/`
//! (docs/test-device.md "Building").
//!
//! The UF2 comes from the protocol crate's writer, which refuses an image
//! without an image definition in its first 4 KiB, where the RP2350's boot
//! ROM looks for it. The build id the firmware reports in `hello` is the
//! commit (with `-dirty` for uncommitted changes) and `-faults` for a
//! `debug-faults` build. `--picotool` also checks the UF2 with picotool:
//! `picotool info -a` must name the RP2350 and an ARM Secure image, and
//! picotool's own conversion of the ELF must be byte-identical.

use std::ffi::OsString;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context as _, bail};
use cargo_metadata::Message;

/// The firmware's manifest folder, relative to the workspace root.
const FIRMWARE: &str = "test-device/firmware";
/// The firmware's binary target.
const BIN: &str = "test-device-firmware";
/// Where the UF2 goes, under the target directory.
const OUT: &str = "test-device";

/// What `build` was asked for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Build {
    features: Option<String>,
    picotool: bool,
}

fn parse(args: &[OsString]) -> Result<Build, String> {
    let mut build = Build::default();
    let mut it = args.iter().map(|a| a.to_str());
    match it.next() {
        Some(Some("build")) => {}
        _ => return Err("test-device takes `build`".into()),
    }
    while let Some(arg) = it.next() {
        match arg {
            Some("--picotool") if !build.picotool => build.picotool = true,
            Some("--features") if build.features.is_none() => {
                let list = it.next().flatten().ok_or("--features takes a list")?;
                build.features = Some(list.to_owned());
            }
            _ => return Err(format!("unexpected argument {arg:?}")),
        }
    }
    Ok(build)
}

pub fn run(root: &Path, args: &[OsString]) -> ExitCode {
    let build = match parse(args) {
        Ok(b) => b,
        Err(e) => return crate::usage_error(&e),
    };
    match build_uf2(root, &build) {
        Ok(Ok(path)) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Ok(Err(refusal)) => {
            eprintln!("test-device build: {refusal}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("test-device build: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// The commit, `-dirty` when the worktree has changes, and `-faults` for a
/// build with the debug faults; `nogit` outside a git checkout.
fn build_id(root: &Path, features: Option<&str>) -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let mut id = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "nogit".into());
    if git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        id.push_str("-dirty");
    }
    if features.is_some_and(|f| f.split([',', ' ']).any(|f| f == "debug-faults")) {
        id.push_str("-faults");
    }
    id
}

/// Builds the firmware and writes its UF2: the UF2's path, or why the
/// image was refused.
fn build_uf2(root: &Path, build: &Build) -> anyhow::Result<Result<PathBuf, String>> {
    let id = build_id(root, build.features.as_deref());
    eprintln!("test-device build: build id {id}");
    let mut cmd = Command::new(crate::cargo());
    cmd.current_dir(root.join(FIRMWARE))
        .args([
            "build",
            "--release",
            "--locked",
            "--message-format=json-render-diagnostics",
        ])
        .env("TEST_DEVICE_BUILD", &id)
        .stdout(Stdio::piped());
    if let Some(f) = &build.features {
        cmd.args(["--features", f]);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    let stdout = child.stdout.take().context("no stdout from cargo build")?;
    let mut elf = None;
    for message in Message::parse_stream(BufReader::new(stdout)) {
        if let Message::CompilerArtifact(a) = message.context("reading cargo's messages")?
            && a.target.name == BIN
        {
            elf = a.executable.map(PathBuf::from);
        }
    }
    let status = child.wait().context("waiting for cargo build")?;
    if !status.success() {
        bail!("cargo build failed ({status})");
    }
    let elf = elf.context("cargo build reported no firmware executable")?;
    let bytes = fs::read(&elf).with_context(|| format!("reading {}", elf.display()))?;
    let image = match test_device::uf2::from_elf(&bytes) {
        Ok(i) => i,
        Err(e) => return Ok(Err(e)),
    };
    let uf2 = match image.uf2() {
        Ok(u) => u,
        Err(e) => return Ok(Err(e)),
    };
    // The target directory: `CARGO_TARGET_DIR` when set (the Nix shells
    // keep it off the Windows filesystem), else the workspace's.
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let out = target.join(OUT);
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let name = if id.ends_with("-faults") {
        "firmware-faults.uf2"
    } else {
        "firmware.uf2"
    };
    let path = out.join(name);
    fs::write(&path, &uf2).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("test-device build: {} blocks", uf2.len() / 512);
    if build.picotool {
        if let Err(e) = picotool(&elf, &path, &uf2)? {
            return Ok(Err(e));
        }
        eprintln!("test-device build: picotool agrees");
    }
    Ok(Ok(path))
}

/// What `picotool info -a` must say of the UF2.
const INFO_NEEDS: &[&str] = &["RP2350", "ARM Secure", "image def"];

/// The first of [`INFO_NEEDS`] that `info` lacks.
fn check_info(info: &str) -> Result<(), String> {
    match INFO_NEEDS.iter().find(|n| !info.contains(*n)) {
        Some(n) => Err(format!("picotool info -a does not say {n:?}:\n{info}")),
        None => Ok(()),
    }
}

/// Checks the UF2 with picotool.
fn picotool(elf: &Path, path: &Path, uf2: &[u8]) -> anyhow::Result<Result<(), String>> {
    let info = Command::new("picotool")
        .args(["info", "-a"])
        .arg(path)
        .output()
        .context("failed to run picotool (the Nix `test-device` shell has it)")?;
    if !info.status.success() {
        bail!(
            "picotool info failed: {}",
            String::from_utf8_lossy(&info.stderr)
        );
    }
    let text = String::from_utf8_lossy(&info.stdout);
    print!("{text}");
    if let Err(e) = check_info(&text) {
        return Ok(Err(e));
    }
    let theirs = path.with_extension("picotool.uf2");
    let convert = Command::new("picotool")
        .args(["uf2", "convert"])
        .arg(elf)
        .args(["-t", "elf"])
        .arg(&theirs)
        .args(["--family", "rp2350-arm-s"])
        .output()
        .context("failed to run picotool uf2 convert")?;
    if !convert.status.success() {
        bail!(
            "picotool uf2 convert failed: {}",
            String::from_utf8_lossy(&convert.stderr)
        );
    }
    let theirs_bytes =
        fs::read(&theirs).with_context(|| format!("reading {}", theirs.display()))?;
    if theirs_bytes != uf2 {
        return Ok(Err(format!(
            "the UF2 differs from picotool's conversion ({} vs {} bytes, {})",
            uf2.len(),
            theirs_bytes.len(),
            theirs.display()
        )));
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn build_takes_features_and_picotool_once() {
        assert_eq!(parse(&args(&["build"])), Ok(Build::default()));
        assert_eq!(
            parse(&args(&[
                "build",
                "--features",
                "debug-faults",
                "--picotool"
            ])),
            Ok(Build {
                features: Some("debug-faults".into()),
                picotool: true
            })
        );
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["flash"])).is_err());
        assert!(parse(&args(&["build", "--features"])).is_err());
        assert!(parse(&args(&["build", "--picotool", "--picotool"])).is_err());
    }

    /// The lines picotool 2.2.0-a4 prints for the firmware's UF2.
    #[test]
    fn picotool_info_must_name_the_chip_and_a_secure_image() {
        let info = "File firmware.uf2 family ID 'rp2350-arm-s':\n\nProgram Information\n \
                    target chip:  RP2350\n image type:   ARM Secure\n\nMetadata Block 1\n \
                    address:      0x10000114\n block type:   image def\n";
        assert_eq!(check_info(info), Ok(()));
        let nonsecure = info.replace("ARM Secure", "ARM Non-Secure");
        assert!(
            check_info(&nonsecure)
                .unwrap_err()
                .contains("\"ARM Secure\"")
        );
        assert!(
            check_info("target chip: RP2040")
                .unwrap_err()
                .contains("\"RP2350\"")
        );
    }
}
