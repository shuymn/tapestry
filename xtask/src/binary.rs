//! `cargo xtask check-binary (--absent|--present) <text> <file>`: byte search
//! used by CI to prove `dev-tools` code is absent from production binaries.

use std::ffi::OsString;
use std::process::ExitCode;

pub fn run(args: &[OsString]) -> ExitCode {
    let [mode, text, file] = args else {
        eprintln!("usage: cargo xtask check-binary (--absent|--present) <text> <file>");
        return ExitCode::from(2);
    };
    let want_present = match mode.to_str() {
        Some("--present") => true,
        Some("--absent") => false,
        _ => {
            eprintln!("check-binary: mode must be --absent or --present");
            return ExitCode::from(2);
        }
    };
    let Some(text) = text.to_str().filter(|t| !t.is_empty()) else {
        eprintln!("check-binary: <text> must be non-empty UTF-8");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("check-binary: cannot read {}: {e}", file.to_string_lossy());
            return ExitCode::from(2);
        }
    };
    let present = contains(&bytes, text.as_bytes());
    let file = file.to_string_lossy();
    if present == want_present {
        eprintln!(
            "check-binary: {file}: {:?} is {} as expected",
            text,
            if present { "present" } else { "absent" }
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "check-binary: {file}: {:?} is {}, expected {}",
            text,
            if present { "present" } else { "absent" },
            if want_present { "present" } else { "absent" }
        );
        ExitCode::from(1)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::contains;

    #[test]
    fn finds_bytes_anywhere() {
        assert!(contains(b"\0\x01probe\xff", b"probe"));
        assert!(contains(b"probe", b"probe"));
        assert!(!contains(b"prob", b"probe"));
        assert!(!contains(b"pro\0be", b"probe"));
    }
}
