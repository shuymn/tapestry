//! A burst of plays started faster than the audio thread steps: every
//! retired command queue comes back to the game thread to be dropped
//! there, none is leaked, and the audio thread neither allocates nor frees.
//!
//! A counting global allocator counts allocations and frees on the audio
//! thread only (a thread-local flag). This file holds a single test.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{ChartBuilder, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use game::{audio_control, audio_link_at};

struct Counting;

static CALLS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        CALLS.fetch_add(1, Ordering::Relaxed);
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
        note();
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

/// Starts `PLAYS` plays as fast as the control takes them while another
/// thread steps the audio loop. With `keep`, the test keeps each play's
/// sink and returns how many saw their queue dropped; without, the sinks go
/// at once, as a finished play's logic step drops them, so dropping a
/// queue frees it and a free on the audio thread is counted.
fn burst(keep: bool) -> usize {
    let chart = ChartBuilder::new(150_000)
        .sound("s.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let stop = AtomicBool::new(false);
    let mut sinks = Vec::with_capacity(PLAYS);
    thread::scope(|s| {
        let audio = s.spawn(|| {
            TRACK.with(|t| t.set(true));
            while !stop.load(Ordering::Acquire) {
                worker.step(&mut out).unwrap();
                thread::yield_now();
            }
            TRACK.with(|t| t.set(false));
            worker
        });
        for play in 0..PLAYS {
            let origin = TimeUs(i64::try_from(play).unwrap() * 10 * MS);
            let (sink, queue) = audio_link_at(4, origin);
            if keep {
                sinks.push(sink);
            }
            control.play(origin, queue);
        }
        stop.store(true, Ordering::Release);
        drop(audio.join().unwrap());
    });
    drop(control);
    sinks.iter().filter(|s| s.is_abandoned()).count()
}

const PLAYS: usize = 50;

#[test]
fn a_burst_of_plays_returns_every_queue_without_audio_side_frees() {
    // Every queue came back (the last one with the worker, dropped after
    // it): none was leaked.
    assert_eq!(burst(true), PLAYS);
    assert_eq!(CALLS.load(Ordering::Relaxed), 0);
    // Queues whose last owner is the retired end are freed on the game
    // thread, never the audio thread.
    assert_eq!(burst(false), 0);
    assert_eq!(CALLS.load(Ordering::Relaxed), 0);
}
