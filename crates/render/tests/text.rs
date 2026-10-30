//! Menu text drawn on WARP and compared with its expected image (design.md
//! "Testing Strategy").
//!
//! The image is drawn with a pinned font file, so a font update cannot pass
//! for a rendering change: the test first checks the file's SHA-256. When the
//! image changes, the test writes the drawn image and a diff next to each
//! other under the target folder; after reviewing them, run the test with
//! `TAPESTRY_UPDATE_EXPECTED=1` to write the new expected image (docs/setup.md
//! "CI").

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use render::text::{Fonts, MenuText, TextBlock, TextStyle, fonts_dir};
use render::{AdapterChoice, Renderer, Target, text_screen};
use sha2::{Digest as _, Sha256};

/// The font file the expected image is drawn with, and its SHA-256.
const PINNED_FONT: &str = "YuGothM.ttc";
const PINNED_SHA256: &str = "cb2021bbfe40b21ec564848b8d702125661594e1b4f1eb2e77fd8a98b485cf19";

/// The layout at 1/3 scale.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;

/// A pixel differs when a channel is off by more than this, and at most
/// [`MAX_DIFFERING`] pixels may differ.
const CHANNEL_TOLERANCE: u8 = 16;
const MAX_DIFFERING: usize = 64;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The bytes of `path`, when their SHA-256 is `pinned`; otherwise an error
/// naming both hashes.
fn pinned_font(path: &Path, pinned: &str) -> Result<Vec<u8>, String> {
    let data = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let found = hex(&Sha256::digest(&data));
    if found == pinned {
        Ok(data)
    } else {
        Err(format!(
            "{} has SHA-256 {found}, but the expected image is drawn with {pinned}",
            path.display()
        ))
    }
}

#[test]
fn pinned_font_names_both_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("font.ttc");
    fs::write(&path, b"abc").unwrap();
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(pinned_font(&path, abc).unwrap(), b"abc");
    let wrong = "00".repeat(32);
    let e = pinned_font(&path, &wrong).unwrap_err();
    assert!(e.contains(abc) && e.contains(&wrong), "{e}");
}

fn png_rgb(path: &Path) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(
        (info.width, info.height),
        (WIDTH, HEIGHT),
        "{}",
        path.display()
    );
    assert_eq!(info.color_type, png::ColorType::Rgb);
    buf.truncate(info.buffer_size());
    buf
}

fn write_png_rgb(path: &Path, rgb: &[u8]) {
    let file = fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), WIDTH, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(rgb)
        .unwrap();
}

fn expected_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/expected")
        .join(name)
}

/// Compares `rgb` with the expected image `name`, or writes it there when
/// `TAPESTRY_UPDATE_EXPECTED` is set.
fn assert_matches_expected(name: &str, rgb: &[u8]) {
    let expected = expected_path(name);
    if std::env::var_os("TAPESTRY_UPDATE_EXPECTED").is_some() {
        fs::create_dir_all(expected.parent().unwrap()).unwrap();
        write_png_rgb(&expected, rgb);
        println!("wrote {}", expected.display());
        return;
    }
    assert!(
        expected.is_file(),
        "{} is missing: run with TAPESTRY_UPDATE_EXPECTED=1 to write it",
        expected.display()
    );
    let want = png_rgb(&expected);
    let mut diff = vec![0; rgb.len()];
    let mut differing = 0;
    for ((d, got), want) in diff
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(rgb.as_chunks::<3>().0)
        .zip(want.as_chunks::<3>().0)
    {
        let off = got
            .iter()
            .zip(want)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        if off > CHANNEL_TOLERANCE {
            differing += 1;
            d.copy_from_slice(&[255, 0, 255]);
        } else {
            d.copy_from_slice(&[got[0] / 4, got[1] / 4, got[2] / 4]);
        }
    }
    if differing > MAX_DIFFERING {
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("expected");
        fs::create_dir_all(&out).unwrap();
        let actual = out.join(name.replace(".png", ".actual.png"));
        let marked = out.join(name.replace(".png", ".diff.png"));
        write_png_rgb(&actual, rgb);
        write_png_rgb(&marked, &diff);
        panic!(
            "{differing} pixels differ from {} (at most {MAX_DIFFERING} may): drawn {}, differences in magenta {}",
            expected.display(),
            actual.display(),
            marked.display()
        );
    }
}

#[test]
fn japanese_title_renders_on_warp() {
    let path = fonts_dir().join(PINNED_FONT);
    let data = pinned_font(&path, PINNED_SHA256).unwrap_or_else(|e| panic!("{e}"));
    let mut fonts = Fonts::from_data(data, "Yu Gothic UI").unwrap();
    let mut r = Renderer::new(
        Target::Offscreen {
            width: WIDTH,
            height: HEIGHT,
        },
        AdapterChoice::Warp,
    )
    .expect("WARP renderer");
    assert_eq!(r.gpu().adapter().name, common::WARP);

    // A title over the bitmap font's sprites, which it must cover: the
    // text pass comes after the sprite pass.
    let title = TextBlock::new(
        &mut fonts,
        "夜明けの譜面 ★12",
        TextStyle {
            left: 60.0,
            top: 40.0,
            size: 120.0,
            color: [255, 255, 255, 255],
        },
    );
    let artist = TextBlock::new(
        &mut fonts,
        "アーティスト：テスト\nBPM 150・ジャンル：電子音楽",
        TextStyle {
            left: 60.0,
            top: 260.0,
            size: 72.0,
            color: [255, 200, 64, 255],
        },
    );
    assert!(title.width() > 600.0, "{}", title.width());
    let mut text = MenuText::new(&r);
    text.prepare(&r, &mut fonts, &[&title, &artist]).unwrap();
    let timing = r
        .menu_frame(|b| text_screen(&["MENU MENU MENU"], b), &text)
        .unwrap();
    assert!(timing.presented);
    let rgb: Vec<u8> = r
        .read_back()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    assert_matches_expected("japanese_title.png", &rgb);
}
