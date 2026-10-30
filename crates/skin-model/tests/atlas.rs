//! Atlas pages: no mip level mixes two frames, gutters repeat the edges
//! through the whole allocation, the side is the cheapest counting the
//! reserved layers, @2x files replace their 1x cells, shared rectangles
//! share a frame's place, and the build is deterministic.

mod image_support;

use image::{Rgba, RgbaImage};
use image_support::{LIMITS, atlas, content, dir, file, load, save, solid, texel, whole};
use skin_model::{AtlasLimits, ImageDef, Variant, variant_for};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const WHITE: [u8; 4] = [255; 4];
/// Green with no alpha: premultiplied, it is all zeros.
const CLEAR_GREEN: [u8; 4] = [0, 255, 0, 0];

/// A bilinear sample of level `k` of `page` at (`u`, `v`), as a sampler
/// with clamp-to-edge addressing reads it.
fn bilinear(a: &skin_model::Atlas, page: usize, k: usize, u: f32, v: f32) -> [f32; 4] {
    let side = a.side() >> k;
    let (x, y) = (u * side as f32 - 0.5, v * side as f32 - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |x: f32, y: f32| {
        let cx = (x.max(0.0) as u32).min(side - 1);
        let cy = (y.max(0.0) as u32).min(side - 1);
        texel(a, page, k, cx, cy).map(f32::from)
    };
    let (p00, p10, p01, p11) = (
        at(x0, y0),
        at(x0 + 1.0, y0),
        at(x0, y0 + 1.0),
        at(x0 + 1.0, y0 + 1.0),
    );
    std::array::from_fn(|c| {
        let top = p00[c] * (1.0 - fx) + p10[c] * fx;
        let bottom = p01[c] * (1.0 - fx) + p11[c] * fx;
        top * (1.0 - fy) + bottom * fy
    })
}

#[test]
fn no_level_mixes_two_frames() {
    let d = dir();
    // A 2×2 grid of 13×7 frames: red, blue / green, white.
    let mut grid = RgbaImage::new(26, 14);
    for (x, y, p) in grid.enumerate_pixels_mut() {
        *p = Rgba(match (x / 13, y / 7) {
            (0, 0) => RED,
            (1, 0) => BLUE,
            (0, 1) => GREEN,
            _ => WHITE,
        });
    }
    save(d.path(), "grid.png", &grid);
    // Red framed by transparent green.
    let mut framed = RgbaImage::from_pixel(5, 5, Rgba(CLEAR_GREEN));
    for y in 1..4 {
        for x in 1..4 {
            framed.put_pixel(x, y, Rgba(RED));
        }
    }
    save(d.path(), "framed.png", &framed);
    let sheet = load(
        d.path(),
        vec![file("grid.png"), file("framed.png")],
        vec![
            ImageDef {
                grid: [2, 2],
                ..whole(0)
            },
            whole(1),
        ],
    )
    .unwrap();
    let a = atlas(&sheet);
    let colours = [RED, BLUE, GREEN, WHITE];
    for (frame, colour) in (0_u32..).zip(colours) {
        let ([x, y, w, h], page) = content(&a, frame, 1);
        assert_eq!([w, h], [13, 7]);
        // The content plus its gutter of 8.
        let (x0, y0, x1, y1) = (x - 8, y - 8, x + w + 8, y + h + 8);
        assert!(x0 % 8 == 0 && y0 % 8 == 0, "frame {frame} at {x}, {y}");
        for k in 0..4 {
            // Every level-k texel whose 2^k block touches the allocation
            // shows exactly the frame's colour.
            for ty in (y0 >> k)..((y1 - 1) >> k) + 1 {
                for tx in (x0 >> k)..((x1 - 1) >> k) + 1 {
                    assert_eq!(
                        texel(&a, page, k, tx, ty),
                        colour,
                        "frame {frame} level {k} at {tx}, {ty}"
                    );
                }
            }
            // Bilinear samples at the uv corners.
            let uv = a.uvs().get(frame).unwrap().uv;
            for (u, v) in [
                (uv[0], uv[1]),
                (uv[2], uv[1]),
                (uv[0], uv[3]),
                (uv[2], uv[3]),
            ] {
                let s = bilinear(&a, page, k, u, v);
                for (got, want) in s.iter().zip(colour) {
                    assert!(
                        (got - f32::from(want)).abs() < 0.01,
                        "frame {frame} level {k}: {s:?}"
                    );
                }
            }
        }
    }
    // The framed image: no green anywhere near it at any level.
    let ([x, y, w, h], page) = content(&a, 4, 1);
    for k in 0..4 {
        for ty in ((y - 8) >> k)..((y + h + 7) >> k) + 1 {
            for tx in ((x - 8) >> k)..((x + w + 7) >> k) + 1 {
                let t = texel(&a, page, k, tx, ty);
                assert_eq!(t[1], 0, "level {k} at {tx}, {ty}: {t:?}");
            }
        }
        let uv = a.uvs().get(4).unwrap().uv;
        for (u, v) in [(uv[0], uv[1]), (uv[2], uv[3])] {
            assert_eq!(bilinear(&a, page, k, u, v)[1], 0.0);
        }
    }
    // Level 0 of the framed image: clear edges, premultiplied red inside.
    assert_eq!(texel(&a, page, 0, x, y), [0; 4]);
    assert_eq!(texel(&a, page, 0, x + 2, y + 2), RED);
}

#[test]
fn gutters_repeat_the_nearest_edge_through_the_allocation() {
    let d = dir();
    // Every texel of a 13×7 image distinct, half transparent.
    let img = RgbaImage::from_fn(13, 7, |x, y| Rgba([x as u8 * 19, y as u8 * 36, 7, 128]));
    save(d.path(), "a.png", &img);
    let sheet = load(d.path(), vec![file("a.png")], vec![whole(0)]).unwrap();
    let a = atlas(&sheet);
    let ([x, y, w, h], page) = content(&a, 0, 1);
    // The allocation is at least the content and gutter rounded up to 8:
    // 13 + 16 = 29 → 32 wide, 7 + 16 = 23 → 24 tall.
    for ty in y - 8..y - 8 + 24 {
        for tx in x - 8..x - 8 + 32 {
            let sx = tx.saturating_sub(x).min(w - 1);
            let sy = ty.saturating_sub(y).min(h - 1);
            let [r, g, b, al] = img.get_pixel(sx, sy).0;
            let pre = |c: u8| ((u16::from(c) * u16::from(al) + 127) / 255) as u8;
            assert_eq!(
                texel(&a, page, 0, tx, ty),
                [pre(r), pre(g), pre(b), al],
                "at {tx}, {ty}"
            );
        }
    }
}

#[test]
fn the_side_counts_the_reserved_layers() {
    let d = dir();
    // Four frames of 240 (256 padded): four pages of 256 or one of 512.
    save(d.path(), "a.png", &solid(480, 480, WHITE));
    let sheet = load(
        d.path(),
        vec![file("a.png")],
        vec![ImageDef {
            grid: [2, 2],
            ..whole(0)
        }],
    )
    .unwrap();
    let limits = |reserved| AtlasLimits {
        max_side: 512,
        max_layers: 16,
        reserved_layers: reserved,
    };
    // With a reserved layer: 5 × 256² < 2 × 512².
    let a = sheet.atlas(Variant::Lo, limits(1)).unwrap();
    assert_eq!((a.side(), a.pages().len()), (256, 4));
    // With none, the two cost the same and fewer pages win.
    let a = sheet.atlas(Variant::Lo, limits(0)).unwrap();
    assert_eq!((a.side(), a.pages().len()), (512, 1));
}

#[test]
fn the_variant_follows_the_target() {
    assert_eq!(variant_for([1920, 1080], [1920, 1080]), Variant::Lo);
    assert_eq!(variant_for([1921, 1081], [1920, 1080]), Variant::Hi);
    assert_eq!(variant_for([3840, 1080], [1920, 1080]), Variant::Lo);
    assert_eq!(variant_for([3840, 2160], [1920, 1080]), Variant::Hi);
    assert_eq!(variant_for([1280, 720], [1920, 1080]), Variant::Lo);

    let d = dir();
    save(d.path(), "a.png", &solid(6, 4, RED));
    save(d.path(), "a@2x.png", &solid(12, 8, BLUE));
    save(d.path(), "b.png", &solid(5, 3, GREEN));
    let mut a = file("a.png");
    a.hi = Some("a@2x.png".into());
    let half = ImageDef {
        rect: Some([2, 0, 3, 4]),
        ..whole(0)
    };
    let sheet = load(d.path(), vec![a, file("b.png")], vec![half, whole(1)]).unwrap();
    let lo = sheet.atlas(Variant::Lo, LIMITS).unwrap();
    let hi = sheet.atlas(Variant::Hi, LIMITS).unwrap();
    assert_eq!((lo.variant(), hi.variant()), (Variant::Lo, Variant::Hi));
    let (r, p) = content(&lo, 0, 1);
    assert_eq!(r[2..], [3, 4]);
    assert_eq!(texel(&lo, p, 0, r[0], r[1]), RED);
    // The @2x file at twice the rectangle.
    let (r, p) = content(&hi, 0, 1);
    assert_eq!(r[2..], [6, 8]);
    assert_eq!(texel(&hi, p, 0, r[0], r[1]), BLUE);
    assert_eq!(texel(&hi, p, 0, r[0] + 5, r[1] + 7), BLUE);
    // A file without one keeps its 1x texels.
    let (r, p) = content(&hi, 1, 1);
    assert_eq!(r[2..], [5, 3]);
    assert_eq!(texel(&hi, p, 0, r[0], r[1]), GREEN);
}

#[test]
fn shared_rectangles_share_a_place_and_builds_repeat() {
    let d = dir();
    save(d.path(), "a.png", &solid(40, 30, WHITE));
    save(d.path(), "b.png", &solid(300, 200, BLUE));
    let cut = |r| ImageDef {
        rect: Some(r),
        ..whole(0)
    };
    let mut images = vec![
        cut([0, 0, 20, 10]),
        cut([0, 0, 20, 10]),
        cut([0, 0, 20, 11]),
    ];
    // Enough frames for several pages of 256.
    images.push(ImageDef {
        grid: [3, 2],
        ..whole(1)
    });
    images.push(ImageDef {
        grid: [6, 5],
        ..whole(1)
    });
    let sheet = load(d.path(), vec![file("a.png"), file("b.png")], images).unwrap();
    let a = atlas(&sheet);
    let uvs = a.uvs();
    assert_eq!(uvs.frames().len(), 3 + 6 + 30);
    assert_eq!(uvs.get(0), uvs.get(1));
    assert_ne!(uvs.get(0).unwrap().uv, uvs.get(2).unwrap().uv);
    assert!(a.pages().len() >= 2, "{} pages", a.pages().len());
    for (i, f) in uvs.frames().iter().enumerate() {
        assert!((1..8).contains(&f.layer), "frame {i} on layer {}", f.layer);
        for v in f.uv {
            let t = v * a.side() as f32;
            assert_eq!(t.fract(), 0.0, "frame {i}: {:?}", f.uv);
        }
    }
    let again = atlas(&sheet);
    assert_eq!(again.side(), a.side());
    assert_eq!(again.uvs().frames(), uvs.frames());
    for (p, q) in again.pages().iter().zip(a.pages()) {
        for k in 0..4 {
            assert!(p.level(k) == q.level(k), "level {k}");
        }
    }
    assert!(a.pages()[0].level(4).is_none());
}
