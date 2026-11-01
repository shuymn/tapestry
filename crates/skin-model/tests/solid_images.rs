//! Generated images of one colour: premultiplied into their whole
//! allocation and every mip level, the same size in both variants, packed
//! beside file, @2x and font-page cells without moving their frames.

mod image_support;

use std::fs;

use image_support::{LIMITS, content, dir, save, solid, texel};
use skin_model::{
    Atlas, FileDef, Filter, GUTTER, ImageDef, MIP_LEVELS, Root, Sheet, SheetBuilder, SolidImageDef,
    Variant,
};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const WHITE: [u8; 4] = [255; 4];

fn solid_def(size: [u32; 2], color: [u8; 4]) -> SolidImageDef {
    SolidImageDef {
        size,
        color,
        filter: Filter::Nearest,
    }
}

/// Every texel of frame `frame`'s whole allocation (its content and
/// gutter) at every level is `want`.
#[track_caller]
fn allocation_is(a: &Atlas, frame: u32, want: [u8; 4]) {
    let ([x, y, w, h], page) = content(a, frame, 1);
    let (x0, y0) = (x - GUTTER, y - GUTTER);
    let (x1, y1) = (x + w + GUTTER, y + h + GUTTER);
    for k in 0..MIP_LEVELS {
        let s = 1 << k;
        for ty in (y0 / s)..(y1 / s) {
            for tx in (x0 / s)..(x1 / s) {
                assert_eq!(texel(a, page, k, tx, ty), want, "level {k} at ({tx}, {ty})");
            }
        }
    }
}

/// A sheet of an @2x file, a plain file and a bitmap font page, with
/// generated images added between them; the image indices in add order.
fn mixed() -> (tempfile::TempDir, Sheet, [u32; 6]) {
    let d = dir();
    save(d.path(), "a.png", &solid(6, 4, RED));
    save(d.path(), "a@2x.png", &solid(12, 8, BLUE));
    save(d.path(), "b.png", &solid(5, 3, GREEN));
    fs::write(
        d.path().join("f.fnt"),
        "info face=\"t\" size=8 padding=0,0,0,0\n\
         common lineHeight=8 base=6 scaleW=8 scaleH=8 pages=1\n\
         page id=0 file=\"f.png\"\n\
         char id=65 x=0 y=0 width=4 height=4 xoffset=0 yoffset=0 xadvance=4 page=0\n",
    )
    .unwrap();
    save(d.path(), "f.png", &solid(8, 8, WHITE));
    let mut b = SheetBuilder::new(&Root::new(d.path()).unwrap());
    let half = [255, 0, 0, 128];
    let s0 = b.add_solid_image(solid_def([32, 8], half)).unwrap();
    let a = b
        .admit_file(FileDef {
            path: "a.png".into(),
            hi: Some("a@2x.png".into()),
        })
        .unwrap();
    let i0 = b
        .add_image(ImageDef {
            file: a,
            rect: Some([2, 0, 3, 4]),
            grid: [1, 1],
            cycle: None,
            filter: Filter::Linear,
        })
        .unwrap();
    let s1 = b
        .add_solid_image(solid_def([3, 5], [0, 255, 0, 0]))
        .unwrap();
    let file = b
        .admit_file(FileDef {
            path: "b.png".into(),
            hi: None,
        })
        .unwrap();
    let i1 = b
        .add_image(ImageDef {
            file,
            rect: None,
            grid: [1, 1],
            cycle: None,
            filter: Filter::Linear,
        })
        .unwrap();
    b.admit_fnt("f.fnt", &mut |_| Ok(())).unwrap();
    // The same colour and size again: its own image, the same cell.
    let s2 = b.add_solid_image(solid_def([32, 8], half)).unwrap();
    // The page is image 4.
    (d, b.finish(), [s0, i0, s1, i1, 4, s2])
}

#[test]
fn generated_images_keep_their_add_order_and_colours() {
    let (_d, sheet, ids) = mixed();
    assert_eq!(ids, [0, 1, 2, 3, 4, 5]);
    assert_eq!((sheet.image_count(), sheet.frame_count()), (6, 6));
    for variant in [Variant::Lo, Variant::Hi] {
        let a = sheet.atlas(variant, LIMITS).unwrap();
        // [255, 0, 0, 128] premultiplied; a colour with no alpha is all
        // zeros; both through the gutter and every mip level.
        allocation_is(&a, 0, [128, 0, 0, 128]);
        allocation_is(&a, 2, [0; 4]);
        allocation_is(&a, 5, [128, 0, 0, 128]);
        // One size in both variants.
        assert_eq!(content(&a, 0, 1).0[2..], [32, 8], "{variant:?}");
        assert_eq!(content(&a, 2, 1).0[2..], [3, 5], "{variant:?}");
        // A repeated colour and size shares the cell.
        assert_eq!(a.uvs().get(0), a.uvs().get(5));
        // The files' frames are where their images are.
        let (r, p) = content(&a, 1, 1);
        let (want, size) = match variant {
            Variant::Lo => (RED, [3, 4]),
            Variant::Hi => (BLUE, [6, 8]),
        };
        assert_eq!(r[2..], size);
        assert_eq!(texel(&a, p, 0, r[0], r[1]), want);
        let (r, p) = content(&a, 3, 1);
        assert_eq!(r[2..], [5, 3]);
        assert_eq!(texel(&a, p, 0, r[0], r[1]), GREEN);
        let (r, p) = content(&a, 4, 1);
        assert_eq!(r[2..], [8, 8]);
        assert_eq!(texel(&a, p, 0, r[0], r[1]), WHITE);
    }
}

#[test]
fn a_sheet_of_generated_images_alone_reads_nothing() {
    // The folder is gone by the time the atlas is built: nothing is read.
    let d = dir();
    let mut b = SheetBuilder::new(&Root::new(d.path()).unwrap());
    for c in [RED, [255, 255, 0, 255], WHITE] {
        b.add_solid_image(solid_def([32, 8], c)).unwrap();
    }
    let sheet = b.finish();
    drop(d);
    for variant in [Variant::Lo, Variant::Hi] {
        let a = sheet.atlas(variant, LIMITS).unwrap();
        for (frame, c) in (0..).zip([RED, [255, 255, 0, 255], WHITE]) {
            allocation_is(&a, frame, c);
        }
    }
}
