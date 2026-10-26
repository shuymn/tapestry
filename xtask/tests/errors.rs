//! Environment errors of the cargo-driven commands: exit code 2, and each
//! cause printed once.

use std::process::Command;

/// Runs `xtask <args>` with `CARGO` naming a program that does not exist,
/// so `cargo metadata` cannot start. It exits 2, prints a line of `prefix`
/// and the failed start, and prints the OS error once.
fn assert_fails_without_cargo(args: &[&str], prefix: &str) {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .env("CARGO", dir.path().join("no-cargo"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    let rest = stderr
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starting {prefix:?}: {stderr}"));
    assert!(
        rest.starts_with("cargo metadata failed: failed to start `cargo metadata`: "),
        "{stderr}"
    );
    assert_eq!(stderr.matches("(os error").count(), 1, "{stderr}");
}

#[test]
fn check_arch_reports_a_missing_cargo_once() {
    assert_fails_without_cargo(&["check-arch"], "check-arch: error: ");
}

#[test]
fn check_lints_reports_a_missing_cargo_once() {
    assert_fails_without_cargo(&["check-lints"], "check-lints: error: ");
}

#[test]
fn portable_reports_a_missing_cargo_once() {
    assert_fails_without_cargo(&["portable", "test"], "xtask portable: ");
}
