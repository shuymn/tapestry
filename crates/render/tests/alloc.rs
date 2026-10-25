//! After warmup, a frame allocates nothing in the app's own code (design.md
//! "Scope of No Allocation"): the skin's build with effects, the batch, and
//! the renderer's code around its wgpu calls. Allocations inside wgpu run in
//! the library scope ([`render::in_library_call`]) and are counted apart.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! harness's own threads do not disturb it. This file holds a single test
//! for the same reason. It also prints the render CPU indicator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, input, lane, view, warp};
use eval::{JudgeEvent, JudgeEventKind};
use rules::Judgment;

struct Counting;

static APP: AtomicU64 = AtomicU64::new(0);
static LIBRARY: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        if render::in_library_call() {
            LIBRARY.fetch_add(1, Ordering::Relaxed);
        } else {
            APP.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: delegates to `System`; only adds counters, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
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
fn frame_does_not_allocate_after_warmup() {
    const WARMUP: i64 = 10;
    const FRAMES: i64 = 400;
    const FRAME_US: i64 = 16_667;
    // Taps 25 ms apart over every lane and a long note per key lane, with
    // BPM changes and a STOP, so every frame draws notes, bodies and the
    // effects of the judgments fed to it.
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(2_000 * MS), 180_000)
        .stop(TimeUs(3_000 * MS), TimeUs(250 * MS));
    let mut taps = Vec::new();
    for i in 0..240 {
        let l = u8::try_from(i % 8).unwrap();
        let t = TimeUs(500 * MS + i64::from(i) * 25 * MS);
        b = b.tap(lane(l), t);
        taps.push((l, t));
    }
    for l in 1..8 {
        let start = 7_000 + i64::from(l) * 100;
        b = b.long(lane(l), TimeUs(start * MS), TimeUs((start + 400) * MS));
    }
    let chart = b.build().unwrap();
    let mut r = warp(&chart);
    let v = view(42, 1_234);
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(64);
    let mut next = 0;
    let mut seq = 0;
    let mut cpu: Vec<Duration> = Vec::with_capacity(FRAMES as usize);
    let mut drawn = 0;

    TRACK.with(|t| t.set(true));
    // The count works: an allocation here is app code.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(APP.swap(0, Ordering::Relaxed), 1);
    for frame in 0..WARMUP + FRAMES {
        if frame == WARMUP {
            APP.store(0, Ordering::Relaxed);
            LIBRARY.store(0, Ordering::Relaxed);
        }
        let now = TimeUs(frame * FRAME_US);
        effects.clear();
        // Judge every tap that has reached the line.
        while next < taps.len() && taps[next].1 <= now && effects.len() < effects.capacity() {
            let (l, t) = taps[next];
            let lane_notes = &chart.lanes[usize::from(l)];
            let note = lane_notes.iter().position(|n| n.time == t).unwrap();
            effects.push(JudgeEvent {
                seq,
                time: t,
                origin: Some(t),
                kind: JudgeEventKind::Judged {
                    lane: lane(l),
                    note,
                    judgment: Judgment::ALL[next % 5],
                    deviation: Some(TimeUs(0)),
                    combo: u32::try_from(next).unwrap(),
                },
            });
            next += 1;
            seq += 1;
        }
        let timing = r.frame(&input(&v, now, TimeUs(8 * MS), &effects));
        assert!(timing.presented);
        if frame >= WARMUP {
            cpu.push(timing.cpu);
            drawn += timing.sprites;
        }
    }
    TRACK.with(|t| t.set(false));
    let app = APP.load(Ordering::Relaxed);
    let library = LIBRARY.load(Ordering::Relaxed);
    assert_eq!(next, taps.len(), "every tap judged");
    assert!(drawn > 0);
    assert_eq!(cpu.len(), FRAMES as usize);
    cpu.sort();
    let pick = |q: usize| cpu[(cpu.len() - 1) * q / 100].as_secs_f64() * 1e6;
    println!(
        "render cpu per frame on WARP over {FRAMES} frames: p50 {:.0} us, p99 {:.0} us, max {:.0} us",
        pick(50),
        pick(99),
        pick(100)
    );
    println!("allocations after warmup: app {app}, library {library}");
    assert_eq!(app, 0);
}
