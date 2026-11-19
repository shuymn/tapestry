//! The transition probe's `dev: probe-miss` (dev-tools): each refusal of an
//! atlas's rebind is named by `PackedPages::not_held_why` as the check that
//! made it (the pages' variant, kept layers or skin folder; a file not
//! held; a cell not held), and it names none where the rebind succeeds.
#![cfg(feature = "dev-tools")]

mod image_support;

use std::path::Path;

use image::{Rgba, RgbaImage};
use image_support::{LIMITS, dir, file, save, whole};
use skin_model::{
    Atlas, AtlasLimits, ImageDef, KeptLayers, NotHeldWhy, Root, Sheet, SheetBuilder, SolidImageDef,
    Variant,
};

fn pattern(w: u32, h: u32, seed: u8) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| Rgba([x as u8 ^ seed, y as u8, seed, 200]))
}

/// A sheet of the files' rectangles (`None`: the whole file) and a colour
/// cell of `solid`.
fn sheet(root: &Path, cuts: &[(&str, Option<[u32; 4]>)], solid: Option<[u8; 4]>) -> Sheet {
    let mut b = SheetBuilder::new(&Root::new(root).unwrap());
    for &(name, rect) in cuts {
        let f = b.admit_file(file(name)).unwrap();
        b.add_image(ImageDef { rect, ..whole(f) }).unwrap();
    }
    if let Some(color) = solid {
        b.add_solid_image(SolidImageDef {
            size: [4, 4],
            color,
            filter: skin_model::Filter::Linear,
            grid: None,
        })
        .unwrap();
    }
    b.finish()
}

/// Room for a text layer.
const LIMITS_TEXT: AtlasLimits = AtlasLimits {
    max_side: 2_048,
    ..LIMITS
};

/// `not_held_why` names a refusal exactly when the rebind refuses.
fn why(atlas: &Atlas, sheet: &Sheet, variant: Variant, kept: KeptLayers) -> Option<NotHeldWhy> {
    let why = atlas.shared_pages().not_held_why(sheet, variant, kept);
    assert_eq!(
        why.is_none(),
        atlas.rebind_keeping(sheet, variant, kept).is_ok(),
        "{why:?}"
    );
    why
}

#[test]
fn each_refusal_of_a_rebind_is_named() {
    let d = dir();
    save(d.path(), "a.png", &pattern(12, 10, 1));
    save(d.path(), "b.png", &pattern(12, 10, 2));
    let base = sheet(
        d.path(),
        &[("a.png", Some([0, 0, 6, 10]))],
        Some([1, 2, 3, 4]),
    );
    let built = base
        .atlas_reserving(Variant::Lo, LIMITS_TEXT, false, &|| false)
        .unwrap();
    let none = KeptLayers::NONE;
    // Held: no refusal.
    assert_eq!(why(&built, &base, Variant::Lo, none), None);
    assert!(
        built
            .shared_pages()
            .root()
            .ends_with(d.path().file_name().unwrap())
    );
    // The header: each of variant, kept layers and folder apart.
    assert_eq!(
        why(&built, &base, Variant::Hi, none),
        Some(NotHeldWhy::Header {
            variant: true,
            kept: false,
            root: false
        })
    );
    assert_eq!(
        why(&built, &base, Variant::Lo, KeptLayers::text(true)),
        Some(NotHeldWhy::Header {
            variant: false,
            kept: true,
            root: false
        })
    );
    let e = dir();
    save(e.path(), "a.png", &pattern(12, 10, 1));
    let elsewhere = sheet(
        e.path(),
        &[("a.png", Some([0, 0, 6, 10]))],
        Some([1, 2, 3, 4]),
    );
    assert_eq!(
        why(&built, &elsewhere, Variant::Lo, none),
        Some(NotHeldWhy::Header {
            variant: false,
            kept: false,
            root: true
        })
    );
    // A file not held.
    assert_eq!(
        why(
            &built,
            &sheet(d.path(), &[("b.png", None)], None),
            Variant::Lo,
            none
        ),
        Some(NotHeldWhy::File("b.png".to_owned()))
    );
    // A cell not held: another rectangle of a held file, another colour.
    assert_eq!(
        why(
            &built,
            &sheet(d.path(), &[("a.png", Some([6, 0, 6, 10]))], None),
            Variant::Lo,
            none
        ),
        Some(NotHeldWhy::Cell("a.png".to_owned(), [6, 0, 6, 10]))
    );
    assert!(matches!(
        why(
            &built,
            &sheet(d.path(), &[], Some([1, 2, 3, 5])),
            Variant::Lo,
            none
        ),
        Some(NotHeldWhy::Cell(_, [0, 0, 4, 4]))
    ));
    // A file rewritten at other dimensions: another identity, not held.
    save(d.path(), "a.png", &pattern(14, 10, 1));
    assert_eq!(
        why(
            &built,
            &sheet(d.path(), &[("a.png", Some([0, 0, 6, 10]))], None),
            Variant::Lo,
            none
        ),
        Some(NotHeldWhy::File("a.png".to_owned()))
    );
}
