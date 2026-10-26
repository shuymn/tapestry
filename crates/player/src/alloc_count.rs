//! The counting allocator of development builds (D13; design.md:860,
//! ported from the stage 0 spike's `alloc.rs`): every allocation is
//! counted against the thread that made it, by the tag the thread gave
//! itself, so the measurement overlay shows each play's allocations per
//! thread (PR17). The allocator itself never allocates: the tag is a
//! const-initialized thread-local and the counters are atomics.

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
    [1, 2, 3, 4].map(|i| COUNTS[i].load(Ordering::Relaxed))
}

/// This thread's allocations so far, whatever its tag.
#[cfg(test)]
pub fn this_thread() -> u64 {
    MINE.with(Cell::get)
}

fn note() {
    #[cfg(test)]
    let _ = MINE.try_with(|m| m.set(m.get() + 1));
    let t = TAG.try_with(Cell::get).unwrap_or(0);
    COUNTS[usize::from(t)].fetch_add(1, Ordering::Relaxed);
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
    use super::{Tag, counts, tag};

    /// A tagged thread's allocation counts against its tag only.
    #[test]
    fn allocations_count_against_the_thread_tag() {
        let h = std::thread::spawn(|| {
            tag(Tag::Audio);
            let before = counts();
            let v: Vec<u8> = Vec::with_capacity(64);
            std::hint::black_box(&v);
            let after = counts();
            (before, after)
        });
        let (before, after) = h.join().unwrap();
        assert!(after[2] > before[2]);
    }
}
