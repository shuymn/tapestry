//! The worker's development commands (dev-tools only):
//!
//! - `--dev-tools-probe`: prints the dev-tools marker.
//! - `--dev-gen-clip <out> <codec> <format> <W>x<H> <rate> <frames> <gop>`:
//!   encodes a synthesized clip through the build's encoder and muxer of
//!   those FFmpeg names. Frame `i` shows `i` in its top row of 16 × 16
//!   blocks, bit `b` of `i` in block `b` (luma 235 for a one, 16 for a
//!   zero), over a pattern that moves with `i`.
//! - `--dev-secondary-open <file> <refuse|default>`: opens `file` as the
//!   worker opens a video, with its own `io_open` or FFmpeg's, and prints
//!   what a secondary open through the opened file's context gives, as a
//!   playlist or a reference would make it.
//! - `--dev-max-alloc <bytes>`: serves the control pipe with FFmpeg's
//!   largest allocation lowered to `bytes`.
//! - `--dev-no-codec-list`: serves the control pipe with FFmpeg's codec
//!   whitelist left unset at every open, as an allocation refused while it
//!   is set would leave it.
//! - `--dev-refuse-at-park`: serves the control pipe with an allocation
//!   refused for the player each time it parks its decoder at a video's end
//!   (the park seam), as a decoder's thread still at work when its other
//!   thread gave the end would refuse one.
//! - `--dev-probe <probe>…`: answers the hello, then tries what the
//!   sandbox denies the worker and what it leaves it ([`probe`]).
//! - `--dev-silent <ms>`: waits `ms` milliseconds without answering the
//!   game's launch, then exits.
//! - `--dev-deaf`: answers the hello, then never reads its commands.
//! - `--dev-flood <n>`: answers the hello and, at the game's first
//!   command, sends `n` replies the game never asked for, as fast as the
//!   pipe takes them, then waits.

mod probe;

use std::ffi::OsString;
use std::process::ExitCode;

use crate::ffmpeg::{self, Clip, Input, IoOpen};
use crate::os::Source;

/// The marker a dev-tools build prints and holds.
const MARKER: &str = "tapestry dev-tools enabled";

/// Runs the development command `args` names, or `None` when it names
/// none.
pub fn run(args: &[OsString]) -> Option<ExitCode> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let (command, rest) = args.split_first()?;
    let result = match command.as_str() {
        "--dev-tools-probe" => {
            println!("{MARKER}");
            Ok(())
        }
        "--dev-gen-clip" => gen_clip(rest),
        "--dev-secondary-open" => secondary_open(rest),
        "--dev-no-codec-list" => {
            return Some(match rest {
                [] => {
                    ffmpeg::leave_codec_list_unset();
                    crate::serve(ffmpeg::MAX_ALLOC)
                }
                _ => usage("--dev-no-codec-list"),
            });
        }
        "--dev-refuse-at-park" => {
            return Some(match rest {
                [] => {
                    ffmpeg::refuse_while_parked();
                    crate::serve(ffmpeg::MAX_ALLOC)
                }
                _ => usage("--dev-refuse-at-park"),
            });
        }
        "--dev-max-alloc" => {
            return Some(match rest {
                [bytes] => match bytes.parse() {
                    Ok(bytes) => crate::serve(bytes),
                    Err(e) => usage(&format!("--dev-max-alloc {bytes}: {e}")),
                },
                _ => usage("--dev-max-alloc <bytes>"),
            });
        }
        "--dev-probe" => return Some(probe::run(rest)),
        "--dev-deaf" => {
            let _ = crate::hello(ffmpeg::MAX_ALLOC);
            wait_for_ever()
        }
        "--dev-flood" => match rest {
            [n] => n
                .parse::<u64>()
                .map(|n| {
                    let out = crate::hello(ffmpeg::MAX_ALLOC);
                    // After the launch has taken the hello.
                    let _ = crate::read_command(&mut std::io::stdin().lock());
                    for _ in 0..n {
                        out.send(&skin_model::media::control::Reply::Closed { player: 0 });
                    }
                    wait_for_ever()
                })
                .map_err(|e| format!("--dev-flood {n}: {e}")),
            _ => Err("--dev-flood <n>".to_owned()),
        },
        "--dev-silent" => match rest {
            [ms] => ms
                .parse()
                .map(|ms| std::thread::sleep(std::time::Duration::from_millis(ms)))
                .map_err(|e| format!("--dev-silent {ms}: {e}")),
            _ => Err("--dev-silent <ms>".to_owned()),
        },
        _ => return None,
    };
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => usage(&e),
    })
}

/// Waits until the game ends the worker.
fn wait_for_ever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Reports `message` and fails.
fn usage(message: &str) -> ExitCode {
    eprintln!("tapestry-media: {message}");
    ExitCode::FAILURE
}

/// `--dev-gen-clip`.
fn gen_clip(args: &[String]) -> Result<(), String> {
    let [out, codec, format, size, rate, frames, gop] = args else {
        return Err(
            "--dev-gen-clip <out> <codec> <format> <W>x<H> <rate> <frames> <gop>".to_owned(),
        );
    };
    let number = |s: &str| s.parse::<i32>().map_err(|e| format!("{s}: {e}"));
    let (w, h) = size
        .split_once('x')
        .ok_or_else(|| format!("{size}: not <W>x<H>"))?;
    let clip = Clip {
        codec: codec.clone(),
        format: format.clone(),
        width: number(w)?,
        height: number(h)?,
        rate: number(rate)?,
        frames: frames.parse().map_err(|e| format!("{frames}: {e}"))?,
        gop: number(gop)?,
    };
    if clip.width <= 0 || clip.height <= 0 || clip.rate <= 0 || clip.gop <= 0 {
        return Err("sizes, the rate and the GOP are positive".to_owned());
    }
    let (w, h) = (clip.width.unsigned_abs(), clip.height.unsigned_abs());
    ffmpeg::encode_clip(std::path::Path::new(out), &clip, &mut |i, frame| {
        paint(i, w, h, frame);
    })
}

/// Paints frame `i` of a `w` × `h` clip into `frame`, tight I420.
fn paint(i: u32, w: u32, h: u32, frame: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let (luma, chroma) = frame.split_at_mut(w * h);
    for (y, row) in luma.chunks_exact_mut(w).enumerate() {
        for (x, p) in row.iter_mut().enumerate() {
            let block = x / 16;
            *p = if y < 16 && block < 16 && (block + 1) * 16 <= w {
                if i >> block & 1 == 1 { 235 } else { 16 }
            } else {
                let v = (x + 2 * y + 3 * i as usize) % 160;
                u8::try_from(48 + v).unwrap_or(128)
            };
        }
    }
    let (u, v) = chroma.split_at_mut(chroma.len() / 2);
    let cw = w.div_ceil(2);
    for (k, p) in u.iter_mut().enumerate() {
        *p = u8::try_from(112 + (k % cw) % 32).unwrap_or(128);
    }
    for (k, p) in v.iter_mut().enumerate() {
        *p = u8::try_from(112 + (k / cw + i as usize) % 32).unwrap_or(128);
    }
}

/// `--dev-secondary-open`.
fn secondary_open(args: &[String]) -> Result<(), String> {
    let [file, io] = args else {
        return Err("--dev-secondary-open <file> <refuse|default>".to_owned());
    };
    let io = match io.as_str() {
        "refuse" => IoOpen::Refuse,
        "default" => IoOpen::Default,
        other => return Err(format!("{other}: not refuse or default")),
    };
    let file = std::fs::File::open(file).map_err(|e| format!("{file}: {e}"))?;
    let (mut input, _) = Input::open(Source::opened(file), io)
        .map_err(|(why, code)| format!("refused: {why:?} {code}"))?;
    let code = input.secondary_open(c"segment-0.ts");
    println!("secondary open: {code}");
    Ok(())
}
