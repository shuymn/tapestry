//! Sound bank loading: dedupe, the caps of stage1.md D14, decoding and rate
//! conversion. Every sound is generated here as WAV bytes.

mod common;

use std::thread;

use audio::{BankError, BankLimits, BankWarning, Cap, Found, load_bank};
use bms_model::SoundId;
use common::{chart_of, put, ramp, tmp, wav, wav_declaring, wav_streaming};

const RATE: u32 = 48_000;

#[test]
fn default_limits_are_the_plan_caps() {
    let l = BankLimits::default();
    assert_eq!(l.max_sound_seconds, 600);
    assert_eq!(l.max_bank_bytes, 536_870_912);
    assert_eq!(l.max_decode_threads, 4);
}

#[test]
fn identical_files_are_stored_once() {
    let dir = tmp();
    let a_samples = ramp(300, -9_000, 61);
    let c_samples = ramp(200, 4_000, -37);
    let a = put(dir.path(), "a.wav", &wav(RATE, 1, &a_samples));
    let b = put(dir.path(), "b.wav", &wav(RATE, 1, &a_samples));
    let c = put(dir.path(), "c.wav", &wav(RATE, 2, &c_samples));
    let junk = put(dir.path(), "junk.wav", b"not a sound");
    // Sounds: a, b (same bytes as a), c, a again by path, unresolved.
    let mut chart = chart_of(&[Some(a.clone()), Some(b), Some(c), Some(a), None]);
    // An unreferenced sound is never read: its junk would warn.
    chart.sounds.push(bms_model::SoundRef {
        name: "unused.wav".into(),
        path: Some(junk),
    });
    let bank = load_bank(&chart, RATE, &BankLimits::default()).unwrap();

    assert_eq!(bank.stored_count(), 2);
    let slot = |i: u16| bank.slot(SoundId(i));
    assert!(slot(0).is_some());
    assert_eq!(slot(1), slot(0));
    assert_eq!(slot(3), slot(0));
    assert!(slot(2).is_some() && slot(2) != slot(0));
    assert_eq!(slot(4), None);
    assert_eq!(slot(5), None);
    assert_eq!(bank.get(SoundId(0)).unwrap().samples(), &a_samples[..]);
    let c = bank.get(SoundId(2)).unwrap();
    assert!(c.is_stereo());
    assert_eq!(c.samples(), &c_samples[..]);
    assert_eq!(c.frames(), 100);
    assert_eq!(bank.bytes(), 600 + 400);
    assert_eq!(bank.stats().files, 3);
    assert!(bank.warnings().is_empty(), "{:?}", bank.warnings());
}

/// Four stereo sounds each declaring the 10-minute cap (28 800 000 frames at
/// 48 kHz, 115 200 000 bytes) and a fifth declaring the rest of 512 MiB:
/// 536 870 912 − 4 × 115 200 000 = 76 070 912 bytes = 19 017 728 frames
/// (by hand). Each file carries only 16 frames.
fn declared_bank(last_frames: u64) -> Vec<Vec<u8>> {
    (0..5)
        .map(|k| {
            let frames = if k == 4 { last_frames } else { 28_800_000 };
            wav_declaring(RATE, 2, frames, &ramp(32, 100 * k as i16 + 1, 3))
        })
        .collect()
}

fn load_files(
    files: &[Vec<u8>],
    rate: u32,
    limits: &BankLimits,
) -> Result<audio::SoundBank, BankError> {
    let dir = tmp();
    let paths: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(i, bytes)| Some(put(dir.path(), &format!("{i}.wav"), bytes)))
        .collect();
    load_bank(&chart_of(&paths), rate, limits)
}

#[test]
fn bank_over_memory_cap_is_refused_before_decoding() {
    let limits = BankLimits::default();
    // Exactly at the cap: accepted, and decoding finds the short data.
    let bank = load_files(&declared_bank(19_017_728), RATE, &limits).unwrap();
    assert_eq!(bank.stored_count(), 5);
    assert_eq!(bank.get(SoundId(4)).unwrap().frames(), 16);
    // One frame over: refused from the declared counts. Decoding would have
    // found only 16 frames per file, far under the cap.
    let err = load_files(&declared_bank(19_017_729), RATE, &limits).unwrap_err();
    assert_eq!(
        err,
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::Declared,
            sound: SoundId(4),
            amount: 536_870_916,
            limit: 536_870_912,
        }
    );
}

#[test]
fn sound_over_length_cap_is_refused_before_decoding() {
    let limits = BankLimits::default();
    let at = wav_declaring(RATE, 1, 28_800_000, &ramp(16, 7, 5));
    assert!(load_files(&[at], RATE, &limits).is_ok());
    let over = wav_declaring(RATE, 1, 28_800_001, &ramp(16, 7, 5));
    assert_eq!(
        load_files(&[over], RATE, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::SoundLength,
            found: Found::Declared,
            sound: SoundId(0),
            amount: 28_800_001,
            limit: 28_800_000,
        }
    );
    // The declared count is converted to the device rate first: 26 460 001
    // frames at 44.1 kHz are ceil(26 460 001 × 48 000 / 44 100) =
    // 28 800 002 at 48 kHz (by hand).
    let resampled = wav_declaring(44_100, 1, 26_460_001, &ramp(16, 7, 5));
    assert_eq!(
        load_files(&[resampled], RATE, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::SoundLength,
            found: Found::Declared,
            sound: SoundId(0),
            amount: 28_800_002,
            limit: 28_800_000,
        }
    );
}

#[test]
fn sound_without_declared_length_is_bounded_while_decoding() {
    // At a 1 kHz device rate the 10-minute cap is 600 000 frames.
    let limits = BankLimits::default();
    let at = wav_streaming(1_000, 1, &ramp(600_000, 0, 1));
    let bank = load_files(&[at], 1_000, &limits).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().frames(), 600_000);
    let over = wav_streaming(1_000, 1, &ramp(600_001, 0, 1));
    assert_eq!(
        load_files(&[over], 1_000, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::SoundLength,
            found: Found::WhileDecoding,
            sound: SoundId(0),
            amount: 600_001,
            limit: 600_000,
        }
    );
}

#[test]
fn bank_without_declared_lengths_is_bounded_while_decoding() {
    let limits = BankLimits {
        max_bank_bytes: 2_000,
        max_decode_threads: 1,
        ..BankLimits::default()
    };
    let files = |second: usize| {
        vec![
            wav_streaming(1_000, 1, &ramp(500, 11, 2)),
            wav_streaming(1_000, 1, &ramp(second, 13, 3)),
        ]
    };
    let bank = load_files(&files(500), 1_000, &limits).unwrap();
    assert_eq!(bank.bytes(), 2_000);
    assert_eq!(
        load_files(&files(501), 1_000, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            sound: SoundId(1),
            amount: 2_002,
            limit: 2_000,
        }
    );
}

#[test]
fn undecodable_files_are_left_silent_with_a_warning() {
    let mut odd_codec = wav(RATE, 1, &ramp(64, 1, 1));
    odd_codec[20..22].copy_from_slice(&0x1234u16.to_le_bytes());
    let good = wav(RATE, 1, &ramp(64, 2, 1));
    let bank = load_files(
        &[b"RIFF garbage".to_vec(), odd_codec, good],
        RATE,
        &BankLimits::default(),
    )
    .unwrap();
    assert_eq!(bank.stored_count(), 1);
    assert!(bank.get(SoundId(2)).is_some());
    let warned: Vec<SoundId> = bank
        .warnings()
        .iter()
        .map(|BankWarning::Undecodable { sound, .. }| *sound)
        .collect();
    assert_eq!(warned, [SoundId(0), SoundId(1)]);
}

#[test]
fn zero_rate_is_refused() {
    let chart = chart_of(&[None]);
    assert_eq!(
        load_bank(&chart, 0, &BankLimits::default()).unwrap_err(),
        BankError::ZeroRate
    );
}

#[test]
fn source_at_the_device_rate_is_copied_exactly() {
    let samples = ramp(4_801, -20_000, 7);
    let bank = load_files(&[wav(RATE, 1, &samples)], RATE, &BankLimits::default()).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().samples(), &samples[..]);
}

#[test]
fn source_at_another_rate_is_converted_with_its_length_and_onset() {
    // One second of a 1 kHz tone at 44.1 kHz becomes exactly 48 000 frames
    // of the same tone at 48 kHz, with no delay before its first cycle.
    let amp = 16_000.0;
    let tone = |rate: f64, n: usize| -> Vec<i16> {
        (0..n)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / rate).sin()) as i16)
            .collect()
    };
    let bank = load_files(
        &[wav(44_100, 1, &tone(44_100.0, 44_100))],
        RATE,
        &BankLimits::default(),
    )
    .unwrap();
    let got = bank.get(SoundId(0)).unwrap().samples();
    assert_eq!(got.len(), 48_000);
    let want = tone(48_000.0, 48_000);
    // Away from the edges, where the clip starts and stops abruptly.
    let worst = (1_000..47_000)
        .map(|i| (i32::from(got[i]) - i32::from(want[i])).abs())
        .max()
        .unwrap();
    assert!(worst < 160, "worst error {worst} of {amp}");
}

#[test]
fn decode_pool_is_capped() {
    let cores = thread::available_parallelism().map_or(1, usize::from);
    let files: Vec<Vec<u8>> = (0..6).map(|k| wav(RATE, 1, &ramp(10, k, 1))).collect();
    let threads = |max: usize, files: &[Vec<u8>]| {
        let limits = BankLimits {
            max_decode_threads: max,
            ..BankLimits::default()
        };
        load_files(files, RATE, &limits)
            .unwrap()
            .stats()
            .decode_threads
    };
    assert_eq!(threads(4, &files), cores.min(4));
    assert_eq!(threads(2, &files), cores.min(2));
    assert_eq!(threads(4, &files[..1]), 1);
    assert_eq!(threads(0, &files), 1);
}

#[test]
fn generated_bank_load_time_is_reported() {
    // 200 half-second sounds, half of them at 44.1 kHz (converted).
    let files: Vec<Vec<u8>> = (0..200)
        .map(|k| {
            let rate = if k % 2 == 0 { RATE } else { 44_100 };
            wav(rate, 2, &ramp(rate as usize, k as i16, 13 + k as i16))
        })
        .collect();
    let bank = load_files(&files, RATE, &BankLimits::default()).unwrap();
    assert_eq!(bank.stored_count(), 200);
    let stats = bank.stats();
    println!(
        "bank load: {} sounds, {} bytes, {} decode threads, {:?}",
        bank.stored_count(),
        bank.bytes(),
        stats.decode_threads,
        stats.elapsed
    );
}
