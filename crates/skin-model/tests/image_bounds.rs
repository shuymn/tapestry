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
    LIMITS, bmp_header, chunk, dir, file, load, load_files, png_header, png_header_with, png_start,
    save, solid, whole, write,
};
use skin_model::{
    AtlasError, AtlasLimits, CELL_TABLE_BYTES, FileAt, ImageDef, ImageError, MAX_FILE_BYTES,
    MAX_IMAGE_TEXELS, MAX_SKIN_FRAMES, MAX_STREAMED_TEXELS, MAX_TOTAL_FILE_BYTES, MAX_TOTAL_TEXELS,
    Sheet, TexelCap, Unbanded, Variant,
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
    // 4 096 × 3 072 is the full-decode texel cap; a PNG one row past it
    // is admitted to be decoded in bands, up to 8 192 × 8 192; one more
    // row passes the banded cap.
    assert_eq!(4_096 * 3_072, MAX_IMAGE_TEXELS);
    assert_eq!(8_192 * 8_192, MAX_STREAMED_TEXELS);
    write(root, "cap.png", &png_header(4_096, 3_072));
    load_small(root, &["cap.png"]).unwrap();
    write(root, "big.png", &png_header(4_096, 3_073));
    load_small(root, &["big.png"]).unwrap();
    write(root, "banded.png", &png_header(8_192, 8_192));
    load_small(root, &["banded.png"]).unwrap();
    write(root, "huge.png", &png_header(8_192, 8_193));
    let err = load_small(root, &["huge.png"]).unwrap_err();
    assert!(
        matches!(
            err,
            ImageError::TooManyTexels {
                at: AT0,
                cap: TexelCap::Banded
            }
        ),
        "{err:?}"
    );
    // Past the full-decode cap, an interlaced or animated PNG, a JPEG and
    // a BMP are refused, each with its reason; at the cap they are not.
    let actl = chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]);
    let cases: [(&str, Vec<u8>, Vec<u8>, Unbanded); 4] = [
        (
            "adam7.png",
            png_header_with(4_096, 3_073, true, &[]),
            png_header_with(4_096, 3_072, true, &[]),
            Unbanded::Interlaced,
        ),
        (
            "anim.png",
            png_header_with(4_096, 3_073, false, &actl),
            png_header_with(4_096, 3_072, false, &actl),
            Unbanded::Animated,
        ),
        (
            "big.jpg",
            jpeg_sized(4_096, 3_073),
            jpeg_sized(4_096, 3_072),
            Unbanded::Format,
        ),
        (
            "big.bmp",
            bmp_header(4_096, 3_073),
            bmp_header(4_096, 3_072),
            Unbanded::Format,
        ),
    ];
    for (name, over, at_cap, why) in cases {
        write(root, name, &over);
        let err = load_small(root, &[name]).unwrap_err();
        assert!(
            matches!(
                err,
                ImageError::TooManyTexels {
                    at: AT0,
                    cap: TexelCap::FullDecode(w)
                } if w == why
            ),
            "{name}: {err:?}"
        );
        write(root, name, &at_cap);
        load_small(root, &[name]).unwrap();
    }
    // Ten at the cap and one of 8 Mi fill the total; one texel more
    // passes it.
    write(root, "rest.png", &png_header(4_096, 2_048));
    write(root, "one.png", &png_header(1, 1));
    assert_eq!(10 * MAX_IMAGE_TEXELS + 4_096 * 2_048, MAX_TOTAL_TEXELS);
    let mut names = vec!["cap.png"; 10];
    names.extend(["rest.png", "one.png"]);
    load_small(root, &names[..11]).unwrap();
    let err = load_small(root, &names).unwrap_err();
    assert!(matches!(
        err,
        ImageError::TotalTooLarge {
            at: FileAt {
                file: 11,
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

/// An 8×8 JPEG whose frame header declares `w`×`h`.
fn jpeg_sized(w: u16, h: u16) -> Vec<u8> {
    let mut out = jpeg_frame_at(0);
    let sof = out
        .windows(2)
        .position(|m| m == [0xFF, 0xC0])
        .expect("a baseline frame header");
    // Marker, length, precision, then height and width.
    out[sof + 5..sof + 7].copy_from_slice(&h.to_be_bytes());
    out[sof + 7..sof + 9].copy_from_slice(&w.to_be_bytes());
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
    sheet.atlas(Variant::Lo, LIMITS, &|| false).unwrap();
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
    let (err, peak) = measure(|| sheet.atlas(Variant::Lo, limits, &|| false).unwrap_err());
    assert!(matches!(err, AtlasError::TooManyPages), "{err:?}");
    assert!(peak < page, "peak {peak}");
    let roomier = AtlasLimits {
        max_layers: 3,
        ..limits
    };
    assert_eq!(
        sheet
            .atlas(Variant::Lo, roomier, &|| false)
            .unwrap()
            .pages()
            .len(),
        2
    );
    // 250 texels pad to 272, past a 256 side.
    save(root, "b.png", &solid(250, 4, [255; 4]));
    let sheet = load_files(root, &["b.png"]).unwrap();
    let (err, peak) = measure(|| sheet.atlas(Variant::Lo, limits, &|| false).unwrap_err());
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
            matches!(
                sheet.atlas(Variant::Lo, bad, &|| false),
                Err(AtlasError::Limits)
            ),
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
    let err = sheet.atlas(Variant::Lo, LIMITS, &|| false).unwrap_err();
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
    let err = sheet.atlas(Variant::Lo, LIMITS, &|| false).unwrap_err();
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
    let err = sheet.atlas(Variant::Lo, LIMITS, &|| false).unwrap_err();
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
    // The widest format at the texel cap: 16-bit RGBA, 4 096 × 3 072.
    let d = dir();
    let (w, h) = (4_096_u32, 3_072_u32);
    let texels = i64::from(w) * i64::from(h);
    assert_eq!(u64::from(w) * u64::from(h), MAX_IMAGE_TEXELS);
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
    let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, LIMITS, &|| false).unwrap());
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

/// Writes a `w`×`h` 8-bit RGBA PNG at `path`, row by row, texel `(x, y)`
/// the colour `texel` gives; each row filtered against the one above, so
/// rows that repeat compress to almost nothing.
fn stream_png(path: &std::path::Path, w: u32, h: u32, texel: impl Fn(u32, u32) -> [u8; 4]) {
    let f = std::io::BufWriter::new(File::create(path).unwrap());
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    enc.set_filter(png::Filter::Up);
    let mut header = enc.write_header().unwrap();
    let mut out = header.stream_writer().unwrap();
    let mut row = Vec::with_capacity(usize::try_from(w).unwrap() * 4);
    for y in 0..h {
        row.clear();
        row.extend((0..w).flat_map(|x| texel(x, y)));
        out.write_all(&row).unwrap();
    }
    out.finish().unwrap();
}

/// The bytes of `pages` pages of `side` with their mips.
fn page_bytes(pages: usize, side: u32) -> i64 {
    let side = i64::from(side);
    let one: i64 = (0..4).map(|k| (side >> k) * (side >> k) * 4).sum();
    i64::try_from(pages).unwrap() * one
}

/// The bound a file decoded in bands stays within beside the pages and
/// the atlas's tables (a term of their own, at most 65 536 ×
/// [`CELL_TABLE_BYTES`]): `W_band = max(2E, E + 16 MiB)` for `E` encoded
/// bytes, `2E` while a file that grew since its length was checked is
/// read, plus the 1 MiB margin the whole-file bound keeps for an entry
/// and canonical path of the file. The 16 MiB holds
/// `png`'s own count (its metadata and output row, within its 1 MiB
/// limit), its window of filtered rows (128 KiB at first, growing by rows
/// of at most 128 KiB + 1 at 16 384 texels of 16-bit RGBA, with the old
/// and new buffers live together while it grows), the inflater's tables
/// (about 18 KiB), the 1 KiB palette table, one expanded row, one band
/// native beside its RGBA8 copy (16 rows × 16 384 texels × (8 + 4) B =
/// 3 MiB at the widest) and the routing index (at most 65 536 cells).
fn banded_bound(encoded: i64) -> i64 {
    (2 * encoded).max(encoded + 16 * MIB) + MIB
}

const LIMITS_8K: AtlasLimits = AtlasLimits {
    max_side: 8_192,
    max_layers: 2,
    reserved_layers: 1,
};

/// The memory witness for a file decoded in bands: a 6400×7941 RGBA8
/// sheet of 55 frames of 1280×720 (five a row, eleven rows, 21 rows
/// below them unused), 48.5 Mi texels, more than 4 × the full-decode
/// cap. Decoded whole it would hold its RGBA8 image, 194 MiB; in bands
/// the build peaks at the pages beside
/// `W_band = max(2E, E + 16 MiB) + 1 MiB` ([`banded_bound`]): with `E`
/// about 0.1 MiB here, 16.1 MiB + 1 MiB. Each frame, padded to 1296×736,
/// packs six a row and eleven rows deep in one page of 8 192, whose four
/// levels hold 256 + 64 + 16 + 4 MiB.
#[test]
fn a_sheet_past_the_full_decode_cap_decodes_in_bands_within_its_bound() {
    let d = dir();
    let (w, h) = (6_400_u32, 7_941_u32);
    let (cw, ch) = (1_280_u32, 720_u32);
    assert!(u64::from(w) * u64::from(h) > 4 * MAX_IMAGE_TEXELS);
    // Frame `(c, r)` is one colour, half transparent in odd frames; its
    // first column marks its index.
    let colour = |c: u32, r: u32| {
        let i = u8::try_from(r * 5 + c).unwrap();
        [i * 4, 255 - i, 77, if i % 2 == 1 { 128 } else { 255 }]
    };
    stream_png(&d.path().join("close.png"), w, h, |x, y| {
        colour((x / cw).min(4), (y / ch).min(10))
    });
    let encoded =
        i64::try_from(std::fs::metadata(d.path().join("close.png")).unwrap().len()).unwrap();
    let frames = ImageDef {
        rect: Some([0, 0, 5 * cw, 11 * ch]),
        grid: [5, 11],
        ..whole(0)
    };
    let sheet = load(d.path(), vec![file("close.png")], vec![frames]).unwrap();
    let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, LIMITS_8K, &|| false).unwrap());
    let pages = page_bytes(atlas.pages().len(), atlas.side());
    let bound = pages + banded_bound(encoded);
    assert!(
        peak <= bound,
        "peak {peak}, bound {bound}, encoded {encoded}"
    );
    // The encoded bytes were live beside the pages.
    assert!(peak >= pages + encoded, "peak {peak}");
    // Every frame holds its colour, premultiplied, at its first texel.
    assert_eq!(sheet.frame_count(), 55);
    let side = atlas.side();
    for (i, f) in atlas.uvs().frames().iter().enumerate() {
        let i = u32::try_from(i).unwrap();
        let [r, g, b, a] = colour(i % 5, i / 5);
        let pre = |c: u8| u8::try_from((u32::from(c) * u32::from(a) + 127) / 255).unwrap();
        let page = &atlas.pages()[usize::from(f.layer) - 1];
        let (x, y) = (
            (f.uv[0] * side as f32) as usize,
            (f.uv[1] * side as f32) as usize,
        );
        let at = (y * side as usize + x) * 4;
        assert_eq!(
            page.level(0).unwrap()[at..at + 4],
            [pre(r), pre(g), pre(b), a],
            "frame {i}"
        );
    }
}

/// Routing keeps a window of the cells a band crosses: 1,100 cells of
/// 1×4000, each its own column of a 1100×12000 file decoded in bands, a
/// third of them crossing every band, decode within the same bound, each
/// cell its column's colour from its first row to its last.
#[test]
fn many_tall_thin_cells_route_within_the_bound() {
    let d = dir();
    let (w, h) = (1_100_u32, 12_000_u32);
    assert!(u64::from(w) * u64::from(h) > MAX_IMAGE_TEXELS);
    let colour = |x: u32, y: u32| {
        let [lo, hi] = u16::try_from(x).unwrap().to_le_bytes();
        [lo, hi, u8::try_from(y / 4_000).unwrap(), 255]
    };
    stream_png(&d.path().join("thin.png"), w, h, colour);
    let encoded =
        i64::try_from(std::fs::metadata(d.path().join("thin.png")).unwrap().len()).unwrap();
    let images = (0..w)
        .map(|x| ImageDef {
            rect: Some([x, (x % 3) * 4_000, 1, 4_000]),
            ..whole(0)
        })
        .collect();
    let sheet = load(d.path(), vec![file("thin.png")], images).unwrap();
    // Two pages of 8 192 and no reserved layer: the 128 Mi texel cap.
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 2,
        reserved_layers: 0,
    };
    let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, limits, &|| false).unwrap());
    let pages = page_bytes(atlas.pages().len(), atlas.side());
    let bound = pages + banded_bound(encoded);
    assert!(
        peak <= bound,
        "peak {peak}, bound {bound}, encoded {encoded}"
    );
    let side = atlas.side() as usize;
    for (x, f) in (0_u32..).zip(atlas.uvs().frames()) {
        let page = atlas.pages()[usize::from(f.layer)].level(0).unwrap();
        let (u, v0, v1) = (
            (f.uv[0] * side as f32) as usize,
            (f.uv[1] * side as f32) as usize,
            (f.uv[3] * side as f32) as usize,
        );
        for v in [v0, v1 - 1] {
            let at = (v * side + u) * 4;
            assert_eq!(page[at..at + 4], colour(x, (x % 3) * 4_000), "cell {x}");
        }
    }
}

/// [`stream_png`] at 16 bits a sample.
fn stream_png16(path: &std::path::Path, w: u32, h: u32, texel: impl Fn(u32, u32) -> [u16; 4]) {
    let f = std::io::BufWriter::new(File::create(path).unwrap());
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Sixteen);
    enc.set_compression(png::Compression::Fast);
    enc.set_filter(png::Filter::Up);
    let mut header = enc.write_header().unwrap();
    let mut out = header.stream_writer().unwrap();
    let mut row = Vec::with_capacity(usize::try_from(w).unwrap() * 8);
    for y in 0..h {
        row.clear();
        row.extend((0..w).flat_map(|x| texel(x, y)).flat_map(u16::to_be_bytes));
        out.write_all(&row).unwrap();
    }
    out.finish().unwrap();
}

/// One file's routing is freed before the next file is read, and the
/// widest band fits the bound. `many.png` (2049×16, decoded whole) has
/// 32 769 cells of 1×1 in its first band; `deep.png` is 16 384 × 1 025
/// RGBA16 (16.8 Mi texels, decoded in bands: each band 16 rows × 16 384
/// texels, 2 MiB native beside 1 MiB RGBA8), padded after `IEND` to
/// `E` = 32 MiB, so its bound is `max(2E, E + 16 MiB) + 1 MiB` =
/// 64 MiB + 1 MiB beside the pages. Built with `many.png` read first, the
/// peak past the pages is that of the build reading `deep.png` first:
/// none of `many.png`'s routing (about 1.5 MiB for its cells) is live
/// while `deep.png` is read and decoded.
#[test]
fn one_files_routing_is_freed_before_the_next_file_is_read() {
    let d = dir();
    save(d.path(), "many.png", &solid(2_049, 16, [9, 8, 7, 255]));
    let deep = d.path().join("deep.png");
    let colour = [0x8080_u16, 0x4000, 0xFFFF, 0xFFFF];
    stream_png16(&deep, 16_384, 1_025, |_, _| colour);
    let f = File::options().write(true).open(&deep).unwrap();
    f.set_len(MAX_FILE_BYTES).unwrap();
    drop(f);
    let encoded = i64::try_from(MAX_FILE_BYTES).unwrap();
    let images = |many: u32, deep: u32| {
        let mut v: Vec<ImageDef> = (0..16)
            .flat_map(|r| {
                [0, 1_024].map(|x| ImageDef {
                    rect: Some([x, r, 1_024, 1]),
                    grid: [1_024, 1],
                    ..whole(many)
                })
            })
            .collect();
        v.push(ImageDef {
            rect: Some([2_048, 0, 1, 1]),
            ..whole(many)
        });
        v.push(ImageDef {
            rect: Some([0, 0, 8, 1_025]),
            ..whole(deep)
        });
        v
    };
    let build = |files: [&str; 2], many: u32, deep: u32| {
        let sheet = load(d.path(), files.map(file).to_vec(), images(many, deep)).unwrap();
        assert_eq!(sheet.frame_count(), 32_770);
        // Two pages of 8 192 and no reserved layer: the 128 Mi texel cap.
        let limits = AtlasLimits {
            max_side: 8_192,
            max_layers: 2,
            reserved_layers: 0,
        };
        let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, limits, &|| false).unwrap());
        let pages = page_bytes(atlas.pages().len(), atlas.side());
        let bound = pages + banded_bound(encoded);
        assert!(peak <= bound, "peak {peak}, bound {bound}");
        // The deep cell's first texel, its 16-bit samples rounded as a
        // whole decode rounds them.
        let f = atlas.uvs().frames()[32_769];
        let side = atlas.side() as usize;
        let (x, y) = (
            (f.uv[0] * side as f32) as usize,
            (f.uv[1] * side as f32) as usize,
        );
        let page = atlas.pages()[usize::from(f.layer)].level(0).unwrap();
        let at = (y * side + x) * 4;
        let want = colour.map(|v| u8::try_from((u32::from(v) + 128) / 257).unwrap());
        assert_eq!(page[at..at + 4], want);
        peak - pages
    };
    let deep_first = build(["deep.png", "many.png"], 1, 0);
    let many_first = build(["many.png", "deep.png"], 0, 1);
    assert!(
        many_first <= deep_first + (256 << 10),
        "many first {many_first}, deep first {deep_first}"
    );
}

/// The atlas's tables are a term of their own beside a file's bound: at
/// most 65 536 cells × [`CELL_TABLE_BYTES`] (16 B for a rectangle and
/// 20 B for a place) = 2 359 296 B, live while every file is read. Here
/// 65 536 cells of 1×1 (64 images of 1 024 frames) cut a 16 384 × 1 025
/// RGBA16 file, 1 MiB when it is admitted, that grows to `E` = 32 MiB as
/// it is read (the cancel's first ask, before the first piece, extends
/// it): the read's last step holds the old 31 MiB beside the new 32 MiB.
/// The peak stays within pages + tables + `max(2E, E + 16 MiB) + 1 MiB`
/// = pages + 2 359 296 B + 65 MiB, and passes pages + 65 MiB, the bound
/// without the tables.
#[test]
fn the_most_cells_and_a_file_grown_while_read_stay_within_the_bound() {
    let d = dir();
    let path = d.path().join("grow.png");
    stream_png16(&path, 16_384, 1_025, |_, _| {
        [0x8080, 0x4000, 0xFFFF, 0xFFFF]
    });
    let f = File::options().write(true).open(&path).unwrap();
    f.set_len(1 << 20).unwrap();
    drop(f);
    let images = (0..64)
        .map(|r| ImageDef {
            rect: Some([0, r, 1_024, 1]),
            grid: [1_024, 1],
            ..whole(0)
        })
        .collect();
    let sheet = load(d.path(), vec![file("grow.png")], images).unwrap();
    assert_eq!(sheet.frame_count(), MAX_SKIN_FRAMES);
    let grown = std::cell::Cell::new(false);
    let grow = || {
        if !grown.replace(true) {
            let f = File::options().write(true).open(&path).unwrap();
            f.set_len(MAX_FILE_BYTES).unwrap();
        }
        false
    };
    // Eight layers and none reserved: a page holds at most 32 767 cells,
    // so the cells take three pages of 4 096.
    let limits = AtlasLimits {
        max_side: 8_192,
        max_layers: 8,
        reserved_layers: 0,
    };
    let (atlas, peak) = measure(|| sheet.atlas(Variant::Lo, limits, &grow).unwrap());
    assert!(grown.get());
    let pages = page_bytes(atlas.pages().len(), atlas.side());
    let encoded = i64::try_from(MAX_FILE_BYTES).unwrap();
    let tables = i64::try_from(MAX_SKIN_FRAMES * CELL_TABLE_BYTES).unwrap();
    assert_eq!(tables, 2_359_296);
    let bound = pages + tables + banded_bound(encoded);
    assert!(peak <= bound, "peak {peak}, bound {bound}");
    let without_tables = pages + banded_bound(encoded);
    assert!(
        peak > without_tables,
        "peak {peak}, bound without the tables {without_tables}"
    );
}
