//! The transition probe's render-side marks (dev-tools only; #611): upload
//! phases, inline drops, first use, chart-image and text figures, forwarded
//! to the sink the player registers when `TAPESTRY_LOOP_PROBE` is set, so
//! they share the player's time base. Without a sink nothing is marked or
//! read. Nothing here waits for or reorders work, and a mark allocates
//! nothing (the sink's buffer is reserved before it is registered).
//!
//! A duration is a [`Span`]: the probe's own work on its thread inside it
//! (marks, memory readings, keys, callbacks armed: [`own`]) is left out, so
//! a figure is what the work it names took, nested spans included.
//!
//! A timed drop (`dr:*_us`, `rel:*_us`) is the drop of the handle it
//! names: wgpu frees an array when its last handle (often a bind group) is
//! dropped and no submitted work uses it, at the device's next maintenance
//! (a submit), which the probe does not force. The readings after a drop
//! (`dr:l1`, `rel:l1`) can therefore show memory still held; the next
//! frames' readings (`pd:*`) show it freed.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
    DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
};

static SINK: OnceLock<fn(&'static str, i64)> = OnceLock::new();
static CALLER: AtomicU8 = AtomicU8::new(OTHER);
static FRAME: AtomicBool = AtomicBool::new(false);
static DXGI: OnceLock<IDXGIAdapter3> = OnceLock::new();
static IN_PLAY: AtomicBool = AtomicBool::new(false);
/// The last atlas array's creation, its page writes and the last error
/// scope pair's pops, in µs (-1: none yet).
pub(crate) static CREATE_US: AtomicI64 = AtomicI64::new(-1);
pub(crate) static WRITE_US: AtomicI64 = AtomicI64::new(-1);
pub(crate) static POP_US: AtomicI64 = AtomicI64::new(-1);
/// Frames left whose submit and memory are marked after a drop (`pd:*`).
pub(crate) static POST: AtomicU32 = AtomicU32::new(0);
/// The array uploaded last, by [`array_key`], until a frame samples it
/// (`fu:*`), whatever bind group the frame binds it with; 0: none.
static FIRST_USE: AtomicU64 = AtomicU64::new(0);
/// The caller [`FIRST_USE`]'s array was uploaded for.
static FIRST_USE_CALLER: AtomicU8 = AtomicU8::new(OTHER);

/// The callers an upload's marks are tagged with (`<family>:<caller>:*`).
pub const OTHER: u8 = 0;
pub const SELECT: u8 = 1;
pub const RESULT: u8 = 2;
pub const ATTEMPT: u8 = 3;
pub const SELECT_SKIN: u8 = 4;
pub const LOADING: u8 = 5;

/// Registers the player's mark function: from then on the probe is on.
pub fn set_sink(f: fn(&'static str, i64)) {
    let _ = SINK.set(f);
}

/// Whether a sink is registered.
pub fn on() -> bool {
    SINK.get().is_some()
}

pub fn mark(tag: &'static str, v: i64) {
    if let Some(f) = SINK.get() {
        own(|| f(tag, v));
    }
}

thread_local! {
    /// The probe's own time on this thread so far ([`own`]).
    static OWN: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    /// Whether this thread is in the probe's own work: nested work is
    /// counted once.
    static IN_OWN: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` as the probe's own work: its time is left out of every
/// [`Span`] on this thread that encloses it.
pub fn own<R>(f: impl FnOnce() -> R) -> R {
    if IN_OWN.with(|c| c.replace(true)) {
        return f();
    }
    let t = Instant::now();
    let r = f();
    let took = t.elapsed();
    OWN.with(|c| c.set(c.get().saturating_add(took)));
    IN_OWN.with(|c| c.set(false));
    r
}

/// The probe's own time on this thread so far.
pub fn own_time() -> Duration {
    OWN.with(Cell::get)
}

/// A probe figure's start: a time on this thread and the probe's own time
/// then, so its [`elapsed`](Self::elapsed) leaves the probe's work out.
/// With the probe off it reads no clock and every figure is zero.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    at: Option<Instant>,
    own: Duration,
}

impl Span {
    pub fn start() -> Self {
        if on() {
            Self::at(Instant::now())
        } else {
            Self {
                at: None,
                own: Duration::ZERO,
            }
        }
    }

    /// A span from `at`, a time this thread took with no probe work since.
    pub fn at(at: Instant) -> Self {
        Self {
            at: Some(at),
            own: own_time(),
        }
    }

    /// The time since its start, less the probe's own work on this thread
    /// since.
    pub fn elapsed(&self) -> Duration {
        self.at.map_or(Duration::ZERO, |at| {
            at.elapsed()
                .saturating_sub(own_time().saturating_sub(self.own))
        })
    }

    /// The probe's own work on this thread since its start.
    pub fn own_since(&self) -> Duration {
        own_time().saturating_sub(self.own)
    }

    /// The probe's own work on this thread from `base` (an [`own_time`])
    /// to its start.
    pub fn own_before(&self, base: Duration) -> Duration {
        self.own.saturating_sub(base)
    }

    /// The time from its start to `to`'s, less the probe's own work
    /// between them.
    pub fn until(&self, to: Self) -> Duration {
        match (self.at, to.at) {
            (Some(from), Some(at)) => at
                .saturating_duration_since(from)
                .saturating_sub(to.own.saturating_sub(self.own)),
            _ => Duration::ZERO,
        }
    }
}

/// Sets the caller the following uploads are tagged with; returns the
/// previous one, to restore.
pub fn set_caller(c: u8) -> u8 {
    CALLER.swap(c, Ordering::Relaxed)
}

pub(crate) fn caller() -> u8 {
    CALLER.load(Ordering::Relaxed)
}

/// Arms (or disarms) the frame pacing marks (`rf:*`) for the next frame.
pub fn arm_frame(on: bool) {
    FRAME.store(on, Ordering::Relaxed);
}

pub(crate) fn frame_armed() -> bool {
    FRAME.load(Ordering::Relaxed)
}

pub(crate) fn us(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// `cmark!("up", "bytes", v)` marks `up:<caller>:bytes`.
macro_rules! cmark {
    ($fam:literal, $fig:literal, $v:expr) => {{
        let v: i64 = $v;
        let tag: &'static str = match $crate::probe::caller() {
            $crate::probe::SELECT => concat!($fam, ":select:", $fig),
            $crate::probe::RESULT => concat!($fam, ":result:", $fig),
            $crate::probe::ATTEMPT => concat!($fam, ":attempt:", $fig),
            $crate::probe::SELECT_SKIN => concat!($fam, ":selskin:", $fig),
            $crate::probe::LOADING => concat!($fam, ":loading:", $fig),
            _ => concat!($fam, ":other:", $fig),
        };
        $crate::probe::mark(tag, v);
    }};
}
pub(crate) use cmark;

pub(crate) fn set_dxgi(a: IDXGIAdapter3) {
    let _ = DXGI.set(a);
}

fn vram(g: DXGI_MEMORY_SEGMENT_GROUP) -> i64 {
    let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    DXGI.get()
        // SAFETY: a plain query of the adapter's memory use into a local.
        .and_then(|a| unsafe { a.QueryVideoMemoryInfo(0, g, &mut i) }.ok())
        .map_or(-1, |()| i64::try_from(i.CurrentUsage).unwrap_or(-1))
}

/// Sets whether the render thread is in play: there the probe reads no
/// memory and arms no completion callback, which allocate inside wgpu
/// (design.md "Scope of No Allocation").
pub fn set_in_play(on: bool) {
    IN_PLAY.store(on, Ordering::Relaxed);
}

pub(crate) fn in_play() -> bool {
    IN_PLAY.load(Ordering::Relaxed)
}

/// Takes one from `n` if it is above 0; whether it did.
pub fn count_down(n: &AtomicU32) -> bool {
    let mut v = n.load(Ordering::Relaxed);
    while v > 0 {
        match n.compare_exchange_weak(v, v - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(now) => v = now,
        }
    }
    false
}

/// Marks `l` (local video memory in use by the process), `n` (non-local)
/// and `r` (the device allocator's reserved bytes); in play, each as -2
/// (not read). The allocator's report allocates inside wgpu: never called
/// inside a timed span or a frame's measured time.
pub(crate) fn mem(d: &wgpu::Device, l: &'static str, n: &'static str, r: &'static str) {
    if !on() {
        return;
    }
    own(|| read_mem(d, l, n, r));
}

fn read_mem(d: &wgpu::Device, l: &'static str, n: &'static str, r: &'static str) {
    if in_play() {
        mark(l, -2);
        mark(n, -2);
        mark(r, -2);
        return;
    }
    mark(l, vram(DXGI_MEMORY_SEGMENT_GROUP_LOCAL));
    mark(n, vram(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL));
    mark(
        r,
        d.generate_allocator_report()
            .map_or(-1, |x| i64::try_from(x.total_reserved_bytes).unwrap_or(-1)),
    );
}

/// Marks one chart image's write (`ci:<caller>:*`): its index, its
/// bytes, its writes from `written` to `waiting` and its wait for the GPU
/// from `waiting` to now, both taken before its bytes are counted or any
/// of its marks made.
pub(crate) fn chart_image(
    index: usize,
    bytes: impl FnOnce() -> usize,
    written: Span,
    waiting: Span,
) {
    if !on() {
        return;
    }
    let wait = waiting.elapsed();
    let write = written.until(waiting);
    let bytes = own(bytes);
    cmark!("ci", "image", i64::try_from(index).unwrap_or(-1));
    cmark!("ci", "bytes", i64::try_from(bytes).unwrap_or(-1));
    cmark!("ci", "write_us", us(write));
    cmark!("ci", "wait_us", us(wait));
}

/// Marks the submit and memory of the next `frames` frames (`pd:*`):
/// after a drop 8, after a first use 3; an armed count is never cut.
pub fn arm_post(frames: u32) {
    if on() {
        POST.fetch_max(frames, Ordering::Relaxed);
    }
}

/// The frames left whose submit and memory are marked (`pd:*`).
pub fn post_left() -> u32 {
    POST.load(Ordering::Relaxed)
}

/// Awaits the first frame that samples the array `key`, uploaded now for
/// the current caller: an array awaited before and not drawn is marked
/// replaced (`fu:superseded`, its caller), so no first use is credited to
/// the wrong upload.
pub fn await_first_use(key: u64) {
    own(|| {
        let replaced = FIRST_USE.swap(key, Ordering::Relaxed);
        let was = FIRST_USE_CALLER.swap(caller(), Ordering::Relaxed);
        if replaced != 0 {
            mark("fu:superseded", i64::from(was));
        }
    });
}

/// The array `key` dropped before any frame sampled it: its first use is
/// no longer awaited (`fu:discarded`, its caller).
pub fn discard_first_use(key: u64) {
    own(|| {
        if FIRST_USE
            .compare_exchange(key, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            mark(
                "fu:discarded",
                i64::from(FIRST_USE_CALLER.load(Ordering::Relaxed)),
            );
        }
    });
}

/// Whether a frame sampling the array `key` would be its first use: the
/// caller it was uploaded for. It stays awaited until a frame sampling it
/// is submitted ([`end_first_use`]): a frame skipped before its submit is
/// not its first use.
pub fn first_use(key: u64) -> Option<u8> {
    own(|| {
        (key != 0 && FIRST_USE.load(Ordering::Relaxed) == key)
            .then(|| FIRST_USE_CALLER.load(Ordering::Relaxed))
    })
}

/// A frame sampling the array `key` was submitted: its first use is no
/// longer awaited.
pub fn end_first_use(key: u64) {
    let _ = FIRST_USE.compare_exchange(key, 0, Ordering::Relaxed, Ordering::Relaxed);
}

/// An array's identity for [`FIRST_USE`], never 0.
pub(crate) fn array_key(b: &wgpu::Texture) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    own(|| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut h);
        h.finish().max(1)
    })
}
