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

#[test]
fn unknown_argument_exits_two() {
    let out = run(&["--no-such-flag"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument: --no-such-flag"));
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
