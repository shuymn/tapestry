//! P1 test (h): the mix loop, and the audio thread's whole signal path
//! (`Engine::render` with triggers), make no allocations.
//!
//! A counting global allocator counts allocations made on threads that
//! opted in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

use spike_core::engine::{Engine, Injection};
use spike_core::mixer::{MAX_VOICES, Mixer, SoundBank};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

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

// SAFETY: delegates to `System`; only adds a counter.
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

#[test]
fn h_mix_loop_makes_no_allocations() {
    // Everything allocated up front, as before the countdown.
    let bank = SoundBank {
        sounds: (0..64)
            .map(|k| (0..4_800).map(|i| ((i * (k + 3)) % 2_000) as i16).collect())
            .collect(),
    };
    let mut mixer = Mixer::new(MAX_VOICES, 96);
    let mut out = vec![0.0f32; 480 * 2];
    let mut engine = Engine::new(
        48_000,
        bank.clone(),
        Some(Injection {
            first: 1_000,
            every: 48_000,
            len: 96,
            count: 10,
        }),
    );
    let mut device = vec![0.0f32; 480 * 4];

    // Sanity: the counter sees allocations on this thread.
    TRACK.with(|t| t.set(true));
    drop(std::hint::black_box(vec![1u8; 16]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);

    // 10 s of 480-frame periods with a trigger every 11 samples (about 436 concurrent, so steals).
    let mut next_trigger = 0u64;
    for block in 0..1_000u64 {
        let start = block * 480;
        while next_trigger < start + 480 {
            mixer.trigger((next_trigger % 64) as u16, next_trigger, 0.01);
            next_trigger += 11;
        }
        out.fill(0.0);
        mixer.mix(&bank, start, &mut out);
    }
    // 10 s of the engine (4-channel device, odd block sizes), with a
    // trigger every 11 device frames.
    let mut next = 0u64;
    for block in 0..1_200u64 {
        let n = [480, 441, 333][(block % 3) as usize];
        let now = engine.device_frames();
        while next < now + 960 {
            engine.trigger_at_device((next % 64) as u16, next);
            next += 11;
        }
        engine.render(&mut device[..n * 4], 4, n);
    }
    TRACK.with(|t| t.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
    assert!(engine.mixer().steals > 0);
    assert!(mixer.steals > 0);
    assert!(mixer.active() > 0);
}
