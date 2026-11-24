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
    /// This thread's frees, for the allocation tests.
    #[cfg(test)]
    static FREED: Cell<u64> = const { Cell::new(0) };
    /// The address whose free this thread watches (0 for none), for the
    /// release witnesses.
    #[cfg(test)]
    static WATCHED: Cell<usize> = const { Cell::new(0) };
    /// Whether this thread freed the watched address since it was set.
    #[cfg(test)]
    static WATCHED_FREED: Cell<bool> = const { Cell::new(false) };
    /// The bytes this thread allocated less those it freed since
    /// [`reset_peak`], and their largest value since (tests).
    #[cfg(test)]
    static HELD: Cell<(i64, i64)> = const { Cell::new((0, 0)) };
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

/// Leaves room in libtest's output capture for the lines a counted section
/// prints (tests): `cargo test`, as CI's release allocation step runs, puts
/// every print of a test's threads into one buffer, which allocates when it
/// grows; a process's own stdout and stderr do not. A line of 64 KiB (a
/// format width's most) grows it past that, then spaces are printed until
/// it grows once more: it doubles, so at least 64 KiB are left free, and a
/// buffer never shrinks. Without a capture (nextest) nothing grows, and the
/// spaces stop at their bound.
#[cfg(test)]
pub fn room_for_output() {
    println!("{:1$}", "", usize::from(u16::MAX));
    for _ in 0..1 << 17 {
        let before = this_thread();
        print!(" ");
        if this_thread() != before {
            break;
        }
    }
    println!();
}

/// This thread's frees so far, whatever its tag.
#[cfg(test)]
pub fn this_thread_frees() -> u64 {
    FREED.with(Cell::get)
}

/// Watches this thread's free of the allocation at `addr` from now on: a
/// release witness for one identified allocation, which no other can share
/// while it lives.
#[cfg(test)]
pub fn watch_free(addr: usize) {
    WATCHED.with(|w| w.set(addr));
    WATCHED_FREED.with(|f| f.set(false));
}

/// Whether this thread freed the watched allocation since [`watch_free`].
#[cfg(test)]
pub fn watched_freed() -> bool {
    WATCHED_FREED.with(Cell::get)
}

/// Starts this thread's byte peak again from now (tests).
#[cfg(test)]
pub fn reset_peak() {
    HELD.set((0, 0));
}

/// The most bytes this thread held at once since [`reset_peak`], counting
/// what it allocated less what it freed (tests).
#[cfg(test)]
pub fn peak_bytes() -> u64 {
    u64::try_from(HELD.get().1).unwrap_or(0)
}

/// Adds `delta` bytes to this thread's held bytes (tests).
#[cfg(test)]
fn held(delta: i64) {
    let _ = HELD.try_with(|h| {
        let (now, peak) = h.get();
        let now = now.saturating_add(delta);
        h.set((now, peak.max(now)));
    });
}

/// `n` bytes as a signed count (tests).
#[cfg(test)]
fn signed(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
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

// SAFETY: delegates to `System`; only adds a counter, which never allocates
// (tests: or returns null for the one allocation `alloc_refusal` chose, as
// an allocator out of memory does).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(test)]
        if crate::alloc_refusal::refuses(layout.size()) {
            return std::ptr::null_mut();
        }
        note();
        #[cfg(test)]
        held(signed(layout.size()));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        #[cfg(test)]
        if crate::alloc_refusal::refuses(layout.size()) {
            return std::ptr::null_mut();
        }
        note();
        #[cfg(test)]
        held(signed(layout.size()));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(test)]
        let _ = FREED.try_with(|f| f.set(f.get() + 1));
        #[cfg(test)]
        held(-signed(layout.size()));
        #[cfg(test)]
        if WATCHED
            .try_with(Cell::get)
            .is_ok_and(|w| w != 0 && w == ptr.addr())
        {
            let _ = WATCHED_FREED.try_with(|f| f.set(true));
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        #[cfg(test)]
        if crate::alloc_refusal::refuses(new_size) {
            return std::ptr::null_mut();
        }
        note();
        #[cfg(test)]
        held(signed(new_size) - signed(layout.size()));
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
