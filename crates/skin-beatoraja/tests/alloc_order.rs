//! The header's record caps are checked before anything is gathered for
//! a record: one option with far more items than the cap allows refuses
//! with no allocation sized by the array.
//!
//! A global allocator records the largest single allocation made on a
//! thread that opts in (a thread-local flag), so the test harness's own
//! threads do not disturb it. This file holds a single test for the same
//! reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{play, skin};
use skin_beatoraja::limits::MAX_OPTION_ITEMS;
use skin_beatoraja::{Definition, ImportError, InputLimit, resolve};

struct Largest;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only records a size, which never
// allocates.
unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Largest = Largest;

/// The largest single allocation `f` makes on this thread.
fn largest<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LARGEST.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    TRACK.with(|t| t.set(false));
    (out, LARGEST.load(Ordering::Relaxed))
}

#[test]
fn option_items_are_counted_before_anything_is_gathered() {
    // Positive control: an allocation of 2 MiB is seen.
    let ((), seen) = largest(|| {
        black_box(vec![0u8; 2 << 20]);
    });
    assert!(seen >= 2 << 20, "{seen}");

    // One option whose item array is 64 times the cap.
    const ITEMS: usize = 64 * MAX_OPTION_ITEMS;
    let items = vec!["{}"; ITEMS].join(",");
    let d = skin(&[(
        "s.json",
        &play(&format!(r#""property": [{{"item": [{items}]}}]"#)),
    )]);
    let mut def = Definition::read(d.path(), "s.json").unwrap();
    let mut resolved = resolve(&mut def, &BTreeSet::new()).unwrap();
    let (result, seen) = largest(|| resolved.header().map(|_| ()));
    assert!(
        matches!(
            result,
            Err(ImportError::Input {
                limit: InputLimit::OptionItems,
                ..
            })
        ),
        "{result:?}"
    );
    // The items kept up to the cap, doubled while they grow, bound every
    // allocation; one id per item of the array would be 4 MiB.
    let bound = 2 * MAX_OPTION_ITEMS * size_of::<skin_beatoraja::OptionItem>();
    assert!(seen <= bound, "{seen} > {bound}");
}
