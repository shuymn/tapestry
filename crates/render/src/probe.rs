//! The transition probe's render-side marks (dev-tools only; #611): upload
//! phases, inline drops, first use, chart-image and text figures, forwarded
//! to the sink the player registers when `TAPESTRY_LOOP_PROBE` is set, so
//! they share the player's time base. Without a sink nothing is marked or
//! read. Nothing here waits for or reorders work, and a mark allocates
//! nothing (the sink's buffer is reserved before it is registered).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};

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
/// The bind group of the array uploaded last, by [`bind_key`], until a
/// frame draws it (`fu:*`); 0: none.
pub(crate) static FIRST_USE: AtomicU64 = AtomicU64::new(0);

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
        f(tag, v);
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
/// from `waiting` to now.
pub(crate) fn chart_image(
    index: usize,
    bytes: usize,
    written: std::time::Instant,
    waiting: std::time::Instant,
) {
    cmark!("ci", "image", i64::try_from(index).unwrap_or(-1));
    cmark!("ci", "bytes", i64::try_from(bytes).unwrap_or(-1));
    cmark!(
        "ci",
        "write_us",
        us(waiting.saturating_duration_since(written))
    );
    cmark!("ci", "wait_us", us(waiting.elapsed()));
}

/// Marks the submit and memory of the next frames after a drop (`pd:*`).
pub(crate) fn arm_post() {
    if on() {
        POST.store(8, Ordering::Relaxed);
    }
}

/// `new` made in place of `old` on the same array (the probe on): a
/// chart-image pool bound, or the placeholder in its place. If `old` is
/// the array uploaded last, not drawn yet, its first use is now the first
/// frame that draws `new` (`fu:*`).
pub(crate) fn rebind(old: &wgpu::BindGroup, new: &wgpu::BindGroup) {
    if on() {
        let key = bind_key(old);
        // A failed exchange: `old` is not the array awaiting its first use.
        let _ =
            FIRST_USE.compare_exchange(key, bind_key(new), Ordering::Relaxed, Ordering::Relaxed);
    }
}

/// A bind group's identity for [`FIRST_USE`], never 0.
pub(crate) fn bind_key(b: &wgpu::BindGroup) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    h.finish().max(1)
}
