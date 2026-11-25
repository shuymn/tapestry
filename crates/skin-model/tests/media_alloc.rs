//! Reading a video's frame ring allocates nothing (design.md "Scope of No
//! Allocation"; "Rendering and Skins", video frames): setting the ring up, a
//! peek that finds nothing due, a peek and the take that copies a frame of
//! two chunks into a sink at a pitch, one that frees the frames of an
//! earlier run unread, starting a run, a first peek and its take (a
//! movie's oldest frame), a peek that fails the player and the peeks and
//! takes after it.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use skin_model::media::{
    ChromaLocation, Fault, Geometry, Reader, SLOT_HEADER_WORDS, Stamp, Window, Writer,
};

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
    assert_eq!(n, 0, "setting the ring up");
    let mut reader = reader;
    // The sink: a buffer at a pitch wider than the rows, as an upload
    // buffer's, allocated before.
    let pitch = [256, 128];
    let mut out = vec![0_u8; reader.pitched_bytes(pitch)];
    let mut written = 0;
    let writer = Writer::attach(ring).unwrap();
    let frame: Vec<u8> = (0..g.frame_bytes()).map(|i| (i % 253) as u8).collect();
    let publish = |seq: u64, run: u64, stamp: i64| {
        let slot = writer.claim(ring).unwrap();
        writer
            .publish(
                ring,
                slot,
                &frame,
                seq,
                run,
                Stamp {
                    at: stamp,
                    duration: 33_333,
                },
                ChromaLocation::Left,
            )
            .unwrap();
    };
    publish(1, 1, 1_000);
    publish(2, 1, 2_000);

    let (n, peeked) = counted(|| reader.peek(ring, 500));
    assert_eq!((n, peeked.unwrap().due), (0, None), "nothing due");
    let (n, taken) = counted(|| {
        let due = reader.peek(ring, 1_500)?.due.ok_or(Fault::Section)?;
        let mut sink = |at: usize, bytes: &[u8]| {
            written += bytes.len();
            out.get_mut(at..at + bytes.len())
                .map(|d| d.copy_from_slice(bytes))
                .is_some()
        };
        reader.take(ring, due, pitch, &mut sink)
    });
    assert_eq!(n, 0, "a peek and a take");
    assert_eq!(taken.unwrap().seq, 1);
    assert_eq!(written, g.frame_bytes(), "the whole frame taken");
    assert_eq!(out[256], frame[255], "the second row at the pitch");
    let (n, run) = counted(|| reader.start_run(window));
    assert_eq!((n, run), (0, 2), "a new run");
    let (n, peeked) = counted(|| reader.peek(ring, 5_000));
    assert_eq!(n, 0, "frame 2 of run 1 freed unread");
    assert!(peeked.unwrap().freed);

    publish(3, 2, 3_000);
    let (n, taken) = counted(|| {
        let due = reader.peek_first(ring)?.due.ok_or(Fault::Section)?;
        reader.take(ring, due, pitch, &mut |_: usize, _: &[u8]| true)
    });
    assert_eq!(n, 0, "a first peek and its take");
    assert_eq!(taken.unwrap().seq, 3);

    publish(4, 2, 4_000);
    mem[g.frame_start(0) - SLOT_HEADER_WORDS + 4].store(9, Ordering::Relaxed);
    let (n, peeked) = counted(|| reader.peek(ring, 5_000));
    assert_eq!(n, 0, "a fault");
    assert_eq!(peeked, Err(Fault::Index { slot: 0, value: 9 }));
    let (n, peeked) = counted(|| reader.peek(ring, 6_000));
    assert_eq!(
        (n, peeked),
        (0, Err(Fault::Index { slot: 0, value: 9 })),
        "a kept fault"
    );
}
