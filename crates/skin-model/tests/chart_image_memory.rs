//! The chart images' memory units at their worst case (design.md "Memory
//! Budget"): a picture of 12 Mi texels whose padded allocation is the
//! largest the caps admit (3 209 × 3 921). Its source holds at most
//! 50 988 032 bytes, a fit with its mip levels at most 67 718 480 bytes,
//! and a derivation's heap at its peak, its output included, at most
//! 101 317 988 bytes of pixels: no copy of the source's texels outlives the
//! downscale.

mod image_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};

use image_support::{dir, save, solid};
use skin_model::chart_image::{ChartDir, FileIdentity, Fitted, Slot, derive, read_source};

/// The heap's bytes now and at their peak since the last reset.
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters only observe the sizes.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller's contract for `alloc`.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = NOW.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        NOW.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: as the caller's contract for `dealloc`.
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// The peak heap growth while `f` runs, and its value.
fn peak<T>(f: impl FnOnce() -> T) -> (usize, T) {
    let base = NOW.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (PEAK.load(Ordering::SeqCst) - base, out)
}

fn bytes(f: &Fitted) -> usize {
    f.levels().iter().map(|l| l.len()).sum()
}

fn fixed(_: &File) -> Option<FileIdentity> {
    Some(FileIdentity(1))
}

const SOURCE_MAX: usize = 50_988_032;
const FIT_MAX: usize = 67_718_480;
/// Its pixel bytes, and 4 KiB for the levels' own table.
const DERIVE_PEAK_MAX: usize = 101_317_988 + 4_096;

/// The worst case's source, a fit at its native size (no downscale, the
/// derivation's largest peak) and a fit downscaled just under it (the
/// copy and the downscale at their largest) each stay within the
/// units design.md states.
#[test]
fn the_worst_cases_units_hold() {
    let d = dir();
    save(d.path(), "s.png", &solid(3_209, 3_921, [10, 20, 30, 255]));
    let root = ChartDir::new(d.path()).unwrap();
    let source = read_source(&root, "s.png", &[], &|| false, &fixed)
        .image
        .unwrap();
    assert_eq!(source.content(), [3_209, 3_921]);
    assert_eq!(bytes(&source), SOURCE_MAX);
    let slot = |side| Slot::new([side, side]).unwrap();
    let (grew, full) = peak(|| derive(&source, slot(4_096), &|| false).unwrap());
    assert_eq!(full.content(), [3_209, 3_921]);
    assert_eq!(bytes(&full), FIT_MAX);
    assert!(grew <= DERIVE_PEAK_MAX, "{grew} > {DERIVE_PEAK_MAX}");
    drop(full);
    let (grew, small) = peak(|| derive(&source, slot(3_904), &|| false).unwrap());
    assert!(small.content()[1] < 3_921);
    assert!(grew <= DERIVE_PEAK_MAX, "{grew} > {DERIVE_PEAK_MAX}");
    assert!(bytes(&small) <= FIT_MAX);
}
