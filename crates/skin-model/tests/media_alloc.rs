//! Reading a video's frame ring allocates nothing (design.md "Scope of No
//! Allocation"; "Rendering and Skins", video frames): a poll that finds
//! nothing due, one that copies a frame of two chunks, one that frees the
//! frames of an earlier run unread, starting a run, a poll that fails the
//! player and the polls after it, and reading the staged frame. Only setting
//! the ring up allocates, its staging buffer, at preparation.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use skin_model::media::{Fault, Geometry, Reader, SLOT_HEADER_WORDS, Window, Writer};

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

/// The allocations `f` makes on this thread.
fn counted<T>(f: impl FnOnce() -> T) -> (u64, T) {
    let before = ALLOCS.load(Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    TRACK.with(|t| t.set(false));
    (ALLOCS.load(Ordering::Relaxed) - before, out)
}

#[test]
fn ring_reads_do_not_allocate() {
    // The positive control: the counter sees an allocation on this thread.
    let (n, ()) = counted(|| {
        std::hint::black_box(Vec::<u8>::with_capacity(64));
    });
    assert!(n > 0, "the counter missed an allocation");

    // Two chunks a frame.
    let g = Geometry::new(255, 193).unwrap();
    let mem: Vec<AtomicU64> = (0..g.words()).map(|_| AtomicU64::new(0)).collect();
    let ring = &*mem;
    let window = Window {
        first: 0,
        last: 60_000_000,
    };
    let (n, reader) = counted(|| Reader::new(ring, g, window).unwrap());
    assert!(n > 0, "setting the ring up allocates its staging buffer");
    let mut reader = reader;
    let writer = Writer::attach(ring).unwrap();
    let frame: Vec<u8> = (0..g.frame_bytes()).map(|i| (i % 253) as u8).collect();
    let publish = |seq: u64, run: u64, stamp: i64| {
        let slot = writer.claim(ring).unwrap();
        writer.publish(ring, slot, &frame, seq, run, stamp).unwrap();
    };
    publish(1, 1, 1_000);
    publish(2, 1, 2_000);

    let (n, polled) = counted(|| reader.poll(ring, 500));
    assert_eq!((n, polled.unwrap().staged), (0, None), "nothing due");
    let (n, polled) = counted(|| reader.poll(ring, 1_500));
    assert_eq!(n, 0, "a copy");
    assert_eq!(polled.unwrap().staged.unwrap().seq, 1);
    let (n, staged) = counted(|| reader.staged().map(|(f, w)| (f.seq, w.len())));
    assert_eq!(
        (n, staged),
        (0, Some((1, g.frame_words()))),
        "the staged frame"
    );
    let (n, run) = counted(|| reader.start_run(window));
    assert_eq!((n, run), (0, 2), "a new run");
    let (n, polled) = counted(|| reader.poll(ring, 5_000));
    assert_eq!(n, 0, "frame 2 of run 1 freed unread");
    assert!(polled.unwrap().freed);

    publish(3, 2, 3_000);
    mem[g.frame_start(0) - SLOT_HEADER_WORDS + 4].store(9, Ordering::Relaxed);
    let (n, polled) = counted(|| reader.poll(ring, 5_000));
    assert_eq!(n, 0, "a fault");
    assert_eq!(polled, Err(Fault::Index { slot: 0, value: 9 }));
    let (n, polled) = counted(|| reader.poll(ring, 6_000));
    assert_eq!(
        (n, polled),
        (0, Err(Fault::Index { slot: 0, value: 9 })),
        "a kept fault"
    );
}
