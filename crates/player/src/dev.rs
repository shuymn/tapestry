//! Development features, compiled only with the `dev-tools` feature.

use std::ffi::OsStr;
use std::process::ExitCode;

/// Handles a development-only argument, or returns `None` for any other one.
pub fn handle_arg(arg: &OsStr) -> Option<ExitCode> {
    if arg == "--dev-tools-probe" {
        // CI searches release binaries for this exact string to prove the
        // feature is absent from production builds.
        println!("tapestry dev-tools enabled");
        return Some(ExitCode::SUCCESS);
    }
    None
}
