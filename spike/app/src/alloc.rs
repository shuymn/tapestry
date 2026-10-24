//! Counting global allocator (G6, G6r; design D3 open choice 7).
//!
//! Every allocation is attributed to the calling thread's tag and, while the
//! thread is inside a named library scope (`scope`), to that scope. Counting
//! covers an absolute QPC interval (`arm_interval`): the play window, fixed
//! once for every thread before any of them leaves the play barrier, so it
//! does not depend on when a thread next runs. The allocator itself never
//! allocates: tags and scopes are const-initialized thread-locals, the
//! counters are atomics and the clock is QueryPerformanceCounter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

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

/// Counting interval `[ARM_FROM, ARM_TO)` in QPC ticks; empty when equal.
static ARM_FROM: AtomicI64 = AtomicI64::new(0);
static ARM_TO: AtomicI64 = AtomicI64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const NO: AtomicBool = AtomicBool::new(false);
/// G6 tags with a thread that took part in the run.
static REGISTERED: [AtomicBool; TAGS] = [NO; TAGS];
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const ROW: [AtomicU64; SCOPES] = [ZERO; SCOPES];
static COUNTS: [[AtomicU64; SCOPES]; TAGS] = [ROW; TAGS];

pub struct Counting;

fn record() {
    let (from, to) = (
        ARM_FROM.load(Ordering::Relaxed),
        ARM_TO.load(Ordering::Relaxed),
    );
    if from >= to {
        return;
    }
    let now = crate::timebase::qpc();
    if now < from || now >= to {
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

/// Tags the calling thread and registers its tag as taking part in the
/// run, so its G6 count is reported (an unused tag is absent, not zero).
pub fn tag_thread(tag: Tag) {
    TAG.with(|t| t.set(tag as u8));
    REGISTERED[tag as usize].store(true, Ordering::Relaxed);
}

/// Starts counting at play start; `disarm` at play end. The interval is
/// shared by every thread.
/// Serializes tests that set the counting interval.
#[cfg(test)]
pub static ARM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Counts allocations from QPC `from` until `to` on every thread.
pub fn arm_interval(from: i64, to: i64) {
    ARM_TO.store(i64::MIN, Ordering::Relaxed);
    ARM_FROM.store(from, Ordering::Relaxed);
    ARM_TO.store(to, Ordering::Relaxed);
}

/// Counts from now on (offscreen run).
pub fn arm() {
    arm_interval(crate::timebase::qpc(), i64::MAX);
}

/// Ends counting now (a run that stops early).
pub fn disarm() {
    ARM_TO.store(i64::MIN, Ordering::Relaxed);
}

/// Runs `f` with allocations attributed to library scope `scope`.
#[inline]
pub fn scope<R>(scope: Scope, f: impl FnOnce() -> R) -> R {
    let prev = SCOPE.with(|s| s.replace(scope as u8));
    let r = f();
    SCOPE.with(|s| s.set(prev));
    r
}

/// App-code allocations by registered G6 thread, and library allocations
/// by scope (all threads), in the P1 record types. Only allocations through
/// Rust's global allocator are seen: FFmpeg's and the D3D12 runtime's own
/// heaps are not.
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
    .filter(|(_, t)| REGISTERED[*t as usize].load(Ordering::Relaxed))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timebase::{qpc, qpc_freq};

    // G6: counting follows the absolute interval, whatever thread runs
    // when; no thread has to arm it.
    #[test]
    fn allocations_count_only_inside_the_armed_interval() {
        let _arming = ARM_TEST_LOCK.lock();
        let ms = qpc_freq() / 1_000;
        let from = qpc() + 40 * ms;
        arm_interval(from, from + 80 * ms);
        let count = || COUNTS[Tag::Logic as usize][0].load(Ordering::Relaxed);
        let n = std::thread::spawn(move || {
            tag_thread(Tag::Logic);
            let before = count();
            drop(std::hint::black_box(vec![1u8; 8]));
            let outside_before = count() - before;
            while qpc() < from + 20 * ms {
                std::thread::yield_now();
            }
            let at = count();
            drop(std::hint::black_box(vec![1u8; 8]));
            let inside = count() - at;
            while qpc() < from + 100 * ms {
                std::thread::yield_now();
            }
            let at = count();
            drop(std::hint::black_box(vec![1u8; 8]));
            (outside_before, inside, count() - at)
        })
        .join()
        .unwrap();
        disarm();
        assert_eq!(n, (0, 1, 0));
    }
}
