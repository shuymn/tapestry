//! An atlas rebound to another sheet of the same skin folder: each frame
//! of the new sheet is found by its cell's identity among the cells the
//! build placed, whatever the new sheet's frame or file order, on any
//! page, with no file read; a cell not held, another folder, variant or
//! text layer is refused. The oracle is the content: each frame's texels,
//! read from the shared pages at its rebound uv, equal its file's
//! rectangle, premultiplied.

mod image_support;

use std::path::Path;
use std::sync::Arc;

use image::{Rgba, RgbaImage};
use image_support::{LIMITS, content, dir, file, save, texel, whole};
use skin_model::{
    Atlas, AtlasError, AtlasLimits, ImageDef, Root, Sheet, SheetBuilder, SolidImageDef, Variant,
};

/// A pattern that differs per texel and per `seed`, with partial alpha.
fn pattern(w: u32, h: u32, seed: u8) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        let (x, y) = (x as u8, y as u8);
        Rgba([
            x.wrapping_mul(29).wrapping_add(seed.wrapping_mul(71)),
            y.wrapping_mul(37).wrapping_add(seed),
            x.wrapping_add(y).wrapping_mul(13) ^ seed,
            128 + (x ^ y ^ seed) % 128,
        ])
    })
}

fn premultiply([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    let p = |c: u8| ((u32::from(c) * u32::from(a) + 127) / 255) as u8;
    [p(r), p(g), p(b), a]
}

/// What a sheet asks for, in its own order.
enum Want {
    /// A file's rectangle cut into a grid.
    Cut {
        name: &'static str,
        rect: Option<[u32; 4]>,
        grid: [u16; 2],
    },
    /// A generated colour.
    Solid([u32; 2], [u8; 4]),
}

fn whole_of(name: &'static str) -> Want {
    Want::Cut {
        name,
        rect: None,
        grid: [1, 1],
    }
}

/// A sheet of `wants` under `root`, its files admitted in the order they
/// are first named.
fn sheet(root: &Path, wants: &[Want]) -> Sheet {
    let mut b = SheetBuilder::new(&Root::new(root).unwrap());
    let mut admitted: Vec<(&str, u32)> = Vec::new();
    for want in wants {
        match *want {
            Want::Cut { name, rect, grid } => {
                let file = match admitted.iter().find(|(n, _)| *n == name) {
                    Some(&(_, f)) => f,
                    None => {
                        let f = b.admit_file(file(name)).unwrap();
                        admitted.push((name, f));
                        f
                    }
                };
                b.add_image(ImageDef {
                    rect,
                    grid,
                    ..whole(file)
                })
                .unwrap();
            }
            Want::Solid(size, color) => {
                b.add_solid_image(SolidImageDef {
                    size,
                    color,
                    filter: skin_model::Filter::Linear,
                })
                .unwrap();
            }
        }
    }
    b.finish()
}

/// Each frame's expected texels, straight, in `wants`' frame order: a
/// file's rectangle read from `root` (its version at `scale`), cut row by
/// row, or the colour.
fn expected(root: &Path, wants: &[Want], scale: u32, hi: &[&str]) -> Vec<RgbaImage> {
    let mut out = Vec::new();
    for want in wants {
        match *want {
            Want::Cut { name, rect, grid } => {
                let k = if hi.contains(&name) { scale } else { 1 };
                let read = if k == 2 {
                    name.replace(".png", "@2x.png")
                } else {
                    name.to_owned()
                };
                let img = image::open(root.join(read)).unwrap().to_rgba8();
                let [x, y, w, h] = rect.unwrap_or([0, 0, img.width() / k, img.height() / k]);
                let (fw, fh) = (w / u32::from(grid[0]), h / u32::from(grid[1]));
                for r in 0..u32::from(grid[1]) {
                    for c in 0..u32::from(grid[0]) {
                        let at = [(x + c * fw) * k, (y + r * fh) * k, fw * k, fh * k];
                        out.push(
                            image::imageops::crop_imm(&img, at[0], at[1], at[2], at[3]).to_image(),
                        );
                    }
                }
            }
            Want::Solid([w, h], color) => out.push(RgbaImage::from_pixel(w, h, Rgba(color))),
        }
    }
    out
}

/// Every frame of `atlas`, sampled from its pages at its uv, equals the
/// expected texels premultiplied.
fn assert_content(atlas: &Atlas, want: &[RgbaImage], reserved: u16) {
    assert_eq!(atlas.uvs().frames().len(), want.len());
    for (frame, img) in (0_u32..).zip(want) {
        let (r, page) = content(atlas, frame, reserved);
        assert_eq!(r[2..], [img.width(), img.height()], "frame {frame}");
        for (x, y, p) in img.enumerate_pixels() {
            assert_eq!(
                texel(atlas, page, 0, r[0] + x, r[1] + y),
                premultiply(p.0),
                "frame {frame} at ({x}, {y})"
            );
        }
    }
}

fn build(sheet: &Sheet, variant: Variant, limits: AtlasLimits, text: bool) -> Atlas {
    sheet
        .atlas_reserving(variant, limits, text, &|| false)
        .unwrap()
}

/// Two files of equal dimensions, a grid of a third and a colour; the new
/// sheet admits the files in another order, names the images in another
/// order and repeats one. Each frame is drawn from its own file's cell,
/// on the same pages, under the new sheet's id.
#[test]
fn a_rebind_draws_each_frame_from_its_own_cell_in_the_new_order() {
    let d = dir();
    save(d.path(), "a.png", &pattern(12, 10, 1));
    save(d.path(), "b.png", &pattern(12, 10, 2));
    save(d.path(), "c.png", &pattern(24, 10, 3));
    let base = [
        whole_of("a.png"),
        whole_of("b.png"),
        Want::Cut {
            name: "c.png",
            rect: None,
            grid: [2, 1],
        },
        Want::Solid([6, 5], [10, 200, 30, 100]),
    ];
    let built = build(&sheet(d.path(), &base), Variant::Lo, LIMITS, false);
    let again = [
        Want::Cut {
            name: "c.png",
            rect: Some([12, 0, 12, 10]),
            grid: [1, 1],
        },
        whole_of("b.png"),
        Want::Solid([6, 5], [10, 200, 30, 100]),
        whole_of("a.png"),
        Want::Cut {
            name: "c.png",
            rect: None,
            grid: [2, 1],
        },
        whole_of("b.png"),
    ];
    let next = sheet(d.path(), &again);
    let rebound = built.rebind(&next, Variant::Lo, false).unwrap();
    assert_eq!(rebound.uvs().sheet_id(), next.id());
    assert_ne!(next.id(), built.uvs().sheet_id());
    assert!(Arc::ptr_eq(rebound.shared_pages(), built.shared_pages()));
    assert_content(&rebound, &expected(d.path(), &again, 1, &[]), 1);
    // The base's own table draws its own frames too.
    assert_content(&built, &expected(d.path(), &base, 1, &[]), 1);
}

/// A subset whose cells all lie past the first page is rebound to that
/// page: no frame of it lies on the first skin layer.
#[test]
fn a_subset_on_a_later_page_is_rebound() {
    let d = dir();
    let names = ["p0.png", "p1.png", "p2.png"];
    // 200 texels with their gutters fill a page of 256: one file a page.
    for (seed, name) in (10_u8..).zip(names) {
        save(d.path(), name, &pattern(200, 200, seed));
    }
    let base: Vec<Want> = names.iter().map(|&n| whole_of(n)).collect();
    let built = build(&sheet(d.path(), &base), Variant::Lo, LIMITS, false);
    assert_eq!(built.pages().len(), 3);
    let last = built.uvs().frames()[2].layer;
    assert!(last > 1, "the last file on layer {last}");
    let subset = [whole_of("p2.png")];
    let rebound = built
        .rebind(&sheet(d.path(), &subset), Variant::Lo, false)
        .unwrap();
    assert_eq!(rebound.uvs().frames()[0].layer, last);
    assert_content(&rebound, &expected(d.path(), &subset, 1, &[]), 1);
}

/// A @2x atlas rebinds a 1x rectangle to the @2x file's doubled one.
#[test]
fn a_hi_rebind_reads_the_hi_cells() {
    let d = dir();
    save(d.path(), "a.png", &pattern(8, 6, 4));
    save(d.path(), "a@2x.png", &pattern(16, 12, 5));
    save(d.path(), "b.png", &pattern(8, 6, 6));
    let hi_sheet = |wants: &[Want]| {
        let mut b = SheetBuilder::new(&Root::new(d.path()).unwrap());
        let mut a = file("a.png");
        a.hi = Some("a@2x.png".into());
        let fa = b.admit_file(a).unwrap();
        let fb = b.admit_file(file("b.png")).unwrap();
        for want in wants {
            let Want::Cut { name, rect, grid } = *want else {
                unreachable!()
            };
            let f = if name == "a.png" { fa } else { fb };
            b.add_image(ImageDef {
                rect,
                grid,
                ..whole(f)
            })
            .unwrap();
        }
        b.finish()
    };
    let base = [
        Want::Cut {
            name: "a.png",
            rect: None,
            grid: [2, 2],
        },
        whole_of("b.png"),
    ];
    let built = build(&hi_sheet(&base), Variant::Hi, LIMITS, false);
    let again = [
        whole_of("b.png"),
        Want::Cut {
            name: "a.png",
            rect: Some([4, 3, 4, 3]),
            grid: [1, 1],
        },
    ];
    let next = hi_sheet(&again);
    let rebound = built.rebind(&next, Variant::Hi, false).unwrap();
    assert_content(&rebound, &expected(d.path(), &again, 2, &["a.png"]), 1);
    // The 1x variant of the same sheet is not this atlas's.
    assert!(matches!(
        built.rebind(&next, Variant::Lo, false),
        Err(AtlasError::NotHeld)
    ));
}

/// A rebind is refused for a rectangle or a file the atlas did not pack,
/// another variant, another text reservation, another skin folder holding
/// the same files, and a file whose header changed since the build.
#[test]
fn a_rebind_refuses_what_the_atlas_does_not_hold() {
    let d = dir();
    save(d.path(), "a.png", &pattern(12, 10, 7));
    save(d.path(), "b.png", &pattern(12, 10, 8));
    let base = [
        Want::Cut {
            name: "a.png",
            rect: Some([0, 0, 6, 10]),
            grid: [1, 1],
        },
        Want::Solid([4, 4], [1, 2, 3, 4]),
    ];
    let built = build(&sheet(d.path(), &base), Variant::Lo, LIMITS, false);
    let texted = build(&sheet(d.path(), &base), Variant::Lo, LIMITS_TEXT, true);
    let refused = |a: &Atlas, wants: &[Want], variant, text| {
        matches!(
            a.rebind(&sheet(d.path(), wants), variant, text),
            Err(AtlasError::NotHeld)
        )
    };
    // The same cells are held, in either reservation.
    assert!(
        built
            .rebind(&sheet(d.path(), &base), Variant::Lo, false)
            .is_ok()
    );
    assert!(
        texted
            .rebind(&sheet(d.path(), &base), Variant::Lo, true)
            .is_ok()
    );
    let other_rect = [Want::Cut {
        name: "a.png",
        rect: Some([6, 0, 6, 10]),
        grid: [1, 1],
    }];
    assert!(refused(&built, &other_rect, Variant::Lo, false));
    assert!(refused(&built, &[whole_of("b.png")], Variant::Lo, false));
    assert!(refused(
        &built,
        &[Want::Solid([4, 4], [1, 2, 3, 5])],
        Variant::Lo,
        false
    ));
    assert!(refused(&built, &base, Variant::Hi, false));
    assert!(refused(&built, &base, Variant::Lo, true));
    assert!(refused(&texted, &base, Variant::Lo, false));
    // The same file under another folder.
    let e = dir();
    save(e.path(), "a.png", &pattern(12, 10, 7));
    assert!(matches!(
        built.rebind(&sheet(e.path(), &base), Variant::Lo, false),
        Err(AtlasError::NotHeld)
    ));
    // The file rewritten at other dimensions: another identity.
    save(d.path(), "a.png", &pattern(14, 10, 7));
    assert!(refused(&built, &base, Variant::Lo, false));
}

/// Room for a text layer: its pages at least 2 048.
const LIMITS_TEXT: AtlasLimits = AtlasLimits {
    max_side: 2_048,
    ..LIMITS
};
