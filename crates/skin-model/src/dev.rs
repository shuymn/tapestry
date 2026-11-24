//! Development probes of the atlas build (dev-tools): the checkpoint
//! site a thread asked at last, the unchecked kinds of work it reached
//! since it was last read, the packings made, and hooks a test of another
//! crate sets to advance its held clock inside the build (design.md
//! "Load Pipeline", the preload's pause latency). None is in a
//! production build.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::image::decode::SITE;

/// The checkpoint sites' codes: [`last_site`]'s values.
pub mod site {
    /// Each piece of a capped read.
    pub const READ: u8 = 0;
    /// Each band of rows.
    pub const BAND: u8 = 1;
    /// Each read of the chunks after the image data.
    pub const TRAILING: u8 = 2;
    /// Each mip level of each page.
    pub const MIP: u8 = 3;
    /// Before each page's allocation.
    pub const PAGE: u8 = 4;
    /// After a chart image's decode.
    pub const DECODED: u8 = 5;
    /// After a chart image's fit.
    pub const FITTED: u8 = 6;
    /// A plan's start, before its rectangles pass.
    pub const PLAN_RECTS: u8 = 7;
    /// Before the cells' order is sorted.
    pub const PLAN_SORT: u8 = 8;
    /// Before the area and largest-side scans.
    pub const PLAN_SCAN: u8 = 9;
    /// Before each candidate side's table setup.
    pub const PLAN_SIDE: u8 = 10;
    /// Before a side's first placement.
    pub const PLAN_PLACE: u8 = 11;
    /// A batch ask inside the rectangles pass.
    pub const PLAN_RECT_WORK: u8 = 12;
    /// A batch ask inside a side's placements.
    pub const PLAN_PLACE_WORK: u8 = 13;
    /// After the side search.
    pub const PLAN_END: u8 = 14;
    /// A batch ask inside a sheet union.
    pub const UNION_WORK: u8 = 15;
    /// The first code a caller's own sites take.
    pub const CALLER: u8 = 16;
}

/// The unchecked kinds of work: bit `k` of [`take_kinds`].
pub mod kind {
    /// A file's path and length resolved.
    pub const METADATA: u8 = 0;
    /// A whole image decoded.
    pub const WHOLE_DECODE: u8 = 1;
    /// A file's or a generated cell's texels written to the pages.
    pub const BLIT: u8 = 2;
    /// The held files and cells of a packing made.
    pub const HELD: u8 = 3;
    /// The sheet's table bound to the filled pages.
    pub const BINDING: u8 = 4;
    /// The kinds defined.
    pub const COUNT: u8 = 5;
}

thread_local! {
    static KINDS: Cell<u32> = const { Cell::new(0) };
}

/// Packings begun in the process ([`packings`]).
pub(crate) static PACKINGS: AtomicU64 = AtomicU64::new(0);

/// A hook a test sets.
pub type Hook = Arc<dyn Fn() + Send + Sync>;

static HOOKED: AtomicBool = AtomicBool::new(false);
static WORK_HOOK: Mutex<Option<Hook>> = Mutex::new(None);
static DECODE_HOOK: Mutex<Option<Hook>> = Mutex::new(None);

/// The code of the site this thread last asked its cancel at, if any.
#[must_use]
pub fn last_site() -> Option<u8> {
    SITE.with(|s| s.get().map(crate::image::decode::Site::code))
}

/// The kinds this thread reached since the last call, as bits, cleared.
#[must_use]
pub fn take_kinds() -> u32 {
    KINDS.with(|k| k.replace(0))
}

/// The packings begun in the process so far.
#[must_use]
pub fn packings() -> u64 {
    PACKINGS.load(Ordering::Relaxed)
}

/// Calls `hook` once per unit of planning work between two asks, on the
/// planning thread; `None` removes it.
pub fn set_work_hook(hook: Option<Hook>) {
    set(&WORK_HOOK, hook);
}

/// Calls `hook` at the entry of each whole-image decode, on the decoding
/// thread; `None` removes it.
pub fn set_decode_hook(hook: Option<Hook>) {
    set(&DECODE_HOOK, hook);
}

fn set(slot: &Mutex<Option<Hook>>, hook: Option<Hook>) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = hook;
    let any = [&WORK_HOOK, &DECODE_HOOK]
        .iter()
        .any(|s| s.lock().unwrap_or_else(PoisonError::into_inner).is_some());
    HOOKED.store(any, Ordering::Relaxed);
}

fn call(slot: &Mutex<Option<Hook>>) {
    if !HOOKED.load(Ordering::Relaxed) {
        return;
    }
    let hook = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Records that this thread reached `kind`.
pub(crate) fn note(kind: u8) {
    KINDS.with(|k| k.set(k.get() | 1 << kind));
}

/// One unit of planning work ran.
pub(crate) fn work_unit() {
    call(&WORK_HOOK);
}

/// A whole-image decode begins.
pub(crate) fn whole_decode() {
    note(kind::WHOLE_DECODE);
    call(&DECODE_HOOK);
}
