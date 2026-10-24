//! `tapestry-spike`: the stage 0 spike executable (issue #3).
//!
//! P0 provides `--version`, which reports the build (commit, tree state,
//! profile) and the linked FFmpeg with the license its libraries report.
//! It exits 1 unless both avcodec and avutil report an LGPL license.

use ffmpeg_next::ffi;
use std::ffi::{CStr, c_char};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [a] if a == "--version" => version(),
        _ => {
            eprintln!("usage: tapestry-spike --version");
            ExitCode::from(2)
        }
    }
}

/// A static NUL-terminated string returned by FFmpeg.
fn ffmpeg_str(p: *const c_char) -> String {
    if p.is_null() {
        return "(null)".into();
    }
    // SAFETY: FFmpeg's version and license getters return pointers to
    // static NUL-terminated strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn lib_version(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

fn version() -> ExitCode {
    println!("tapestry-spike {}", env!("CARGO_PKG_VERSION"));
    println!("commit: {} ({})", env!("SPIKE_COMMIT"), env!("SPIKE_TREE"));
    println!("profile: {}", env!("SPIKE_PROFILE"));
    // SAFETY: plain getters without preconditions.
    let (info, codec_v, util_v, codec_l, util_l) = unsafe {
        (
            ffi::av_version_info(),
            ffi::avcodec_version(),
            ffi::avutil_version(),
            ffi::avcodec_license(),
            ffi::avutil_license(),
        )
    };
    println!("ffmpeg: {}", ffmpeg_str(info));
    let libs = [
        ("avcodec", codec_v, ffmpeg_str(codec_l)),
        ("avutil", util_v, ffmpeg_str(util_l)),
    ];
    let mut lgpl = true;
    for (name, v, license) in &libs {
        println!("{name} {}: license {license}", lib_version(*v));
        lgpl &= license.starts_with("LGPL");
    }
    if lgpl {
        println!("ffmpeg license: LGPL");
        ExitCode::SUCCESS
    } else {
        eprintln!("tapestry-spike: the linked FFmpeg is not an LGPL build");
        ExitCode::from(1)
    }
}
