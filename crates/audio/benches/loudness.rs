//! CPU benchmark of the loudness analysis (design.md "Algorithms"): one
//! minute of a dense schedule, 64 voices sounding throughout, mixed and
//! metered at 48 kHz. The sounds and the schedule are built here.

use std::hint::black_box;
use std::sync::Arc;

use audio::{Analysis, Sound, SoundBank, Step};
use bms_model::{BgmEvent, SoundId, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};

const RATE: u32 = 48_000;
const VOICES: u16 = 64;

/// 64 two-second sounds of distinct pitches, at a level that keeps their
/// sum finite and unclipped (the analysis measures the raw sum anyway).
fn bank() -> Arc<SoundBank> {
    let sounds = (0..VOICES)
        .map(|k| {
            let step = 200.0 + 37.0 * f64::from(k);
            Sound::mono(
                (0..2 * RATE as usize)
                    .map(|i| {
                        let phase = std::f64::consts::TAU * step * i as f64 / f64::from(RATE);
                        (phase.sin() * 400.0) as i16
                    })
                    .collect(),
            )
        })
        .collect();
    Arc::new(SoundBank::from_sounds(RATE, sounds))
}

/// Every sound started again every second for a minute, each id staggered
/// by 15 ms: 64 voices sound throughout.
fn schedule() -> Vec<BgmEvent> {
    let mut events: Vec<BgmEvent> = (0..60_i64)
        .flat_map(|s| {
            (0..VOICES).map(move |k| BgmEvent {
                time: TimeUs(s * 1_000_000 + i64::from(k) * 15_000),
                sound: SoundId(k),
            })
        })
        .collect();
    events.sort_by_key(|e| e.time);
    events
}

fn analyze(bank: &Arc<SoundBank>, schedule: &[BgmEvent]) -> f64 {
    let mut analysis = Analysis::new(Arc::clone(bank)).expect("the meter takes 48 kHz");
    let mut events = schedule.iter().copied();
    loop {
        match analysis.step(&mut events).expect("the meter runs") {
            Step::More => {}
            Step::Done(m) => return m.true_peak,
        }
    }
}

fn bench(c: &mut Criterion) {
    let bank = bank();
    let schedule = schedule();
    let mut group = c.benchmark_group("loudness");
    group.sample_size(10);
    group.bench_function("analyze_one_minute_dense", |b| {
        b.iter(|| black_box(analyze(&bank, &schedule)));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
