//! The counting allocator of development builds (design.md "Scope of No
//! Allocation", ported from the stage 0 spike's `alloc.rs`): every allocation
//! is counted against the thread that made it, by the tag the thread gave
//! itself, so the measurement overlay shows each play's allocations per thread.
//! A thread also counts what it allocates inside its app region apart, which
//! the render thread marks around its own per-frame work (design.md "Scope of
//! No Allocation"). The allocator itself never allocates: the tag, the region
//! depth and its count are const-initialized thread-locals and the counters are
//! atomics.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::marker::PhantomData;
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
    /// How many of this thread's app region guards are alive: it is inside
    /// the region while any is.
    static IN_APP: Cell<u32> = const { Cell::new(0) };
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

/// This thread's app region, entered when made and left when dropped: on
/// every way out of the code it spans, an early return with an error or an
/// unwind included, so nothing after it, such as the error's report, is
/// counted as the region's. Guards nest, the region lasting while any of
/// them lives; a guard stays on the thread that made it, whose region it
/// marks.
#[must_use = "the app region is left when this is dropped"]
pub struct AppRegion(PhantomData<*const ()>);

impl AppRegion {
    /// Enters this thread's app region.
    pub fn enter() -> Self {
        IN_APP.set(IN_APP.get().saturating_add(1));
        Self(PhantomData)
    }
}

impl Drop for AppRegion {
    fn drop(&mut self) {
        IN_APP.set(IN_APP.get().saturating_sub(1));
    }
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
    if IN_APP.try_with(Cell::get).unwrap_or(0) > 0 {
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

    use super::{AppRegion, Tag, app_allocations, counts, tag};

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
            let region = AppRegion::enter();
            black_box(Vec::<u8>::with_capacity(64));
            drop(region);
            black_box(Vec::<u8>::with_capacity(64));
            (outside, app_allocations() - before)
        });
        assert_eq!(h.join().unwrap(), (0, 1));
    }

    /// Work in the region that leaves it early with an error: the region
    /// ends on that way out, so the error's report made after it, which
    /// allocates, is not the region's.
    #[test]
    fn the_app_region_ends_on_an_early_error() {
        fn work(fail: bool) -> Result<(), &'static str> {
            let _region = AppRegion::enter();
            black_box(Vec::<u8>::with_capacity(64));
            if fail {
                return Err("refused");
            }
            Ok(())
        }
        let h = std::thread::spawn(|| {
            let before = app_allocations();
            let e = work(true).unwrap_err();
            let report = black_box(format!("render: {e}"));
            assert!(!report.is_empty());
            work(false).unwrap();
            app_allocations() - before
        });
        assert_eq!(h.join().unwrap(), 2);
    }

    /// A guard stays on its thread: were it `Send`, both impls below would
    /// apply and the item's path would be ambiguous, which fails to compile.
    const _: fn() = || {
        trait AmbiguousIfSend<A> {
            fn item() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        let _ = <AppRegion as AmbiguousIfSend<_>>::item;
    };

    /// A region inside another: leaving the inner one keeps the outer one,
    /// and the thread is outside the region once both are left.
    #[test]
    fn a_nested_region_keeps_the_outer_one() {
        let h = std::thread::spawn(|| {
            let before = app_allocations();
            let outer = AppRegion::enter();
            drop(AppRegion::enter());
            black_box(Vec::<u8>::with_capacity(64));
            drop(outer);
            black_box(Vec::<u8>::with_capacity(64));
            app_allocations() - before
        });
        assert_eq!(h.join().unwrap(), 1);
    }

    /// Work in the region that unwinds: the region ends on that way out, so
    /// what the thread allocates after catching it is not the region's.
    #[test]
    fn the_app_region_ends_on_an_unwind() {
        let h = std::thread::spawn(|| {
            let caught = std::panic::catch_unwind(|| {
                let _region = AppRegion::enter();
                std::panic::resume_unwind(Box::new(()));
            });
            assert!(caught.is_err());
            let before = app_allocations();
            black_box(Vec::<u8>::with_capacity(64));
            app_allocations() - before
        });
        assert_eq!(h.join().unwrap(), 0);
    }
}
