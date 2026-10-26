//! A counting global allocator for allocation tests. It counts allocations
//! made on threads that opted in (a thread-local flag), so the test
//! harness's own threads do not disturb the count. Each test binary
//! registers it with `#[global_allocator]` and calls [`self_check`] where
//! it starts tracking.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// The allocator.
pub struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

/// Starts or stops counting this thread's allocations.
pub fn track(on: bool) {
    TRACK.set(on);
}

/// Allocations counted so far.
pub fn allocations() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
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
