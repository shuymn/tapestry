//! The worker's development commands (dev-tools only):
//!
//! - `--dev-tools-probe`: prints the dev-tools marker.
//! - `--dev-gen-clip <out> <codec> <format> <W>x<H> <rate> <frames> <gop>
//!   [gap=<frame>:<periods>] [paint=chroma-step]`: encodes a synthesized
//!   clip through the build's encoder and muxer of those FFmpeg names.
//!   Frame `i` shows `i` in its top row of 16 × 16 blocks, bit `b` of `i`
//!   in block `b` (luma 235 for a one, 16 for a zero), over a pattern that
//!   moves with `i`. With a gap, frame `frame` and every one after it come
//!   `periods` frame periods later: a clip of variable rate. With
//!   `paint=chroma-step`, every frame is instead luma 128 throughout, its Cb
//!   96 left of a column step and 160 from it and its Cr 96 above a row
//!   step and 160 from it, each step at the multiple of 8 at or below half
//!   its chroma plane (chroma column 80 and row 40 at 320 × 180), so it lies
//!   on the encoders' 8 × 8 chroma block edges and no block holds a step to
//!   ring: a step whose drawn place shows where the frame's chroma is sited.
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
//! - `--dev-serve <option>…`: serves the control pipe changed by each
//!   option: `protocol=<n>` names protocol version `n` in the hello;
//!   `deaf[=<n>]` reads no command after the hello's first `n` (none
//!   without) while the control loop still beats; a `<player>` is its
//!   number or its video's codec as its probe names it;
//!   `freeze=<player>:<play|start|decoding|waiting>` freezes that player
//!   for good before it answers its play, or once it has published a frame
//!   while it says it decodes, or while it says it waits for a free slot;
//!   `slow-convert=<ms>` sleeps `ms` before each frame's conversion;
//!   `stall-convert=<n>:<ms>` sleeps `ms` before the process's conversion
//!   number `n`, counted from 0; `unstated` takes every frame's own
//!   duration and every stream's average frame rate as unknown;
//!   `refuse-alloc=<player>` has the worker's
//!   allocator refuse that player's next decode an allocation, which the
//!   decode's check finds as a decoder's refusal; `garbage=<player>` writes
//!   its next frame in a run not started; `balloon=<step>:<total>` commits
//!   memory as `--dev-balloon` does, from a thread the fire starts; and
//!   `fire` holds `freeze`, `refuse-alloc`, `garbage` and `balloon` until
//!   the game's `Fire` command. Each fault answers `Fired` with the
//!   performance counter at its seam as it takes effect.
//! - `--dev-balloon <step> <total> [<last> [free]]`: answers the hello and
//!   serves the control pipe while a thread commits memory the worker never
//!   frees, `step` MiB at a time about 10 ms apart up to `total` MiB and
//!   then `last` MiB at once, which `free` releases at once, logging each
//!   commit, release and a refusal's error code, after which it commits no
//!   more ([`balloon`]).

mod probe;

use std::ffi::OsString;
use std::process::ExitCode;

use crate::ffmpeg::{self, Clip, Input, IoOpen};
use crate::os::Source;

/// The protocol version a `--dev-serve protocol=<n>` names, else none.
static PROTOCOL: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// The commands the reader reads before it reads no more (`--dev-serve
/// deaf[=<n>]`).
static DEAF: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// The protocol version the hello names instead of the worker's own.
pub fn protocol() -> Option<u32> {
    PROTOCOL.get().copied()
}

/// Whether the command reader, having read `read` commands, reads no
/// more.
pub fn deaf(read: u32) -> bool {
    DEAF.get().is_some_and(|n| read >= *n)
}

/// Answers the game that the fault it fired takes effect now: the
/// performance counter read here, at the fault's seam.
pub fn fired() {
    let mut qpc = 0;
    // SAFETY: an out pointer to a local.
    let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&raw mut qpc) };
    crate::say(&format!("tapestry-media: a fault fires at {qpc}"));
    if let Some(out) = crate::OUT.get() {
        out.send(&skin_model::media::control::Reply::Fired { qpc });
    }
}

/// Applies `--dev-serve`'s options.
fn serve_options(options: &[String]) -> Result<(), String> {
    for option in options {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        match key {
            "protocol" => {
                let n = value.parse().map_err(|e| format!("{option}: {e}"))?;
                let _ = PROTOCOL.set(n);
            }
            "deaf" => {
                let n = match value {
                    "" => 0,
                    n => n.parse().map_err(|e| format!("{option}: {e}"))?,
                };
                let _ = DEAF.set(n);
            }
            "freeze" => {
                let (player, at) = value
                    .split_once(':')
                    .ok_or_else(|| format!("{option}: freeze=<player>:<where>"))?;
                let player = crate::player::Target::parse(player);
                let at = match at {
                    "play" => crate::player::Freeze::Play,
                    "start" => crate::player::Freeze::Start,
                    "decoding" => crate::player::Freeze::Decoding,
                    "waiting" => crate::player::Freeze::Waiting,
                    _ => return Err(format!("{option}: play, start, decoding or waiting")),
                };
                crate::player::freeze(player, at);
            }
            "unstated" => ffmpeg::leave_timing_unstated(),
            "slow-convert" => {
                let ms = value.parse().map_err(|e| format!("{option}: {e}"))?;
                ffmpeg::slow_convert(ms);
            }
            "stall-convert" => {
                let (at, ms) = value
                    .split_once(':')
                    .ok_or_else(|| format!("{option}: <conversion>:<ms>"))?;
                let at = at.parse().map_err(|e| format!("{option}: {e}"))?;
                let ms = ms.parse().map_err(|e| format!("{option}: {e}"))?;
                ffmpeg::stall_convert(at, ms);
            }
            "fire" => crate::player::on_fire(),
            "refuse-alloc" => crate::player::refuse_alloc(crate::player::Target::parse(value)),
            "garbage" => crate::player::garbage(crate::player::Target::parse(value)),
            "balloon" => {
                let (step, total) = value
                    .split_once(':')
                    .ok_or_else(|| format!("{option}: balloon=<step>:<total>"))?;
                let step: usize = step.parse().map_err(|e| format!("{option}: {e}"))?;
                let total = total.parse().map_err(|e| format!("{option}: {e}"))?;
                if step == 0 {
                    return Err(format!("{option}: a step of 0 MiB"));
                }
                crate::player::balloon(step, total);
            }
            _ => return Err(format!("--dev-serve: unknown option {option}")),
        }
    }
    Ok(())
}

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
        "--dev-chroma" => chroma(rest),
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
        "--dev-serve" => {
            return Some(match serve_options(rest) {
                Ok(()) => crate::serve(ffmpeg::MAX_ALLOC),
                Err(e) => usage(&e),
            });
        }
        "--dev-balloon" => return Some(balloon(rest)),
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
                        out.send(&skin_model::media::control::Reply::Closed {
                            id: skin_model::media::control::NO_COMMAND,
                            player: 0,
                        });
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

/// `--dev-balloon`: the memory the job's cap and its watcher are tested
/// with. The worker's own limits bound single operations, the pixel cap a
/// frame and the allocation cap each of FFmpeg's allocations, while the
/// job's cap bounds the worker's commit in all (design.md "Threat Model"
/// T02); the tests reach that cap by committing memory directly.
fn balloon(args: &[String]) -> ExitCode {
    let mib = |s: &String| s.parse::<usize>().map_err(|e| format!("{s}: {e}"));
    let parsed = match args {
        [step, total] => mib(step).and_then(|step| Ok((step, mib(total)?, 0))),
        [step, total, last] => mib(step).and_then(|step| Ok((step, mib(total)?, mib(last)?))),
        [step, total, last, free] if free == "free" => {
            mib(step).and_then(|step| Ok((step, mib(total)?, mib(last)?)))
        }
        _ => Err("--dev-balloon <step> <total> [<last> [free]]".to_owned()),
    };
    let free = args.len() == 4;
    let (step, total, last) = match parsed {
        Ok((0, ..)) => return usage("--dev-balloon: a step of 0 MiB"),
        Ok(sizes) => sizes,
        Err(e) => return usage(&e),
    };
    let out = crate::hello(ffmpeg::MAX_ALLOC);
    let inflating = std::thread::Builder::new()
        .name("balloon".to_owned())
        .spawn(move || inflate(step, total, last, free));
    if let Err(e) = inflating {
        crate::say(&format!("balloon: its thread did not start: {e}"));
    }
    crate::serve_on(&out)
}

/// Commits `step` MiB at a time up to `total`, then `last` at once,
/// released at once when `free` says so, stopping at the first refusal.
pub fn inflate(step: usize, total: usize, last: usize, free: bool) {
    use windows::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    let mut committed = 0;
    while committed < total {
        let n = step.min(total - committed);
        if commit(n).is_none() {
            return;
        }
        committed += n;
        crate::say(&format!("balloon: committed {committed} MiB"));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if last > 0
        && let Some(region) = commit(last)
    {
        if free {
            // SAFETY: the region just committed, which nothing else uses.
            let freed = unsafe { VirtualFree(region, 0, MEM_RELEASE) };
            crate::say(&format!(
                "balloon: committed {} MiB and released {last} MiB ({freed:?})",
                committed + last
            ));
        } else {
            committed += last;
            crate::say(&format!("balloon: committed {committed} MiB"));
        }
    }
    crate::say("balloon: done");
}

/// Commits `mib` MiB the worker never touches: the region, or `None` when
/// the commit was refused, the refusal logged with its error code.
fn commit(mib: usize) -> Option<*mut std::ffi::c_void> {
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};

    // SAFETY: a new private region, which nothing reads.
    let region = unsafe { VirtualAlloc(None, mib << 20, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    if region.is_null() {
        // SAFETY: the calling thread's last error, which the refusal set.
        let code = unsafe { GetLastError() }.0;
        crate::say(&format!("balloon: {mib} MiB refused with {code}"));
        return None;
    }
    Some(region)
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
    let usage = "--dev-gen-clip <out> <codec> <format> <W>x<H> <rate> <frames> <gop>                  [gap=<frame>:<periods>] [paint=chroma-step]";
    let (fixed, options) = args.split_at(args.len().min(7));
    let [out, codec, format, size, rate, frames, gop] = fixed else {
        return Err(usage.to_owned());
    };
    let number = |s: &str| s.parse::<i32>().map_err(|e| format!("{s}: {e}"));
    let (mut gap, mut step) = (None, false);
    for option in options {
        match option.split_once('=') {
            Some(("gap", g)) => {
                let (at, periods) = g.split_once(':').ok_or_else(|| usage.to_owned())?;
                let at = at.parse::<u32>().map_err(|e| format!("{at}: {e}"))?;
                let periods = periods
                    .parse::<u32>()
                    .map_err(|e| format!("{periods}: {e}"))?;
                gap = Some((at, periods));
            }
            Some(("paint", "chroma-step")) => step = true,
            _ => return Err(format!("{option}: {usage}")),
        }
    }
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
        gap,
    };
    if clip.width <= 0 || clip.height <= 0 || clip.rate <= 0 || clip.gop <= 0 {
        return Err("sizes, the rate and the GOP are positive".to_owned());
    }
    let (w, h) = (clip.width.unsigned_abs(), clip.height.unsigned_abs());
    ffmpeg::encode_clip(std::path::Path::new(out), &clip, &mut |i, frame| {
        if step {
            paint_chroma_step(w, h, frame);
        } else {
            paint(i, w, h, frame);
        }
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

/// Paints a `w` × `h` frame of `paint=chroma-step` into `frame`, tight
/// I420: luma 128, Cb 96 left of its step and 160 from it, Cr 96 above its
/// step and 160 from it, each step at the multiple of 8 at or below half
/// its chroma plane.
fn paint_chroma_step(w: u32, h: u32, frame: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (column, row) = (cw / 2 / 8 * 8, ch / 2 / 8 * 8);
    let (luma, chroma) = frame.split_at_mut(w * h);
    luma.fill(128);
    let (u, v) = chroma.split_at_mut(cw * ch);
    for (k, p) in u.iter_mut().enumerate() {
        *p = if k % cw < column { 96 } else { 160 };
    }
    for (k, p) in v.iter_mut().enumerate() {
        *p = if k / cw < row { 96 } else { 160 };
    }
}

/// `--dev-chroma <file>`: opens the file as a player opens a video and
/// prints the chroma locations FFmpeg reports before the worker applies any
/// default (design.md "Rendering and Skins", video frames): the decoder's
/// after the open and each of the first frames', with the build's identity.
fn chroma(args: &[String]) -> Result<(), String> {
    let [file] = args else {
        return Err("--dev-chroma <file>".to_owned());
    };
    let opened = std::fs::File::open(file).map_err(|e| format!("{file}: {e}"))?;
    let (mut input, _) = Input::open(Source::opened(opened), IoOpen::Default)
        .map_err(|(why, code)| format!("refused: {why:?} {code}"))?;
    println!(
        "chroma: build {} {}",
        ffmpeg::version().0,
        ffmpeg::library_versions()
    );
    println!("chroma: decoder {}", input.reported_locations().0);
    for k in 0..4 {
        match input.next() {
            Ok(Some(_)) => println!("chroma: frame {k} {}", input.reported_locations().1),
            Ok(None) => break,
            Err((why, code)) => return Err(format!("frame {k}: {why:?} {code}")),
        }
    }
    Ok(())
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
