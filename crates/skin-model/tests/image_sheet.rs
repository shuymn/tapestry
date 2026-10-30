//! Loading a sheet: the counts come before any file is touched, rectangles
//! and grids are checked against the files, every enabled format decodes
//! to premultiplied texels, and an @2x file must be exactly twice its 1x.

mod image_support;

use std::path::Path;

use bms_model::TimeUs;
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use image_support::{atlas, content, dir, file, load, load_files, save, solid, texel, whole};
use skin_model::{
    CycleDef, FileAt, ImageDef, ImageError, MAX_FILES, MAX_IMAGES, MAX_KEY_US, MAX_SKIN_FRAMES,
    TimerRef, timer,
};

/// A folder that does not exist: a check that reaches the disk fails as
/// `Root`.
fn nowhere() -> std::path::PathBuf {
    dir().path().join("missing")
}

fn grid(file: u32, cols: u16, rows: u16) -> ImageDef {
    ImageDef {
        grid: [cols, rows],
        ..whole(file)
    }
}

fn cycle(period: i64) -> ImageDef {
    ImageDef {
        cycle: Some(CycleDef {
            period: TimeUs(period),
            timer: TimerRef::Builtin(timer::JUDGE),
        }),
        ..whole(0)
    }
}

#[test]
fn counts_come_before_the_disk() {
    let root = nowhere();
    let err = load(&root, vec![file("a.png"); MAX_FILES + 1], Vec::new()).unwrap_err();
    assert!(matches!(err, ImageError::TooManyFiles { n } if n == MAX_FILES + 1));
    let err = load(&root, vec![file("a.png")], vec![whole(0); MAX_IMAGES + 1]).unwrap_err();
    assert!(matches!(err, ImageError::TooManyImages { n } if n == MAX_IMAGES + 1));
    let err = load(&root, vec![file("a.png")], vec![whole(0), whole(1)]).unwrap_err();
    assert!(matches!(err, ImageError::UnknownFile { image: 1 }));
    for g in [[0, 1], [1, 0]] {
        let err = load(&root, vec![file("a.png")], vec![grid(0, g[0], g[1])]).unwrap_err();
        assert!(matches!(err, ImageError::EmptyGrid { image: 0 }), "{g:?}");
    }
    // 32 × 32 is the per-image cap; 33 × 32 passes it.
    let err = load(&root, vec![file("a.png")], vec![grid(0, 33, 32)]).unwrap_err();
    assert!(matches!(
        err,
        ImageError::TooManyImageFrames { image: 0, n: 1_056 }
    ));
    // 64 images of 1 024 frames fill the skin's frames; one more passes.
    let full = vec![grid(0, 32, 32); 64];
    assert_eq!(64 * 1_024, MAX_SKIN_FRAMES);
    let err = load(&root, vec![file("a.png")], full.clone()).unwrap_err();
    assert!(matches!(err, ImageError::Root(_)), "{err:?}");
    let mut over = full;
    over.push(whole(0));
    let err = load(&root, vec![file("a.png")], over).unwrap_err();
    assert!(matches!(err, ImageError::TooManySkinFrames));
    // Cycle periods: 1 µs to MAX_KEY_US.
    for (period, ok) in [
        (0, false),
        (1, true),
        (MAX_KEY_US, true),
        (MAX_KEY_US + 1, false),
    ] {
        let err = load(&root, vec![file("a.png")], vec![cycle(period)]).unwrap_err();
        if ok {
            assert!(matches!(err, ImageError::Root(_)), "{period}: {err:?}");
        } else {
            assert!(
                matches!(err, ImageError::CycleOutOfRange { image: 0 }),
                "{period}: {err:?}"
            );
        }
    }
}

#[test]
fn rectangles_and_frames_fit_their_file() {
    let d = dir();
    save(d.path(), "a.png", &solid(10, 6, [255; 4]));
    let rect = |r: [u32; 4], cols: u16, rows: u16| ImageDef {
        rect: Some(r),
        grid: [cols, rows],
        ..whole(0)
    };
    for r in [
        [0, 0, 11, 6],
        [1, 0, 10, 6],
        [0, 5, 10, 2],
        [u32::MAX, 0, 2, 2],
    ] {
        let err = load(d.path(), vec![file("a.png")], vec![rect(r, 1, 1)]).unwrap_err();
        assert!(matches!(err, ImageError::RectOutside { image: 0 }), "{r:?}");
    }
    let err = load(
        d.path(),
        vec![file("a.png")],
        vec![rect([0, 0, 3, 6], 4, 1)],
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::EmptyCell { image: 0 }));
    let err = load(
        d.path(),
        vec![file("a.png")],
        vec![rect([0, 0, 0, 6], 1, 1)],
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::EmptyCell { image: 0 }));
    // A remainder is left out: 10 / 3 = 3 per frame.
    let sheet = load(
        d.path(),
        vec![file("a.png")],
        vec![rect([0, 0, 10, 6], 3, 2)],
    )
    .unwrap();
    assert_eq!(sheet.frame_count(), 6);
    let a = atlas(&sheet);
    for f in 0..6 {
        assert_eq!(content(&a, f, 1).0[2..], [3, 3]);
    }
}

/// The texel frame 0 shows at its content's top left.
fn first_texel(root: &Path, name: &str) -> [u8; 4] {
    let sheet = load_files(root, &[name]).unwrap();
    let a = atlas(&sheet);
    let ([x, y, ..], page) = content(&a, 0, 1);
    texel(&a, page, 0, x, y)
}

#[test]
fn every_format_decodes_premultiplied() {
    let d = dir();
    let root = d.path();
    // (200, 100, 50) at alpha 128: (c × 128 + 127) / 255.
    let half = [100, 50, 25, 128];
    save(root, "rgba.png", &solid(3, 2, [200, 100, 50, 128]));
    assert_eq!(first_texel(root, "rgba.png"), half);

    RgbImage::from_pixel(3, 2, Rgb([200, 100, 50]))
        .save_with_format(root.join("rgb.png"), ImageFormat::Png)
        .unwrap();
    assert_eq!(first_texel(root, "rgb.png"), [200, 100, 50, 255]);

    // 16 bits per channel: v × 257 reads back as v.
    image::ImageBuffer::<Rgba<u16>, _>::from_pixel(
        3,
        2,
        Rgba([200 * 257, 100 * 257, 50 * 257, 128 * 257]),
    )
    .save_with_format(root.join("deep.png"), ImageFormat::Png)
    .unwrap();
    assert_eq!(first_texel(root, "deep.png"), half);

    image::ImageBuffer::<image::LumaA<u8>, _>::from_pixel(3, 2, image::LumaA([200, 128]))
        .save_with_format(root.join("gray.png"), ImageFormat::Png)
        .unwrap();
    assert_eq!(first_texel(root, "gray.png"), [100, 100, 100, 128]);

    // A palette with transparency, written with `png` directly.
    {
        let f = std::fs::File::create(root.join("pal.png")).unwrap();
        let mut enc = png::Encoder::new(f, 3, 2);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(vec![200, 100, 50, 0, 0, 255]);
        enc.set_trns(vec![128, 255]);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[0, 1, 1, 1, 1, 1]).unwrap();
    }
    assert_eq!(first_texel(root, "pal.png"), half);

    RgbImage::from_pixel(3, 2, Rgb([200, 100, 50]))
        .save_with_format(root.join("a.bmp"), ImageFormat::Bmp)
        .unwrap();
    assert_eq!(first_texel(root, "a.bmp"), [200, 100, 50, 255]);

    RgbImage::from_pixel(16, 16, Rgb([200, 100, 50]))
        .save_with_format(root.join("a.jpg"), ImageFormat::Jpeg)
        .unwrap();
    let got = first_texel(root, "a.jpg");
    for (g, w) in got.iter().zip([200_u8, 100, 50, 255]) {
        assert!(g.abs_diff(w) <= 2, "{got:?}");
    }

    image_support::write(root, "a.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;");
    let err = load_files(root, &["a.gif"]).unwrap_err();
    assert!(matches!(
        err,
        ImageError::Unsupported {
            at: FileAt { file: 0, hi: false }
        }
    ));
}

#[test]
fn an_at2x_file_is_exactly_twice_its_1x() {
    let d = dir();
    save(d.path(), "a.png", &solid(4, 3, [255; 4]));
    for (w, h) in [(8, 5), (7, 6), (4, 3), (16, 12)] {
        save(d.path(), "hi.png", &RgbaImage::new(w, h));
        let mut def = file("a.png");
        def.hi = Some("hi.png".into());
        let err = load(d.path(), vec![def], vec![whole(0)]).unwrap_err();
        assert!(
            matches!(err, ImageError::HiSizeMismatch { file: 0 }),
            "{w}×{h}"
        );
    }
    save(d.path(), "hi.png", &RgbaImage::new(8, 6));
    let mut def = file("a.png");
    def.hi = Some("hi.png".into());
    load(d.path(), vec![def], vec![whole(0)]).unwrap();
}
