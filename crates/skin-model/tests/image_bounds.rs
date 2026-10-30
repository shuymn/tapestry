//! Memory stays bounded by the caps: headers are checked before any pixel
//! memory, the atlas's caps before its pages, and one file's decoding
//! within its stated bound.
//!
//! A global allocator tracks the peak of live bytes allocated while a
//! thread opts in. Each allocation carries a header naming the measurement
//! it was made in (0 for none), so only frees of the current measurement's
//! allocations are subtracted, wherever and whenever they happen; a
//! reallocation counts as a new allocation and a free. Measurements take a
//! lock, so tests in this file do not disturb each other's counts.

mod image_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, ImageFormat, RgbImage};
use image_support::{
    LIMITS, bmp_header, chunk, dir, file, load, load_files, png_header, png_start, save, solid,
    whole, write,
};
use skin_model::{
    AtlasError, AtlasLimits, FileAt, ImageDef, ImageError, MAX_FILE_BYTES, MAX_IMAGE_TEXELS,
    MAX_TOTAL_FILE_BYTES, MAX_TOTAL_TEXELS, Sheet, Variant,
};

struct Peak;

/// The current measurement (0 before the first) in the bits from
/// [`GEN_SHIFT`] up and its live bytes below, so that matching a free's
/// measurement and subtracting it are one atomic step.
static STATE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

/// Room before each allocation for its tag, at least the alignment.
const HEAD: usize = 16;

fn tracking() -> bool {
    TRACK.try_with(Cell::get).unwrap_or(false)
}

const GEN_SHIFT: u32 = 40;
const LIVE_MASK: u64 = (1 << GEN_SHIFT) - 1;

fn generation(state: u64) -> u64 {
    state >> GEN_SHIFT
}

fn live_of(state: u64) -> i64 {
    i64::try_from(state & LIVE_MASK).unwrap_or(i64::MAX)
}

/// The live bytes of the current measurement.
fn live() -> i64 {
    live_of(STATE.load(Ordering::Relaxed))
}

/// Counts `n` bytes in the current measurement; returns that measurement.
fn add(n: usize) -> u64 {
    let n = u64::try_from(n).unwrap_or(LIVE_MASK);
    let mut state = STATE.load(Ordering::Relaxed);
    loop {
        let sum = ((state & LIVE_MASK) + n.min(LIVE_MASK)).min(LIVE_MASK);
        let next = (state & !LIVE_MASK) | sum;
        match STATE.compare_exchange_weak(state, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                PEAK.fetch_max(live_of(next), Ordering::Relaxed);
                return generation(next);
            }
            Err(now) => state = now,
        }
    }
}

/// Uncounts `n` bytes if `tag` is still the current measurement.
fn sub(tag: u64, n: usize) {
    let n = u64::try_from(n).unwrap_or(LIVE_MASK);
    let mut state = STATE.load(Ordering::Relaxed);
    // Another measurement has begun once the generation differs.
    while tag != 0 && generation(state) == tag {
        let next = (state & !LIVE_MASK) | (state & LIVE_MASK).saturating_sub(n);
        match STATE.compare_exchange_weak(state, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(now) => state = now,
        }
    }
}

fn outer(layout: Layout) -> (Layout, usize) {
    let head = layout.align().max(HEAD);
    // SAFETY: the alignment is a power of two and the size cannot overflow
    // for any layout the program can allocate.
    let outer = unsafe { Layout::from_size_align_unchecked(layout.size() + head, head) };
    (outer, head)
}

// SAFETY: delegates to `System` with room for a generation tag before each block.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (outer, head) = outer(layout);
        // SAFETY: `outer` has a non-zero size.
        let base = unsafe { System.alloc(outer) };
        if base.is_null() {
            return base;
        }
        let tag = if tracking() { add(layout.size()) } else { 0 };
        // SAFETY: `head` bytes lie before the returned block, the tag in
        // its last 8, aligned to 8.
        unsafe {
            let user = base.add(head);
            user.sub(8).cast::<u64>().write(tag);
            user
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (outer, head) = outer(layout);
        // SAFETY: `ptr` came from `alloc` with this layout.
        unsafe {
            let tag = ptr.sub(8).cast::<u64>().read();
            sub(tag, layout.size());
            System.dealloc(ptr.sub(head), outer);
        }
    }
}

#[global_allocator]
static GLOBAL: Peak = Peak;

/// Stops tracking when dropped, also on unwinding.
struct Tracking;

impl Tracking {
    fn start() -> Self {
        TRACK.with(|t| t.set(true));
        Self
    }
}

impl Drop for Tracking {
    fn drop(&mut self) {
        TRACK.with(|t| t.set(false));
    }
}

/// Runs `f` tracked as a new measurement; returns its result and the peak
/// of live bytes. `check` runs on the result under the same lock, after
/// tracking stops, so counter reads see only this measurement.
fn measure_then<T>(f: impl FnOnce() -> T, check: impl FnOnce(&T)) -> (T, i64) {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Only this thread, holding the lock, changes the measurement.
    let next = generation(STATE.load(Ordering::Relaxed)) + 1;
    STATE.store(next << GEN_SHIFT, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    let out = {
        let _tracking = Tracking::start();
        f()
    };
    check(&out);
    (out, PEAK.load(Ordering::Relaxed))
}

/// [`measure_then`] with no check.
fn measure<T>(f: impl FnOnce() -> T) -> (T, i64) {
    measure_then(f, |_| {})
}

const MIB: i64 = 1 << 20;
const AT0: FileAt = FileAt { file: 0, hi: false };

#[test]
fn the_count_works() {
    let ((), peak) = measure_then(
        || {
            let v = std::hint::black_box(vec![1_u8; 3 << 20]);
            drop(v);
            let w = std::hint::black_box(vec![1_u8; 1 << 20]);
            drop(w);
        },
        |()| assert_eq!(live(), 0),
    );
    assert_eq!(peak, 3 * MIB);
    // A result that outlives its measurement is not charged to the next
    // one when it is freed there.
    let (kept, _) = measure(|| std::hint::black_box(vec![1_u8; 2 << 20]));
    let ((), peak) = measure_then(
        || {
            let v = std::hint::black_box(vec![1_u8; 1 << 20]);
            drop(kept);
            drop(v);
        },
        |()| assert_eq!(live(), 0),
    );
    assert_eq!(peak, MIB);
}

/// Loads `names` tracked; the peak must stay under 8 MiB.
fn load_small(root: &std::path::Path, names: &[&str]) -> Result<Sheet, ImageError> {
    let (result, peak) = measure(|| load_files(root, names));
    assert!(peak < 8 * MIB, "{names:?}: peak {peak}");
    result
}

#[test]
fn headers_are_checked_before_pixels() {
    let d = dir();
    let root = d.path();
    write(root, "wide.png", &png_header(16_385, 1));
    let err = load_small(root, &["wide.png"]).unwrap_err();
    assert!(matches!(
        err,
        ImageError::TooWide {
            at: AT0,
            w: 16_385,
            h: 1
        }
    ));
    // 4 096 × 2 048 is the texel cap; one more row passes it.
    assert_eq!(4_096 * 2_048, MAX_IMAGE_TEXELS);
    write(root, "big.png", &png_header(4_096, 2_049));
    let err = load_small(root, &["big.png"]).unwrap_err();
    assert!(matches!(err, ImageError::TooManyTexels { at: AT0 }));
    write(root, "cap.png", &png_header(4_096, 2_048));
    load_small(root, &["cap.png"]).unwrap();
    // Sixteen at the cap fill the total; the 17th passes it.
    assert_eq!(16 * MAX_IMAGE_TEXELS, MAX_TOTAL_TEXELS);
    let names = vec!["cap.png"; 17];
    load_small(root, &names[..16]).unwrap();
    let err = load_small(root, &names).unwrap_err();
    assert!(matches!(
        err,
        ImageError::TotalTooLarge {
            at: FileAt {
                file: 16,
                hi: false
            }
        }
    ));
    // BMP: huge declared sizes, refused by the decoder's own limit or ours.
    write(root, "huge.bmp", &bmp_header(100_000, 100_000));
    load_small(root, &["huge.bmp"]).unwrap_err();
    write(root, "wide.bmp", &bmp_header(20_000, 1));
    let err = load_small(root, &["wide.bmp"]).unwrap_err();
    assert!(
        matches!(err, ImageError::TooWide { at: AT0, .. }),
        "{err:?}"
    );
    write(root, "big.bmp", &bmp_header(4_096, 4_096));
    let err = load_small(root, &["big.bmp"]).unwrap_err();
    assert!(
        matches!(err, ImageError::TooManyTexels { at: AT0 }),
        "{err:?}"
    );
    write(root, "zero.png", &png_header(0, 4));
    assert!(load_small(root, &["zero.png"]).is_err());
}

/// A PNG whose image data starts at about `at` bytes: private ancillary
/// chunks between the header and the data.
fn png_data_at(w: u32, h: u32, at: usize) -> Vec<u8> {
    let mut out = png_start(w, h);
    while out.len() + 12 < at {
        let n = (at - out.len() - 12).min(60_000);
        out.extend_from_slice(&chunk(b"prVt", &vec![7; n]));
    }
    // The cut-short image data after the signature and header.
    out.extend_from_slice(&png_header(w, h)[png_start(w, h).len()..]);
    out
}

/// A JPEG with legal application segments before its frame header, which
/// then starts at about `at` bytes.
fn jpeg_frame_at(at: usize) -> Vec<u8> {
    let mut plain = Vec::new();
    RgbImage::from_pixel(8, 8, image::Rgb([9, 9, 9]))
        .write_to(&mut std::io::Cursor::new(&mut plain), ImageFormat::Jpeg)
        .unwrap();
    let mut out = plain[..2].to_vec();
    while out.len() + plain.len() < at {
        let n = (at - out.len() - plain.len()).clamp(2, 60_000);
        out.extend_from_slice(&[0xFF, 0xE9]);
        out.extend_from_slice(&u16::try_from(n + 2).unwrap().to_be_bytes());
        out.extend_from_slice(&vec![7; n]);
    }
    out.extend_from_slice(&plain[2..]);
    out
}

#[test]
fn a_header_must_lie_within_its_first_mebibyte() {
    let d = dir();
    let root = d.path();
    write(root, "late.png", &png_data_at(4, 4, (1 << 20) + 100));
    let err = load_small(root, &["late.png"]).unwrap_err();
    assert!(
        matches!(err, ImageError::HeaderTooLarge { at: AT0 }),
        "{err:?}"
    );
    write(root, "early.png", &png_data_at(4, 4, (1 << 20) - 100));
    load_small(root, &["early.png"]).unwrap();

    write(root, "late.jpg", &jpeg_frame_at((1 << 20) + 100));
    let err = load_small(root, &["late.jpg"]).unwrap_err();
    assert!(
        matches!(err, ImageError::HeaderTooLarge { at: AT0 }),
        "{err:?}"
    );
    write(root, "early.jpg", &jpeg_frame_at((1 << 20) - 100));
    let sheet = load_small(root, &["early.jpg"]).unwrap();
    sheet.atlas(Variant::Lo, LIMITS).unwrap();
}

#[test]
fn atlas_caps_come_before_its_pages() {
    let d = dir();
    let root = d.path();
    // One page of 256 holds four 112-texel frames (128 padded); five need
    // two pages, and the limits leave one.
    save(root, "a.png", &solid(112 * 5, 112, [255; 4]));
    let five = ImageDef {
        grid: [5, 1],
        ..whole(0)
    };
    let sheet = load(root, vec![file("a.png")], vec![five]).unwrap();
    let limits = AtlasLimits {
        max_side: 256,
        max_layers: 2,
        reserved_layers: 1,
    };
    let page = 256 * 256 * 4;
    let (err, peak) = measure(|| sheet.atlas(Variant::Lo, limits).unwrap_err());
    assert!(matches!(err, AtlasError::TooManyPages), "{err:?}");
    assert!(peak < page, "peak {peak}");
    let roomier = AtlasLimits {
        max_layers: 3,
        ..limits
    };
    assert_eq!(sheet.atlas(Variant::Lo, roomier).unwrap().pages().len(), 2);
    // 250 texels pad to 272, past a 256 side.
    save(root, "b.png", &solid(250, 4, [255; 4]));
    let sheet = load_files(root, &["b.png"]).unwrap();
    let (err, peak) = measure(|| sheet.atlas(Variant::Lo, limits).unwrap_err());
    assert!(
        matches!(err, AtlasError::CellTooLarge { w: 250, h: 4 }),
        "{err:?}"
    );
    assert!(peak < page, "peak {peak}");
    // Limits that leave no layer, a side below 256, or a layer past u16.
    for bad in [
        AtlasLimits {
            reserved_layers: 2,
            ..limits
        },
        AtlasLimits {
            max_side: 255,
            ..limits
        },
        AtlasLimits {
            max_layers: 70_000,
            reserved_layers: 65_500,
            ..limits
        },
    ] {
        assert!(
            matches!(sheet.atlas(Variant::Lo, bad), Err(AtlasError::Limits)),
            "{bad:?}"
        );
    }
}

#[test]
fn a_file_whose_dimensions_changed_is_refused() {
    let d = dir();
    save(d.path(), "a.png", &solid(4, 4, [255; 4]));
    let sheet = load_files(d.path(), &["a.png"]).unwrap();
    save(d.path(), "a.png", &solid(5, 4, [255; 4]));
    let err = sheet.atlas(Variant::Lo, LIMITS).unwrap_err();
    assert!(
        matches!(
            err,
            AtlasError::Image(ImageError::DimensionsChanged { at: AT0 })
        ),
        "{err:?}"
    );
}

#[test]
fn files_grown_since_load_are_charged_again() {
    let d = dir();
    let root = d.path();
    save(root, "a.png", &solid(4, 4, [255; 4]));
    let sheet = load_files(root, &["a.png"]).unwrap();
    // Trailing bytes after the image's end do not change its header.
    let f = File::options()
        .write(true)
        .open(root.join("a.png"))
        .unwrap();
    f.set_len(MAX_FILE_BYTES + 1).unwrap();
    let err = sheet.atlas(Variant::Lo, LIMITS).unwrap_err();
    assert!(
        matches!(err, AtlasError::Image(ImageError::FileTooLarge { at: AT0 })),
        "{err:?}"
    );
    // Seventeen files grown to the per-file cap pass the build's total.
    let names: Vec<String> = (0..17).map(|i| format!("{i}.png")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    for n in &names {
        save(root, n, &solid(4, 4, [255; 4]));
    }
    let sheet = load_files(root, &refs).unwrap();
    for n in &names {
        let f = File::options().write(true).open(root.join(n)).unwrap();
        f.set_len(MAX_FILE_BYTES).unwrap();
    }
    const { assert!(17 * MAX_FILE_BYTES > MAX_TOTAL_FILE_BYTES) };
    let err = sheet.atlas(Variant::Lo, LIMITS).unwrap_err();
    assert!(
        matches!(
            err,
            AtlasError::Image(ImageError::TotalBytesTooLarge {
                at: FileAt {
                    file: 16,
                    hi: false
                }
            })
        ),
        "{err:?}"
    );
}

#[test]
fn one_file_decodes_within_its_bound() {
    // The widest format at the texel cap: 16-bit RGBA, 4 096 × 2 048.
    let d = dir();
    let (w, h) = (4_096_u32, 2_048_u32);
    let texels = i64::from(w) * i64::from(h);
    {
        let pixels = vec![0x80_u8; usize::try_from(texels * 8).unwrap()];
        let mut f = std::io::BufWriter::new(File::create(d.path().join("deep.png")).unwrap());
        PngEncoder::new_with_quality(&mut f, CompressionType::Default, FilterType::NoFilter)
            .write_image(&pixels, w, h, ExtendedColorType::Rgba16)
            .unwrap();
        f.flush().unwrap();
    }
    let encoded =
        i64::try_from(std::fs::metadata(d.path().join("deep.png")).unwrap().len()).unwrap();
    let corner = ImageDef {
        rect: Some([0, 0, 8, 8]),
        ..whole(0)
    };
    let sheet = load(d.path(), vec![file("deep.png")], vec![corner]).unwrap();
    let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, LIMITS).unwrap());
    assert_eq!(atlas.side(), 256);
    // One page of 256 with its mips, then the file: the larger of the
    // encoded bytes beside the native image (8 B a texel) and the native
    // image beside its RGBA8 copy (4 B a texel), plus 1 MiB for the
    // decoder's own buffers and the tables.
    let pages = 256 * 256 * 4 + 128 * 128 * 4 + 64 * 64 * 4 + 32 * 32 * 4;
    let file = (encoded + 8 * texels).max(12 * texels);
    assert!(
        peak <= pages + file + MIB,
        "peak {peak}, bound {}",
        pages + file + MIB
    );
    // The bound is reached: the conversion did run beside the native image.
    assert!(peak >= 12 * texels, "peak {peak}");
}
