//! WASAPI output on the default render endpoint. Needs a device, so the
//! test is ignored; run it with `--ignored --nocapture`. It plays a quiet
//! tone for about 2 s.
//!
//! A counting global allocator counts allocations made on threads that
//! opted in (a thread-local flag), so the audio loop can be checked for
//! allocations without the harness's threads disturbing the count.

#![cfg(windows)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use audio::{AudioEngine, RunReport, ScheduledKind, SoundBank, SoundCmd, WasapiShared, run_audio};
use bms_model::{BgmEvent, SoundId, TimeSource, TimeUs};
use rtrb::RingBuffer;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// µs since the clock was made; copies share the origin.
#[derive(Clone, Copy)]
struct Elapsed(Instant);

impl TimeSource for Elapsed {
    fn now(&self) -> TimeUs {
        TimeUs(self.0.elapsed().as_micros() as i64)
    }
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
    let report: RunReport = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(RUN);
            stop.store(true, Ordering::Release);
        });
        TRACK.set(true);
        let r = run_audio(&mut out, &mut engine, &mut rx, &stop);
        TRACK.set(false);
        r.unwrap()
    });
    let ran = started.elapsed();
    let stats = *out.stats();

    println!(
        "granted period: {} frames ({} us) at {} Hz, {} channels; buffer {} frames, {} queued periods; engine periods {:?}",
        g.period,
        g.period_us(),
        g.rate,
        g.channels,
        g.buffer,
        g.queue_periods,
        g.engine_periods
    );
    println!(
        "fallback: {} ({:?}); mmcss: {}",
        g.fallback.is_some(),
        g.fallback,
        g.mmcss
    );
    println!(
        "underruns: {}; steps {}, wakes {}, frames {} in {:?}; device position {:?} frames ({} read failures), mix clock rebases {}",
        engine.underruns(),
        report.steps,
        stats.wakes,
        report.frames,
        ran,
        stats.position,
        stats.position_failures,
        engine.clock().rebases()
    );

    // Every period of the run was written: the device took what the engine
    // rendered, and that covers the 2 s (less the 5 % a late stop or start
    // can take).
    assert_eq!(report.frames, engine.written());
    assert_eq!(report.frames, stats.frames);
    let expected = u64::from(g.rate) * RUN.as_millis() as u64 / 1_000;
    assert!(report.frames >= expected * 95 / 100, "{report:?}");
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
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "the audio loop allocated"
    );
    out.close().unwrap();
}
