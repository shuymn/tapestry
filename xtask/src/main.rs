//! Repository checks and helpers. Run as `cargo xtask <command>`.
//!
//! Exit codes: 0 success, 1 check failed, 2 usage or environment error.

mod arch;
mod banned_words;
mod binary;
mod demo;
#[cfg(test)]
mod fixture;
mod portable;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  check-arch                              check the architecture rules (R1-R3)
  banned-words (--staged|--all) [--list <path>]
                                          check paths and contents in the index for
                                          terms in banned-words.txt
  portable <cargo-command> [args...]      run cargo on every non-Windows-only package
  demo-chart <dir>                        write a self-made two-minute chart and its
                                          generated sounds into <dir>
  check-binary (--absent|--present) <text> <file>
                                          check whether a file contains <text>";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        return usage_error("missing command");
    };
    let rest: Vec<OsString> = args.collect();
    match command.to_str() {
        Some("check-arch") if rest.is_empty() => {
            report(arch::run(&workspace_manifest(), &["--locked"]))
        }
        Some("banned-words") => banned_words::run(&rest),
        Some("portable") => portable::run(&workspace_manifest(), &rest),
        Some("check-binary") => binary::run(&rest),
        Some("demo-chart") => demo_chart(&rest),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => usage_error(&format!("unknown command: {}", command.to_string_lossy())),
    }
}

fn report(result: Result<Vec<String>, String>) -> ExitCode {
    match result {
        Ok(diagnostics) if diagnostics.is_empty() => {
            eprintln!("check-arch: ok");
            ExitCode::SUCCESS
        }
        Ok(diagnostics) => {
            for d in &diagnostics {
                eprintln!("check-arch: {d}");
            }
            eprintln!("check-arch: {} violation(s)", diagnostics.len());
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("check-arch: error: {e}");
            ExitCode::from(2)
        }
    }
}

/// The workspace root manifest, found from this crate's location.
fn workspace_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("Cargo.toml")
}

/// The cargo that invoked us (`cargo xtask` sets `CARGO`), so the pinned
/// toolchain is used for nested commands.
fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("xtask: {message}\n\n{USAGE}");
    ExitCode::from(2)
}

/// `demo-chart <dir>`: writes the demo chart and prints its path.
fn demo_chart(rest: &[OsString]) -> ExitCode {
    let [dir] = rest else {
        return usage_error("demo-chart takes one directory");
    };
    match demo::write(&PathBuf::from(dir)) {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("demo-chart: {e}");
            ExitCode::from(1)
        }
    }
}
