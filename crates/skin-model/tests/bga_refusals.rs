//! A BGA picture's allocations under an allocator that refuses them
//! (design.md "Rendering and Skins", BGA; "Memory Budget"): every buffer
//! the read and the cell make themselves is allocated fallibly, so a
//! refusal of any of them leaves the picture absent as refused, with no
//! panic and no abort; and a decoder whose own buffers would pass the
//! limit by their bound is never built.

mod image_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::File;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use image::{ImageFormat, RgbImage};
use image_support::dir;
use skin_model::bga::{BgaBinding, CELL_GUTTER, probe, read_picture, resolve};
use skin_model::chart_image::{Absent, ChartDir, FileIdentity};

/// The size of the allocation to refuse (0: none), how many of that size
/// to let through first, and whether one was refused.
static SIZE: AtomicUsize = AtomicUsize::new(0);
static SKIP: AtomicUsize = AtomicUsize::new(0);
static REFUSED: AtomicBool = AtomicBool::new(false);
/// The largest allocation made on a watching thread since the last reset.
static LARGEST: AtomicUsize = AtomicUsize::new(0);
/// The sizes of the first allocations of at least [`LOGGED`] bytes made on
/// a watching thread since the last reset, in order, and their count.
static LOG: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];
static LOG_LEN: AtomicUsize = AtomicUsize::new(0);
const LOGGED: usize = 4 << 10;

thread_local! {
    /// Whether this thread's allocations are watched: the test's own.
    static WATCHED: Cell<bool> = const { Cell::new(false) };
}

/// One test at a time: the allocator's state is the process's.
static SERIAL: Mutex<()> = Mutex::new(());

struct Refusing;

impl Refusing {
    /// Whether an allocation of `size` bytes on this thread is refused.
    fn refuses(size: usize) -> bool {
        if !WATCHED.with(Cell::get) {
            return false;
        }
        LARGEST.fetch_max(size, Ordering::SeqCst);
        if size >= LOGGED {
            let i = LOG_LEN.fetch_add(1, Ordering::SeqCst);
            if let Some(slot) = LOG.get(i) {
                slot.store(size, Ordering::SeqCst);
            }
        }
        if size == 0 || SIZE.load(Ordering::SeqCst) != size {
            return false;
        }
        if SKIP.load(Ordering::SeqCst) > 0 {
            SKIP.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        SIZE.store(0, Ordering::SeqCst);
        REFUSED.store(true, Ordering::SeqCst);
        true
    }
}

// SAFETY: every call the allocator does not refuse is forwarded to the
// system allocator unchanged; a refusal returns null, as an allocator out
// of memory does.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Self::refuses(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: as the caller's contract for `alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if Self::refuses(layout.size()) {
            return std::ptr::null_mut();
        }
        // SAFETY: as the caller's contract for `alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if Self::refuses(new_size) {
            return std::ptr::null_mut();
        }
        // SAFETY: as the caller's contract for `realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as the caller's contract for `dealloc`.
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static ALLOC: Refusing = Refusing;

fn fixed(_: &File) -> Option<FileIdentity> {
    Some(FileIdentity(1))
}

fn never() -> bool {
    false
}

/// The low byte of `v`.
fn byte(v: u32) -> u8 {
    u8::try_from(v % 256).unwrap()
}

/// The production limit of each of a BGA job's allocations: one picture's
/// whole-decode bound.
const LIMIT: u64 = 161 << 20;

/// `name` in `root` read into a cell of `content` through the production
/// reader, with the allocation of `size` bytes after `skip` of that size
/// refused (none for 0), on this thread only. Returns the outcome, whether
/// the refusal happened, and the largest allocation and those of at least
/// [`LOGGED`] bytes, in order, made while the cell was made.
fn cell(
    root: &Path,
    name: &str,
    content: [u32; 2],
    limit: u64,
    size: usize,
    skip: usize,
) -> (Result<[u32; 2], Absent>, bool, usize, Vec<usize>) {
    let dir = ChartDir::new(root).unwrap();
    let BgaBinding::Resolved { path, key, .. } = resolve(&dir, name, &fixed) else {
        panic!("{name} resolves");
    };
    let native = probe(&path, &key, &fixed).unwrap();
    SKIP.store(skip, Ordering::SeqCst);
    REFUSED.store(false, Ordering::SeqCst);
    SIZE.store(size, Ordering::SeqCst);
    WATCHED.with(|w| w.set(true));
    let read = read_picture(&path, &key, native, &never, &fixed, Some(limit));
    LARGEST.store(0, Ordering::SeqCst);
    LOG_LEN.store(0, Ordering::SeqCst);
    let out = read.and_then(|r| r.cell(content, &never, Some(limit)));
    WATCHED.with(|w| w.set(false));
    SIZE.store(0, Ordering::SeqCst);
    let largest = LARGEST.load(Ordering::SeqCst);
    let logged = LOG_LEN.load(Ordering::SeqCst).min(LOG.len());
    let log = LOG
        .iter()
        .take(logged)
        .map(|s| s.load(Ordering::SeqCst))
        .collect();
    (
        out.map(|c| c.padded),
        REFUSED.load(Ordering::SeqCst),
        largest,
        log,
    )
}

/// Each buffer the read and the cell allocate, refused on its own, leaves
/// the picture refused, without a panic: the header's and the whole file's
/// reads, the native image, its RGBA8 copy, the canvas of a small picture,
/// the downscale's output and row sums, and the cell with its gutter. Each
/// refusal is checked to have happened. A 24-bit BMP, whose decoder
/// allocates nothing of its own, so no other allocation has these sizes;
/// and while its cell is made, these are its only buffers of 4 KiB or more,
/// so no other copy of the picture is made where a refusal would abort.
// Covers: T08
#[test]
fn each_buffer_of_a_bga_picture_refused_leaves_it_absent() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let d = dir();
    let big = RgbImage::from_fn(640, 480, |x, y| image::Rgb([byte(x), byte(y), 7]));
    big.save_with_format(d.path().join("big.bmp"), ImageFormat::Bmp)
        .unwrap();
    let small = RgbImage::from_fn(100, 50, |x, y| image::Rgb([byte(x), byte(y), 9]));
    small
        .save_with_format(d.path().join("small.bmp"), ImageFormat::Bmp)
        .unwrap();
    let file = 54 + 640 * 480 * 3;
    let gutter = usize::try_from(CELL_GUTTER).unwrap();
    let padded = |[w, h]: [usize; 2]| (w + 2 * gutter) * (h + 2 * gutter) * 4;
    let cases = [
        ("big.bmp", [64, 48], file, 0, "the header's read"),
        ("big.bmp", [64, 48], file, 1, "the whole file's read"),
        ("big.bmp", [64, 48], 640 * 480 * 3, 0, "the native image"),
        ("big.bmp", [64, 48], 640 * 480 * 4, 0, "the RGBA8 copy"),
        ("big.bmp", [64, 48], 64 * 48 * 4, 0, "the downscale"),
        (
            "big.bmp",
            [64, 48],
            640 * 4 * 8,
            0,
            "the downscale's row sums",
        ),
        ("big.bmp", [64, 48], padded([64, 48]), 0, "the cell"),
        ("small.bmp", [64, 64], 256 * 256 * 4, 0, "the canvas"),
    ];
    let (_, _, _, log) = cell(d.path(), "big.bmp", [64, 48], LIMIT, 0, 0);
    let buffers = [
        640 * 480 * 3,
        640 * 480 * 4,
        64 * 48 * 4,
        640 * 4 * 8,
        padded([64, 48]),
    ];
    assert_eq!(log, buffers, "the cell's buffers of 4 KiB or more");
    for (name, content, size, skip, what) in cases {
        let (control, refused, _, _) = cell(d.path(), name, content, LIMIT, 0, 0);
        assert!(control.is_ok() && !refused, "{what}: the control reads");
        let (out, refused, _, _) = cell(d.path(), name, content, LIMIT, size, skip);
        assert!(refused, "{what}: an allocation of {size} bytes was made");
        assert_eq!(out, Err(Absent::Refused), "{what}");
    }
}

/// A JPEG whose decoder's own buffers could pass the limit by their bound
/// is refused before its decoder is built: nothing near the picture's size
/// is allocated while its cell is made, where under a limit its bound
/// fits, the decode allocates its native image.
// Covers: T08
#[test]
fn a_picture_whose_decoder_could_pass_the_limit_is_refused_before_it_decodes() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let d = dir();
    let rgb = RgbImage::from_fn(640, 480, |x, y| image::Rgb([byte(x), byte(y), 7]));
    rgb.save_with_format(d.path().join("big.jpg"), ImageFormat::Jpeg)
        .unwrap();
    let native = 640 * 480 * 3;
    // The native image and its RGBA8 copy fit; the envelope does not.
    let (out, _, largest, _) = cell(d.path(), "big.jpg", [64, 48], 1_300_000, 0, 0);
    assert_eq!(out, Err(Absent::Refused));
    assert!(
        largest < native,
        "{largest} bytes at once before the refusal"
    );
    let (out, _, largest, _) = cell(d.path(), "big.jpg", [64, 48], LIMIT, 0, 0);
    assert!(out.is_ok());
    assert!(largest >= native, "the decode made its native image");
}

/// Under a limit, each phase of a picture's cell holds at most the limit at
/// once, checked before its first buffer: at the edge of the phase that
/// holds the most the picture is read, one byte below it the picture is
/// refused with no buffer near its size allocated. Decoding (a JPEG: its
/// envelope beside the native image), converting (a BMP's native image at
/// its largest beside the RGBA8 copy), placing a small picture on its
/// canvas (the picture beside the canvas, for a picture larger than the
/// downscale it is fitted to), downscaling (a small picture's canvas beside
/// the downscale and its row sums) and the cell (the content beside the
/// cell).
// Covers: T08
#[test]
fn each_phase_of_a_bga_picture_is_held_to_the_limit_at_its_edge() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let d = dir();
    let rgb = |w, h| RgbImage::from_fn(w, h, |x, y| image::Rgb([byte(x), byte(y), 7]));
    rgb(640, 480)
        .save_with_format(d.path().join("big.jpg"), ImageFormat::Jpeg)
        .unwrap();
    rgb(640, 480)
        .save_with_format(d.path().join("rgb.bmp"), ImageFormat::Bmp)
        .unwrap();
    rgb(100, 50)
        .save_with_format(d.path().join("small.bmp"), ImageFormat::Bmp)
        .unwrap();
    rgb(200, 200)
        .save_with_format(d.path().join("square.bmp"), ImageFormat::Bmp)
        .unwrap();
    image::RgbaImage::from_pixel(300, 300, image::Rgba([1, 2, 3, 255]))
        .save_with_format(d.path().join("rgba.bmp"), ImageFormat::Bmp)
        .unwrap();
    let len = |name: &str| fs_len(&d.path().join(name));
    let gutter = u64::from(CELL_GUTTER);
    let padded = |w: u64, h: u64| (w + 2 * gutter) * (h + 2 * gutter) * 4;
    // A JPEG's envelope: 8 times the file, 64 KiB, 8 (w + 31)(h + 31) and
    // 3 410 (w + 31), beside its native image of 3 bytes a texel.
    let jpeg = 8 * len("big.jpg") + (64 << 10) + 8 * 671 * 511 + 3_410 * 671 + 640 * 480 * 3;
    let cases = [
        ("decoding", "big.jpg", [64, 48], jpeg, 640 * 480 * 3),
        (
            "converting",
            "rgb.bmp",
            [64, 48],
            2 * 640 * 480 * 4,
            640 * 480 * 3,
        ),
        (
            "placing on the canvas",
            "square.bmp",
            [64, 64],
            200 * 200 * 4 + 256 * 256 * 4,
            200 * 200 * 3,
        ),
        (
            "downscaling",
            "small.bmp",
            [64, 64],
            256 * 256 * 4 + 64 * 64 * 4 + 256 * 4 * 8,
            100 * 50 * 3,
        ),
        (
            "the cell",
            "rgba.bmp",
            [300, 300],
            300 * 300 * 4 + padded(300, 300),
            300 * 300 * 4,
        ),
    ];
    for (what, name, content, edge, native) in cases {
        let (out, _, _, _) = cell(d.path(), name, content, edge, 0, 0);
        assert!(out.is_ok(), "{what}: read at {edge}: {out:?}");
        let (out, _, largest, _) = cell(d.path(), name, content, edge - 1, 0, 0);
        assert_eq!(out, Err(Absent::Refused), "{what}: refused below {edge}");
        assert!(
            largest < usize::try_from(native).unwrap(),
            "{what}: {largest} bytes at once before the refusal"
        );
    }
}

fn fs_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}
