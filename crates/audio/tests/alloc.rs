//! Once built, the engine allocates nothing per step: draining commands,
//! scheduling BGM, stealing voices, mixing, limiting and writing; nor does
//! the audio thread loop around it.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use audio::{
    AudioEngine, AudioOutput, OutputError, RecordingOutput, SHIFT_HISTORY, SimConfig, SimDevice,
    Sound, SoundBank, SoundCmd, Wake, run_audio,
};
use bms_model::{BgmEvent, SoundId, TimeUs};
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

/// Sets `stop` during its `left`th wait from now.
struct StopAfter<'a, O> {
    inner: &'a mut O,
    left: u32,
    stop: &'a AtomicBool,
}

impl<O: AudioOutput> AudioOutput for StopAfter<'_, O> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        self.left -= 1;
        if self.left == 0 {
            self.stop.store(true, Ordering::Release);
        }
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.inner.write(f)
    }
}

#[test]
fn mixing_does_not_allocate() {
    const RATE: u32 = 48_000;
    const STEPS: u32 = 3_000;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![20_000; 9_000]),
            Sound::stereo((0..4_000).map(|i| (i % 7_000) as i16 - 3_500).collect()),
        ],
    ));
    // BGM every 20 ms over the 30 s run; 8 voices, so voices are stolen.
    let bgm: Vec<BgmEvent> = (0..1_500)
        .map(|k| BgmEvent {
            time: TimeUs(k * 20_000),
            sound: SoundId(1),
        })
        .collect();
    let mut e = AudioEngine::new(bank, &bgm, RATE, 480, 8, 64).unwrap();
    // 40 stalls of three missed wakes, two underruns each: more than the
    // simulator's shift history holds.
    let missed_wakes = (0..40u64)
        .flat_map(|k| [50 + 70 * k, 51 + 70 * k, 52 + 70 * k])
        .collect();
    let dev = SimDevice::new(SimConfig {
        missed_wakes,
        drift_ppm: 80,
        jitter_us: 1_500,
        seed: 11,
        ..SimConfig::steady(RATE, 480, TimeUs(0))
    });
    let mut out = RecordingOutput::new(dev, 48_000);
    let (mut tx, mut rx) = RingBuffer::<SoundCmd>::new(256);

    let mut frames = 0u64;
    let mut commands = 0u32;
    TRACK.set(true);
    for step in 0..STEPS {
        for k in 0..3 {
            tx.push(SoundCmd {
                sound: SoundId(0),
                origin: TimeUs(i64::from(step) * 10_000 + k),
            })
            .unwrap();
        }
        let r = e.step(&mut out, &mut rx).unwrap();
        frames += u64::from(r.frames);
        commands += r.commands;
        e.drain_scheduled(|_| {});
    }
    let stop = AtomicBool::new(false);
    let mut looped = StopAfter {
        inner: &mut out,
        left: 500,
        stop: &stop,
    };
    let run = run_audio(&mut looped, &mut e, &mut rx, &stop).unwrap();
    TRACK.set(false);

    assert_eq!(run.steps, 500);
    assert_eq!(commands, 3 * STEPS);
    assert!(frames > u64::from(STEPS) * 480, "{frames}");
    assert!(out.inner().underruns() > SHIFT_HISTORY as u64);
    assert_eq!(e.underruns(), 40);
    assert!(e.mixer().steals() > 1_000, "{}", e.mixer().steals());
    assert!(e.mixer().limiter().limited() > 0);
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
