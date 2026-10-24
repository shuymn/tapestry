//! Counting global allocator (G6, G6r; design D3 open choice 7).
//!
//! Every allocation is attributed to the calling thread's tag and, while the
//! thread is inside a named library scope (`scope`), to that scope. Counting
//! starts when `arm` is called after the countdown. The allocator itself
//! never allocates: tags and scopes are const-initialized thread-locals and
//! the counters are atomics.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spike_core::run::{LIBRARY_SCOPES, ThreadTag};

/// Threads that G6 covers, plus everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    Other = 0,
    Input = 1,
    Logic = 2,
    Audio = 3,
    Render = 4,
}

const TAGS: usize = 5;

/// Named library scopes; outside any scope (index 0) is app code. Names
/// match `spike_core::run::LIBRARY_SCOPES` (index `i` here is
/// `LIBRARY_SCOPES[i - 1]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Scope {
    CreateCommandEncoder = 1,
    WriteTexture = 2,
    WriteBuffer = 3,
    Submit = 4,
    GetCurrentTexture = 5,
    Present = 6,
    #[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
    FfmpegDecode = 7,
    OtherLibrary = 8,
}

const SCOPES: usize = 9;

thread_local! {
    static TAG: Cell<u8> = const { Cell::new(0) };
    static SCOPE: Cell<u8> = const { Cell::new(0) };
}

static ARMED: AtomicBool = AtomicBool::new(false);
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const ROW: [AtomicU64; SCOPES] = [ZERO; SCOPES];
static COUNTS: [[AtomicU64; SCOPES]; TAGS] = [ROW; TAGS];

pub struct Counting;

fn record() {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let tag = TAG.try_with(Cell::get).unwrap_or(0) as usize;
    let scope = SCOPE.try_with(Cell::get).unwrap_or(0) as usize;
    COUNTS[tag][scope].fetch_add(1, Ordering::Relaxed);
}

// SAFETY: forwards to `System`; only adds counting, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Tags the calling thread.
pub fn tag_thread(tag: Tag) {
    TAG.with(|t| t.set(tag as u8));
}

/// Starts counting (after the countdown).
pub fn arm() {
    ARMED.store(true, Ordering::Relaxed);
}

pub fn disarm() {
    ARMED.store(false, Ordering::Relaxed);
}

/// Runs `f` with allocations attributed to library scope `scope`.
#[inline]
pub fn scope<R>(scope: Scope, f: impl FnOnce() -> R) -> R {
    let prev = SCOPE.with(|s| s.replace(scope as u8));
    let r = f();
    SCOPE.with(|s| s.set(prev));
    r
}

/// App-code allocations by G6 thread, and library allocations by scope
/// (all threads), in the P1 record types.
pub fn snapshot() -> (
    std::collections::BTreeMap<ThreadTag, u64>,
    std::collections::BTreeMap<String, u64>,
) {
    let get = |t: Tag, s: usize| COUNTS[t as usize][s].load(Ordering::Relaxed);
    let app = [
        (ThreadTag::Input, Tag::Input),
        (ThreadTag::Logic, Tag::Logic),
        (ThreadTag::Audio, Tag::Audio),
        (ThreadTag::Render, Tag::Render),
    ]
    .into_iter()
    .map(|(k, t)| (k, get(t, 0)))
    .collect();
    let lib = (1..SCOPES)
        .map(|s| {
            let n = (0..TAGS)
                .map(|t| COUNTS[t][s].load(Ordering::Relaxed))
                .sum();
            (LIBRARY_SCOPES[s - 1].to_owned(), n)
        })
        .collect();
    (app, lib)
}

/// App-code allocations on threads outside G6 (main, decode, recorder).
pub fn other_app() -> u64 {
    COUNTS[Tag::Other as usize][0].load(Ordering::Relaxed)
}
