//! Sound bank loading: dedupe, the caps of design.md "Audio", decoding and rate
//! conversion. Every sound is generated here as WAV bytes.

mod common;

use std::thread;

use audio::{
    BankError, BankLimits, BankWarning, Cap, Found, Speed, load_bank, load_bank_at, plan_conversion,
};
use bms_model::SoundId;
use common::{chart_of, put, ramp, tmp, wav, wav_declaring, wav_padded, wav_streaming};

const RATE: u32 = 48_000;

#[test]
fn default_limits_are_the_plan_caps() {
    let l = BankLimits::default();
    assert_eq!(l.max_sound_seconds, 600);
    assert_eq!(l.max_bank_bytes, 536_870_912);
    assert_eq!(l.max_file_bytes, 134_217_728);
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
    // One decode thread, so the declared sizes add up in sound order and
    // the fifth sound is the one that crosses the cap.
    let limits = BankLimits {
        max_decode_threads: 1,
        ..BankLimits::default()
    };
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
    // A 1-second cap at 8 kHz is 8000 frames.
    let limits = BankLimits {
        max_sound_seconds: 1,
        ..BankLimits::default()
    };
    let at = wav_streaming(8_000, 1, &ramp(8_000, 0, 3));
    let bank = load_files(&[at], 8_000, &limits).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().frames(), 8_000);
    let over = wav_streaming(8_000, 1, &ramp(8_001, 0, 3));
    assert_eq!(
        load_files(&[over], 8_000, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::SoundLength,
            found: Found::WhileDecoding,
            sound: SoundId(0),
            amount: 8_001,
            limit: 8_000,
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
            wav_streaming(8_000, 1, &ramp(500, 11, 2)),
            wav_streaming(8_000, 1, &ramp(second, 13, 3)),
        ]
    };
    let bank = load_files(&files(500), 8_000, &limits).unwrap();
    assert_eq!(bank.bytes(), 2_000);
    assert_eq!(
        load_files(&files(501), 8_000, &limits).unwrap_err(),
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
fn source_and_converted_pcm_are_charged_while_converting() {
    // 400 frames at 16 kHz become 200 at 8 kHz. Converting holds both, 800
    // source bytes plus 400 converted bytes, and the stage's workspace
    // bound W, so a cap of 1199 + W refuses it and 1200 + W does not,
    // though the stored sound is only 400 bytes.
    let file = || vec![wav_streaming(16_000, 1, &ramp(400, 5, 9))];
    let limits = |max_bank_bytes| BankLimits {
        max_bank_bytes,
        ..BankLimits::default()
    };
    let w = plan_conversion(16_000, 8_000, Speed::NORMAL, 1)
        .device
        .unwrap()
        .workspace_bound();
    let bank = load_files(&file(), 8_000, &limits(1_200 + w)).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().frames(), 200);
    assert_eq!(bank.bytes(), 400);
    assert_eq!(
        load_files(&file(), 8_000, &limits(1_199 + w)).unwrap_err(),
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            sound: SoundId(0),
            amount: 1_200 + w,
            limit: 1_199 + w,
        }
    );
}

#[test]
fn speed_stage_charges_its_input_output_and_workspace() {
    // 400 frames at the device rate become 800 at 50 %: 800 input bytes,
    // 1600 output bytes and the speed stage's workspace bound W, so a cap
    // of 2399 + W refuses it and 2400 + W does not.
    let half = Speed::SLOWEST;
    let limits = |max_bank_bytes| BankLimits {
        max_bank_bytes,
        ..BankLimits::default()
    };
    let w = plan_conversion(8_000, 8_000, half, 1)
        .speed
        .unwrap()
        .workspace_bound();
    let dir = tmp();
    let file = wav_streaming(8_000, 1, &ramp(400, 5, 9));
    let chart = chart_of(&[Some(put(dir.path(), "a.wav", &file))]);
    let bank = load_bank_at(&chart, 8_000, half, &limits(2_400 + w)).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().frames(), 800);
    assert_eq!(bank.bytes(), 1_600);
    assert_eq!(
        load_bank_at(&chart, 8_000, half, &limits(2_399 + w)).unwrap_err(),
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            sound: SoundId(0),
            amount: 2_400 + w,
            limit: 2_399 + w,
        }
    );
}

#[test]
fn pcm_is_charged_at_its_allocation_while_growing() {
    // 20 000 mono frames are 40 000 bytes. Declared, they are allocated
    // exactly once and fit a 40 000-byte bank. Streamed, the buffer grows
    // packet by packet; while it reallocates, the old and new buffers are
    // both charged, so the same bank refuses it, though the stored sound,
    // shrunk to its length, is the same 40 000 bytes.
    let samples = ramp(20_000, -7, 3);
    let limits = |max_bank_bytes| BankLimits {
        max_bank_bytes,
        ..BankLimits::default()
    };
    let known = load_files(&[wav(RATE, 1, &samples)], RATE, &limits(40_000)).unwrap();
    assert_eq!(known.bytes(), 40_000);
    let streamed = || vec![wav_streaming(RATE, 1, &samples)];
    match load_files(&streamed(), RATE, &limits(40_000)).unwrap_err() {
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            amount,
            ..
        } => assert!(amount > 40_000, "{amount}"),
        e => panic!("{e:?}"),
    }
    let bank = load_files(&streamed(), RATE, &limits(1_000_000)).unwrap();
    assert_eq!(bank.get(SoundId(0)).unwrap().samples(), &samples[..]);
    assert_eq!(bank.bytes(), 40_000);
}

#[test]
fn raw_bytes_held_at_once_stay_within_one_file_per_worker() {
    // 32 files of 1 MiB padding and 16 frames of audio, each contents
    // twice: 32 MiB read in all, 16 distinct sounds of 32 bytes.
    let pad = 1 << 20;
    let files: Vec<Vec<u8>> = (0..32)
        .map(|k| wav_padded(RATE, 1, &ramp(16, (k / 2) as i16 * 40 + 1, 1), pad))
        .collect();
    let len = files[0].len() as u64;
    let limits = BankLimits {
        max_decode_threads: 2,
        ..BankLimits::default()
    };
    let bank = load_files(&files, RATE, &limits).unwrap();
    assert_eq!(bank.stored_count(), 16);
    assert_eq!(bank.bytes(), 16 * 32);
    let stats = bank.stats();
    assert!(stats.peak_raw_bytes >= len, "{}", stats.peak_raw_bytes);
    assert!(
        stats.peak_raw_bytes <= stats.decode_threads as u64 * len,
        "{} raw bytes held with {} threads",
        stats.peak_raw_bytes,
        stats.decode_threads
    );
}

#[test]
fn file_over_size_cap_is_refused_before_decoding() {
    // A WAV of n mono frames is 44 + 2n bytes: 478 frames are 1000 bytes.
    // Reading stops one byte past the cap, so the excess reads as 1001.
    let limits = BankLimits {
        max_file_bytes: 1_000,
        ..BankLimits::default()
    };
    assert!(load_files(&[wav(RATE, 1, &ramp(478, 1, 1))], RATE, &limits).is_ok());
    assert_eq!(
        load_files(&[wav(RATE, 1, &ramp(479, 1, 1))], RATE, &limits).unwrap_err(),
        BankError::OverCap {
            cap: Cap::FileSize,
            found: Found::Declared,
            sound: SoundId(0),
            amount: 1_001,
            limit: 1_000,
        }
    );
}

fn warned(bank: &audio::SoundBank) -> Vec<SoundId> {
    bank.warnings()
        .iter()
        .map(|BankWarning::Undecodable { sound, .. }| *sound)
        .collect()
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
    assert_eq!(warned(&bank), [SoundId(0), SoundId(1)]);
}

#[test]
fn malformed_header_that_breaks_the_decoder_is_left_silent() {
    // 32 768 channels of 16-bit samples: the block alignment the header
    // implies does not fit in 16 bits.
    let huge = wav(RATE, 32_768, &ramp(64, 3, 1));
    let good = wav(RATE, 1, &ramp(64, 4, 1));
    let bank = load_files(&[huge, good], RATE, &BankLimits::default()).unwrap();
    assert_eq!(bank.stored_count(), 1);
    assert_eq!(warned(&bank), [SoundId(0)]);
}

#[test]
fn source_rate_outside_the_range_is_left_silent() {
    // u32::MAX would size the resampler's FFT at gigabytes from a one-frame
    // file. 8 kHz and 384 kHz are the edges of the range.
    let files: Vec<Vec<u8>> = [u32::MAX, 7_999, 8_000, 384_000, 384_001]
        .iter()
        .enumerate()
        .map(|(k, &rate)| wav(rate, 1, &ramp(16, k as i16 * 50, 1)))
        .collect();
    let bank = load_files(&files, RATE, &BankLimits::default()).unwrap();
    assert_eq!(warned(&bank), [SoundId(0), SoundId(1), SoundId(4)]);
    // 16 frames: ceil(16 × 6) = 96 at 48 kHz from 8 kHz, ceil(16 / 8) = 2
    // from 384 kHz (by hand).
    assert_eq!(bank.get(SoundId(2)).unwrap().frames(), 96);
    assert_eq!(bank.get(SoundId(3)).unwrap().frames(), 2);
}

#[test]
fn device_rate_outside_the_range_is_refused() {
    let chart = chart_of(&[None]);
    for rate in [0, 7_999, 384_001] {
        assert_eq!(
            load_bank(&chart, rate, &BankLimits::default()).unwrap_err(),
            BankError::RateOutOfRange { rate }
        );
    }
    for rate in [8_000, 384_000] {
        assert!(load_bank(&chart, rate, &BankLimits::default()).is_ok());
    }
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
fn bank_lengths_follow_the_two_stages() {
    // A 60 s 441 Hz tone at 44.1 kHz, right at half the left: 2 880 000
    // frames at 48 kHz, then floor(2 880 000 × 20/k) (by hand). Pitch
    // follows speed: output frame j holds the source at j / 48 000 × k/20 s,
    // with neither stage's delay left. The resampler centres its filter on
    // the lower middle sample, so a stage whose transform takes an odd
    // number of input frames (261 at 29:20) leads by half an input frame
    // (0.5 / 48 000 s here); that offset is allowed.
    let amp = 16_000.0;
    let at = |t: f64| amp * (2.0 * std::f64::consts::PI * 441.0 * t).sin();
    let samples: Vec<i16> = (0..2_646_000)
        .flat_map(|i| {
            let x = at(f64::from(i) / 44_100.0);
            [x as i16, (x / 2.0) as i16]
        })
        .collect();
    let dir = tmp();
    let path = put(dir.path(), "tone.wav", &wav(44_100, 2, &samples));
    let chart = chart_of(&[Some(path)]);
    for (k, frames) in [(10, 5_760_000), (13, 4_430_769), (29, 1_986_206)] {
        let speed = Speed::from_twentieths(k).unwrap();
        let bank = load_bank_at(&chart, RATE, speed, &BankLimits::default()).unwrap();
        assert_eq!((bank.rate(), bank.speed()), (RATE, speed));
        let sound = bank.get(SoundId(0)).unwrap();
        assert!(sound.is_stereo());
        assert_eq!(sound.frames(), frames, "{k}/20");
        assert_eq!(bank.bytes(), frames * 4);
        let got = sound.samples();
        // Away from the edges, at the better of the two offsets.
        let worst = [0.0, 0.5 / 48_000.0]
            .map(|lead| {
                (10_000..20_000)
                    .chain(frames as usize - 20_000..frames as usize - 10_000)
                    .map(|j| {
                        let want = at(j as f64 / 48_000.0 * f64::from(k) / 20.0 + lead);
                        let l = (f64::from(got[2 * j]) - want).abs();
                        let r = (f64::from(got[2 * j + 1]) - want / 2.0).abs();
                        l.max(r)
                    })
                    .fold(0.0, f64::max)
            })
            .into_iter()
            .fold(f64::INFINITY, f64::min);
        assert!(worst < 160.0, "{k}/20: worst error {worst} of {amp}");
        println!(
            "60 s stereo 44.1 kHz at {k}/20: loaded in {:?}",
            bank.stats().elapsed
        );
    }
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
