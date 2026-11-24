//! The render thread's failures on the real binary (dev-tools): a panic
//! at a frame and one while it builds its first screen, and a first screen
//! past the startup's bound, each the run's failure (exit 1) with why on
//! stderr and in the run's log (design.md "Log File", "Showing the
//! window"). Headless, on WARP offscreen.

#![cfg(feature = "dev-tools")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// 120 BPM, lane 1 on every beat of measure 1.
const CHART: &str = "#PLAYER 1\n#TITLE startup\n#BPM 120\n#00111:01010101\n";

/// A temporary folder with the chart; returns it, the chart's path and the
/// data root's parent.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let chart = dir.path().join("startup.bms");
    fs::write(&chart, CHART).unwrap();
    let appdata = dir.path().join("appdata");
    (dir, chart, appdata)
}

/// `play --auto <chart> --dev-headless`, its render bound `timeout` s,
/// with `env` set.
fn run(chart: &Path, appdata: &Path, timeout: &str, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tapestry"));
    command
        .args(["play", "--auto"])
        .arg(chart)
        .args(["--dev-headless", "--dev-render-timeout", timeout])
        .env("LOCALAPPDATA", appdata);
    for (k, v) in env {
        command.env(k, v);
    }
    command.output().expect("run tapestry")
}

/// The run's log.
fn log(appdata: &Path) -> String {
    let path = appdata.join("tapestry").join("logs").join("tapestry.log");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Checks the run failed with `line` on stderr and in the log, and the
/// log's exit line is 1's.
fn failed_with(out: &Output, appdata: &Path, line: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let log = log(appdata);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(line), "stderr lacks {line:?}:\n{stderr}");
    assert!(log.contains(line), "the log lacks {line:?}:\n{log}");
    assert!(
        log.lines().last().is_some_and(|l| l.ends_with(" exit 1")),
        "{log}"
    );
}

/// W2.1: a panic at the render thread's third frame ends the run with
/// exit 1, its message on stderr and in the log.
#[test]
fn a_render_panic_at_a_frame_fails_the_run_with_its_message() {
    let (_dir, chart, appdata) = fixture();
    let out = run(
        &chart,
        &appdata,
        "600",
        &[("TAPESTRY_DEV_RENDER_PANIC", "frame:3")],
    );
    failed_with(
        &out,
        &appdata,
        "render: the render thread panicked: dev: the render thread panics at its frame 3",
    );
}

/// W2.2: a panic while the first screen is built ends the run with exit
/// 1, its message on stderr and in the log.
#[test]
fn a_render_panic_while_starting_fails_the_run_with_its_message() {
    let (_dir, chart, appdata) = fixture();
    let out = run(
        &chart,
        &appdata,
        "600",
        &[("TAPESTRY_DEV_RENDER_PANIC", "build")],
    );
    failed_with(
        &out,
        &appdata,
        "the render thread panicked while starting: dev: the render thread panics while building its first screen",
    );
}

/// W3.6: a first screen not built within the startup's bound (30 s, a
/// headless run's whatever its render bound) fails the run at the bound,
/// not at the build's end, which is left detached.
#[test]
fn a_first_screen_past_the_bound_fails_the_run() {
    let (_dir, chart, appdata) = fixture();
    let began = Instant::now();
    let out = run(
        &chart,
        &appdata,
        "600",
        &[("TAPESTRY_DEV_BUILD_HOLD_MS", "90000")],
    );
    failed_with(
        &out,
        &appdata,
        "the render thread did not build its first screen in time",
    );
    assert!(
        began.elapsed() < Duration::from_secs(80),
        "{:?}",
        began.elapsed()
    );
}
