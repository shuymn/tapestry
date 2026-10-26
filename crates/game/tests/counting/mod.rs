//! A counting global allocator for the audio allocation tests: it counts
//! allocations and frees made on threads that opted in (a thread-local
//! flag), so the harness's threads and the game-side work between steps
//! are not counted. Each test binary installs it itself:
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
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts or stops counting on this thread.
pub fn track(on: bool) {
    TRACK.with(|t| t.set(on));
}

/// Allocations and frees counted so far, on every tracked thread.
pub fn calls() -> u64 {
    CALLS.load(Ordering::Relaxed)
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
