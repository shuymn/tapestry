//! Fixtures for the image and atlas tests, all generated here: images
//! saved with `image`, hand-made PNG and BMP headers, and folders.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use image::{ImageFormat, Rgba, RgbaImage};
use skin_model::{Atlas, AtlasLimits, FileDef, Filter, ImageDef, ImageError, Sheet, Variant};
use tempfile::TempDir;

/// One page layer reserved, room for a few pages of 256.
pub const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 8,
    reserved_layers: 1,
};

pub fn dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

pub fn save(root: &Path, name: &str, img: &RgbaImage) {
    img.save_with_format(root.join(name), ImageFormat::Png)
        .unwrap();
}

/// A `w`×`h` image of one colour.
pub fn solid(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(w, h, Rgba(c))
}

/// A folder holding the bitmap fonts `a.fnt` (only `A`) and `b.fnt`
/// (only `B`), each glyph 6×9 advancing 8 on a white 16×16 page, and a
/// sheet whose bitmap font 0 is `a` falling back to `b`. The folder must
/// live until the atlas is built.
pub fn ab_fonts() -> (TempDir, Sheet) {
    let root = dir();
    for (name, code) in [("a", 65), ("b", 66)] {
        let s = format!(
            "info face=\"t\" size=10 padding=0,0,0,0\n\
             common lineHeight=12 base=10 scaleW=16 scaleH=16 pages=1\n\
             page id=0 file=\"{name}.png\"\n\
             char id={code} x=0 y=0 width=6 height=9 xoffset=1 yoffset=1 xadvance=8 page=0\n"
        );
        fs::write(root.path().join(format!("{name}.fnt")), s).unwrap();
        save(
            root.path(),
            &format!("{name}.png"),
            &solid(16, 16, [255; 4]),
        );
    }
    let mut b = skin_model::SheetBuilder::new(&skin_model::Root::new(root.path()).unwrap());
    let a = b.admit_fnt("a.fnt", &mut |_| Ok(())).unwrap();
    let f = b.admit_fnt("b.fnt", &mut |_| Ok(())).unwrap();
    b.add_bitmap_font(a, &[f]).unwrap();
    (root, b.finish())
}

pub fn file(path: &str) -> FileDef {
    FileDef {
        path: path.into(),
        hi: None,
    }
}

/// The whole of file `file` as one frame.
pub fn whole(file: u32) -> ImageDef {
    ImageDef {
        file,
        rect: None,
        grid: [1, 1],
        cycle: None,
        filter: Filter::Linear,
    }
}

pub fn load(root: &Path, files: Vec<FileDef>, images: Vec<ImageDef>) -> Result<Sheet, ImageError> {
    Sheet::new(root, files, images)
}

/// A sheet of the files named, each one whole image.
pub fn load_files(root: &Path, names: &[&str]) -> Result<Sheet, ImageError> {
    let files = names.iter().map(|n| file(n)).collect();
    let images = (0..names.len())
        .map(|i| whole(u32::try_from(i).unwrap()))
        .collect();
    load(root, files, images)
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

/// A PNG chunk: length, type, data, CRC.
pub fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
    out
}

/// The PNG signature and an IHDR of `w`×`h`, 8-bit RGBA.
pub fn png_start(w: u32, h: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    out
}

/// A header of `w`×`h` followed by the start of an image-data chunk that
/// the file cuts short: a header with no pixels behind it.
pub fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut out = png_start(w, h);
    out.extend_from_slice(&1_000_u32.to_be_bytes());
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&[0x78, 0x9c, 0, 0]);
    out
}

/// A 24-bit BMP header declaring `w`×`h` with no pixels behind it.
pub fn bmp_header(w: i32, h: i32) -> Vec<u8> {
    let mut out = b"BM".to_vec();
    out.extend_from_slice(&54_u32.to_le_bytes()); // file size, not checked
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54_u32.to_le_bytes()); // pixel offset
    out.extend_from_slice(&40_u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes()); // planes
    out.extend_from_slice(&24_u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&[0; 24]); // no compression, sizes, palette
    out
}

pub fn write(root: &Path, name: &str, bytes: &[u8]) {
    fs::write(root.join(name), bytes).unwrap();
}

/// A directory junction on Windows, a symbolic link elsewhere.
pub fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// The texel at (`x`, `y`) of level `k` of page `page`.
pub fn texel(atlas: &Atlas, page: usize, k: usize, x: u32, y: u32) -> [u8; 4] {
    let side = atlas.side() >> k;
    let level = atlas.pages()[page].level(k).unwrap();
    let at = ((y * side + x) * 4) as usize;
    level[at..at + 4].try_into().unwrap()
}

/// Frame `frame`'s content rectangle in level-0 texels (left, top, width,
/// height) and its page.
pub fn content(atlas: &Atlas, frame: u32, reserved: u16) -> ([u32; 4], usize) {
    let f = atlas.uvs().get(frame).unwrap();
    let side = atlas.side() as f32;
    let [l, t, r, b] = f.uv.map(|v| v * side);
    for v in [l, t, r, b] {
        assert_eq!(v.fract(), 0.0, "uv {:?} is not on a texel edge", f.uv);
    }
    let rect = [l as u32, t as u32, (r - l) as u32, (b - t) as u32];
    (rect, usize::from(f.layer - reserved))
}

/// The atlas of `sheet` at [`LIMITS`], `Lo`.
pub fn atlas(sheet: &Sheet) -> Atlas {
    sheet.atlas(Variant::Lo, LIMITS).unwrap()
}
