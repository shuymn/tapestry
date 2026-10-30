//! A counting global allocator for the audio allocation tests: it counts
//! allocations and frees made on threads that opted in (a thread-local
//! flag), so the harness's threads and the game-side work between steps
//! are not counted. Each test binary installs it itself, and calls
//! [`self_check`] on each thread it tracks:
//!
//! ```ignore
//! #[global_allocator]
//! static GLOBAL: counting::Counting = counting::Counting;
//! ```

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counts allocations and frees on tracked threads.
pub struct Counting;

static CALLS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static MINE: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        let _ = MINE.try_with(|c| c.set(c.get() + 1));
    }
}

/// Allocations and frees counted so far on this thread alone, which no
/// other thread can move.
pub fn thread_calls() -> u64 {
    MINE.with(Cell::get)
}

/// Starts or stops counting on this thread.
pub fn track(on: bool) {
    TRACK.with(|t| t.set(on));
}

/// Allocations and frees counted so far, on every tracked thread.
pub fn calls() -> u64 {
    CALLS.load(Ordering::Relaxed)
}

/// The positive control, called while this thread is tracked and no other
/// thread is: one allocation that escapes through `black_box`, so no
/// profile elides it, and its free must both be counted. They are taken
/// back out of the count, so a test's bound stays zero, and a build whose
/// count never moves fails here instead of passing.
pub fn self_check() {
    let before = calls();
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(calls() - before, 2, "the allocation was not counted");
    CALLS.fetch_sub(2, Ordering::Relaxed);
}

/// The positive control of [`thread_calls`], called while this thread is
/// tracked: other tracked threads may run meanwhile, and a test compares
/// this thread's counts before and after what it checks.
pub fn thread_self_check() {
    let mine = thread_calls();
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(thread_calls() - mine, 2, "the allocation was not counted");
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
