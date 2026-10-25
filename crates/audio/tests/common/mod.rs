//! Test helpers: sounds written as WAV bytes by hand (not by the decoder
//! under test) into temporary directories under the target directory, and
//! charts that reference them.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};

/// A fresh directory under Cargo's per-target temporary directory, removed
/// on drop.
pub fn tmp() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(root).unwrap();
    tempfile::Builder::new()
        .prefix("audio-")
        .tempdir_in(root)
        .unwrap()
}

fn header(rate: u32, channels: u16, data_len: u32, riff_len: u32) -> Vec<u8> {
    let block = channels * 2;
    let mut b = Vec::with_capacity(44);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&riff_len.to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    b.extend_from_slice(&block.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    b
}

fn append(mut b: Vec<u8>, samples: &[i16]) -> Vec<u8> {
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// A 16-bit PCM WAV holding `samples` (interleaved when stereo).
pub fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    append(header(rate, channels, data_len, 36 + data_len), samples)
}

/// A WAV whose header declares `declared_frames` frames but which carries
/// only `samples`.
pub fn wav_declaring(rate: u32, channels: u16, declared_frames: u64, samples: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(declared_frames * u64::from(channels) * 2).unwrap();
    append(header(rate, channels, data_len, 36 + data_len), samples)
}

/// A WAV with the "length unknown" sizes a streaming writer leaves
/// (0xFFFFFFFF), so it declares no frame count.
pub fn wav_streaming(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
    append(header(rate, channels, u32::MAX, u32::MAX), samples)
}

/// Writes `bytes` to `dir/name` and returns the path.
pub fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A chart whose sound `i` is `paths[i]` (`None`: unresolved), each played
/// by one BGM event at `i` ms.
pub fn chart_of(paths: &[Option<PathBuf>]) -> Chart {
    let mut b = ChartBuilder::new(120_000);
    for (i, _) in paths.iter().enumerate() {
        b = b
            .sound(&format!("s{i}.wav"))
            .bgm(TimeUs(i as i64 * 1_000), SoundId(i as u16));
    }
    let mut chart = b.build().unwrap();
    for (s, p) in chart.sounds.iter_mut().zip(paths) {
        s.path.clone_from(p);
    }
    chart
}

/// Distinct mono samples: `n` values starting at `first`, stepping by
/// `step`, wrapping within i16.
pub fn ramp(n: usize, first: i16, step: i16) -> Vec<i16> {
    (0..n)
        .map(|i| first.wrapping_add(step.wrapping_mul(i as i16)))
        .collect()
}
