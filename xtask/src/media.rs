//! `cargo xtask media fixtures [--record]`: the committed video clips the
//! media worker's tests decode (docs/setup.md "FFmpeg", "Media fixtures"),
//! made from synthesized frames in the flake's `ffmpeg` shell with
//! nixpkgs' FFmpeg (libx264, zlib) and mkvtoolnix, which tapestry's own
//! build cannot replace: it has no H.264 encoder and no filters.
//!
//! - H.264 with B-frames at 2 560×1 080 and 1 920×1 080, the largest chart
//!   video and skin movie sizes, for the worker's memory bound.
//! - Two MPEG-4 Part 2 clips whose demuxing needs zlib, with the plain
//!   files they are made from (their decoded frames are the reference): a
//!   MOV whose movie header is rewritten into a zlib `cmov` atom here
//!   ([`cmov`]), and a Matroska file whose frames mkvmerge stores
//!   zlib-compressed.
//! - The colour contract's clips (design.md "Rendering and Skins", video
//!   frames): frames of one known colour, lossless H.264 so they decode to
//!   exactly the values made, tagged with a matrix that differs from the
//!   one their size suggests, untagged, full range, an MJPEG in a JPEG
//!   format, and two whose second half changes its range or its matrix
//!   alone (two encodes joined in one stream, [`Make::Joined`]).
//!
//! The clips are made into `target/media-fixtures/` and checked: each has
//! its codec, size and frame count, the H.264 clips other than the
//! lossless ones have B-frames, each frame has the colour tags its clip
//! names, the zlib clips use zlib and decode to the frames of their plain
//! sources. Without
//! `--record` each clip's sha256 must then equal both `fixtures.toml`'s and
//! the committed file's; `--record` copies them into the fixture directory
//! and rewrites `fixtures.toml` with the commands, the tools' versions and
//! the hashes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context as _, bail, ensure};

use crate::ffmpeg::sha256;

/// Where the clips are committed, relative to the repository root.
pub const FIXTURE_DIR: &str = "crates/player/tests/media";

/// Options every FFmpeg run gets, so a rerun with the same tools writes the
/// same bytes: no encoder or version tags, no metadata, one thread.
const BITEXACT: [&str; 8] = [
    "-fflags",
    "+bitexact",
    "-flags:v",
    "+bitexact",
    "-map_metadata",
    "-1",
    "-threads",
    "1",
];

/// One step that makes a clip.
enum Make {
    /// `ffmpeg` with these input and codec arguments, then [`BITEXACT`].
    Ffmpeg(&'static [&'static str]),
    /// The MOV at this path with its movie header rewritten by [`cmov`].
    Cmov(&'static str),
    /// `mkvmerge` storing the frames of the Matroska file at this path
    /// zlib-compressed.
    ZlibMkv(&'static str),
    /// Two `ffmpeg` encodes to raw H.264 (each with these input and codec
    /// arguments, then [`BITEXACT`]), joined into one stream and copied
    /// into the clip's container at 10 frames a second: the second part's
    /// parameter sets come in the stream, mid-way.
    Joined([&'static [&'static str]; 2]),
}

/// What a made clip must be.
struct Expect {
    codec: &'static str,
    width: u32,
    height: u32,
    frames: u32,
    /// The clip whose decoded frames this one's must equal.
    same_frames_as: Option<&'static str>,
    /// Whether it has B-frames (checked for H.264).
    b_frames: bool,
    /// Each frame's colour range and matrix as FFmpeg's decoder gives them
    /// (`range/matrix`, as ffprobe names them), runs of one counted once;
    /// empty when not checked.
    colours: &'static [&'static str],
    /// The Y, U and V every decoded sample has, when checked.
    solid: Option<[u8; 3]>,
}

struct Clip {
    path: &'static str,
    make: Make,
    expect: Expect,
}

const MPEG4_320: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=320x240:rate=30:duration=4",
    "-c:v",
    "mpeg4",
    "-g",
    "15",
];

/// Two seconds of synthesized frames in H.264 (High profile, 3 B-frames),
/// one key frame a second.
macro_rules! h264 {
    ($size:literal) => {
        &[
            "-f",
            "lavfi",
            "-i",
            concat!("testsrc2=size=", $size, ":rate=30:duration=2"),
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "34",
            "-bf",
            "3",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
        ]
    };
}

/// Frames of one colour, `$yuv` as `lutyuv` sets it, `$size`, 0.2 s at 10
/// frames a second, in lossless H.264 with the tags `$tag`s give. The
/// frames are tagged as the stream (`$params`, `setparams`'s) before they
/// reach the encoder, so FFmpeg converts nothing on the way: they decode to
/// exactly `$yuv`.
macro_rules! solid {
    ($size:literal, $yuv:literal, $params:literal $(, $tag:literal)*) => {
        &[
            "-f",
            "lavfi",
            "-i",
            concat!(
                "color=c=black:s=",
                $size,
                ":r=10:d=0.2,format=yuv420p,lutyuv=",
                $yuv,
                $params
            ),
            "-c:v",
            "libx264",
            "-qp",
            "0",
            "-pix_fmt",
            "yuv420p"
            $(, $tag)*
        ]
    };
}

/// What a colour clip is: H.264 of `frames` frames without B-frames, of
/// `colours`, decoding to `solid`.
macro_rules! colour {
    ($w:literal, $h:literal, $frames:literal, $colours:expr, $solid:expr) => {
        Expect {
            codec: "h264",
            width: $w,
            height: $h,
            frames: $frames,
            same_frames_as: None,
            b_frames: false,
            colours: $colours,
            solid: Some($solid),
        }
    };
}

const CLIPS: [Clip; 15] = [
    Clip {
        path: "zlib/plain.mov",
        make: Make::Ffmpeg(MPEG4_320),
        expect: Expect {
            codec: "mpeg4",
            width: 320,
            height: 240,
            frames: 120,
            same_frames_as: None,
            b_frames: false,
            // Nothing signals its range: the colour contract's unspecified
            // range.
            colours: &["unknown/unknown"],
            solid: None,
        },
    },
    Clip {
        path: "zlib/plain.mkv",
        make: Make::Ffmpeg(MPEG4_320),
        expect: Expect {
            codec: "mpeg4",
            width: 320,
            height: 240,
            frames: 120,
            same_frames_as: None,
            b_frames: false,
            colours: &[],
            solid: None,
        },
    },
    Clip {
        path: "zlib/mpeg4-cmov.mov",
        make: Make::Cmov("zlib/plain.mov"),
        expect: Expect {
            codec: "mpeg4",
            width: 320,
            height: 240,
            frames: 120,
            same_frames_as: Some("zlib/plain.mov"),
            b_frames: false,
            colours: &[],
            solid: None,
        },
    },
    Clip {
        path: "zlib/mpeg4-zlib.mkv",
        make: Make::ZlibMkv("zlib/plain.mkv"),
        expect: Expect {
            codec: "mpeg4",
            width: 320,
            height: 240,
            frames: 120,
            same_frames_as: Some("zlib/plain.mkv"),
            b_frames: false,
            colours: &[],
            solid: None,
        },
    },
    Clip {
        path: "h264/h264-2560x1080.mp4",
        make: Make::Ffmpeg(h264!("2560x1080")),
        expect: Expect {
            codec: "h264",
            width: 2560,
            height: 1080,
            frames: 60,
            same_frames_as: None,
            b_frames: true,
            colours: &[],
            solid: None,
        },
    },
    Clip {
        path: "h264/h264-1920x1080.mp4",
        make: Make::Ffmpeg(h264!("1920x1080")),
        expect: Expect {
            codec: "h264",
            width: 1920,
            height: 1080,
            frames: 60,
            same_frames_as: None,
            b_frames: true,
            colours: &[],
            solid: None,
        },
    },
    // A matrix tag that differs from the size's guess, both ways.
    Clip {
        path: "colour/bt709-640x480.mkv",
        make: Make::Ffmpeg(solid!(
            "640x480",
            "y=100:u=90:v=190",
            ",setparams=range=tv:colorspace=bt709:color_primaries=bt709:color_trc=bt709",
            "-color_range",
            "tv",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709"
        )),
        expect: colour!(640, 480, 2, &["tv/bt709"], [100, 90, 190]),
    },
    Clip {
        path: "colour/bt601-1920x1080.mkv",
        make: Make::Ffmpeg(solid!(
            "1920x1080",
            "y=100:u=90:v=190",
            ",setparams=range=tv:colorspace=smpte170m:color_primaries=smpte170m:color_trc=smpte170m",
            "-color_range",
            "tv",
            "-colorspace",
            "smpte170m",
            "-color_primaries",
            "smpte170m",
            "-color_trc",
            "smpte170m"
        )),
        expect: colour!(1920, 1080, 2, &["tv/smpte170m"], [100, 90, 190]),
    },
    // Untagged: the size's guess, limited range.
    Clip {
        path: "colour/untagged-1280x720.mkv",
        make: Make::Ffmpeg(solid!("1280x720", "y=100:u=90:v=190", "")),
        expect: colour!(1280, 720, 2, &["tv/unknown"], [100, 90, 190]),
    },
    Clip {
        path: "colour/untagged-640x480.mkv",
        make: Make::Ffmpeg(solid!("640x480", "y=100:u=90:v=190", "")),
        expect: colour!(640, 480, 2, &["tv/unknown"], [100, 90, 190]),
    },
    // Full range in I420 (not a JPEG format), white.
    Clip {
        path: "colour/full-320x240.mkv",
        make: Make::Ffmpeg(solid!(
            "320x240",
            "y=255:u=128:v=128",
            ",setparams=range=pc:colorspace=smpte170m",
            "-color_range",
            "pc",
            "-colorspace",
            "smpte170m"
        )),
        expect: colour!(320, 240, 2, &["pc/smpte170m"], [255, 128, 128]),
    },
    // A JPEG format: full range though nothing tags it.
    Clip {
        path: "colour/mjpeg-320x240.avi",
        make: Make::Ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x240:r=10:d=0.2,format=yuvj420p,lutyuv=y=100:u=90:v=190",
            "-c:v",
            "mjpeg",
            "-q:v",
            "2",
            "-pix_fmt",
            "yuvj420p",
        ]),
        expect: Expect {
            codec: "mjpeg",
            width: 320,
            height: 240,
            frames: 2,
            same_frames_as: None,
            b_frames: false,
            colours: &[],
            solid: None,
        },
    },
    // The range alone changes mid-stream, then the matrix alone.
    Clip {
        path: "colour/range-change-128x96.mkv",
        make: Make::Joined([
            solid!(
                "128x96",
                "y=100:u=90:v=190",
                ",setparams=range=tv:colorspace=smpte170m",
                "-color_range",
                "tv",
                "-colorspace",
                "smpte170m"
            ),
            solid!(
                "128x96",
                "y=100:u=90:v=190",
                ",setparams=range=pc:colorspace=smpte170m",
                "-color_range",
                "pc",
                "-colorspace",
                "smpte170m"
            ),
        ]),
        expect: colour!(
            128,
            96,
            4,
            &["tv/smpte170m", "pc/smpte170m"],
            [100, 90, 190]
        ),
    },
    Clip {
        path: "colour/matrix-change-128x96.mkv",
        make: Make::Joined([
            solid!(
                "128x96",
                "y=100:u=90:v=190",
                ",setparams=range=tv:colorspace=bt709",
                "-color_range",
                "tv",
                "-colorspace",
                "bt709"
            ),
            solid!(
                "128x96",
                "y=100:u=90:v=190",
                ",setparams=range=tv:colorspace=smpte170m",
                "-color_range",
                "tv",
                "-colorspace",
                "smpte170m"
            ),
        ]),
        expect: colour!(128, 96, 4, &["tv/bt709", "tv/smpte170m"], [100, 90, 190]),
    },
    // A matrix change whose first colour is outside the R'G'B' cube: its
    // conversion keeps the negative component until it quantizes.
    Clip {
        path: "colour/gamut-change-128x96.mkv",
        make: Make::Joined([
            solid!(
                "128x96",
                "y=50:u=64:v=176",
                ",setparams=range=tv:colorspace=bt709",
                "-color_range",
                "tv",
                "-colorspace",
                "bt709"
            ),
            solid!(
                "128x96",
                "y=50:u=64:v=176",
                ",setparams=range=tv:colorspace=smpte170m",
                "-color_range",
                "tv",
                "-colorspace",
                "smpte170m"
            ),
        ]),
        expect: colour!(128, 96, 4, &["tv/bt709", "tv/smpte170m"], [50, 64, 176]),
    },
];

pub fn run(root: &Path, args: &[OsString]) -> ExitCode {
    let result = match args.iter().map(|a| a.to_str()).collect::<Vec<_>>()[..] {
        [Some("fixtures")] => fixtures(root, false),
        [Some("fixtures"), Some("--record")] => fixtures(root, true),
        _ => Err(anyhow::anyhow!(
            "usage: cargo xtask media fixtures [--record]"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("media: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn tool_version(program: &str, arg: &str) -> anyhow::Result<String> {
    let out = Command::new(program)
        .arg(arg)
        .output()
        .with_context(|| format!("cannot run {program}: run in the flake's `ffmpeg` shell"))?;
    ensure!(out.status.success(), "{program} {arg} failed");
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or_default().trim().to_owned())
}

fn run_tool(program: &str, args: &[&str], dir: &Path) -> anyhow::Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    // mkvmerge exits 1 when it finished with warnings.
    let ok = out.status.success() || (program == "mkvmerge" && out.status.code() == Some(1));
    ensure!(
        ok,
        "{program} {} failed ({}):\n{}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The command line a clip is made with, as `fixtures.toml` records it.
fn command(clip: &Clip) -> String {
    match clip.make {
        Make::Ffmpeg(args) => {
            let mut parts = vec!["ffmpeg"];
            parts.extend(args);
            parts.extend(BITEXACT);
            parts.push(clip.path);
            parts.join(" ")
        }
        Make::Cmov(src) => format!(
            "{}: the cmov rewrite of {src} (cargo xtask media fixtures, zlib level 9)",
            clip.path
        ),
        Make::ZlibMkv(src) => {
            format!(
                "mkvmerge -o {} --deterministic 1 --compression 0:zlib {src}",
                clip.path
            )
        }
        Make::Joined(parts) => {
            let mut steps: Vec<String> = parts
                .iter()
                .enumerate()
                .map(|(i, args)| {
                    let mut p = vec!["ffmpeg"];
                    p.extend(*args);
                    p.extend(BITEXACT);
                    p.extend(["-f", "h264"]);
                    format!("{} {}.{i}.h264", p.join(" "), clip.path)
                })
                .collect();
            steps.push(format!("cat {0}.0.h264 {0}.1.h264 > {0}.h264", clip.path));
            let mut p = vec!["ffmpeg", "-f", "h264", "-framerate", "10", "-i"];
            let joined = format!("{}.h264", clip.path);
            p.push(&joined);
            p.extend(["-c", "copy"]);
            p.extend(BITEXACT);
            p.push(clip.path);
            steps.push(p.join(" "));
            steps.join("; ")
        }
    }
}

fn make(clip: &Clip, dir: &Path) -> anyhow::Result<()> {
    let out = dir.join(clip.path);
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    match clip.make {
        Make::Ffmpeg(args) => {
            let mut all = vec!["-hide_banner", "-v", "error", "-y"];
            all.extend(args);
            all.extend(BITEXACT);
            all.push(clip.path);
            run_tool("ffmpeg", &all, dir)?;
        }
        Make::Cmov(src) => {
            let bytes = fs::read(dir.join(src))?;
            fs::write(
                &out,
                cmov(&bytes).map_err(|e| anyhow::anyhow!("{src}: {e}"))?,
            )?;
        }
        Make::Joined(parts) => {
            let mut joined = Vec::new();
            for (i, args) in parts.iter().enumerate() {
                let part = format!("{}.{i}.h264", clip.path);
                let mut all = vec!["-hide_banner", "-v", "error", "-y"];
                all.extend(*args);
                all.extend(BITEXACT);
                all.extend(["-f", "h264", &part]);
                run_tool("ffmpeg", &all, dir)?;
                joined.extend(fs::read(dir.join(&part))?);
            }
            let stream = format!("{}.h264", clip.path);
            fs::write(dir.join(&stream), joined)?;
            let mut all = vec![
                "-hide_banner",
                "-v",
                "error",
                "-y",
                "-f",
                "h264",
                "-framerate",
                "10",
                "-i",
                &stream,
                "-c",
                "copy",
            ];
            all.extend(BITEXACT);
            all.push(clip.path);
            run_tool("ffmpeg", &all, dir)?;
        }
        Make::ZlibMkv(src) => {
            run_tool(
                "mkvmerge",
                &[
                    "-q",
                    "-o",
                    clip.path,
                    "--deterministic",
                    "1",
                    "--compression",
                    "0:zlib",
                    src,
                ],
                dir,
            )?;
        }
    }
    Ok(())
}

/// Checks a made clip against [`Expect`], and that a zlib clip uses zlib.
fn check(clip: &Clip, dir: &Path) -> anyhow::Result<()> {
    let e = &clip.expect;
    let probe = run_tool(
        "ffprobe",
        &[
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames,has_b_frames",
            "-of",
            "default=noprint_wrappers=1",
            clip.path,
        ],
        dir,
    )?;
    let field = |key: &str| {
        probe
            .lines()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            .unwrap_or_default()
            .to_owned()
    };
    let got = (
        field("codec_name"),
        field("width"),
        field("height"),
        field("nb_read_frames"),
    );
    let want = (
        e.codec.to_owned(),
        e.width.to_string(),
        e.height.to_string(),
        e.frames.to_string(),
    );
    ensure!(
        got == want,
        "{}: codec, size and frames {got:?}, want {want:?}",
        clip.path
    );
    if e.codec == "h264" {
        let delay: u32 = field("has_b_frames").parse().unwrap_or(0);
        ensure!(
            (delay > 0) == e.b_frames,
            "{}: has_b_frames {delay}, B-frames wanted: {}",
            clip.path,
            e.b_frames
        );
    }
    if !e.colours.is_empty() {
        let frames = run_tool(
            "ffprobe",
            &[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=color_range,color_space",
                "-of",
                "csv=p=0",
                clip.path,
            ],
            dir,
        )?;
        let mut colours: Vec<String> = frames
            .lines()
            .map(|l| {
                // A frame with side data ends its line with an empty field.
                l.trim()
                    .split(',')
                    .filter(|f| !f.is_empty())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|l| !l.is_empty())
            .collect();
        colours.dedup();
        ensure!(
            colours == e.colours,
            "{}: frame colours {colours:?}, want {:?}",
            clip.path,
            e.colours
        );
    }
    match clip.make {
        Make::Cmov(_) => {
            let bytes = fs::read(dir.join(clip.path))?;
            ensure!(
                top_level_atoms(&bytes)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .last()
                    .is_some_and(|a| a.kind == *b"moov"
                        && bytes.get(a.offset + 12..a.offset + 16) == Some(b"cmov")),
                "{}: its moov atom holds no cmov atom",
                clip.path
            );
        }
        Make::ZlibMkv(_) => {
            let info = run_tool("mkvinfo", &[clip.path], dir)?;
            ensure!(
                info.contains("Content compression"),
                "{}: mkvinfo shows no content compression",
                clip.path
            );
        }
        Make::Ffmpeg(_) | Make::Joined(_) => {}
    }
    if let Some(yuv) = e.solid {
        // The frames as decoded, in their own format: no conversion, not
        // even where a frame's colour tags change mid-stream (which would
        // otherwise rebuild the filters into one).
        let raw = Command::new("ffmpeg")
            .args(["-hide_banner", "-v", "error", "-reinit_filter", "0"])
            .args(["-i", clip.path])
            .args(["-map", "0:v:0", "-c:v", "rawvideo", "-f", "rawvideo", "-"])
            .current_dir(dir)
            .output()
            .context("cannot run ffmpeg")?;
        ensure!(raw.status.success(), "{}: decoding failed", clip.path);
        let (w, h) = (e.width as usize, e.height as usize);
        let luma = w * h;
        let chroma = w.div_ceil(2) * h.div_ceil(2);
        let frame = luma + 2 * chroma;
        ensure!(
            raw.stdout.len() == frame * e.frames as usize,
            "{}: {} bytes decoded, not {} frames of {frame}",
            clip.path,
            raw.stdout.len(),
            e.frames
        );
        for (i, f) in raw.stdout.chunks(frame).enumerate() {
            let planes = [&f[..luma], &f[luma..luma + chroma], &f[luma + chroma..]];
            for (plane, want) in planes.into_iter().zip(yuv) {
                ensure!(
                    plane.iter().all(|b| *b == want),
                    "{}: frame {i} decodes to {}, not {want}",
                    clip.path,
                    plane[0]
                );
            }
        }
    }
    if let Some(reference) = e.same_frames_as {
        let frames = |path: &str| -> anyhow::Result<Vec<String>> {
            let out = run_tool(
                "ffmpeg",
                &[
                    "-hide_banner",
                    "-v",
                    "error",
                    "-i",
                    path,
                    "-map",
                    "0:v:0",
                    "-f",
                    "framemd5",
                    "-",
                ],
                dir,
            )?;
            Ok(out
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| l.rsplit(',').next().map(|h| h.trim().to_owned()))
                .collect())
        };
        let (a, b) = (frames(clip.path)?, frames(reference)?);
        ensure!(
            a.len() == e.frames as usize && a == b,
            "{}: its {} decoded frames differ from {reference}'s {}",
            clip.path,
            a.len(),
            b.len()
        );
    }
    Ok(())
}

fn fixtures(root: &Path, record: bool) -> anyhow::Result<()> {
    let ffmpeg = tool_version("ffmpeg", "-version")?;
    let mkvmerge = tool_version("mkvmerge", "--version")?;
    let config = run_tool("ffmpeg", &["-hide_banner", "-buildconf"], root)?;
    ensure!(
        config.contains("--enable-libx264") && config.contains("--enable-zlib"),
        "this ffmpeg lacks libx264 or zlib: run in the flake's `ffmpeg` shell"
    );
    let made = root.join("target").join("media-fixtures");
    if made.exists() {
        fs::remove_dir_all(&made)?;
    }
    fs::create_dir_all(&made)?;
    let mut hashes = BTreeMap::new();
    for clip in &CLIPS {
        make(clip, &made)?;
        check(clip, &made)?;
        let hash = sha256(&fs::read(made.join(clip.path))?);
        println!("{hash}  {}", clip.path);
        hashes.insert(clip.path, hash);
    }
    let fixture_dir = root.join(FIXTURE_DIR);
    let manifest = fixture_dir.join("fixtures.toml");
    if record {
        for clip in &CLIPS {
            let to = fixture_dir.join(clip.path);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(made.join(clip.path), &to)?;
        }
        fs::write(&manifest, manifest_text(&ffmpeg, &mkvmerge, &hashes))?;
        println!(
            "media: {} clips recorded in {}",
            CLIPS.len(),
            fixture_dir.display()
        );
        return Ok(());
    }
    let pinned = pinned_hashes(
        &fs::read_to_string(&manifest)
            .with_context(|| format!("cannot read {}", manifest.display()))?,
    )?;
    let mut diff = Vec::new();
    for (path, hash) in &hashes {
        match pinned.get(*path) {
            Some(want) if want == hash => {}
            Some(want) => diff.push(format!("{path}: made {hash}, fixtures.toml {want}")),
            None => diff.push(format!("{path}: made {hash}, not in fixtures.toml")),
        }
        let committed = fixture_dir.join(path);
        match fs::read(&committed) {
            Ok(bytes) if sha256(&bytes) == *hash => {}
            Ok(bytes) => diff.push(format!(
                "{path}: made {hash}, committed file {}",
                sha256(&bytes)
            )),
            Err(_) => diff.push(format!("{path}: not committed")),
        }
    }
    for path in pinned.keys() {
        if !hashes.contains_key(path.as_str()) {
            diff.push(format!("{path}: in fixtures.toml, not made"));
        }
    }
    ensure!(
        diff.is_empty(),
        "the clips differ from the committed ones (tools here: {ffmpeg}; {mkvmerge}):\n  {}",
        diff.join("\n  ")
    );
    println!("media: every clip matches fixtures.toml and the committed file");
    Ok(())
}

fn manifest_text(ffmpeg: &str, mkvmerge: &str, hashes: &BTreeMap<&str, String>) -> String {
    let mut s = String::from(
        "# Written by `cargo xtask media fixtures --record` in the flake's `ffmpeg`\n\
         # shell (docs/setup.md \"Media fixtures\"); `cargo xtask media fixtures`\n\
         # remakes the clips and checks them against these hashes. Self-made from\n\
         # FFmpeg's synthesized test frames.\n",
    );
    let _ = writeln!(s, "ffmpeg = {}", toml::Value::from(ffmpeg));
    let _ = writeln!(s, "mkvmerge = {}", toml::Value::from(mkvmerge));
    s.push_str("\n[commands]\n");
    for clip in &CLIPS {
        let _ = writeln!(
            s,
            "\"{}\" = {}",
            clip.path,
            toml::Value::from(command(clip))
        );
    }
    s.push_str("\n[files]\n");
    for (path, hash) in hashes {
        let _ = writeln!(s, "\"{path}\" = \"{hash}\"");
    }
    s
}

fn pinned_hashes(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let t: toml::Table = toml::from_str(text).context("fixtures.toml is not TOML")?;
    let Some(files) = t.get("files").and_then(toml::Value::as_table) else {
        bail!("fixtures.toml has no [files]");
    };
    files
        .iter()
        .map(|(k, v)| {
            Ok((
                k.clone(),
                v.as_str().with_context(|| format!("files.{k}"))?.to_owned(),
            ))
        })
        .collect()
}

/// A top-level atom of a QuickTime file.
#[derive(Debug, PartialEq, Eq)]
struct Atom {
    offset: usize,
    header: usize,
    size: usize,
    kind: [u8; 4],
}

fn top_level_atoms(data: &[u8]) -> Result<Vec<Atom>, String> {
    let mut atoms = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let head = data
            .get(offset..offset + 8)
            .ok_or_else(|| format!("truncated atom header at {offset}"))?;
        let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let kind = [head[4], head[5], head[6], head[7]];
        let mut header = 8;
        if size == 1 {
            let large = data
                .get(offset + 8..offset + 16)
                .ok_or_else(|| format!("truncated 64-bit size at {offset}"))?;
            size = u64::from_be_bytes(large.try_into().map_err(|_| "64-bit size")?);
            header = 16;
        } else if size == 0 {
            size = (data.len() - offset) as u64;
        }
        let size = usize::try_from(size).map_err(|_| format!("atom size {size} at {offset}"))?;
        if size < header || offset + size > data.len() {
            return Err(format!("bad size {size} at {offset}"));
        }
        atoms.push(Atom {
            offset,
            header,
            size,
            kind,
        });
        offset += size;
    }
    Ok(atoms)
}

/// Rewrites a MOV's movie header into a zlib `cmov` atom, as FFmpeg's MOV
/// demuxer reads it (`mov_read_cmov`): the file's last top-level atom, its
/// only `moov`, becomes
/// `moov { cmov { dcom "zlib", cmvd [u32 length][zlib stream of the whole moov] } }`.
/// Every byte before it is kept, so the media data's offsets still hold.
pub fn cmov(data: &[u8]) -> Result<Vec<u8>, String> {
    let atoms = top_level_atoms(data)?;
    let moovs = atoms.iter().filter(|a| a.kind == *b"moov").count();
    if moovs != 1 {
        return Err(format!("want one top-level moov, found {moovs}"));
    }
    let Some(last) = atoms.last().filter(|a| a.kind == *b"moov") else {
        return Err("the moov atom is not the last atom".into());
    };
    if !atoms.iter().any(|a| a.kind == *b"mdat") {
        return Err("no mdat atom before the moov atom".into());
    }
    let moov = &data[last.offset..last.offset + last.size];
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(moov, 9);
    let u32_of =
        |n: usize, what: &str| u32::try_from(n).map_err(|_| format!("{what} {n} exceeds 32 bits"));
    let cmvd_size = u32_of(12 + compressed.len(), "cmvd size")?;
    let cmov_size = u32_of(8 + 12 + 12 + compressed.len(), "cmov size")?;
    let moov_size = u32_of(8 + 8 + 12 + 12 + compressed.len(), "moov size")?;
    let mut out = data[..last.offset].to_vec();
    out.extend(moov_size.to_be_bytes());
    out.extend(b"moov");
    out.extend(cmov_size.to_be_bytes());
    out.extend(b"cmov");
    out.extend(12_u32.to_be_bytes());
    out.extend(b"dcomzlib");
    out.extend(cmvd_size.to_be_bytes());
    out.extend(b"cmvd");
    out.extend(u32_of(moov.len(), "moov length")?.to_be_bytes());
    out.extend(compressed);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut a = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        a.extend(kind);
        a.extend(body);
        a
    }

    /// The rewrite keeps every byte before the movie header and stores the
    /// whole `moov` atom, inflatable, in `moov { cmov { dcom, cmvd } }`.
    #[test]
    fn cmov_compresses_the_whole_movie_header_in_place() {
        let head = [atom(b"ftyp", b"qt  \0\0\0\0qt  "), atom(b"mdat", &[7; 64])].concat();
        let moov = atom(b"moov", &atom(b"mvhd", &[1; 100]));
        let file = [head.clone(), moov.clone()].concat();
        let out = cmov(&file).unwrap();
        assert_eq!(out[..head.len()], head[..]);
        let atoms = top_level_atoms(&out).unwrap();
        assert_eq!(atoms.len(), 3);
        let new = &out[atoms[2].offset..];
        assert_eq!(&new[4..8], b"moov");
        assert_eq!(&new[12..16], b"cmov");
        assert_eq!(
            &new[16..28],
            [&12_u32.to_be_bytes()[..], b"dcomzlib"].concat()
        );
        assert_eq!(&new[32..36], b"cmvd");
        assert_eq!(
            new[36..40],
            u32::try_from(moov.len()).unwrap().to_be_bytes()
        );
        let inflated = miniz_oxide::inflate::decompress_to_vec_zlib(&new[40..]).unwrap();
        assert_eq!(inflated, moov);

        assert!(
            cmov(&[moov.clone(), head.clone()].concat()).is_err(),
            "moov not last"
        );
        assert!(cmov(&moov).is_err(), "no mdat");
    }

    #[test]
    fn every_clip_has_a_command_and_a_unique_path() {
        let mut paths: Vec<_> = CLIPS.iter().map(|c| c.path).collect();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), CLIPS.len());
        for clip in &CLIPS {
            assert!(command(clip).contains(clip.path), "{}", clip.path);
            if let Some(reference) = clip.expect.same_frames_as {
                let pos = |p: &str| CLIPS.iter().position(|c| c.path == p);
                assert!(pos(reference) < pos(clip.path), "{reference} is made first");
            }
        }
    }
}
