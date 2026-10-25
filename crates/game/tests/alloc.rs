//! After construction, a whole play allocates nothing: scripted input,
//! publication, the logic step, and draining the render side.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use common::{MS, autoplay_script, chart, count_judged, device, window_capacity};
use game::{LogicConfig, RunOutcome, Runner};
use input::SourceSet;
use rules::Judgment;

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

#[test]
fn logic_does_not_allocate_during_play() {
    const START: TimeUs = TimeUs(-1_000_000);
    // 300 notes 61 ms apart over every lane, every 5th a 40 ms long note,
    // and two gauges: each judgment emits three events.
    let chart = chart(300, 61 * MS, 40 * MS);
    let rule = rules::custom();
    let dev = device(&[2 * MS]);
    let (events, script) = autoplay_script(&chart, &dev);
    let config = LogicConfig {
        inputs: window_capacity(START, TimeUs(20_000 * MS), 8),
        effects: 1_024,
    };
    let sources = SourceSet::new(common::sources()).unwrap();
    let mut runner = Runner::new(&chart, &rule, &dev, sources, script, config, START).unwrap();

    TRACK.with(|t| t.set(true));
    let outcome = runner.run_to_end(TimeUs(MS));
    TRACK.with(|t| t.set(false));

    assert_eq!(outcome, RunOutcome::Complete);
    assert_eq!(events.len(), 600);
    assert_eq!(runner.record().events.len(), 600);
    assert_eq!(count_judged(runner.stream(), Judgment::Perfect), 300);
    assert_eq!(runner.effects().len(), runner.stream().len());
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
