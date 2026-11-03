//! The banded path against the whole-file decode, and its refusals.
//!
//! A file under [`MAX_IMAGE_TEXELS`] is forced through the banded path by
//! setting its admitted mode here, so the two paths decode the same bytes.

use std::cell::Cell as Counter;
use std::fs;
use std::path::Path;

use super::decode::{Mode, PngHeader, SITE, Site};
use super::*;
use crate::atlas::{AtlasError, AtlasLimits, MIP_LEVELS, Variant};
use crate::quad::Filter;

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 512,
    max_layers: 8,
    reserved_layers: 1,
};

fn never() -> bool {
    false
}

/// The CRC of a PNG chunk's type and data (ISO 3309, bit by bit).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A zlib stream of `data` in stored blocks, with its Adler-32.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let n = u16::try_from(block.len()).unwrap();
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// A PNG to encode: its size, colour type and depth, samples as `png`
/// takes them (big-endian at 16 bits), and chunks before and after the
/// image data.
struct Png {
    w: u32,
    h: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    data: Vec<u8>,
    before: Vec<([u8; 4], Vec<u8>)>,
    after: Vec<([u8; 4], Vec<u8>)>,
}

impl Png {
    fn new(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth) -> Self {
        let bits = color.samples() * depth as usize;
        let line = (usize::try_from(w).unwrap() * bits).div_ceil(8);
        let mut seed = 0x2545_f491_u32 ^ w ^ (h << 8) ^ (u32::from(color as u8) << 16);
        seed ^= u32::try_from(bits).unwrap();
        let data = (0..line * usize::try_from(h).unwrap())
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        Self {
            w,
            h,
            color,
            depth,
            data,
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    fn before(mut self, kind: &[u8; 4], data: &[u8]) -> Self {
        self.before.push((*kind, data.to_vec()));
        self
    }

    fn after(mut self, kind: &[u8; 4], data: &[u8]) -> Self {
        self.after.push((*kind, data.to_vec()));
        self
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.w, self.h);
        enc.set_color(self.color);
        enc.set_depth(self.depth);
        for (kind, data) in &self.before {
            match kind {
                b"PLTE" => enc.set_palette(data.clone()),
                b"tRNS" => enc.set_trns(data.clone()),
                _ => {}
            }
        }
        let mut w = enc.write_header().unwrap();
        for (kind, data) in &self.before {
            if kind != b"PLTE" && kind != b"tRNS" {
                w.write_chunk(png::chunk::ChunkType(*kind), data).unwrap();
            }
        }
        w.write_image_data(&self.data).unwrap();
        for (kind, data) in &self.after {
            w.write_chunk(png::chunk::ChunkType(*kind), data).unwrap();
        }
        w.finish().unwrap();
        out
    }
}

/// Each chunk of `png`: its type and the offset of its length field.
fn chunks(png: &[u8]) -> Vec<([u8; 4], usize)> {
    let mut out = Vec::new();
    let mut at = 8;
    while at + 8 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        out.push((png[at + 4..at + 8].try_into().unwrap(), at));
        at += 12 + len;
    }
    out
}

/// The offset of the length field of `png`'s last chunk of `kind`.
fn last_chunk(png: &[u8], kind: &[u8; 4]) -> usize {
    chunks(png)
        .into_iter()
        .filter(|(k, _)| k == kind)
        .map(|(_, at)| at)
        .next_back()
        .unwrap()
}

/// Recomputes the CRC of the chunk at `at`.
fn fix_crc(png: &mut [u8], at: usize) {
    let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
    let crc = crc32(&png[at + 4..at + 8 + len]);
    png[at + 8 + len..at + 12 + len].copy_from_slice(&crc.to_be_bytes());
}

fn header_of(bytes: &[u8]) -> PngHeader {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    let info = d.read_header_info().unwrap();
    PngHeader {
        color: info.color_type,
        depth: info.bit_depth,
        interlaced: info.interlaced,
    }
}

fn image(rect: [u32; 4], grid: [u16; 2]) -> ImageDef {
    ImageDef {
        file: 0,
        rect: Some(rect),
        grid,
        cycle: None,
        filter: Filter::Linear,
    }
}

/// The cells the equivalence tests cut from a file of `w`×`h`: the whole
/// file, sizes off the alignment and the band height, cells overlapping
/// each other, and cells at the file's right and bottom edges; those that
/// fit.
fn cells(w: u32, h: u32) -> Vec<ImageDef> {
    [
        [0, 0, w, h],
        [3, 5, 17, 22],
        [10, 10, 30, 30],
        [20, 15, 25, 35],
        [w.saturating_sub(11), h.saturating_sub(13), 11, 13],
        [0, h - 1, w, 1],
        [w - 1, 0, 1, h],
    ]
    .into_iter()
    .map(|r| image(r, [1, 1]))
    .chain([image([1, 2, 18, 33], [3, 3])])
    .filter(|d| {
        d.rect
            .is_some_and(|[x, y, cw, ch]| x + cw <= w && y + ch <= h)
    })
    .collect()
}

/// A sheet of `bytes` as `a.png` in `root` with `images`.
fn sheet(root: &Path, bytes: &[u8], images: Vec<ImageDef>) -> Sheet {
    fs::write(root.join("a.png"), bytes).unwrap();
    let files = vec![FileDef {
        path: "a.png".into(),
        hi: None,
    }];
    Sheet::new(root, files, images).unwrap()
}

/// `sheet` with file 0 decoded in bands, as admitted for `bytes`.
fn banded(mut sheet: Sheet, bytes: &[u8]) -> Sheet {
    sheet.files[0].lo.mode = Mode::Banded(header_of(bytes));
    sheet
}

/// Every page's every level.
fn levels(atlas: &crate::Atlas) -> Vec<Vec<u8>> {
    atlas
        .pages()
        .iter()
        .flat_map(|p| (0..MIP_LEVELS).map(|k| p.level(k).unwrap().to_vec()))
        .collect()
}

/// Both paths over `png`: their atlases' levels.
fn both(png: &Png) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png.encode();
    let whole = sheet(dir.path(), &bytes, cells(png.w, png.h));
    assert!(matches!(whole.files[0].lo.mode, Mode::Full(Some(_))));
    let full = levels(&whole.atlas(Variant::Lo, LIMITS, &never).unwrap());
    let bands = banded(whole, &bytes);
    let band = levels(&bands.atlas(Variant::Lo, LIMITS, &never).unwrap());
    (full, band)
}

#[test]
fn bands_match_the_whole_decode_in_every_colour_type() {
    use png::BitDepth::{Eight, One, Sixteen, Two};
    use png::ColorType::{Grayscale, GrayscaleAlpha, Indexed, Rgb, Rgba};
    let palette: Vec<u8> = (0..=255_u8).flat_map(|i| [i, 255 - i, i / 2]).collect();
    let alphas: Vec<u8> = (0..=255_u8).map(|i| i.wrapping_mul(37)).collect();
    let srgb: [u8; 1] = [0];
    let gamma = 45_455_u32.to_be_bytes();
    let mut iccp = b"profile\0\0".to_vec();
    iccp.extend_from_slice(&zlib_stored(&[7; 300]));
    // 61×53: no side a multiple of the alignment or the band.
    let (w, h) = (61, 53);
    let cases = [
        Png::new(w, h, Rgba, Eight),
        Png::new(w, h, Rgba, Sixteen),
        Png::new(w, h, Rgb, Eight),
        Png::new(w, h, Rgb, Sixteen).before(b"tRNS", &[0, 9, 0, 9, 0, 9]),
        Png::new(w, h, Rgb, Eight).before(b"tRNS", &[0, 0x40, 0, 0x80, 0, 0xC0]),
        Png::new(w, h, Grayscale, Eight),
        Png::new(w, h, Grayscale, Sixteen),
        Png::new(w, h, Grayscale, One).before(b"tRNS", &[0, 1]),
        Png::new(w, h, Grayscale, Two),
        Png::new(w, h, GrayscaleAlpha, Eight),
        Png::new(w, h, GrayscaleAlpha, Sixteen),
        Png::new(w, h, Indexed, Eight).before(b"PLTE", &palette),
        Png::new(w, h, Indexed, Eight)
            .before(b"PLTE", &palette)
            .before(b"tRNS", &alphas),
        Png::new(w, h, Indexed, Two)
            .before(b"PLTE", &palette[..12])
            .before(b"tRNS", &[0, 128]),
        // Tagged with a gamma, a colour space and a profile, none applied.
        Png::new(w, h, Rgba, Eight)
            .before(b"gAMA", &gamma)
            .before(b"sRGB", &srgb),
        Png::new(w, h, Rgba, Sixteen).before(b"iCCP", &iccp),
    ];
    for (i, png) in cases.iter().enumerate() {
        let (full, band) = both(png);
        assert!(full.iter().any(|l| l.iter().any(|&b| b != 0)), "case {i}");
        assert_eq!(full, band, "case {i}: {:?} {:?}", png.color, png.depth);
    }
}

#[test]
fn sixteen_bit_samples_round_as_the_whole_decode_does() {
    // (v + 128) / 257 changes between 128 and 129, and between 385 and
    // 386; opaque, so premultiplication keeps each value.
    let (w, h) = (8, 40);
    let mut png = Png::new(w, h, png::ColorType::Rgba, png::BitDepth::Sixteen);
    let values = [0_u16, 127, 128, 129, 130, 385, 386, 65_407, 65_408, 65_535];
    for (i, px) in png.data.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let v = values[i % values.len()];
        px[..6].copy_from_slice(&[v.to_be_bytes(); 3].concat());
        px[6..].copy_from_slice(&u16::MAX.to_be_bytes());
    }
    let (full, band) = both(&png);
    assert_eq!(full, band);
    let level0 = &band[0];
    for (want, v) in [
        (0_u32, 128_u16),
        (1, 129),
        (1, 385),
        (2, 386),
        (255, 65_408),
    ] {
        assert_eq!((u32::from(v) + 128) / 257, want);
        assert!(level0.contains(&u8::try_from(want).unwrap()), "{v}");
    }
}

#[test]
fn transparent_borders_extend_premultiplied_through_the_padding() {
    // A cell whose edge texels are transparent and coloured: the gutter and
    // the alignment padding hold their premultiplied (zero) colour, in both
    // paths, at every level.
    let (w, h) = (21, 19);
    let mut png = Png::new(w, h, png::ColorType::Rgba, png::BitDepth::Eight);
    for (i, px) in (0_u32..).zip(png.data.as_chunks_mut::<4>().0) {
        let (x, y) = (i % w, i / w);
        let edge = x == 0 || y == 0 || x == w - 1 || y == h - 1;
        px.copy_from_slice(&if edge {
            [200, 50, 10, 0]
        } else {
            [9, 99, 199, 255]
        });
    }
    let (full, band) = both(&png);
    assert_eq!(full, band);
}

/// The banded atlas of `bytes`, written over a file admitted as
/// `admitted`.
fn banded_build(admitted: &[u8], bytes: &[u8]) -> Result<crate::Atlas, AtlasError> {
    let dir = tempfile::tempdir().unwrap();
    let s = banded(sheet(dir.path(), admitted, cells(16, 20)), admitted);
    fs::write(dir.path().join("a.png"), bytes).unwrap();
    s.atlas(Variant::Lo, LIMITS, &never)
}

/// The error [`banded_build`] meets.
fn banded_error(admitted: &[u8], bytes: &[u8]) -> AtlasError {
    banded_build(admitted, bytes).unwrap_err()
}

fn small() -> Png {
    Png::new(16, 20, png::ColorType::Rgba, png::BitDepth::Eight)
}

#[test]
fn damaged_files_are_refused_in_bands() {
    let good = small().after(b"tEXt", b"k\0v").encode();
    let png_error = |e: &AtlasError| matches!(e, AtlasError::Image(ImageError::Png { .. }));
    // Cut short in its image data.
    let idat = last_chunk(&good, b"IDAT");
    let e = banded_error(&good, &good[..idat + 20]);
    assert!(png_error(&e), "{e:?}");
    // A critical chunk's CRC.
    let mut bad = good.clone();
    bad[idat + 9] ^= 1;
    let e = banded_error(&good, &bad);
    assert!(png_error(&e), "{e:?}");
    // An ancillary chunk's CRC, after the image data: the whole decode
    // skips it, the banded path refuses.
    let mut bad = good.clone();
    let text = last_chunk(&good, b"tEXt");
    bad[text + 8] ^= 1;
    let e = banded_error(&good, &bad);
    assert!(png_error(&e), "{e:?}");
    let dir = tempfile::tempdir().unwrap();
    let s = sheet(dir.path(), &bad, cells(16, 20));
    s.atlas(Variant::Lo, LIMITS, &never).unwrap();
    // The Adler-32 after the last row's data, its chunk's CRC made good.
    let mut bad = good.clone();
    let len = u32::from_be_bytes(bad[idat..idat + 4].try_into().unwrap()) as usize;
    bad[idat + 8 + len - 1] ^= 1;
    fix_crc(&mut bad, idat);
    let e = banded_error(&good, &bad);
    assert!(png_error(&e), "{e:?}");
    // The undamaged file decodes.
    let dir = tempfile::tempdir().unwrap();
    banded(sheet(dir.path(), &good, cells(16, 20)), &good)
        .atlas(Variant::Lo, LIMITS, &never)
        .unwrap();
}

#[test]
fn metadata_past_the_decoders_budget_is_refused_and_text_is_skipped() {
    // An eXIf of 1.5 MiB after the image data: kept by the decoder, past
    // its 1 MiB.
    let exif = small().after(b"eXIf", &vec![0; 3 << 19]).encode();
    let e = banded_error(&exif, &exif);
    assert!(
        matches!(e, AtlasError::Image(ImageError::DecoderLimit { .. })),
        "{e:?}"
    );
    // Text and a profile of 2 MiB each, never inflated: skipped within the
    // same budget, their CRCs still checked. The file admitted had none,
    // so its header lay within the first mebibyte.
    let big = vec![b'x'; 2 << 20];
    let mut iccp = b"p\0\0".to_vec();
    iccp.extend_from_slice(&zlib_stored(&big));
    let mut ztxt = b"k\0\0".to_vec();
    ztxt.extend_from_slice(&zlib_stored(&big));
    let mut itxt = b"k\0\x01\0\0\0".to_vec();
    itxt.extend_from_slice(&zlib_stored(&big));
    let skipped = small()
        .before(b"iCCP", &iccp)
        .after(b"zTXt", &ztxt)
        .after(b"iTXt", &itxt)
        .encode();
    let plain = small().encode();
    banded_build(&plain, &skipped).unwrap();
    for kind in [b"iCCP", b"zTXt", b"iTXt"] {
        let mut bad = skipped.clone();
        let at = last_chunk(&skipped, kind);
        bad[at + 9] ^= 1;
        let e = banded_error(&plain, &bad);
        assert!(
            matches!(e, AtlasError::Image(ImageError::Png { .. })),
            "{kind:?}: {e:?}"
        );
    }
}

#[test]
fn a_header_changed_since_admission_is_refused() {
    let good = small().encode();
    let changed = |e: &AtlasError| {
        matches!(
            e,
            AtlasError::Image(ImageError::HeaderChanged { at }) if *at == FileAt { file: 0, hi: false }
        )
    };
    // Another colour type, bit depth or interlacing at the same size.
    for other in [
        Png::new(16, 20, png::ColorType::Rgb, png::BitDepth::Eight).encode(),
        Png::new(16, 20, png::ColorType::Rgba, png::BitDepth::Sixteen).encode(),
        {
            let mut i = good.clone();
            i[28] = 1;
            fix_crc(&mut i, 8);
            i
        },
    ] {
        let e = banded_error(&good, &other);
        assert!(changed(&e), "{e:?}");
    }
    // Animated now.
    let mut anim = good.clone();
    let mut actl = 8_u32.to_be_bytes().to_vec();
    actl.extend_from_slice(b"acTL\0\0\0\x01\0\0\0\0");
    actl.extend_from_slice(&crc32(&actl[4..]).to_be_bytes());
    anim.splice(33..33, actl);
    let e = banded_error(&good, &anim);
    assert!(changed(&e), "{e:?}");
    // Another format.
    let mut jpeg = Vec::new();
    ::image::RgbImage::new(16, 20)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            ::image::ImageFormat::Jpeg,
        )
        .unwrap();
    let e = banded_error(&good, &jpeg);
    assert!(changed(&e), "{e:?}");
    // Other dimensions.
    let e = banded_error(
        &good,
        &Png::new(16, 21, png::ColorType::Rgba, png::BitDepth::Eight).encode(),
    );
    assert!(
        matches!(e, AtlasError::Image(ImageError::DimensionsChanged { .. })),
        "{e:?}"
    );
}

/// The banded atlas of `bytes`, cancelled at the `n`th ask at `site`, and
/// the asks at it.
fn cancel_at(bytes: &[u8], site: Site, n: u32) -> (Result<crate::Atlas, AtlasError>, u32) {
    let dir = tempfile::tempdir().unwrap();
    let s = banded(sheet(dir.path(), bytes, cells(16, 40)), bytes);
    let asked = Counter::new(0);
    let cancel = || {
        if SITE.with(Counter::get) != Some(site) {
            return false;
        }
        asked.set(asked.get() + 1);
        asked.get() >= n
    };
    let r = s.atlas(Variant::Lo, LIMITS, &cancel);
    (r, asked.get())
}

#[test]
fn a_cancel_stops_the_build_at_each_site() {
    let bytes = Png::new(16, 40, png::ColorType::Rgba, png::BitDepth::Eight)
        .after(b"tEXt", b"k\0v")
        .encode();
    // Each site is asked; a cancel there stops the build as a
    // cancellation, not a file's failure.
    for (site, n) in [
        (Site::Read, 1),
        (Site::Band, 2),
        (Site::Trailing, 1),
        (Site::Mip, 2),
    ] {
        let (r, asked) = cancel_at(&bytes, site, n);
        assert!(matches!(r, Err(AtlasError::Cancelled)), "{site:?}: {r:?}");
        assert_eq!(asked, n, "{site:?}");
    }
    // Asked but never cancelled: three bands of 16 rows, three mip levels
    // a page.
    let (r, asked) = cancel_at(&bytes, Site::Band, u32::MAX);
    r.unwrap();
    assert_eq!(asked, 3);
}

#[test]
fn only_bands_something_draws_are_converted() {
    // Bands of 16 rows, the last one short; a band nothing draws is
    // decoded but never handed over.
    #[derive(Default)]
    struct Log {
        asked: Vec<(u32, u32)>,
        put: Vec<(u32, u32, u32)>,
    }
    impl decode::Bands for Log {
        fn wants(&mut self, top: u32, bottom: u32) -> bool {
            self.asked.push((top, bottom));
            bottom > 40
        }
        fn put(&mut self, top: u32, band: &::image::RgbaImage) {
            self.put.push((top, band.width(), band.height()));
        }
    }
    let bytes = Png::new(7, 50, png::ColorType::Rgb, png::BitDepth::Eight).encode();
    let header = header_of(&bytes);
    let mut log = Log::default();
    decode::banded(bytes, [7, 50], header, &never, &mut log).unwrap();
    assert_eq!(log.asked, [(0, 16), (16, 32), (32, 48), (48, 50)]);
    assert_eq!(log.put, [(32, 7, 16), (48, 7, 2)]);
}

#[test]
fn a_whole_png_whose_header_changed_is_refused() {
    // Admitted as a 61×53 RGBA8 PNG decoded whole, then replaced at the
    // same size by a 16-bit RGB one, and by an interlaced one.
    let good = Png::new(61, 53, png::ColorType::Rgba, png::BitDepth::Eight).encode();
    let mut adam7 = good.clone();
    adam7[28] = 1;
    fix_crc(&mut adam7, 8);
    for other in [
        Png::new(61, 53, png::ColorType::Rgb, png::BitDepth::Sixteen).encode(),
        adam7,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let s = sheet(dir.path(), &good, cells(61, 53));
        assert!(matches!(s.files[0].lo.mode, Mode::Full(Some(_))));
        fs::write(dir.path().join("a.png"), &other).unwrap();
        let e = s.atlas(Variant::Lo, LIMITS, &never).unwrap_err();
        assert!(
            matches!(e, AtlasError::Image(ImageError::HeaderChanged { .. })),
            "{e:?}"
        );
    }
}

#[test]
fn a_cancel_after_the_last_band_is_seen_in_the_rest_of_the_image_data() {
    // A 4 097 × 3 073 PNG, decoded in bands, whose image data ends with an
    // extra chunk whose CRC is wrong: drained after the last row, before
    // the chunks after the image data. A cancel from the last band on
    // stops the build there as a cancellation, never as that chunk's
    // failure.
    let (w, h) = (4_097, 3_073);
    let mut png = Png::new(w, h, png::ColorType::Grayscale, png::BitDepth::One);
    png.data.fill(0);
    let mut bytes = png.encode();
    let iend = last_chunk(&bytes, b"IEND");
    let mut extra = 4_u32.to_be_bytes().to_vec();
    extra.extend_from_slice(b"IDAT\x01\x02\x03\x04\0\0\0\0");
    bytes.splice(iend..iend, extra);
    let dir = tempfile::tempdir().unwrap();
    let corners = vec![
        image([0, 0, 16, 16], [1, 1]),
        image([w - 16, h - 16, 16, 16], [1, 1]),
    ];
    let s = sheet(dir.path(), &bytes, corners);
    assert!(matches!(s.files[0].lo.mode, Mode::Banded(_)));
    let cancel = || SITE.with(Counter::get) == Some(Site::Trailing);
    let e = s.atlas(Variant::Lo, LIMITS, &cancel).unwrap_err();
    assert!(matches!(e, AtlasError::Cancelled), "{e:?}");
    // Not cancelled, the extra chunk's CRC refuses the file.
    let e = s.atlas(Variant::Lo, LIMITS, &never).unwrap_err();
    assert!(
        matches!(e, AtlasError::Image(ImageError::Png { .. })),
        "{e:?}"
    );
}
