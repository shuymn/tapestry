//! A sheet union's asks (E16's union case): after every batch of its
//! units, a cell inserted or a file compared, and nowhere else.

use std::cell::{Cell as Counter, RefCell};
use std::path::Path;

use ::image::{ImageFormat, Rgba, RgbaImage};

use super::decode::{SITE, Site};
use super::*;
use crate::atlas::AtlasError;
use crate::atlas::pack::{PLAN_BATCH, UNION_UNITS};
use crate::quad::Filter;
use crate::{Root, SheetBuilder};

/// The files both sheets cut.
const FILES: u32 = 40;

fn png(dir: &Path, name: &str) {
    RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]))
        .save_with_format(dir.join(name), ImageFormat::Png)
        .unwrap();
}

/// A sheet of `names` in that order, each cut into `grid`, and one
/// generated cell of `solid` colour.
fn sheet(dir: &Path, names: &[String], grid: [u16; 2], solid: u8) -> Sheet {
    let mut b = SheetBuilder::new(&Root::new(dir).unwrap());
    for name in names {
        let file = b
            .admit_file(FileDef {
                path: name.clone(),
                hi: None,
            })
            .unwrap();
        b.add_image(ImageDef {
            file,
            rect: None,
            grid,
            cycle: None,
            filter: Filter::Nearest,
        })
        .unwrap();
    }
    b.add_solid_image(SolidImageDef {
        size: [4, 4],
        color: [solid, 0, 0, 255],
        filter: Filter::Nearest,
        grid: None,
    })
    .unwrap();
    b.finish()
}

/// Two sheets of one folder: the first every shared file in order, cut
/// 2 × 2; the second the shared files in reverse, cut whole, and a file
/// of its own.
fn sheets() -> (tempfile::TempDir, Sheet, Sheet) {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..FILES).map(|f| format!("f{f}.png")).collect();
    for n in &names {
        png(dir.path(), n);
    }
    png(dir.path(), "own.png");
    let a = sheet(dir.path(), &names, [2, 2], 1);
    let mut rev: Vec<String> = names.iter().rev().cloned().collect();
    rev.push("own.png".to_owned());
    let b = sheet(dir.path(), &rev, [1, 1], 2);
    (dir, a, b)
}

/// The union's units counted from the sheets alone: each of `a`'s cells
/// inserted; for each of `b`'s file cells, the files compared until its
/// match in `a`'s list or past its end (a new file joins it), then its
/// insert; one insert for each generated cell.
fn expected_units(a: &Sheet, b: &Sheet) -> u64 {
    let mut files: Vec<&File> = a.files.iter().collect();
    let mut units = a.cells.len() as u64;
    for c in b.cells.iter() {
        if let CellSource::File(f) = c.source {
            let file = &b.files[f as usize];
            match files
                .iter()
                .position(|g| g.lo == file.lo && g.hi == file.hi)
            {
                Some(i) => units += i as u64 + 1,
                None => {
                    units += files.len() as u64;
                    files.push(file);
                }
            }
        }
        units += 1;
    }
    units
}

/// Each ask's site and the units counted then.
type Asks = Vec<(Site, u64)>;

/// Unions `a` and `b` asking a cancel that stops at its `stop`th ask
/// (1-based; 0 never), recording each ask's site and the units then.
fn union(a: &Sheet, b: &Sheet, stop: usize) -> (Result<Option<Sheet>, AtlasError>, Asks) {
    UNION_UNITS.with(|c| c.set(0));
    let asks = RefCell::new(Vec::new());
    let cancel = || {
        let mut asks = asks.borrow_mut();
        asks.push((
            SITE.with(Counter::get).unwrap(),
            UNION_UNITS.with(Counter::get),
        ));
        asks.len() == stop
    };
    let r = a.union_checked(b, &cancel);
    (r, asks.into_inner())
}

#[test]
fn a_union_asks_after_every_batch_of_its_units() {
    let (_dir, a, b) = sheets();
    let (r, asks) = union(&a, &b, 0);
    let u = r.unwrap().unwrap();
    let total = UNION_UNITS.with(Counter::get);

    // Positive control: the units the sheets imply, more than three
    // batches, most of them file comparisons.
    let expected = expected_units(&a, &b);
    assert_eq!(total, expected, "control");
    assert!(total > 3 * PLAN_BATCH as u64, "control: {total}");

    // Every batch asks, at the union's site, and no unit runs past a
    // batch unasked.
    assert_eq!(
        asks.len() as u64,
        total / PLAN_BATCH as u64,
        "asks: {asks:?}"
    );
    for (k, &(site, units)) in asks.iter().enumerate() {
        assert_eq!(site, Site::UnionWork, "site at {k}");
        assert_eq!(units, (k as u64 + 1) * PLAN_BATCH as u64, "bound at {k}");
    }

    // The union is the never-asking one's.
    let plain = a.union(&b).unwrap();
    assert_eq!(
        format!("{:?}", u.cells),
        format!("{:?}", plain.cells),
        "same"
    );
    assert_eq!(u.files.len(), plain.files.len(), "same");
    assert_eq!(u32::try_from(u.files.len()).unwrap(), FILES + 1, "same");

    // A stop at any ask ends the union cancelled, with no unit after it.
    for k in 1..=asks.len() {
        let (r, seen) = union(&a, &b, k);
        assert!(
            matches!(r, Err(AtlasError::Cancelled)),
            "stop at {k}: {r:?}"
        );
        assert_eq!(seen.len(), k, "stop at {k}");
        assert_eq!(
            UNION_UNITS.with(Counter::get),
            (k * PLAN_BATCH) as u64,
            "stop at {k}"
        );
    }
}

#[test]
fn a_union_of_another_folder_is_none_unasked() {
    let (_dir, a, _) = sheets();
    let (_other, _, b) = sheets();
    let (r, asks) = union(&a, &b, 1);
    assert!(matches!(r, Ok(None)), "{r:?}");
    assert!(asks.is_empty(), "{asks:?}");
}
