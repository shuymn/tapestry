//! A counting global allocator for allocation tests. It counts allocations
//! made on threads that opted in (a thread-local flag), so the test
//! harness's own threads do not disturb the count. Each test binary
//! registers it with `#[global_allocator]` and calls [`self_check`] where
//! it starts tracking.
//!
//! On a tracked thread it also keeps the bytes allocated, the bytes live
//! and their peak, and, while [`watch_all`] is on, the largest single
//! allocation on any thread.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The allocator.
pub struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static WATCH_ALL: AtomicBool = AtomicBool::new(false);
static LARGEST: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    // Signed: a tracked thread may free what it allocated before tracking.
    static LIVE: Cell<i64> = const { Cell::new(0) };
    static PEAK: Cell<i64> = const { Cell::new(0) };
    static TOTAL: Cell<u64> = const { Cell::new(0) };
}

/// Starts or stops counting this thread's allocations.
pub fn track(on: bool) {
    TRACK.set(on);
}

/// Allocations counted so far.
pub fn allocations() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

/// Starts measuring this thread's peak from the bytes live now.
pub fn reset_peak() {
    PEAK.set(LIVE.get());
}

/// The most bytes this thread held since [`reset_peak`], above what it held
/// then.
pub fn peak_bytes(since: i64) -> u64 {
    u64::try_from(PEAK.get() - since).unwrap_or(0)
}

/// Bytes this thread has allocated while tracked, freed or not; a
/// reallocation counts its new size.
pub fn allocated_bytes() -> u64 {
    TOTAL.get()
}

/// Bytes this thread allocated and has not freed while tracked.
pub fn live_bytes() -> i64 {
    LIVE.get()
}

/// Starts or stops recording the largest allocation on every thread, from
/// zero.
pub fn watch_all(on: bool) {
    LARGEST.store(0, Ordering::Relaxed);
    WATCH_ALL.store(on, Ordering::Relaxed);
}

/// The largest allocation, in bytes, since [`watch_all`] was turned on.
pub fn largest() -> u64 {
    LARGEST.load(Ordering::Relaxed)
}

/// The positive control, called while this thread is tracked: one
/// allocation that escapes through `black_box`, so no profile elides it,
/// must be counted. It is taken back out of the count, so a test's bound
/// stays zero, and a build whose count never moves fails here instead of
/// passing.
pub fn self_check() {
    let before = allocations();
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(allocations() - before, 1, "the allocation was not counted");
    ALLOCS.fetch_sub(1, Ordering::Relaxed);
}

fn note(size: usize) {
    if WATCH_ALL.load(Ordering::Relaxed) {
        LARGEST.fetch_max(size as u64, Ordering::Relaxed);
    }
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        let _ = TOTAL.try_with(|total| total.set(total.get() + size as u64));
        let _ = LIVE.try_with(|live| {
            let now = live.get() + size as i64;
            live.set(now);
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
        });
    }
}

fn freed(size: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        let _ = LIVE.try_with(|live| live.set(live.get() - size as i64));
    }
}

// SAFETY: delegates to `System`; only adds counters, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        freed(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Counted as the new block allocated before the old one is freed,
        // which is the most a reallocation holds at once.
        note(new_size);
        freed(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
