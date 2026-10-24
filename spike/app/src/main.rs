//! `tapestry-spike`: the stage 0 spike executable (issue #3).
//!
//! ```text
//! tapestry-spike --version
//! tapestry-spike gen-assets --ffmpeg <exe> --out <dir> [--seconds N]
//! tapestry-spike run --out <dir> [--bga <video>] [--fps-cap N]
//!     [--loops N | --duration-s N] [--window WxH]
//!     [--offscreen WxH --adapter warp --audio null --frames N]
//! ```
//!
//! `--version` reports the build and the linked FFmpeg with the license its
//! libraries report, and exits 1 unless both avcodec and avutil are LGPL.

mod alloc;
mod bga;
mod gen_assets;
mod render;
mod run;
mod scene;
mod timebase;
mod window;

use run::{AdapterChoice, RunArgs};
use std::path::PathBuf;
use std::process::ExitCode;

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

const USAGE: &str = "usage:
  tapestry-spike --version
  tapestry-spike gen-assets --ffmpeg <exe> --out <dir> [--seconds N]
  tapestry-spike run --out <dir> [--bga <video>] [--fps-cap N] [--loops N | --duration-s N]
      [--window WxH] [--offscreen WxH --adapter warp|hardware --audio null --frames N]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => Ok(version()),
        Some("gen-assets") => gen_assets_cmd(&args[1..]),
        Some("run") => run_cmd(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    result.unwrap_or_else(|e| {
        eprintln!("tapestry-spike: {e}");
        ExitCode::from(2)
    })
}

/// `--name value` pairs.
fn options(args: &[String]) -> Result<Vec<(&str, &str)>, String> {
    if !args.len().is_multiple_of(2) {
        return Err(format!("expected --name value pairs\n{USAGE}"));
    }
    args.chunks(2)
        .map(|p| match p[0].strip_prefix("--") {
            Some(name) => Ok((name, p[1].as_str())),
            None => Err(format!("unexpected argument {}\n{USAGE}", p[0])),
        })
        .collect()
}

fn size(v: &str) -> Result<(u32, u32), String> {
    let (w, h) = v.split_once('x').ok_or(format!("size {v}: expected WxH"))?;
    Ok((
        w.parse().map_err(|_| format!("size {v}"))?,
        h.parse().map_err(|_| format!("size {v}"))?,
    ))
}

fn num<T: std::str::FromStr>(name: &str, v: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("--{name} {v}: not a number"))
}

fn gen_assets_cmd(args: &[String]) -> Result<ExitCode, String> {
    let mut ffmpeg = None;
    let mut out = None;
    let mut seconds = 10;
    for (k, v) in options(args)? {
        match k {
            "ffmpeg" => ffmpeg = Some(PathBuf::from(v)),
            "out" => out = Some(PathBuf::from(v)),
            "seconds" => seconds = num(k, v)?,
            _ => return Err(format!("unknown option --{k}\n{USAGE}")),
        }
    }
    gen_assets::run(
        &ffmpeg.ok_or("--ffmpeg is required")?,
        &out.ok_or("--out is required")?,
        seconds,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn run_cmd(args: &[String]) -> Result<ExitCode, String> {
    let mut a = RunArgs {
        offscreen: None,
        window: None,
        adapter: AdapterChoice::Hardware,
        frames: None,
        duration_s: None,
        loops: 4,
        fps_cap: None,
        bga: None,
        out: PathBuf::new(),
    };
    let mut out = None;
    for (k, v) in options(args)? {
        match k {
            "offscreen" => a.offscreen = Some(size(v)?),
            "window" => a.window = Some(size(v)?),
            "adapter" => {
                a.adapter = match v {
                    "warp" => AdapterChoice::Warp,
                    "hardware" => AdapterChoice::Hardware,
                    _ => return Err(format!("--adapter {v}: expected warp or hardware")),
                }
            }
            // Audio arrives in P3; only the null device exists yet.
            "audio" if v == "null" => {}
            "frames" => a.frames = Some(num(k, v)?),
            "duration-s" => a.duration_s = Some(num(k, v)?),
            "loops" => a.loops = num(k, v)?,
            "fps-cap" => a.fps_cap = Some(num(k, v)?),
            "bga" => a.bga = Some(PathBuf::from(v)),
            "out" => out = Some(PathBuf::from(v)),
            _ => return Err(format!("unknown option --{k} {v}\n{USAGE}")),
        }
    }
    a.out = out.ok_or("--out is required")?;
    match a.offscreen {
        Some(sz) => run::offscreen(&a, sz),
        None => window::windowed(&a),
    }
}

fn version() -> ExitCode {
    println!("tapestry-spike {}", env!("CARGO_PKG_VERSION"));
    println!("commit: {} ({})", env!("SPIKE_COMMIT"), env!("SPIKE_TREE"));
    println!("profile: {}", env!("SPIKE_PROFILE"));
    ffmpeg_version()
}

#[cfg(not(feature = "ffmpeg"))]
fn ffmpeg_version() -> ExitCode {
    println!("ffmpeg: not built (no ffmpeg feature)");
    ExitCode::from(1)
}

#[cfg(feature = "ffmpeg")]
fn ffmpeg_version() -> ExitCode {
    use ffmpeg_next::ffi;
    use std::ffi::{CStr, c_char};

    /// A static NUL-terminated string returned by FFmpeg.
    fn ffmpeg_str(p: *const c_char) -> String {
        if p.is_null() {
            return "(null)".into();
        }
        // SAFETY: FFmpeg's version and license getters return pointers to
        // static NUL-terminated strings.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
    let lib_version = |v: u32| format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
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
