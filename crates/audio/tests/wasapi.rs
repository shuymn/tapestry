//! WASAPI output on the default render endpoint. Needs a device, so the
//! test is ignored; run it with `--ignored --nocapture`. It plays a quiet
//! tone for about 2 s.
//!
//! The counting allocator checks that the audio loop allocates nothing,
//! from the priming wake on.

#![cfg(windows)]

use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use audio::{
    AudioEngine, RunReport, ScheduledKind, SoundBank, SoundCmd, WakeTrace, WasapiShared, run_audio,
};
use bms_model::{BgmEvent, SoundId, TimeSource, TimeUs};
use rtrb::RingBuffer;

mod counting;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

/// µs since the clock was made; copies share the origin.
#[derive(Clone, Copy)]
struct Elapsed(Instant);

impl TimeSource for Elapsed {
    fn now(&self) -> TimeUs {
        TimeUs(self.0.elapsed().as_micros() as i64)
    }
}

/// Wakes traced one by one at startup.
const TRACED: usize = 8;

/// One startup wake: the output's trace (none for the priming wake) and
/// what the step did.
#[derive(Clone, Copy, Debug, Default)]
struct Traced {
    wake: Option<WakeTrace>,
    frames: u32,
    underrun: bool,
}

/// A 440 Hz sine at -20 dBFS (amplitude 0.1 of full scale, 3277 of 32767),
/// with 10 ms linear fades so it starts and ends without a click.
fn tone(rate: u32, len: usize) -> Vec<i16> {
    let fade = (rate / 100) as usize;
    (0..len)
        .map(|i| {
            let edge = i.min(len - 1 - i) as f64 / fade as f64;
            let phase = TAU * 440.0 * i as f64 / f64::from(rate);
            (3_277.0 * edge.min(1.0) * phase.sin()).round() as i16
        })
        .collect()
}

#[test]
#[ignore = "needs an audio output device; plays a quiet tone for 2 s"]
fn wasapi_plays_a_scheduled_tone() {
    const RUN: Duration = Duration::from_secs(2);
    const TONE_AT: TimeUs = TimeUs(250_000);
    let clock = Elapsed(Instant::now());
    let mut out = WasapiShared::open(clock).unwrap();
    let g = *out.grant();
    let bank = SoundBank::from_sounds(
        g.rate,
        vec![audio::Sound::mono(tone(g.rate, g.rate as usize * 3 / 2))],
    );
    let bgm = [BgmEvent {
        time: TONE_AT,
        sound: SoundId(0),
    }];
    let mut engine = AudioEngine::new(Arc::new(bank), &bgm, g.rate, g.period, 8, 4).unwrap();
    engine.restart(clock.now());
    let (_tx, mut rx) = RingBuffer::<SoundCmd>::new(16);
    let stop = AtomicBool::new(false);

    let started = Instant::now();
    let mut trace = [Traced::default(); TRACED];
    let report: RunReport = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(RUN);
            stop.store(true, Ordering::Release);
        });
        counting::track(true);
        // The first wakes one by one, for the startup trace.
        for t in &mut trace {
            let step = engine.step(&mut out, &mut rx).unwrap();
            *t = Traced {
                wake: out.stats().last,
                frames: step.frames,
                underrun: step.underrun,
            };
        }
        let r = run_audio(&mut out, &mut engine, &mut rx, &stop);
        counting::track(false);
        r.unwrap()
    });
    let ran = started.elapsed();
    let stats = *out.stats();
    let traced_frames: u64 = trace.iter().map(|t| u64::from(t.frames)).sum();

    println!("startup (time in us since open; none for the priming wake):");
    for (k, t) in trace.iter().enumerate() {
        println!(
            "  wake {k}: woke {:?}, padding read {:?}, padding {:?}, wrote {} frames, underrun {}",
            t.wake.map(|w| w.woke.0),
            t.wake.map(|w| w.read.0),
            t.wake.map(|w| w.padding),
            t.frames,
            t.underrun
        );
    }
    println!(
        "granted period: {} frames ({} us) at {} Hz, {} channels; buffer {} frames, {} queued periods (at most {}, widened {}); engine periods {:?}",
        g.period,
        g.period_us(),
        g.rate,
        g.channels,
        g.buffer,
        g.queue_periods,
        g.max_queue_periods,
        g.widened,
        g.engine_periods
    );
    println!(
        "fallback: {} ({:?}); mmcss: {}",
        g.fallback.is_some(),
        g.fallback,
        g.mmcss
    );
    let frames = traced_frames + report.frames;
    println!(
        "underruns: {}; steps {}, wakes {}, frames {} in {:?}; device position {:?} frames ({} read failures), mix clock rebases {}",
        engine.underruns(),
        TRACED as u64 + report.steps,
        stats.wakes,
        frames,
        ran,
        stats.position,
        stats.position_failures,
        engine.clock().rebases()
    );

    // Every period of the run was written: the device took what the engine
    // rendered, and that covers the 2 s (less the 5 % a late stop or start
    // can take).
    let frames = traced_frames + report.frames;
    assert_eq!(frames, engine.written());
    assert_eq!(frames, stats.frames);
    let expected = u64::from(g.rate) * RUN.as_millis() as u64 / 1_000;
    assert!(frames >= expected * 95 / 100, "{report:?}");
    // The priming wake is the only untimed one.
    assert_eq!(trace[0].wake, None);
    assert!(trace[1..].iter().all(|t| t.wake.is_some()));
    assert!(stats.wakes >= expected / u64::from(g.period) * 95 / 100);
    // The tone was scheduled once, on the mix clock.
    let mut tones = 0;
    engine.drain_scheduled(|s| {
        tones += 1;
        assert_eq!(s.kind, ScheduledKind::Bgm { time: TONE_AT });
        assert!(s.at.is_some());
    });
    assert_eq!(tones, 1);
    assert_eq!(engine.bgm_left(), 0);
    assert_eq!(counting::allocations(), 0, "the audio loop allocated");
    out.close().unwrap();
}
