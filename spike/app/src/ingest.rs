//! `ingest-check --url <rtmp> [--delay-s N] [--seconds N]`: reads the local
//! ingest (what OBS streams to MediaMTX) and checks that it carries the
//! game: a 1920×1080 video stream with packets, and an audio stream that is
//! not silent (V7's "game visible and audio present", checked before an OBS
//! cell instead of trusted). Video is not decoded; audio is decoded to
//! measure its peak. Exit 0 when both are present, 1 otherwise.

use std::time::Duration;

/// Audio quieter than this is treated as absent (the spike's key sounds and
/// pilot are far louder).
pub const SILENCE_DBFS: f64 = -60.0;

#[derive(Debug, Default, PartialEq)]
pub struct Ingest {
    pub video: Option<(u32, u32)>,
    pub video_packets: u64,
    /// Sample rate and channels of the audio stream.
    pub audio: Option<(u32, u16)>,
    pub audio_frames: u64,
    pub peak_dbfs: Option<f64>,
}

/// What is missing from the ingest; empty when it carries the game.
pub fn problems(i: &Ingest) -> Vec<String> {
    let mut p = vec![];
    match i.video {
        None => p.push("no video stream".to_owned()),
        Some((w, h)) if (w, h) != (1920, 1080) => {
            p.push(format!("video is {w}x{h}, not 1920x1080"))
        }
        Some(_) => {}
    }
    if i.video.is_some() && i.video_packets == 0 {
        p.push("no video packets".into());
    }
    match (i.audio, i.peak_dbfs) {
        (None, _) => p.push("no audio stream".into()),
        (Some(_), None) => p.push("no audio decoded".into()),
        (Some(_), Some(db)) if db < SILENCE_DBFS => {
            p.push(format!("audio is silent (peak {db:.1} dBFS)"))
        }
        _ => {}
    }
    p
}

/// Peak of normalized samples as dBFS (`None` for no samples).
pub fn peak_dbfs(peak: f32, samples: u64) -> Option<f64> {
    (samples > 0).then(|| 20.0 * f64::from(peak.max(1e-10)).log10())
}

#[cfg(feature = "ffmpeg")]
fn read(url: &str, seconds: u64) -> Result<Ingest, String> {
    use ffmpeg_next as ff;
    use std::time::Instant;
    ff::init().map_err(|e| e.to_string())?;
    let mut opts = ff::Dictionary::new();
    // Socket timeout (µs) so a stalled ingest ends the read.
    opts.set("rw_timeout", "5000000");
    let mut ictx =
        ff::format::input_with_dictionary(&url, opts).map_err(|e| format!("open {url}: {e}"))?;
    let mut out = Ingest::default();
    let video = ictx.streams().best(ff::media::Type::Video).map(|s| {
        let size = ff::codec::context::Context::from_parameters(s.parameters())
            .and_then(|c| c.decoder().video())
            .map(|v| (v.width(), v.height()))
            .ok();
        (s.index(), size)
    });
    out.video = video.and_then(|v| v.1);
    let mut audio = match ictx.streams().best(ff::media::Type::Audio) {
        Some(s) => {
            let dec = ff::codec::context::Context::from_parameters(s.parameters())
                .and_then(|c| c.decoder().audio())
                .map_err(|e| format!("audio decoder: {e}"))?;
            out.audio = Some((dec.rate(), dec.channels()));
            Some((s.index(), dec))
        }
        None => None,
    };
    let (mut peak, mut samples) = (0f32, 0u64);
    let mut frame = ff::frame::Audio::empty();
    let end = Instant::now() + Duration::from_secs(seconds);
    for (stream, packet) in ictx.packets() {
        if Instant::now() >= end {
            break;
        }
        if video.is_some_and(|v| v.0 == stream.index()) {
            out.video_packets += 1;
        }
        let Some((index, dec)) = audio.as_mut() else {
            continue;
        };
        if stream.index() != *index || dec.send_packet(&packet).is_err() {
            continue;
        }
        while dec.receive_frame(&mut frame).is_ok() {
            out.audio_frames += 1;
            let planes = if frame.is_planar() {
                usize::from(frame.channels())
            } else {
                1
            };
            let n = frame.samples()
                * if frame.is_planar() {
                    1
                } else {
                    usize::from(frame.channels())
                };
            for p in 0..planes {
                let data = frame.data(p);
                let mut level = |x: f32| peak = peak.max(x.abs());
                match frame.format() {
                    ff::format::Sample::F32(_) => data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .take(n)
                        .for_each(|b| level(f32::from_le_bytes(*b))),
                    ff::format::Sample::I16(_) => data
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .take(n)
                        .for_each(|b| level(f32::from(i16::from_le_bytes(*b)) / 32768.0)),
                    _ => return Err("unsupported audio sample format".into()),
                }
            }
            samples += n as u64;
        }
    }
    out.peak_dbfs = peak_dbfs(peak, samples);
    Ok(out)
}

#[cfg(not(feature = "ffmpeg"))]
fn read(_: &str, _: u64) -> Result<Ingest, String> {
    Err("built without the ffmpeg feature".into())
}

pub fn run(url: &str, delay_s: u64, seconds: u64) -> Result<std::process::ExitCode, String> {
    std::thread::sleep(Duration::from_secs(delay_s));
    let i = read(url, seconds)?;
    println!(
        "ingest: video {:?} ({} packets), audio {:?} ({} frames, peak {:?} dBFS)",
        i.video,
        i.video_packets,
        i.audio,
        i.audio_frames,
        i.peak_dbfs.map(|d| (d * 10.0).round() / 10.0)
    );
    let p = problems(&i);
    if p.is_empty() {
        println!("ingest check: pass (game video and audio present)");
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        println!("ingest check: FAIL ({})", p.join("; "));
        Ok(std::process::ExitCode::from(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ingest_needs_full_hd_video_and_audible_audio() {
        let good = Ingest {
            video: Some((1920, 1080)),
            video_packets: 600,
            audio: Some((48_000, 2)),
            audio_frames: 470,
            peak_dbfs: peak_dbfs(0.5, 480_000),
        };
        assert!(problems(&good).is_empty());
        let silent = Ingest {
            peak_dbfs: peak_dbfs(0.0, 480_000),
            ..good
        };
        assert_eq!(problems(&silent).len(), 1);
        let no_audio = Ingest {
            audio: None,
            peak_dbfs: None,
            video: Some((1920, 1080)),
            video_packets: 600,
            audio_frames: 0,
        };
        assert_eq!(problems(&no_audio), vec!["no audio stream".to_owned()]);
        let scaled = Ingest {
            video: Some((1440, 810)),
            video_packets: 1,
            audio: Some((48_000, 2)),
            audio_frames: 1,
            peak_dbfs: Some(-6.0),
        };
        assert_eq!(problems(&scaled).len(), 1);
        assert_eq!(problems(&Ingest::default()).len(), 2);
    }
}
