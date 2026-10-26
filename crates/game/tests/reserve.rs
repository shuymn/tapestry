//! What a play reserves before its countdown, measured: the bytes the
//! constructors allocate for the logic step and the live play's buffers
//! match [`Reservation`], the recording policy's account (stage1.md D14).
//!
//! A global allocator sums the bytes allocated on threads that opted in;
//! this file holds a single test.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, lane};
use eval::{InputStart, JudgeEvent};
use game::{
    Logic, LogicConfig, Reservation, SMALL_PARTS_BYTES, TABLE_BYTES_PER_OBJECT, audio_link_at,
};
use input::source_queue;

struct Summing;

static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn add(n: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        BYTES.fetch_add(n, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds to a counter, which never
// allocates.
unsafe impl GlobalAlloc for Summing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size());
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size);
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Summing = Summing;

#[test]
fn reservation_matches_what_a_play_allocates() {
    // 300 taps, 40 long notes and 25 mines, over three sources.
    let mut b = ChartBuilder::new(150_000);
    for i in 0..300 {
        let l = lane(u8::try_from(i % 6).unwrap() + 1);
        b = b.tap(l, TimeUs(1_000 * MS + i * 37 * MS));
    }
    for i in 0..40 {
        let t = 2_000 * MS + i * 300 * MS;
        b = b.long(lane(0), TimeUs(t), TimeUs(t + 100 * MS));
    }
    for i in 0..25 {
        b = b.mine(lane(7), TimeUs(1_017 * MS + i * 401 * MS), 3);
    }
    let chart = b.build().unwrap();
    let objects: usize = chart.lanes.iter().map(Vec::len).sum();
    let rule = rules::custom();
    let dev = device(&[0, 0, 0]);
    let effects = 1_000;
    let r = Reservation::of(&chart, &rule, &dev, effects).unwrap();
    // A play's reservations, as `live` makes them: the logic step, the
    // audio command queue and the copy of the events.
    let measure = |inputs: usize| {
        let readers: Vec<_> = (0..3).map(|_| source_queue(4).1).collect();
        let config = LogicConfig { inputs, effects };
        BYTES.store(0, Ordering::Relaxed);
        TRACK.with(|t| t.set(true));
        let (logic, side) = Logic::new(
            &chart,
            &rule,
            &dev,
            &InputStart::default(),
            readers,
            config,
            Vec::new(),
        )
        .unwrap();
        let audio = audio_link_at(logic.max_events(), TimeUs(0));
        let live: Vec<JudgeEvent> = Vec::with_capacity(logic.max_events());
        TRACK.with(|t| t.set(false));
        drop((logic, side, audio, live));
        BYTES.load(Ordering::Relaxed)
    };
    let (small, large) = (1_000, 101_000);
    let (at_small, at_large) = (measure(small), measure(large));
    // Every byte that grows with the capacity is counted exactly.
    assert_eq!(at_large - at_small, r.per_input * (large - small));
    // The rest is the chart's tables and small fixed parts, within their
    // allowances.
    assert!(
        at_small <= r.bytes(small),
        "{at_small} > {}",
        r.bytes(small)
    );
    assert!(
        r.bytes(small) - at_small <= objects * TABLE_BYTES_PER_OBJECT + SMALL_PARTS_BYTES,
        "{} unaccounted",
        r.bytes(small) - at_small
    );
    println!(
        "indicator: {} bytes per input; fixed {} bytes, measured fixed {}",
        r.per_input,
        r.fixed,
        at_small - r.per_input * small
    );
}
