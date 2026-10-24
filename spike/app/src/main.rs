//! `tapestry-spike`: the stage 0 spike executable (issue #3).
//!
//! ```text
//! tapestry-spike --version
//! tapestry-spike gen-assets --ffmpeg <exe> --out <dir> [--seconds N]
//! tapestry-spike run --out <dir> [--bga <video>] [--fps-cap N]
//!     [--loops N | --duration-s N] [--window WxH]
//!     [--audio wasapi|null] [--audio-period min|next|<frames>]
//!     [--audio-device <part>] [--inject-gap-every-s N --inject-gap-ms M]
//!     [--starve-ms N]
//!     [--offscreen WxH --adapter warp --audio null --frames N]
//! tapestry-spike audio-devices
//! ```
//!
//! `--version` reports the build and the linked FFmpeg with the license its
//! libraries report, and exits 1 unless both avcodec and avutil are LGPL.

mod alloc;
mod audio;
mod bga;
mod conditions;
mod display;
mod gen_assets;
mod measure;
mod obs;
mod pacing;
mod render;
mod report;
mod run;
mod rundata;
mod scene;
mod session;
mod threads;
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
  tapestry-spike displays
  tapestry-spike audio-devices
  tapestry-spike run --out <dir> [--bga <video>] [--fps-cap N] [--loops N | --duration-s N]
      [--window WxH] [--monitor <part>] [--adapter warp|hardware|low-power | --adapter-name <part>]
      [--audio wasapi|null] [--audio-period min|next|<frames>] [--audio-queue <periods>]
      [--audio-device <part>]
      [--inject-gap-every-s N --inject-gap-ms M] [--starve-ms N]
      [--profile standard|streaming|no-bga] [--countdown-s N]
      [--offscreen WxH --audio null --frames N]
  tapestry-spike measure --presentmon <exe> [--obs <obs64.exe> --obs-profile <name>
      --obs-collection <name> --obs-encoder hardware|x264] [--dry-run] <run options>
  tapestry-spike conditions [--out <dir>] [--audio-device <part>] [--bga <video>]
  tapestry-spike report <run-dir> [--notes <owner-notes.toml>]
  tapestry-spike report --from-export <zip>
  tapestry-spike obs-files --out <dir> [--overlay <html>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--version") if args.len() == 1 => Ok(version()),
        Some("gen-assets") => gen_assets_cmd(&args[1..]),
        Some("run") => run_cmd(&args[1..]),
        Some("measure") => measure::run(&args[1..]),
        Some("conditions") => conditions_cmd(&args[1..]),
        Some("report") => report_cmd(&args[1..]),
        Some("obs-files") => obs_files_cmd(&args[1..]),
        Some("displays") if args.len() == 1 => {
            display::print_displays();
            Ok(ExitCode::SUCCESS)
        }
        Some("audio-devices") if args.len() == 1 => {
            audio::print_endpoints().map(|()| ExitCode::SUCCESS)
        }
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
    let a = parse_run(args)?;
    match a.offscreen {
        Some(sz) => run::offscreen(&a, sz),
        None => window::windowed(&a),
    }
}

/// The options of `run` (also the run part of `measure`).
pub fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut a = RunArgs {
        offscreen: None,
        window: None,
        adapter: AdapterChoice::Hardware,
        frames: None,
        duration_s: None,
        loops: 4,
        fps_cap: None,
        bga: None,
        monitor: None,
        audio: None,
        countdown_us: run::COUNTDOWN_US,
        meta: rundata::RunMeta {
            profile: spike_core::run::Profile::Standard,
            kind: spike_core::run::RunKind::Diagnostic,
            obs_encoder: None,
        },
        out: PathBuf::new(),
    };
    let mut out = None;
    let mut audio_on = true;
    let mut audio = audio::AudioArgs {
        period: audio::PeriodChoice::Min,
        device: None,
        inject: None,
        starve_ms: None,
        queue_periods: 2,
    };
    let (mut every, mut gap) = (None, None);
    for (k, v) in options(args)? {
        match k {
            "offscreen" => a.offscreen = Some(size(v)?),
            "window" => a.window = Some(size(v)?),
            "adapter" => {
                a.adapter = match v {
                    "warp" => AdapterChoice::Warp,
                    "hardware" => AdapterChoice::Hardware,
                    "low-power" => AdapterChoice::LowPower,
                    _ => {
                        return Err(format!(
                            "--adapter {v}: expected warp, hardware or low-power"
                        ));
                    }
                }
            }
            "adapter-name" => a.adapter = AdapterChoice::Name(v.to_owned()),
            "audio" => {
                audio_on = match v {
                    "wasapi" => true,
                    "null" => false,
                    _ => return Err(format!("--audio {v}: expected wasapi or null")),
                }
            }
            "audio-period" => {
                audio.period = match v {
                    "min" => audio::PeriodChoice::Min,
                    "next" => audio::PeriodChoice::Next,
                    n => audio::PeriodChoice::Frames(num(k, n)?),
                }
            }
            "audio-device" => audio.device = Some(v.to_owned()),
            "inject-gap-every-s" => every = Some(num(k, v)?),
            "inject-gap-ms" => gap = Some(num(k, v)?),
            "starve-ms" => audio.starve_ms = Some(num(k, v)?),
            "audio-queue" => audio.queue_periods = num(k, v)?,
            "frames" => a.frames = Some(num(k, v)?),
            "duration-s" => a.duration_s = Some(num(k, v)?),
            "loops" => a.loops = num(k, v)?,
            "fps-cap" => a.fps_cap = Some(num(k, v)?),
            "bga" => a.bga = Some(PathBuf::from(v)),
            "monitor" => a.monitor = Some(v.to_owned()),
            "countdown-s" => a.countdown_us = num::<i64>(k, v)? * 1_000_000,
            "profile" => {
                a.meta.profile = match v {
                    "standard" => spike_core::run::Profile::Standard,
                    "streaming" => spike_core::run::Profile::Streaming,
                    "no-bga" => spike_core::run::Profile::NoBga,
                    _ => {
                        return Err(format!(
                            "--profile {v}: expected standard, streaming or no-bga"
                        ));
                    }
                }
            }
            "out" => out = Some(PathBuf::from(v)),
            _ => return Err(format!("unknown option --{k} {v}\n{USAGE}")),
        }
    }
    a.out = out.ok_or("--out is required")?;
    audio.inject = match (every, gap) {
        (Some(e), Some(g)) if e > 0 && g > 0 => Some((e, g)),
        (None, None) => None,
        _ => return Err("--inject-gap-every-s and --inject-gap-ms go together (both > 0)".into()),
    };
    if audio_on {
        if a.offscreen.is_some() {
            return Err("--offscreen runs need --audio null".into());
        }
        a.audio = Some(audio);
    }
    Ok(a)
}

/// `conditions [--out DIR] [--audio-device PART] [--bga FILE]`.
fn conditions_cmd(args: &[String]) -> Result<ExitCode, String> {
    let (mut out, mut device, mut bga) = (PathBuf::from("."), None, None);
    for (k, v) in options(args)? {
        match k {
            "out" => out = PathBuf::from(v),
            "audio-device" => device = Some(v.to_owned()),
            "bga" => bga = Some(PathBuf::from(v)),
            _ => return Err(format!("unknown option --{k}\n{USAGE}")),
        }
    }
    conditions::run(&out, device.as_deref(), bga.as_deref())?;
    Ok(ExitCode::SUCCESS)
}

/// `obs-files --out DIR [--overlay FILE]`.
fn obs_files_cmd(args: &[String]) -> Result<ExitCode, String> {
    let (mut out, mut overlay) = (None, PathBuf::from("obs/overlay.html"));
    for (k, v) in options(args)? {
        match k {
            "out" => out = Some(PathBuf::from(v)),
            "overlay" => overlay = PathBuf::from(v),
            _ => return Err(format!("unknown option --{k}\n{USAGE}")),
        }
    }
    obs::write(&out.ok_or("--out is required")?, &overlay)?;
    Ok(ExitCode::SUCCESS)
}

/// `report <run-dir> [--notes FILE]` or `report --from-export <zip>`.
fn report_cmd(args: &[String]) -> Result<ExitCode, String> {
    match args {
        [flag, zip] if flag == "--from-export" => report::from_export(&PathBuf::from(zip)),
        [dir, rest @ ..] => {
            let mut notes = PathBuf::from("owner-notes.toml");
            for (k, v) in options(rest)? {
                match k {
                    "notes" => notes = PathBuf::from(v),
                    _ => return Err(format!("unknown option --{k}\n{USAGE}")),
                }
            }
            report::run(&PathBuf::from(dir), &notes)
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn version() -> ExitCode {
    println!("tapestry-spike {}", env!("CARGO_PKG_VERSION"));
    println!("commit: {} ({})", env!("SPIKE_COMMIT"), env!("SPIKE_TREE"));
    println!("profile: {}", env!("SPIKE_PROFILE"));
    ffmpeg_version()
}

/// The linked FFmpeg's version string (conditions).
#[cfg(feature = "ffmpeg")]
pub fn ffmpeg_version_string() -> String {
    // SAFETY: returns a static NUL-terminated string.
    let p = unsafe { ffmpeg_next::ffi::av_version_info() };
    if p.is_null() {
        return "unknown".into();
    }
    // SAFETY: as above.
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(feature = "ffmpeg"))]
pub fn ffmpeg_version_string() -> String {
    "not built".into()
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
