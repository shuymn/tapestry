//! A chart's BGA pictures before play (design.md "Rendering and Skins",
//! BGA): each name resolved by the source's format priority inside the
//! chart's folder (T08), each picture read through the production reader
//! (every BMP variant a real library holds, the format from the content,
//! GIF absent), a small picture centred at the top of its 256 canvas, and
//! the packing ladder, which always ends within its budget.

mod image_support;

use std::fs::{self, File};
use std::path::Path;

use image::{ImageFormat, RgbaImage};
use image_support::{dir, save, solid};
use skin_model::bga::{
    self, BgaBinding, CELL_GUTTER, CellPixels, GRID_SIDE, PAGE_BUDGET, PAGE_SIDE, Step, pack,
    probe, read_cell, resolve, stored_size,
};
use skin_model::chart_image::{Absent, ChartDir, FileIdentity, Unresolved};

fn fixed(_: &File) -> Option<FileIdentity> {
    Some(FileIdentity(1))
}

fn never() -> bool {
    false
}

/// The file `name` resolves to in `root`, by its file name.
fn resolved(root: &Path, name: &str) -> Result<(String, bool), Unresolved> {
    let dir = ChartDir::new(root).unwrap();
    match resolve(&dir, name, &fixed) {
        BgaBinding::Resolved { path, video, .. } => Ok((
            path.file_name().unwrap().to_str().unwrap().to_owned(),
            video,
        )),
        BgaBinding::Unresolved(why) => Err(why),
    }
}

fn touch(root: &Path, name: &str) {
    fs::write(root.join(name), b"x").unwrap();
}

// Covers: T08
#[test]
fn image_resolves_through_the_source_priority_order() {
    let d = dir();
    let root = d.path();
    touch(root, "scene.bmp");
    touch(root, "scene.jpg");
    // An existing picture name is searched for again by priority.
    assert_eq!(resolved(root, "scene.bmp"), Ok(("scene.jpg".into(), false)));
    assert_eq!(resolved(root, "scene.jpg"), Ok(("scene.jpg".into(), false)));
    // A missing name: videos first, then pictures.
    touch(root, "clip.png");
    touch(root, "clip.wmv");
    assert_eq!(resolved(root, "clip.mpg"), Ok(("clip.wmv".into(), true)));
    assert_eq!(resolved(root, "clip"), Ok(("clip.wmv".into(), true)));
    touch(root, "still.png");
    assert_eq!(resolved(root, "still.bmp"), Ok(("still.png".into(), false)));
    // An existing name of another extension is used as written.
    touch(root, "odd.dat");
    touch(root, "odd.png");
    assert_eq!(resolved(root, "odd.dat"), Ok(("odd.dat".into(), false)));
    // An existing video name is searched for in the video list.
    touch(root, "mv.avi");
    touch(root, "mv.mp4");
    assert_eq!(resolved(root, "mv.avi"), Ok(("mv.mp4".into(), true)));
    assert_eq!(resolved(root, "none.png"), Err(Unresolved::Missing));
    // Names that leave the folder read nothing (T08).
    let outer = dir();
    touch(outer.path(), "secret.png");
    let inner = outer.path().join("chart");
    fs::create_dir(&inner).unwrap();
    assert_eq!(resolved(&inner, "../secret.png"), Err(Unresolved::Rejected));
    let absolute = outer.path().join("secret.png");
    assert_eq!(
        resolved(&inner, absolute.to_str().unwrap()),
        Err(Unresolved::Rejected)
    );
}

/// The cell of `name` in `root` at its full-quality content.
fn cell(root: &Path, name: &str) -> Result<CellPixels, Absent> {
    let dir = ChartDir::new(root).unwrap();
    let BgaBinding::Resolved { path, key, .. } = resolve(&dir, name, &fixed) else {
        panic!("{name} does not resolve");
    };
    let native = probe(&path, &key, &fixed)?;
    read_cell(
        &path,
        &key,
        native,
        stored_size(native),
        &never,
        &fixed,
        None,
    )
}

/// The cell's texel at content `(x, y)`.
fn at(c: &CellPixels, x: u32, y: u32) -> [u8; 4] {
    let i = (((y + CELL_GUTTER) * c.padded[0] + x + CELL_GUTTER) * 4) as usize;
    c.texels[i..i + 4].try_into().unwrap()
}

/// A small picture with an alpha channel sits at the top centre of a
/// transparent canvas, a PNG's transparency chunk and a BMP's alpha mask
/// included; a larger one keeps its own size.
// Source: SF-017
#[test]
fn small_picture_with_alpha_centres_on_a_transparent_canvas() {
    let d = dir();
    save(d.path(), "s.png", &solid(100, 50, [255, 0, 0, 255]));
    let c = cell(d.path(), "s.png").unwrap();
    assert_eq!(c.native, [100, 50]);
    assert_eq!(c.padded, [258, 258]);
    // ⌊(256 − 100) / 2⌋ = 78: columns 78 to 177, rows 0 to 49.
    assert_eq!(at(&c, 78, 0), [255, 0, 0, 255]);
    assert_eq!(at(&c, 177, 49), [255, 0, 0, 255]);
    for (x, y) in [(77, 0), (178, 0), (78, 50), (0, 255), (255, 255)] {
        assert_eq!(at(&c, x, y), [0; 4], "({x}, {y}) is transparent");
    }
    // A picture past 256 keeps its own size.
    save(d.path(), "l.png", &solid(300, 20, [0, 255, 0, 255]));
    let c = cell(d.path(), "l.png").unwrap();
    assert_eq!(c.padded, [302, 22]);
    assert_eq!(at(&c, 0, 0), [0, 255, 0, 255]);
    // The gutter repeats the edge.
    assert_eq!(&c.texels[..4], &[0, 255, 0, 255]);
    // An RGB PNG with a transparency chunk, and a BMP with an alpha mask.
    let mut png = Vec::new();
    {
        let mut e = ::png::Encoder::new(&mut png, 4, 2);
        e.set_color(::png::ColorType::Rgb);
        e.set_depth(::png::BitDepth::Eight);
        e.set_trns(vec![0, 0, 255, 0, 0, 255]);
        let mut w = e.write_header().unwrap();
        w.write_image_data(&[255_u8, 0, 0].repeat(8)).unwrap();
    }
    fs::write(d.path().join("trns.png"), &png).unwrap();
    fs::write(d.path().join("mask.bmp"), bmp(32, 3, 108, &[], &argb(255))).unwrap();
    for name in ["trns.png", "mask.bmp"] {
        let c = cell(d.path(), name).unwrap();
        assert_eq!(at(&c, 126, 0)[3], 255, "{name}: the picture");
        assert_eq!(at(&c, 0, 255), [0; 4], "{name}: the margin is transparent");
    }
}

/// 32-bit BMP rows of `PIXELS` with `a` in every texel's fourth byte.
fn argb(a: u8) -> Vec<u8> {
    rows(|r| {
        r.iter()
            .flat_map(|&i| {
                let [cr, cg, cb] = rgb(i);
                [cb, cg, cr, a]
            })
            .collect()
    })
}

/// A small picture with no alpha channel stays opaque, its margin included:
/// its canvas is black (`play/bga/BGImageProcessor.java:50` at `0.8.9`
/// shows it). An RGB PNG, a JPEG, a paletted BMP, a 32-bit BMP without an
/// alpha mask (its fourth byte reserved, not read), and one with an alpha
/// mask but alpha 0 everywhere, which has no alpha: drawn opaque.
// Source: SF-017
#[test]
fn small_opaque_picture_centres_on_a_black_canvas() {
    let d = dir();
    image::RgbImage::from_pixel(100, 50, image::Rgb([255, 0, 0]))
        .save_with_format(d.path().join("rgb.png"), ImageFormat::Png)
        .unwrap();
    image::RgbImage::from_pixel(100, 50, image::Rgb([255, 0, 0]))
        .save_with_format(d.path().join("rgb.jpg"), ImageFormat::Jpeg)
        .unwrap();
    fs::write(
        d.path().join("pal.bmp"),
        bmp(8, 0, 40, &PALETTE, &rows(|r| r.to_vec())),
    )
    .unwrap();
    fs::write(d.path().join("rgb32.bmp"), bmp(32, 0, 40, &[], &argb(7))).unwrap();
    fs::write(d.path().join("zero.bmp"), bmp(32, 3, 108, &[], &argb(0))).unwrap();
    for (name, w, h) in [
        ("rgb.png", 100, 50),
        ("rgb.jpg", 100, 50),
        ("pal.bmp", 4, 2),
        ("rgb32.bmp", 4, 2),
        ("zero.bmp", 4, 2),
    ] {
        let c = cell(d.path(), name).unwrap();
        assert_eq!(c.padded, [258, 258], "{name}");
        let left = (256 - w) / 2;
        assert_eq!(at(&c, left, 0)[3], 255, "{name}: the picture");
        for (x, y) in [
            (left - 1, 0),
            (left + w, 0),
            (left, h),
            (0, 255),
            (255, 255),
        ] {
            assert_eq!(at(&c, x, y), [0, 0, 0, 255], "{name}: ({x}, {y}) is black");
        }
    }
}

/// The test picture's colours: red, green, blue and white.
const PALETTE: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
/// Its 4×2 indices, top row first.
const PIXELS: [[u8; 4]; 2] = [[0, 1, 2, 3], [3, 2, 1, 0]];

/// A BMP of `PIXELS` (bits per pixel `bpp`, compression `compression`,
/// info header of `header` bytes) whose rows `rows` gives bottom-up.
fn bmp(bpp: u16, compression: u32, header: u32, palette: &[[u8; 3]], data: &[u8]) -> Vec<u8> {
    let masks = header > 40;
    let offset = 14 + header + 4 * palette.len() as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(offset + data.len() as u32).to_le_bytes());
    b.extend_from_slice(&0_u32.to_le_bytes());
    b.extend_from_slice(&offset.to_le_bytes());
    b.extend_from_slice(&header.to_le_bytes());
    b.extend_from_slice(&4_i32.to_le_bytes());
    b.extend_from_slice(&2_i32.to_le_bytes());
    b.extend_from_slice(&1_u16.to_le_bytes());
    b.extend_from_slice(&bpp.to_le_bytes());
    b.extend_from_slice(&compression.to_le_bytes());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&2835_i32.to_le_bytes());
    b.extend_from_slice(&2835_i32.to_le_bytes());
    b.extend_from_slice(&(palette.len() as u32).to_le_bytes());
    b.extend_from_slice(&0_u32.to_le_bytes());
    if masks {
        // Red, green, blue and alpha masks, then the rest of the V4 or V5
        // header zeroed (colour space 0, `LCS_CALIBRATED_RGB`).
        for m in [0x00FF_0000_u32, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000] {
            b.extend_from_slice(&m.to_le_bytes());
        }
        b.resize(14 + header as usize, 0);
    }
    for [r, g, bl] in palette {
        b.extend_from_slice(&[*bl, *g, *r, 0]);
    }
    b.extend_from_slice(data);
    b
}

/// Rows bottom-up, each padded to 4 bytes, of `row(indices)`.
fn rows(row: impl Fn(&[u8; 4]) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for r in PIXELS.iter().rev() {
        let mut bytes = row(r);
        bytes.resize(bytes.len().div_ceil(4) * 4, 0);
        out.extend_from_slice(&bytes);
    }
    out
}

fn rgb(i: u8) -> [u8; 3] {
    PALETTE[usize::from(i)]
}

/// Every BMP variant a surveyed library holds, built here.
fn variants() -> Vec<(&'static str, Vec<u8>)> {
    let two = [PALETTE[0], PALETTE[2]];
    let one = |r: &[u8; 4]| {
        // Red where the index is even, blue where odd.
        let bits = r.iter().fold(0_u8, |acc, &i| (acc << 1) | (i & 1));
        vec![bits << 4]
    };
    let four = |r: &[u8; 4]| vec![(r[0] << 4) | r[1], (r[2] << 4) | r[3]];
    let eight = |r: &[u8; 4]| r.to_vec();
    let sixteen = |r: &[u8; 4]| {
        r.iter()
            .flat_map(|&i| {
                let [cr, cg, cb] = rgb(i).map(|c| u16::from(c >> 3));
                ((cr << 10) | (cg << 5) | cb).to_le_bytes()
            })
            .collect()
    };
    let twenty_four = |r: &[u8; 4]| {
        r.iter()
            .flat_map(|&i| {
                let [cr, cg, cb] = rgb(i);
                [cb, cg, cr]
            })
            .collect()
    };
    let thirty_two = |r: &[u8; 4]| {
        r.iter()
            .flat_map(|&i| {
                let [cr, cg, cb] = rgb(i);
                [cb, cg, cr, 255]
            })
            .collect()
    };
    // RLE: each pixel a run of one, an end of line after each row, an end
    // of bitmap after the last.
    let rle = |nibbles: bool| {
        let mut out = Vec::new();
        for r in PIXELS.iter().rev() {
            for &i in r {
                out.extend_from_slice(&[1, if nibbles { i << 4 } else { i }]);
            }
            out.extend_from_slice(&[0, 0]);
        }
        out.extend_from_slice(&[0, 1]);
        out
    };
    vec![
        ("1-bit", bmp(1, 0, 40, &two, &rows(one))),
        ("4-bit", bmp(4, 0, 40, &PALETTE, &rows(four))),
        ("8-bit", bmp(8, 0, 40, &PALETTE, &rows(eight))),
        ("16-bit", bmp(16, 0, 40, &[], &rows(sixteen))),
        ("24-bit", bmp(24, 0, 40, &[], &rows(twenty_four))),
        ("32-bit", bmp(32, 0, 40, &[], &rows(thirty_two))),
        ("RLE4", bmp(4, 2, 40, &PALETTE, &rle(true))),
        ("RLE8", bmp(8, 1, 40, &PALETTE, &rle(false))),
        ("bitfields 108", bmp(32, 3, 108, &[], &rows(thirty_two))),
        ("bitfields 124", bmp(32, 3, 124, &[], &rows(thirty_two))),
    ]
}

#[test]
fn bmp_variants_decode_through_the_production_reader() {
    let d = dir();
    for (name, bytes) in variants() {
        let file = format!("{}.bmp", name.replace(' ', "_"));
        fs::write(d.path().join(&file), &bytes).unwrap();
        let c = cell(d.path(), &file).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(c.native, [4, 2], "{name}");
        for (y, row) in PIXELS.iter().enumerate() {
            for (x, &i) in row.iter().enumerate() {
                let want = if name == "1-bit" {
                    if i & 1 == 0 { PALETTE[0] } else { PALETTE[2] }
                } else {
                    rgb(i)
                };
                let [r, g, b, a] = at(&c, 126 + x as u32, y as u32);
                assert_eq!(([r, g, b], a), (want, 255), "{name} ({x}, {y})");
            }
        }
    }
    // The format is the content's: a PNG named `.bmp` decodes.
    save(d.path(), "png.bmp", &solid(3, 3, [9, 8, 7, 255]));
    assert_eq!(
        at(&cell(d.path(), "png.bmp").unwrap(), 126, 0),
        [9, 8, 7, 255]
    );
    // A GIF, and a file of no known signature, are absent, not an error.
    let mut gif = Vec::new();
    RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
        .write_to(&mut std::io::Cursor::new(&mut gif), ImageFormat::Gif)
        .unwrap_or_else(|_| gif.extend_from_slice(b"GIF89a\x02\x00\x02\x00\x00\x00\x00;"));
    fs::write(d.path().join("g.gif"), &gif).unwrap();
    assert_eq!(cell(d.path(), "g.gif"), Err(Absent::Format));
    fs::write(d.path().join("u.png"), b"not a picture at all").unwrap();
    assert_eq!(cell(d.path(), "u.png"), Err(Absent::Format));
}

#[test]
fn a_refused_allocation_leaves_the_picture_absent() {
    let d = dir();
    save(d.path(), "s.png", &solid(300, 300, [1, 2, 3, 255]));
    let dir = ChartDir::new(d.path()).unwrap();
    let BgaBinding::Resolved { path, key, .. } = resolve(&dir, "s.png", &fixed) else {
        panic!();
    };
    let native = probe(&path, &key, &fixed).unwrap();
    let read = |limit| read_cell(&path, &key, native, [300, 300], &never, &fixed, Some(limit));
    // The file's bytes, then the cell (302² × 4 bytes).
    assert_eq!(read(0), Err(Absent::Refused));
    assert_eq!(read(302 * 302 * 4 - 1), Err(Absent::Refused));
    assert!(read(302 * 302 * 4).is_ok());
}

/// A large picture fitted to a small cell, its file under the limit: the
/// decoded picture is held to the limit too, refused from the header's
/// size before it is allocated, so the picture is absent rather than the
/// allocation past it made.
#[test]
fn a_large_picture_in_a_small_cell_is_refused_before_it_is_decoded() {
    let d = dir();
    let rgb = image::RgbImage::from_pixel(640, 480, image::Rgb([10, 20, 30]));
    rgb.save_with_format(d.path().join("big.png"), ImageFormat::Png)
        .unwrap();
    let limit = 64 << 10;
    assert!(fs::metadata(d.path().join("big.png")).unwrap().len() < limit);
    let dir = ChartDir::new(d.path()).unwrap();
    let BgaBinding::Resolved { path, key, .. } = resolve(&dir, "big.png", &fixed) else {
        panic!();
    };
    let native = probe(&path, &key, &fixed).unwrap();
    let read = |limit| read_cell(&path, &key, native, [64, 48], &never, &fixed, limit);
    assert_eq!(read(Some(limit)), Err(Absent::Refused));
    let cell = read(None).unwrap();
    assert_eq!(cell.native, [640, 480]);
    assert_eq!(cell.padded, [64 + 2 * CELL_GUTTER, 48 + 2 * CELL_GUTTER]);
}

fn sizes(n: usize, size: [u32; 2]) -> Vec<Option<[u32; 2]>> {
    vec![Some(stored_size(size)); n]
}

/// Every cell lies on its page, its gutter included, and no two overlap.
fn check(p: &bga::Packing, budget: u32) {
    assert!(p.pages <= budget, "{} pages", p.pages);
    let cells: Vec<_> = p.cells.iter().flatten().collect();
    for c in &cells {
        let [w, h] = c.padded();
        assert!(c.page < p.pages);
        assert!(c.origin[0] + w <= PAGE_SIDE && c.origin[1] + h <= PAGE_SIDE);
    }
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            let [aw, ah] = a.padded();
            let [bw, bh] = b.padded();
            let apart = a.page != b.page
                || a.origin[0] + aw <= b.origin[0]
                || b.origin[0] + bw <= a.origin[0]
                || a.origin[1] + ah <= b.origin[1]
                || b.origin[1] + bh <= a.origin[1];
            assert!(apart, "{a:?} overlaps {b:?}");
        }
    }
}

#[test]
fn packing_ladder_steps_down_and_terminates() {
    // A set that fits stays exact.
    let small = sizes(200, [256, 256]);
    let p = pack(&small, PAGE_BUDGET);
    assert_eq!(p.step, Step::Full);
    check(&p, PAGE_BUDGET);
    assert!(p.cells.iter().flatten().all(|c| c.content == [256, 256]));
    // Over budget: steps down to the first cap that fits, the small ones
    // exact. Thirteen 4 094×2 000 pictures take two to a page (seven
    // pages); capped at 2 048 (2 048×1 000) they take four (four pages).
    let mut mixed = sizes(13, [4_094, 2_000]);
    mixed.extend(sizes(10, [100, 100]));
    mixed.push(None);
    let p = pack(&mixed, PAGE_BUDGET);
    assert_eq!(p.step, Step::Cap(2_048));
    check(&p, PAGE_BUDGET);
    assert_eq!(p.cells[0].unwrap().content, [2_048, 1_000]);
    assert_eq!(p.cells[13].unwrap().content, [256, 256]);
    assert_eq!(p.cells[23], None);
    // Seven 4 094² pictures fit one to a page until capped at 1 024.
    let p = pack(&sizes(7, [4_094, 4_094]), PAGE_BUDGET);
    assert_eq!(p.step, Step::Cap(1_024));
    check(&p, PAGE_BUDGET);
    // 1 296 canvases need 7 pages exact: the last step, two pages.
    let most = sizes(1_296, [256, 256]);
    let p = pack(&most, PAGE_BUDGET);
    assert_eq!(p.step, Step::Grid);
    assert_eq!(p.pages, 2);
    check(&p, PAGE_BUDGET);
    assert!(
        p.cells
            .iter()
            .flatten()
            .all(|c| c.content == [GRID_SIDE, GRID_SIDE])
    );
    // The last step keeps the aspect within 128; a wide set that fits
    // before it stops at its cap.
    assert_eq!(Step::Grid.content([4_094, 1_000]), [128, 31]);
    let wide = pack(&[Some([4_094, 1_000]); 1_296], PAGE_BUDGET);
    assert_eq!(wide.step, Step::Cap(512));
    check(&wide, PAGE_BUDGET);
}
