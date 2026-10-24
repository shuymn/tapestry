//! The game executable.

#[cfg(not(windows))]
compile_error!("player is Windows-only");

#[cfg(feature = "dev-tools")]
mod dev;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(arg) = args.next() else {
        return ExitCode::SUCCESS;
    };
    if let Some(extra) = args.next() {
        return unknown_argument(&extra);
    }

    #[cfg(feature = "dev-tools")]
    if let Some(code) = dev::handle_arg(&arg) {
        return code;
    }

    if arg == "--version" {
        println!("tapestry {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    unknown_argument(&arg)
}

fn unknown_argument(arg: &OsString) -> ExitCode {
    eprintln!("unknown argument: {}", arg.to_string_lossy());
    ExitCode::from(2)
}
