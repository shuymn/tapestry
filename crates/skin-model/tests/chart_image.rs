//! A chart's own images (`skin_model::chart_image`): names confined to the
//! chart folder (design.md "Threat Model", T08), the skin images' caps,
//! the observation a read returns, and the pixels fitted into a slot as
//! the atlas lays out a cell. Every image is generated here.

mod image_support;

use std::cell::Cell;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::Path;

use image::{ImageFormat, Rgba, RgbaImage};
use image_support::{dir, link_dir, save, solid};
use skin_model::chart_image::{
    Absent, Binding, Cap, ChartDir, FileIdentity, Fitted, Read, Slot, Unresolved, read, resolve,
};
use skin_model::{GUTTER, MAX_FILE_BYTES, MAX_PATH_BYTES, MIP_LEVELS};

/// An identity every file shares: observations then differ only by path,
/// length and modification time.
fn fixed(_: &File) -> Option<FileIdentity> {
    Some(FileIdentity(1))
}

fn never() -> bool {
    false
}

fn slot(side: u32) -> Slot {
    Slot::new([side, side]).unwrap()
}

/// Reads `label` in `root` into a slot of `side`, never cancelled.
fn read_in(root: &Path, label: &str, side: u32) -> Read {
    let dir = ChartDir::new(root).unwrap();
    read(&dir, label, &[], slot(side), &never, &fixed)
}

fn fitted(r: Read) -> Fitted {
    match r.image {
        Ok(f) => f,
        Err(e) => panic!("not read: {e:?} ({:?})", r.binding),
    }
}

fn absent(r: &Read) -> Absent {
    r.image.clone().unwrap_err()
}

/// The texel at (`x`, `y`) of level `k`.
fn texel(f: &Fitted, k: usize, x: u32, y: u32) -> [u8; 4] {
    let [w, _] = f.level_size(k).unwrap();
    let at = ((y * w + x) * 4) as usize;
    f.levels()[k][at..at + 4].try_into().unwrap()
}

/// `img` encoded as `format`.
fn encoded(img: &RgbaImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    match format {
        // JPEG and BMP here carry no alpha.
        ImageFormat::Jpeg => image::DynamicImage::ImageRgba8(img.clone())
            .to_rgb8()
            .write_to(&mut bytes, format)
            .unwrap(),
        _ => img.write_to(&mut bytes, format).unwrap(),
    }
    bytes.into_inner()
}

/// A PNG whose header says `w`×`h`, with an empty image data chunk:
/// enough for the header's caps, which refuse it before any decode.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut c = !0_u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for chunk in [ihdr, b"IDAT".to_vec(), b"IEND".to_vec()] {
        out.extend(u32::try_from(chunk.len() - 4).unwrap().to_be_bytes());
        out.extend(&chunk);
        out.extend(crc(&chunk).to_be_bytes());
    }
    out
}

// Covers: T08
#[test]
fn names_that_leave_the_folder_are_unresolved_without_reading() {
    let outer = dir();
    let root = outer.path().join("chart");
    fs::create_dir_all(root.join("a")).unwrap();
    // Every target exists, so only the name rules can refuse them.
    save(outer.path(), "x.png", &solid(1, 1, [255; 4]));
    save(&root, "x.png", &solid(1, 1, [255; 4]));
    let unc = "\\\\server\\s\\x.png".to_owned();
    let verbatim = format!("\\\\?\\{}", outer.path().join("x.png").display());
    let absolute = outer.path().join("x.png").display().to_string();
    for name in [
        "../x.png",
        "a/../../x.png",
        "a\\..\\..\\x.png",
        "/x.png",
        "\\x.png",
        unc.as_str(),
        verbatim.as_str(),
        absolute.as_str(),
        "C:\\x.png",
        "c:x.png",
        "x.png:s",
    ] {
        let r = read_in(&root, name, 64);
        assert!(
            matches!(
                r.binding,
                Binding::Unresolved {
                    why: Unresolved::Rejected,
                    ..
                }
            ),
            "{name}: {:?}",
            r.binding
        );
        assert_eq!(
            absent(&r),
            Absent::Unresolved(Unresolved::Rejected),
            "{name}"
        );
    }
}

// Covers: T08
#[test]
fn an_overlong_name_is_refused_whole_never_shortened() {
    let d = dir();
    // The first `MAX_PATH_BYTES` bytes would name a file that exists if
    // the name were cut; no such cut name can be created on every file
    // system, so the check is that the refusal comes before any lookup.
    let long = format!("{}.png", "a".repeat(MAX_PATH_BYTES + 1 - 4));
    assert_eq!(long.len(), MAX_PATH_BYTES + 1);
    let r = read_in(d.path(), &long, 64);
    assert_eq!(
        r.binding,
        Binding::Unresolved {
            name: String::new(),
            why: Unresolved::Overlong
        }
    );
    // Trimming happens before the length is checked: at the limit with
    // surrounding spaces is a name, kept whole, which the file system then
    // finds missing or refuses itself.
    let at = format!("  {}.png ", "a".repeat(MAX_PATH_BYTES - 4));
    let r = read_in(d.path(), &at, 64);
    let Binding::Unresolved { name, why } = &r.binding else {
        panic!("{:?}", r.binding);
    };
    assert_eq!(name.len(), MAX_PATH_BYTES);
    assert!(
        matches!(why, Unresolved::Missing | Unresolved::Io(_)),
        "{why:?}"
    );
}

#[test]
fn an_empty_name_reads_nothing() {
    let d = dir();
    for label in ["", "   ", "\u{3000}\t"] {
        let r = read_in(d.path(), label, 64);
        assert_eq!(r.binding, Binding::NoHeader);
        assert_eq!(absent(&r), Absent::NoHeader);
    }
}

// Covers: T08
#[test]
fn links_out_of_the_folder_escape_and_links_inside_resolve() {
    let outer = dir();
    let root = outer.path().join("chart");
    let away = outer.path().join("away");
    fs::create_dir_all(root.join("inside")).unwrap();
    fs::create_dir_all(&away).unwrap();
    save(&away, "x.png", &solid(2, 2, [9; 4]));
    save(&root.join("inside"), "x.png", &solid(2, 2, [9; 4]));
    link_dir(&root.join("out"), &away);
    link_dir(&root.join("in"), &root.join("inside"));
    let r = read_in(&root, "out/x.png", 64);
    assert_eq!(absent(&r), Absent::Unresolved(Unresolved::Escapes));
    let r = read_in(&root, "in\\x.png", 64);
    let Binding::Resolved { path, .. } = &r.binding else {
        panic!("{:?}", r.binding);
    };
    assert_eq!(
        path,
        &ChartDir::new(&root.join("inside"))
            .unwrap()
            .path()
            .join("x.png")
    );
    assert_eq!(fitted(r).content(), [2, 2]);
}

#[test]
fn missing_files_and_folders_are_unresolved_and_no_other_extension_is_tried() {
    let d = dir();
    fs::create_dir_all(d.path().join("folder")).unwrap();
    save(d.path(), "x.bmp", &solid(1, 1, [255; 4]));
    let r = read_in(d.path(), "x.png", 64);
    assert_eq!(absent(&r), Absent::Unresolved(Unresolved::Missing));
    let r = read_in(d.path(), "folder", 64);
    assert_eq!(absent(&r), Absent::Unresolved(Unresolved::NotAFile));
    // A caller's fallbacks (S15's BGA) are tried in order after the name.
    let chart = ChartDir::new(d.path()).unwrap();
    let r = read(&chart, "x.png", &["jpg", "bmp"], slot(64), &never, &fixed);
    let Binding::Resolved { path, name, .. } = &r.binding else {
        panic!("{:?}", r.binding);
    };
    assert_eq!(name, "x.png");
    assert!(path.ends_with("x.bmp"));
}

/// Opening a FIFO with no writer blocks, so its type must be checked
/// before it is opened.
// Covers: T08
#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_opening_it() {
    use std::sync::mpsc;
    use std::time::Duration;

    let d = dir();
    let status = std::process::Command::new("mkfifo")
        .arg(d.path().join("p.png"))
        .status()
        .expect("this test needs `mkfifo`");
    assert!(status.success());
    let root = d.path().to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(absent(&read_in(&root, "p.png", 64)));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(a) => assert_eq!(a, Absent::Unresolved(Unresolved::NotAFile)),
        Err(e) => panic!("reading a FIFO blocked or failed: {e:?}"),
    }
}

// Covers: T08
#[test]
fn files_over_the_byte_cap_are_refused_before_reading() {
    let d = dir();
    File::create(d.path().join("big.png"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let r = read_in(d.path(), "big.png", 64);
    assert_eq!(absent(&r), Absent::Unresolved(Unresolved::TooLarge));
}

/// A side of exactly the cap is admitted and decoded.
#[test]
fn a_side_of_exactly_the_cap_is_decoded() {
    let d = dir();
    save(d.path(), "line.png", &solid(16_384, 1, [3, 6, 9, 255]));
    let f = fitted(read_in(d.path(), "line.png", 64));
    assert_eq!((f.native(), f.content()), ([16_384, 1], [48, 1]));
    assert_eq!(texel(&f, 0, GUTTER, GUTTER), [3, 6, 9, 255]);
}

#[test]
fn the_side_and_texel_caps_refuse_by_the_header() {
    let d = dir();
    fs::write(d.path().join("wide.png"), png_header(16_385, 1)).unwrap();
    // 2 191 × 5 743 is 12 Mi + 1 texels.
    assert_eq!(2_191 * 5_743, (12 << 20) + 1);
    fs::write(d.path().join("over.png"), png_header(2_191, 5_743)).unwrap();
    assert_eq!(
        absent(&read_in(d.path(), "wide.png", 64)),
        Absent::OverCap(Cap::Side)
    );
    assert_eq!(
        absent(&read_in(d.path(), "over.png", 64)),
        Absent::OverCap(Cap::Texels)
    );
}

#[test]
fn an_image_of_exactly_the_texel_cap_is_decoded_and_fitted() {
    let d = dir();
    // 4 096 × 3 072 is exactly 12 Mi texels.
    save(d.path(), "cap.png", &solid(4_096, 3_072, [10, 20, 30, 255]));
    let f = fitted(read_in(d.path(), "cap.png", 1_024));
    assert_eq!(f.native(), [4_096, 3_072]);
    assert_eq!(f.content(), [1_008, 756]);
    assert_eq!(texel(&f, 0, GUTTER, GUTTER), [10, 20, 30, 255]);
}

#[test]
fn png_jpeg_and_bmp_decode_and_other_formats_are_absent() {
    let d = dir();
    let img = solid(4, 2, [200, 100, 50, 255]);
    for (name, format) in [
        ("a.png", ImageFormat::Png),
        ("a.jpg", ImageFormat::Jpeg),
        ("a.bmp", ImageFormat::Bmp),
    ] {
        fs::write(d.path().join(name), encoded(&img, format)).unwrap();
        let f = fitted(read_in(d.path(), name, 64));
        assert_eq!((f.native(), f.content()), ([4, 2], [4, 2]), "{name}");
    }
    // A GIF's signature is recognised, but no GIF decoder is built in.
    fs::write(
        d.path().join("a.gif"),
        b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
    )
    .unwrap();
    assert_eq!(absent(&read_in(d.path(), "a.gif", 64)), Absent::Format);
    fs::write(d.path().join("a.txt"), b"not an image at all").unwrap();
    assert_eq!(absent(&read_in(d.path(), "a.txt", 64)), Absent::Format);
    // A PNG cut after its header decodes to nothing.
    let mut cut = encoded(&solid(64, 64, [1; 4]), ImageFormat::Png);
    cut.truncate(cut.len() - 20);
    fs::write(d.path().join("cut.png"), cut).unwrap();
    assert_eq!(absent(&read_in(d.path(), "cut.png", 64)), Absent::Decode);
}

#[test]
fn slots_take_gutters_aligned_sides_only() {
    assert!(Slot::new([1_024, 1_024]).is_some());
    assert!(Slot::new([2 * GUTTER + 8, 2 * GUTTER + 8]).is_some());
    assert!(Slot::new([2 * GUTTER, 64]).is_none());
    assert!(Slot::new([1_020, 1_024]).is_none());
    assert!(Slot::new([8_200, 1_024]).is_none());
    assert_eq!(slot(1_024).max_content(), [1_008, 1_008]);
}

/// A picture that fills its slot less the gutters is written at its own
/// size, and its allocation, gutters included, is the whole slot: level 0
/// is uploaded at the slot's origin, its content `GUTTER` texels in.
#[test]
fn a_full_slot_is_one_allocation_with_its_gutters() {
    let d = dir();
    let mut img = solid(1_008, 1_008, [0, 0, 255, 255]);
    img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
    img.put_pixel(1_007, 1_007, Rgba([0, 255, 0, 255]));
    save(d.path(), "full.png", &img);
    let f = fitted(read_in(d.path(), "full.png", 1_024));
    assert_eq!((f.content(), f.padded()), ([1_008, 1_008], [1_024, 1_024]));
    assert_eq!(texel(&f, 0, GUTTER, GUTTER), [255, 0, 0, 255]);
    assert_eq!(texel(&f, 0, 0, 0), [255, 0, 0, 255]);
    assert_eq!(texel(&f, 0, 1_023, 1_023), [0, 255, 0, 255]);
    assert_eq!(
        texel(&f, 0, GUTTER + 1_007, GUTTER + 1_007),
        [0, 255, 0, 255]
    );
    for k in 0..MIP_LEVELS {
        let [w, h] = f.level_size(k).unwrap();
        assert_eq!([w, h], [1_024 >> k, 1_024 >> k]);
        assert_eq!(f.levels()[k].len(), (w * h * 4) as usize);
    }
}

#[test]
fn a_large_picture_is_downscaled_keeping_its_aspect() {
    let d = dir();
    save(
        d.path(),
        "wide.png",
        &solid(1_920, 1_080, [40, 80, 120, 255]),
    );
    let f = fitted(read_in(d.path(), "wide.png", 1_024));
    assert_eq!(f.native(), [1_920, 1_080]);
    assert_eq!(f.content(), [1_008, 567]);
    // 567 rounds up to 568, then a gutter each side.
    assert_eq!(f.padded(), [1_024, 584]);
    assert_eq!(texel(&f, 0, GUTTER + 500, GUTTER + 300), [40, 80, 120, 255]);
}

/// A very thin picture keeps at least one texel across.
#[test]
fn thin_pictures_keep_a_texel_and_pad_to_alignment() {
    let d = dir();
    save(d.path(), "h.png", &solid(4_000, 1, [255; 4]));
    save(d.path(), "v.png", &solid(1, 4_000, [255; 4]));
    let h = fitted(read_in(d.path(), "h.png", 1_024));
    assert_eq!((h.content(), h.padded()), ([1_008, 1], [1_024, 24]));
    let v = fitted(read_in(d.path(), "v.png", 1_024));
    assert_eq!((v.content(), v.padded()), ([1, 1_008], [24, 1_024]));
    for k in 0..MIP_LEVELS {
        assert_eq!(h.level_size(k).unwrap(), [1_024 >> k, 24 >> k]);
    }
}

/// An odd size is padded to the alignment, and every texel outside the
/// content, padding and gutter alike, repeats the nearest content texel,
/// premultiplied.
#[test]
fn odd_sizes_are_padded_and_extruded() {
    let d = dir();
    let mut img = RgbaImage::new(5, 3);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = Rgba([(x * 50) as u8, (y * 100) as u8, 7, 255]);
    }
    img.put_pixel(0, 0, Rgba([255, 0, 0, 128]));
    save(d.path(), "odd.png", &img);
    let f = fitted(read_in(d.path(), "odd.png", 64));
    assert_eq!((f.content(), f.padded()), ([5, 3], [24, 24]));
    // Premultiplied: 255 · 128 / 255 rounds to 128.
    assert_eq!(texel(&f, 0, GUTTER, GUTTER), [128, 0, 0, 128]);
    assert_eq!(texel(&f, 0, 0, 0), [128, 0, 0, 128]);
    let last = [200, 200, 7, 255];
    assert_eq!(texel(&f, 0, GUTTER + 4, GUTTER + 2), last);
    // The alignment padding right of and below the content, and the far
    // gutter corner.
    assert_eq!(texel(&f, 0, GUTTER + 5, GUTTER + 2), last);
    assert_eq!(texel(&f, 0, GUTTER + 4, GUTTER + 4), last);
    assert_eq!(texel(&f, 0, 23, 23), last);
    assert_eq!(texel(&f, 0, GUTTER + 2, 23), [100, 200, 7, 255]);
    // Level 1 is the 2×2 mean of level 0, rounded to nearest.
    let mean = |c: usize| {
        let s: u32 = [(22, 22), (23, 22), (22, 23), (23, 23)]
            .iter()
            .map(|&(x, y)| u32::from(texel(&f, 0, x, y)[c]))
            .sum();
        ((s + 2) / 4) as u8
    };
    assert_eq!(texel(&f, 1, 11, 11), [mean(0), mean(1), mean(2), mean(3)]);
}

/// Filtering premultiplied texels keeps colour off fully transparent
/// texels: every texel of a downscaled white over transparent black stays
/// white premultiplied (colour equal to alpha). Filtering straight colour
/// first would darken the half-covered texels.
#[test]
fn the_area_filter_runs_on_premultiplied_texels() {
    let d = dir();
    let mut img = RgbaImage::new(2_016, 2);
    for (x, _, p) in img.enumerate_pixels_mut() {
        *p = if x % 2 == 0 {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([255, 255, 255, 255])
        };
    }
    save(d.path(), "fringe.png", &img);
    let f = fitted(read_in(d.path(), "fringe.png", 1_024));
    assert_eq!(f.content(), [1_008, 1]);
    for x in 0..1_008 {
        let [r, g, b, a] = texel(&f, 0, GUTTER + x, GUTTER);
        assert_eq!([r, g, b], [a, a, a], "x {x}");
        assert_eq!(a, 128, "x {x}");
    }
}

/// The observation a read returns is the one whose bytes it decoded: a
/// file replaced while it is read is read again, and the new observation
/// returned with the new pixels. Restoring the original bytes, length
/// and time afterwards is still a change against that observation.
#[test]
fn a_read_returns_the_observation_it_decoded() {
    let d = dir();
    let first = encoded(&solid(2, 2, [1, 1, 1, 255]), ImageFormat::Png);
    let second = encoded(&solid(3, 3, [2, 2, 2, 255]), ImageFormat::Png);
    let file = d.path().join("s.png");
    fs::write(&file, &first).unwrap();
    let original = fs::metadata(&file).unwrap().modified().unwrap();
    let chart = ChartDir::new(d.path()).unwrap();
    // Replaced once, at the first checkpoint, after resolution.
    let calls = Cell::new(0);
    let replace_once = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            fs::write(&file, &second).unwrap();
        }
        false
    };
    let r = read(&chart, "s.png", &[], slot(64), &replace_once, &fixed);
    let Binding::Resolved { key, .. } = &r.binding else {
        panic!("{:?}", r.binding);
    };
    assert_eq!(key.len, second.len() as u64);
    let f = fitted(r.clone());
    assert_eq!(f.native(), [3, 3]);
    assert_eq!(texel(&f, 0, GUTTER, GUTTER), [2, 2, 2, 255]);
    // The original bytes and time back: not the observation just read.
    fs::write(&file, &first).unwrap();
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(original)
        .unwrap();
    let now = resolve(&chart, "s.png", &[], &fixed);
    assert!(!now.same_as(&r.binding));
    // And an unchanged file is the same observation.
    assert!(now.same_as(&resolve(&chart, "s.png", &[], &fixed)));
}

/// A file changed in place after the handle's first observation (here
/// while its identity is asked, after its metadata was read) is not the
/// file resolved, whatever its new bytes make of the read: a valid image,
/// a header over a cap, or bytes that are no image. Each outcome carries
/// the observation of the bytes it came from, so restoring the original
/// bytes, time and identity is not taken for the file just read.
#[test]
fn an_outcome_carries_the_observation_of_its_own_bytes() {
    let original = encoded(&solid(2, 2, [1, 1, 1, 255]), ImageFormat::Png);
    // What each replacement is, its bytes, and the native size or absence
    // a read of it gives.
    type Case = (&'static str, Vec<u8>, Result<[u32; 2], Absent>);
    let replacements: [Case; 3] = [
        (
            "valid",
            encoded(&solid(3, 3, [2, 2, 2, 255]), ImageFormat::Png),
            Ok([3, 3]),
        ),
        (
            "over a cap",
            png_header(2_191, 5_743),
            Err(Absent::OverCap(Cap::Texels)),
        ),
        (
            "no image",
            b"not an image at all, longer".to_vec(),
            Err(Absent::Format),
        ),
    ];
    for (what, replacement, expected) in replacements {
        let d = dir();
        let file = d.path().join("s.png");
        fs::write(&file, &original).unwrap();
        let time = fs::metadata(&file).unwrap().modified().unwrap();
        let chart = ChartDir::new(d.path()).unwrap();
        // The second identity asked is the data handle's first
        // observation, after resolution's.
        let asked = Cell::new(0);
        let change_in_place = |_: &File| {
            asked.set(asked.get() + 1);
            if asked.get() == 2 {
                fs::write(&file, &replacement).unwrap();
            }
            Some(FileIdentity(1))
        };
        let r = read(&chart, "s.png", &[], slot(64), &never, &change_in_place);
        let Binding::Resolved { key, .. } = &r.binding else {
            panic!("{what}: {:?}", r.binding);
        };
        assert_eq!(key.len, replacement.len() as u64, "{what}");
        assert_eq!(
            r.image.as_ref().map(Fitted::native).map_err(Clone::clone),
            expected,
            "{what}"
        );
        // The original bytes, time and identity back.
        fs::write(&file, &original).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let restored = resolve(&chart, "s.png", &[], &fixed);
        assert!(!restored.same_as(&r.binding), "{what}");
    }
}

#[test]
fn a_file_changing_on_every_read_is_absent() {
    let d = dir();
    let file = d.path().join("s.png");
    let bytes = |n: u32| encoded(&solid(n, n, [n as u8; 4]), ImageFormat::Png);
    fs::write(&file, bytes(1)).unwrap();
    let chart = ChartDir::new(d.path()).unwrap();
    let n = Cell::new(1);
    // Replaced at the first checkpoint of each attempt: before the open.
    let replace = || {
        n.set(n.get() + 1);
        fs::write(&file, bytes(n.get())).unwrap();
        false
    };
    let r = read(&chart, "s.png", &[], slot(64), &replace, &fixed);
    assert_eq!(absent(&r), Absent::Changed);
}

/// Two observations match only when both identities are known: without
/// one, a file is never taken as unchanged.
#[test]
fn an_unavailable_identity_never_matches() {
    let d = dir();
    save(d.path(), "s.png", &solid(1, 1, [255; 4]));
    let chart = ChartDir::new(d.path()).unwrap();
    let unknown = |_: &File| None;
    let a = resolve(&chart, "s.png", &[], &unknown);
    let b = resolve(&chart, "s.png", &[], &unknown);
    assert!(matches!(a, Binding::Resolved { .. }));
    assert!(!a.same_as(&b));
    let other = |_: &File| Some(FileIdentity(2));
    let c = resolve(&chart, "s.png", &[], &fixed);
    assert!(!c.same_as(&resolve(&chart, "s.png", &[], &other)));
    assert!(c.same_as(&resolve(&chart, "s.png", &[], &fixed)));
}

/// Resolving again observes absence too: a file that appears, and a link
/// moved to another target, are changes.
#[test]
fn a_missing_file_appearing_and_a_retargeted_link_are_changes() {
    let d = dir();
    let chart = ChartDir::new(d.path()).unwrap();
    let missing = resolve(&chart, "s.png", &[], &fixed);
    assert!(missing.same_as(&resolve(&chart, "s.png", &[], &fixed)));
    save(d.path(), "s.png", &solid(1, 1, [255; 4]));
    assert!(!missing.same_as(&resolve(&chart, "s.png", &[], &fixed)));

    for sub in ["one", "two"] {
        fs::create_dir_all(d.path().join(sub)).unwrap();
        save(&d.path().join(sub), "x.png", &solid(1, 1, [255; 4]));
    }
    let link = d.path().join("img");
    link_dir(&link, &d.path().join("one"));
    let before = resolve(&chart, "img/x.png", &[], &fixed);
    remove_link(&link);
    link_dir(&link, &d.path().join("two"));
    let after = resolve(&chart, "img/x.png", &[], &fixed);
    assert!(matches!(after, Binding::Resolved { .. }));
    assert!(!before.same_as(&after));
}

fn remove_link(link: &Path) {
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
}

/// Every checkpoint cancels: before the open, each piece of the read,
/// after the decode, after the fit and before each mip level.
#[test]
fn each_checkpoint_cancels_the_read() {
    let d = dir();
    save(d.path(), "s.png", &solid(300, 200, [5; 4]));
    let chart = ChartDir::new(d.path()).unwrap();
    let count = Cell::new(0);
    let counting = || {
        count.set(count.get() + 1);
        false
    };
    let whole = read(&chart, "s.png", &[], slot(128), &counting, &fixed);
    assert!(whole.image.is_ok());
    let total = count.get();
    // The open, one read piece and the read's end (the file is under one
    // piece), the decode, the fit and the three mip levels.
    assert_eq!(total, 8, "checkpoints");
    for at in 1..=total {
        let n = Cell::new(0);
        let cancel = || {
            n.set(n.get() + 1);
            n.get() == at
        };
        let r = read(&chart, "s.png", &[], slot(128), &cancel, &fixed);
        assert_eq!(absent(&r), Absent::Cancelled, "checkpoint {at} of {total}");
    }
}

/// A source is read once, level 0 only, fitted to the largest
/// chart slot; every slot's fit derives from it as a read for that slot
/// would place it (the same content, gutters and levels), up to the
/// native size in a larger slot and an area downscale in a smaller one,
/// never upscaled; a fit derives as itself only for slots that would
/// derive the same content.
#[test]
fn a_source_derives_each_slots_fit() {
    use skin_model::chart_image::{derive, read_source, source_slot};
    let d = dir();
    save(d.path(), "s.png", &solid(1_600, 1_200, [10, 20, 30, 255]));
    let root = ChartDir::new(d.path()).unwrap();
    let source = fitted(read_source(&root, "s.png", &[], &never, &fixed));
    assert_eq!(source.levels().len(), 1);
    assert_eq!(source.content(), [1_600, 1_200]);
    assert!(source_slot().unwrap().holds(&source));
    let large = derive(&source, slot(2_048), &never).unwrap();
    assert_eq!(large.content(), [1_600, 1_200], "its native size");
    assert_eq!(large.levels().len(), MIP_LEVELS);
    assert_eq!(large, fitted(read_in(d.path(), "s.png", 2_048)));
    let small = derive(&source, slot(1_152), &never).unwrap();
    assert_eq!(small.content(), [1_136, 852]);
    assert_eq!(texel(&small, 0, GUTTER + 5, GUTTER + 5), [10, 20, 30, 255]);
    assert!(slot(2_048).derives_as(&large));
    assert!(slot(4_096).derives_as(&large), "the native size again");
    assert!(!slot(1_152).derives_as(&large));
    assert!(slot(1_152).derives_as(&small));
    assert!(!slot(2_048).derives_as(&small), "a larger slot gets more");
    assert_eq!(
        derive(&source, slot(1_152), &|| true),
        Err(Absent::Cancelled)
    );
}
