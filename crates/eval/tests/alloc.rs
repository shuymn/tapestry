//! After construction, the evaluator allocates nothing while inputs are
//! pushed and the watermark advances.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! It also prints the evaluator time per input, an indicator only.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{Evaluator, InputStart, JudgeEventKind};
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
fn advance_does_not_allocate() {
    const NOTES: i64 = 2_000;
    let mut builder = ChartBuilder::new(150_000);
    let mut inputs = Vec::new();
    for i in 0..NOTES {
        let n = u8::try_from(i % 7).unwrap() + 1;
        let t = 1_000 * MS + i * 50 * MS;
        builder = builder.tap(lane(n), TimeUs(t));
        inputs.push(press(t, 0, n));
        inputs.push(release(t + 20 * MS, 0, n));
    }
    let chart = builder.build().unwrap();
    let preset = rules::custom();
    let dev = device(&[0]);
    let end = 1_000 * MS + NOTES * 50 * MS + 1_000 * MS;
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 64).unwrap();
    let mut out = Vec::with_capacity(4 * 2_000);

    TRACK.with(|t| t.set(true));
    let started = Instant::now();
    let mut next = 0;
    let mut w = 0;
    while w <= end {
        // What the merge would hand over at this watermark.
        while next < inputs.len() && inputs[next].received.0 <= w {
            ev.push_input(inputs[next]);
            next += 1;
        }
        ev.advance_to(TimeUs(w), &mut out);
        w += MS;
    }
    let elapsed = started.elapsed();
    TRACK.with(|t| t.set(false));

    let allocations = ALLOCS.load(Ordering::Relaxed);
    let perfect = out
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                JudgeEventKind::Judged {
                    judgment: Judgment::Perfect,
                    ..
                }
            )
        })
        .count();
    assert_eq!(perfect, 2_000);
    assert_eq!(out.len(), 2_000);
    assert_eq!(allocations, 0);
    let per_input = elapsed.as_nanos() / u128::try_from(inputs.len()).unwrap();
    println!(
        "indicator: {} inputs and {} watermark steps in {elapsed:?}, {per_input} ns per input (steps included)",
        inputs.len(),
        end / MS + 1
    );
}
