//! The audio thread neither allocates nor frees across retries: each
//! play's command queue goes back to the game thread to be dropped there,
//! and the control messages travel through a preallocated queue.
//!
//! A counting global allocator counts allocations and frees made while the
//! test thread runs the audio loop's steps (a thread-local flag), so the
//! game-side work between them is not counted. This file holds a single
//! test for the same reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use eval::{JudgeEvent, JudgeEventKind};
use game::{EventSink, audio_control, audio_link_at};

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

#[test]
fn audio_loop_neither_allocates_nor_frees_across_retries() {
    let chart = ChartBuilder::new(150_000)
        .sound("s.wav")
        .bgm(TimeUs(0), SoundId(0))
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let mut steps = |worker: &mut game::AudioWorker, n: usize| {
        TRACK.with(|t| t.set(true));
        for _ in 0..n {
            worker.step(&mut out).unwrap();
        }
        TRACK.with(|t| t.set(false));
    };
    steps(&mut worker, 5);
    // The first play and 3 retries, each with key sounds; the second is
    // cancelled and the last sealed.
    for play in 0..4_i64 {
        let origin = TimeUs((100 + play * 400) * MS);
        let (mut sink, queue) = audio_link_at(64, origin);
        control.play(origin, queue);
        for k in 0..3 {
            sink.receive(&JudgeEvent {
                seq: k,
                time: TimeUs(0),
                origin: Some(TimeUs(i64::try_from(k).unwrap() * 20 * MS)),
                kind: JudgeEventKind::PlaySound {
                    lane: lane(1),
                    sound: SoundId(0),
                },
            });
        }
        match play {
            1 => control.cancel(),
            3 => control.seal(),
            _ => {}
        }
        steps(&mut worker, 40);
    }
    assert!(control.done());
    assert_eq!(CALLS.load(Ordering::Relaxed), 0);
}
