use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args(args)
        .output()
        .expect("run tapestry")
}

#[test]
fn version_exits_zero() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("tapestry "));
}

#[track_caller]
fn assert_unknown(args: &[&str], arg: &str) {
    let out = run(args);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("unknown argument: {arg}")),
        "{stderr}"
    );
}

#[test]
fn unknown_argument_still_exits_two() {
    assert_unknown(&["--no-such-flag"], "--no-such-flag");
    // After `play` too, before or after the chart, and a second chart.
    assert_unknown(&["play", "--no-such-flag", "a.bms"], "--no-such-flag");
    assert_unknown(&["play", "a.bms", "--no-such-flag"], "--no-such-flag");
    assert_unknown(&["play", "a.bms", "b.bms"], "b.bms");
    assert_unknown(&["keys", "--no-such-flag"], "--no-such-flag");
    assert_unknown(&["keys", "a.bms"], "a.bms");
}

#[test]
fn play_without_chart_is_an_error() {
    for args in [&["play"][..], &[]] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("usage: tapestry play"), "{stderr}");
        if !args.is_empty() {
            assert!(stderr.contains("play needs a chart file"), "{stderr}");
        }
    }
}

/// A chart that does not exist is refused before any window opens, in a
/// data root of its own.
#[test]
fn missing_chart_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let chart = dir.path().join("missing.bms");
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args(["play".as_ref(), chart.as_os_str()])
        .env("LOCALAPPDATA", dir.path())
        .output()
        .expect("run tapestry");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing.bms"), "{stderr}");
    // The first start wrote the default key config.
    assert!(dir.path().join("tapestry").join("device.toml").is_file());
}

#[cfg(not(feature = "dev-tools"))]
#[test]
fn dev_scripted_input_is_unknown_without_feature() {
    assert_unknown(&["--dev-scripted-input", "x"], "--dev-scripted-input");
    assert_unknown(
        &["play", "a.bms", "--dev-scripted-input", "x"],
        "--dev-scripted-input",
    );
    assert_unknown(&["play", "a.bms", "--dev-frames", "x"], "--dev-frames");
    assert_unknown(&["play", "a.bms", "--dev-auto-retry"], "--dev-auto-retry");
    assert_unknown(&["--dev-adjust-at", "1"], "--dev-adjust-at");
    for opt in ["--dev-adjust-at", "--dev-adjust", "--dev-adjust-end"] {
        assert_unknown(&["play", "a.bms", opt, "1"], opt);
    }
    assert_unknown(&["keys", "--dev-scripted-keys", "1"], "--dev-scripted-keys");
}

#[test]
fn extra_argument_exits_two() {
    let out = run(&["--version", "extra"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument: extra"));
}

#[cfg(not(feature = "dev-tools"))]
#[test]
fn dev_tools_probe_is_unknown_without_feature() {
    let out = run(&["--dev-tools-probe"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument: --dev-tools-probe"));
}

#[cfg(feature = "dev-tools")]
#[test]
fn dev_tools_probe_runs_with_feature() {
    let out = run(&["--dev-tools-probe"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        "tapestry dev-tools enabled"
    );
}

/// The positive control of the production check: a development build
/// takes the same forms and fails on the missing files instead.
#[cfg(feature = "dev-tools")]
#[test]
fn dev_options_are_taken_with_the_feature() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("missing.tprl");
    for extra in [
        vec!["--dev-scripted-input".as_ref(), log.as_os_str()],
        vec!["--dev-frames".as_ref(), dir.path().as_os_str()],
        vec!["--dev-auto-retry".as_ref()],
        vec!["--dev-adjust-at".as_ref(), "1".as_ref()],
        vec!["--dev-adjust".as_ref(), "F1".as_ref()],
        vec!["--dev-adjust-end".as_ref(), "F1".as_ref()],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
            .arg("play")
            .arg(dir.path().join("missing.bms"))
            .args(&extra)
            .env("LOCALAPPDATA", dir.path())
            .output()
            .expect("run tapestry");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(!stderr.contains("unknown argument"), "{stderr}");
        assert!(stderr.contains("missing."), "{stderr}");
    }
}

/// `measure` exists only in development builds: production treats it as
/// any other unknown argument.
#[cfg(not(feature = "dev-tools"))]
#[test]
fn measure_is_unknown_without_feature() {
    assert_unknown(&["measure", "play", "a.bms"], "measure");
    assert_unknown(&["measure"], "measure");
}

/// The positive control: a development build takes `measure play`, and
/// without PresentMon it fails with the reason before anything starts.
#[cfg(feature = "dev-tools")]
#[test]
fn measure_play_needs_presentmon() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("PresentMon.exe");
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("measure")
        .arg("play")
        .arg(dir.path().join("missing.bms"))
        .env("LOCALAPPDATA", dir.path())
        .env("TAPESTRY_PRESENTMON", &missing)
        .output()
        .expect("run tapestry");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("TAPESTRY_PRESENTMON names"), "{stderr}");
    assert!(!stderr.contains("unknown argument"), "{stderr}");
    // Nothing was started, so no capture was made.
    assert!(!dir.path().join("tapestry").join("measure").exists());
    assert_unknown(&["measure", "run", "a.bms"], "run");
    assert_unknown(&["measure", "play", "a.bms", "b.bms"], "b.bms");
    let usage = run(&["measure"]);
    assert_eq!(usage.status.code(), Some(2));
}
