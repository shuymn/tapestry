//! The counting allocator of development builds (D13; design.md:860,
//! ported from the stage 0 spike's `alloc.rs`): every allocation is
//! counted against the thread that made it, by the tag the thread gave
//! itself, so the measurement overlay shows each play's allocations per
//! thread (PR17). A thread also counts what it allocates inside its app
//! region apart, which the render thread marks around its own per-frame
//! work (design.md "Scope of No Allocation"). The allocator itself never
//! allocates: the tag, the region flag and its count are const-initialized
//! thread-locals and the counters are atomics.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// The threads counted apart; untagged threads count together, unshown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    /// The input thread.
    Input = 1,
    /// The game thread, which runs the logic step.
    Logic = 2,
    /// The audio thread.
    Audio = 3,
    /// The render thread.
    Render = 4,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
static COUNTS: [AtomicU64; 5] = [ZERO; 5];

thread_local! {
    static TAG: Cell<u8> = const { Cell::new(0) };
    /// Whether this thread is inside its app region.
    static IN_APP: Cell<bool> = const { Cell::new(false) };
    /// This thread's allocations inside its app region.
    static APP: Cell<u64> = const { Cell::new(0) };
    /// This thread's allocations, for the allocation tests.
    #[cfg(test)]
    static MINE: Cell<u64> = const { Cell::new(0) };
}

/// Counts this thread's allocations under `t` from now on.
pub fn tag(t: Tag) {
    TAG.set(t as u8);
}

/// Allocations so far: input, logic, audio and render.
pub fn counts() -> [u64; 4] {
    let [_, input, logic, audio, render] = &COUNTS;
    [input, logic, audio, render].map(|c| c.load(Ordering::Relaxed))
}

/// Enters (`true`) or leaves this thread's app region.
pub fn app_region(on: bool) {
    IN_APP.set(on);
}

/// This thread's allocations inside its app region so far.
pub fn app_allocations() -> u64 {
    APP.get()
}

/// This thread's allocations so far, whatever its tag.
#[cfg(test)]
pub fn this_thread() -> u64 {
    MINE.with(Cell::get)
}

fn note() {
    #[cfg(test)]
    let _ = MINE.try_with(|m| m.set(m.get() + 1));
    if IN_APP.try_with(Cell::get).unwrap_or(false) {
        let _ = APP.try_with(|a| a.set(a.get() + 1));
    }
    let t = TAG.try_with(Cell::get).unwrap_or(0);
    // Every tag is below the counters' length; an untagged thread counts
    // at 0.
    if let Some(c) = COUNTS.get(usize::from(t)) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// The allocator.
pub struct Counting;

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
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

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use super::{Tag, app_allocations, app_region, counts, tag};

    /// A tagged thread's allocation counts against its tag only.
    #[test]
    fn allocations_count_against_the_thread_tag() {
        let h = std::thread::spawn(|| {
            tag(Tag::Audio);
            let before = counts();
            let v: Vec<u8> = Vec::with_capacity(64);
            black_box(&v);
            let after = counts();
            (before, after)
        });
        let (before, after) = h.join().unwrap();
        assert!(after[2] > before[2]);
    }

    /// Only what a thread allocates inside its app region counts as the
    /// region's.
    #[test]
    fn only_the_app_region_counts_as_app() {
        let h = std::thread::spawn(|| {
            let before = app_allocations();
            black_box(Vec::<u8>::with_capacity(64));
            let outside = app_allocations() - before;
            app_region(true);
            black_box(Vec::<u8>::with_capacity(64));
            app_region(false);
            black_box(Vec::<u8>::with_capacity(64));
            (outside, app_allocations() - before)
        });
        assert_eq!(h.join().unwrap(), (0, 1));
    }
}
