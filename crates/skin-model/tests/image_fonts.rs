//! A sheet's font files: each check reaching its bound.

use std::fs::File;
use std::path::Path;

use skin_model::{
    FontAdmitError, FontDef, FontError, MAX_FILE_BYTES, MAX_FONT_BYTES, MAX_FONTS,
    MAX_TOTAL_FONT_BYTES, Root, Sheet, SheetBuilder,
};

/// Writes `name` in `dir`: `head`, then zeros to `len` bytes (sparse).
fn write(dir: &Path, name: &str, head: &[u8], len: u64) {
    std::fs::write(dir.join(name), head).unwrap();
    File::options()
        .write(true)
        .open(dir.join(name))
        .unwrap()
        .set_len(len)
        .unwrap();
}

fn fonts(names: &[&str]) -> Vec<FontDef> {
    names
        .iter()
        .map(|&n| FontDef { path: n.to_owned() })
        .collect()
}

fn with(dir: &Path, names: &[&str]) -> Result<Sheet, FontError> {
    Sheet::new(dir, Vec::new(), Vec::new())
        .unwrap()
        .with_fonts(fonts(names))
}

const TRUETYPE: [u8; 4] = [0, 1, 0, 0];

#[test]
fn a_sheet_names_at_most_16_fonts() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.ttf", &TRUETYPE, 4);
    let names = vec!["a.ttf"; MAX_FONTS];
    assert_eq!(with(d.path(), &names).unwrap().font_count(), 16);
    let names = vec!["a.ttf"; MAX_FONTS + 1];
    let err = with(d.path(), &names).unwrap_err();
    assert!(matches!(err, FontError::TooManyFonts { n: 17 }), "{err:?}");
    // No fonts on a sheet without a folder, but an empty list is fine.
    assert_eq!(
        Sheet::empty().with_fonts(Vec::new()).unwrap().font_count(),
        0
    );
    let err = Sheet::empty().with_fonts(fonts(&["a.ttf"])).unwrap_err();
    assert!(matches!(err, FontError::NoFolder), "{err:?}");
}

#[test]
fn a_font_is_at_most_32_mib_and_the_fonts_64_mib() {
    assert_eq!(MAX_FONT_BYTES, 32 << 20);
    assert_eq!(MAX_TOTAL_FONT_BYTES, 64 << 20);
    // The fonts' cap is their own, below an image file's.
    const { assert!(MAX_FONT_BYTES < MAX_FILE_BYTES) };
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "cap.ttf", &TRUETYPE, MAX_FONT_BYTES);
    write(d.path(), "over.ttf", &TRUETYPE, MAX_FONT_BYTES + 1);
    write(d.path(), "image.ttf", &TRUETYPE, MAX_FILE_BYTES);
    let err = with(d.path(), &["image.ttf"]).unwrap_err();
    assert!(matches!(err, FontError::TooLarge { font: 0 }), "{err:?}");
    // A builder's admission checks the same cap.
    let mut b = SheetBuilder::new(&Root::new(d.path()).unwrap());
    assert_eq!(
        b.admit_font(FontDef {
            path: "cap.ttf".into()
        })
        .unwrap(),
        0
    );
    for name in ["over.ttf", "image.ttf"] {
        let err = b.admit_font(FontDef { path: name.into() }).unwrap_err();
        assert!(
            matches!(err, FontAdmitError::File(FontError::TooLarge { font: 1 })),
            "{name}: {err:?}"
        );
    }
    write(d.path(), "small.ttf", &TRUETYPE, 4);
    write(d.path(), "cap2.ttf", &TRUETYPE, MAX_FONT_BYTES);
    with(d.path(), &["cap.ttf"]).unwrap();
    let err = with(d.path(), &["small.ttf", "over.ttf"]).unwrap_err();
    assert!(matches!(err, FontError::TooLarge { font: 1 }), "{err:?}");
    with(d.path(), &["cap.ttf", "cap2.ttf"]).unwrap();
    let err = with(d.path(), &["cap.ttf", "cap2.ttf", "small.ttf"]).unwrap_err();
    assert!(
        matches!(err, FontError::TotalTooLarge { font: 2 }),
        "{err:?}"
    );
}

#[test]
fn fonts_are_truetype_opentype_or_collections() {
    let d = tempfile::tempdir().unwrap();
    for (name, head) in [
        ("tt.ttf", TRUETYPE),
        ("otto.otf", *b"OTTO"),
        ("true.ttf", *b"true"),
        ("ttcf.ttc", *b"ttcf"),
        ("woff.woff", *b"wOFF"),
    ] {
        write(d.path(), name, &head, 64);
    }
    write(d.path(), "short.ttf", &TRUETYPE[..3], 3);
    let ok = ["tt.ttf", "otto.otf", "true.ttf", "ttcf.ttc"];
    assert_eq!(with(d.path(), &ok).unwrap().font_count(), 4);
    for (bad, font) in [("woff.woff", 4), ("short.ttf", 4)] {
        let mut names = ok.to_vec();
        names.push(bad);
        let err = with(d.path(), &names).unwrap_err();
        assert!(
            matches!(err, FontError::Unsupported { font: f } if f == font),
            "{bad}: {err:?}"
        );
    }
    // The first bytes are read only after every path and size passed.
    let err = with(d.path(), &["woff.woff", "missing.ttf"]).unwrap_err();
    assert!(matches!(err, FontError::Missing { font: 1 }), "{err:?}");
}

#[test]
fn fonts_stay_in_the_skin_folder() {
    let outer = tempfile::tempdir().unwrap();
    let skin = outer.path().join("skin");
    std::fs::create_dir(&skin).unwrap();
    write(outer.path(), "x.ttf", &TRUETYPE, 4);
    std::fs::create_dir(skin.join("dir.ttf")).unwrap();
    let err = with(&skin, &["../x.ttf"]).unwrap_err();
    assert!(
        matches!(err, FontError::PathRejected { font: 0 }),
        "{err:?}"
    );
    let err = with(&skin, &["dir.ttf"]).unwrap_err();
    assert!(matches!(err, FontError::NotAFile { font: 0 }), "{err:?}");
    let err = with(&skin, &["none.ttf"]).unwrap_err();
    assert!(matches!(err, FontError::Missing { font: 0 }), "{err:?}");
}

#[path = "image_support/mod.rs"]
mod image_support;

#[test]
fn font_reader_budgets() {
    use skin_model::FontReader;
    let d = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..MAX_FONTS).map(|i| format!("f{i}.ttf")).collect();
    for n in &names {
        write(d.path(), n, &TRUETYPE, 4);
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let sheet = with(d.path(), &refs).unwrap();
    // Grown after load to 32 MiB each: two fill the budget by the bytes
    // read, and the third is past what is left.
    for n in &names {
        write(d.path(), n, &TRUETYPE, MAX_FONT_BYTES);
    }
    let mut reader = FontReader::new(&sheet);
    assert_eq!(reader.read(0).unwrap().len() as u64, MAX_FONT_BYTES);
    assert_eq!(reader.remaining(), MAX_TOTAL_FONT_BYTES - MAX_FONT_BYTES);
    reader.read(1).unwrap();
    assert_eq!(reader.remaining(), 0);
    let err = reader.read(2).unwrap_err();
    assert!(
        matches!(err, FontError::TotalTooLarge { font: 2 }),
        "{err:?}"
    );
    // Each file's own cap holds even with the budget left, an image
    // file's length included.
    for len in [MAX_FONT_BYTES + 1, MAX_FILE_BYTES] {
        write(d.path(), "f3.ttf", &TRUETYPE, len);
        let err = FontReader::new(&sheet).read(3).unwrap_err();
        assert!(matches!(err, FontError::TooLarge { font: 3 }), "{err:?}");
    }
    let err = FontReader::new(&sheet).read(16).unwrap_err();
    assert!(matches!(err, FontError::Unknown { font: 16 }), "{err:?}");
}

#[test]
fn font_reader_checks_the_files_again() {
    use skin_model::FontReader;
    let outer = tempfile::tempdir().unwrap();
    let skin = outer.path().join("skin");
    std::fs::create_dir_all(skin.join("sub")).unwrap();
    std::fs::create_dir(outer.path().join("away")).unwrap();
    write(&skin, "a.ttf", &TRUETYPE, 8);
    write(&skin, "b.ttf", b"OTTO", 8);
    write(&skin.join("sub"), "c.ttf", &TRUETYPE, 8);
    write(&outer.path().join("away"), "c.ttf", &TRUETYPE, 8);
    let sheet = with(&skin, &["a.ttf", "b.ttf", "sub/c.ttf"]).unwrap();
    // As loaded, each reads.
    let mut reader = FontReader::new(&sheet);
    for f in 0..3 {
        assert_eq!(reader.read(f).unwrap().len(), 8);
    }
    // A file swapped for a folder, a magic changed, a folder swapped for
    // a link out of the skin.
    std::fs::remove_file(skin.join("a.ttf")).unwrap();
    std::fs::create_dir(skin.join("a.ttf")).unwrap();
    write(&skin, "b.ttf", b"wOFF", 8);
    std::fs::remove_dir_all(skin.join("sub")).unwrap();
    image_support::link_dir(&skin.join("sub"), &outer.path().join("away"));
    let mut reader = FontReader::new(&sheet);
    let err = reader.read(0).unwrap_err();
    assert!(matches!(err, FontError::NotAFile { font: 0 }), "{err:?}");
    let err = reader.read(1).unwrap_err();
    assert!(matches!(err, FontError::Unsupported { font: 1 }), "{err:?}");
    let err = reader.read(2).unwrap_err();
    assert!(matches!(err, FontError::PathEscapes { font: 2 }), "{err:?}");
    // The bytes read are charged, a refused file's included.
    assert_eq!(reader.remaining(), MAX_TOTAL_FONT_BYTES - 8);
}
