//! The conversion stages' workspace bound (design.md "Audio"): it is planned
//! from the rates and channels alone, covers what each stage allocates at
//! its peak, and is charged before the stage is built.
//!
//! The counting allocator keeps each tracked thread's live bytes and peak,
//! and the largest allocation on any thread while watching. The tests that
//! read it take `SERIAL`, so one's allocations never land in another's
//! count.

mod common;
mod counting;

use std::sync::Mutex;

use audio::{
    BankError, BankLimits, Cap, Conversion, Found, Speed, Stage, load_bank, plan_conversion,
};
use bms_model::SoundId;
use common::{chart_of, put, ramp, tmp, wav};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

static SERIAL: Mutex<()> = Mutex::new(());

const MIB: u64 = 1 << 20;

#[test]
fn speed_stage_workspace_is_bounded() {
    // The coprime pair 383 999 : 384 000 is the first stage's. The speed
    // stage works at the reduced 29:20, so its workspace stays small
    // whatever the source rate.
    let speed = Speed::from_twentieths(29).unwrap();
    let plan = plan_conversion(383_999, 384_000, speed, 2);
    let stage = plan.speed.unwrap();
    assert_eq!((stage.rates(), stage.channels()), ((29, 20), 2));
    assert!(stage.workspace_bound() < MIB, "{}", stage.workspace_bound());
    let device = plan.device.unwrap();
    assert_eq!(device.rates(), (383_999, 384_000));
    println!(
        "383 999 -> 384 000 Hz stereo at 145 %: device stage bound {} B, speed stage bound {} B",
        device.workspace_bound(),
        stage.workspace_bound()
    );
    // Equal rates and 1× need no stage.
    let none = plan_conversion(48_000, 48_000, Speed::NORMAL, 2);
    assert_eq!(
        none,
        Conversion {
            device: None,
            speed: None
        }
    );
    // The ladder's stages, input to output, by hand.
    let rates: Vec<(u32, u32)> = Speed::ladder().map(Speed::stage_rates).collect();
    assert_eq!(rates[0], (1, 2));
    assert_eq!(rates[3], (13, 20));
    assert_eq!(rates[10], (1, 1));
    assert_eq!(rates[19], (29, 20));
    assert_eq!(rates[20], (3, 2));
}

/// The most bytes converting `samples` (`frames` frames out) allocated at
/// once, besides the output it returns.
fn workspace_peak(stage: &Stage, samples: &[i16], frames: u64) -> u64 {
    counting::track(true);
    let base = counting::live_bytes();
    counting::reset_peak();
    let out = stage.convert(samples, frames).unwrap();
    let peak = counting::peak_bytes(base);
    counting::track(false);
    assert_eq!(out.len() as u64, frames * stage.channels() as u64);
    peak - out.capacity() as u64 * 2
}

/// Converts one second of a ramp at the stage's input rate (at most 48 000
/// frames), returning the bound and the measured peak.
fn measure(stage: &Stage) -> (u64, u64) {
    let (from, to) = stage.rates();
    let ch = stage.channels();
    let frames = u64::from(from.min(48_000));
    let samples = ramp(frames as usize * ch, -12_000, 7);
    let out = frames * u64::from(to) / u64::from(from);
    let peak = workspace_peak(stage, &samples, out);
    (stage.workspace_bound(), peak)
}

#[test]
fn resampler_workspace_stays_within_its_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The positive control, on this thread alone: the counter moves.
    counting::track(true);
    counting::self_check();
    counting::track(false);
    let mut stages = vec![
        Stage::new(8_001, 8_000, 1),
        Stage::new(44_100, 48_000, 2),
        Stage::new(383_999, 384_000, 2),
    ];
    for speed in Speed::ladder().filter(|&s| s != Speed::NORMAL) {
        // The stages a bank builds for 44.1 kHz stereo at 48 kHz.
        let plan = plan_conversion(44_100, 48_000, speed, 2);
        assert_eq!(plan.device, Some(Stage::new(44_100, 48_000, 2)));
        stages.push(plan.speed.unwrap());
    }
    for stage in &stages {
        let (bound, peak) = measure(stage);
        let (from, to) = stage.rates();
        println!(
            "{from} -> {to} x{}: bound {bound} B, peak {peak} B ({:.2}x)",
            stage.channels(),
            bound as f64 / peak as f64
        );
        assert!(peak <= bound, "{from} -> {to}: peak {peak} > bound {bound}");
    }
}

#[test]
fn oversized_resampler_workspace_is_refused_before_construction() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // 1000 mono frames at 383 999 Hz for a 384 000 Hz device: 2000 source
    // bytes and 1001 frames (2002 bytes) out, far under a 1 MiB bank, but
    // the stage's workspace bound W is not.
    let dir = tmp();
    let path = put(
        dir.path(),
        "odd.wav",
        &wav(383_999, 1, &ramp(1_000, -5_000, 11)),
    );
    let chart = chart_of(&[Some(path)]);
    let w = plan_conversion(383_999, 384_000, Speed::NORMAL, 1)
        .device
        .unwrap()
        .workspace_bound();
    assert!(w > MIB, "{w}");
    let small = BankLimits {
        max_bank_bytes: MIB,
        max_decode_threads: 1,
        ..BankLimits::default()
    };
    counting::watch_all(true);
    let refused = load_bank(&chart, 384_000, &small);
    let largest_refused = counting::largest();
    counting::watch_all(false);
    assert_eq!(
        refused.unwrap_err(),
        BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            sound: SoundId(0),
            amount: 2_000 + 2_002 + w,
            limit: MIB,
        }
    );
    // The positive control: with room for it, the stage is built, and it
    // allocates its transform buffers of 2 × 383 999 f32 (synchro.rs
    // filter_t, input_buf).
    counting::watch_all(true);
    let bank = load_bank(&chart, 384_000, &BankLimits::default()).unwrap();
    let largest_accepted = counting::largest();
    counting::watch_all(false);
    assert_eq!(bank.get(SoundId(0)).unwrap().frames(), 1_001);
    assert!(largest_accepted >= 8 * 383_999, "{largest_accepted}");
    // Refused, nothing of that size, nor of the cap's, was allocated.
    assert!(largest_refused < MIB, "{largest_refused}");
    println!("largest allocation: refused {largest_refused} B, accepted {largest_accepted} B");
}

/// Every transform length a stage can be given, 1 to 384 001 on either
/// side, built as a mono stage: what the constructor allocates stays within
/// the bound. Minutes in a release build:
/// `cargo test --release -p audio --test workspace -- --ignored`.
#[test]
#[ignore = "sweeps every transform length; run in release"]
fn every_transform_length_stays_within_the_bound() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let next = AtomicU32::new(2);
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    let worst = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    // The highest peak / bound and its rates.
                    let mut worst = (0.0f64, 0u32, 0u64, 0u64);
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        if n > 384_001 {
                            break worst;
                        }
                        let stage = Stage::new(n, n - 1, 1);
                        let peak = workspace_peak(&stage, &[], 0);
                        let bound = stage.workspace_bound();
                        assert!(peak <= bound, "{n} -> {}: {peak} > {bound}", n - 1);
                        let r = peak as f64 / bound as f64;
                        if r > worst.0 {
                            worst = (r, n, peak, bound);
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().unwrap())
            .fold((0.0, 0, 0, 0), |a, b| if b.0 > a.0 { b } else { a })
    });
    println!(
        "highest peak / bound {:.3} at {} -> {}: {} of {} B",
        worst.0,
        worst.1,
        worst.1 - 1,
        worst.2,
        worst.3
    );
}
