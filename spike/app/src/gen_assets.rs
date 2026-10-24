//! `gen-assets`: the BGA test video, generated with an FFmpeg executable
//! (its output is data, not linked code; design D2).
//!
//! 1920×1080, 60 fps, `testsrc2` with fixed-seed temporal noise, H.264
//! (libopenh264 from the LGPL build) at about 10 Mbps with a 2 s GOP. The
//! atlas and digit glyphs are generated at startup from a fixed seed, so
//! the video is the only file asset. Writes `bga.mp4` and `bga.mp4.sha256`.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

pub fn run(ffmpeg: &Path, out: &Path, seconds: u32) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let video = out.join("bga.mp4");
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!(
            "testsrc2=size=1920x1080:rate=60:duration={seconds}"
        ))
        .args([
            "-vf",
            "noise=alls=12:allf=t+u:all_seed=20261024,format=yuv420p",
            "-c:v",
            "libopenh264",
            "-b:v",
            "10M",
            "-g",
            "120",
            "-an",
        ])
        .arg(&video)
        .status()
        .map_err(|e| format!("run {}: {e}", ffmpeg.display()))?;
    if !status.success() {
        return Err(format!("ffmpeg failed: {status}"));
    }
    let bytes = std::fs::read(&video).map_err(|e| e.to_string())?;
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(out.join("bga.mp4.sha256"), format!("{hash}  bga.mp4\n"))
        .map_err(|e| e.to_string())?;
    println!(
        "gen-assets: {} ({} bytes, {seconds} s) sha256 {hash}",
        video.display(),
        bytes.len()
    );
    Ok(())
}
