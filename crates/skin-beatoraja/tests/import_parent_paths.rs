//! Asset names that step up with `..` from an entry file in a subfolder:
//! resolved inside the skin folder, never above it, and every escape
//! refused as before.

mod common;

use std::path::Path;

use common::{png, write};
use skin_beatoraja::{
    Cancel, Config, ImportError, Imported, LuaImported, NoteKind, Severity, import, read_header,
    read_lua_header,
};

/// A size-24 bitmap font `name` with an `A` glyph and one 64×32 page
/// `<page>`, written relative to the font's folder.
fn fnt(dir: &Path, name: &str, page: &str) {
    let s = format!(
        "info face=\"t\" size=24 padding=0,0,0,0\n\
         common lineHeight=24 base=20 scaleW=64 scaleH=32 pages=1\n\
         page id=0 file=\"{page}\"\n\
         char id=65 x=0 y=0 width=10 height=20 xoffset=2 yoffset=3 xadvance=12 page=0 chnl=15\n"
    );
    write(dir, name, s.as_bytes());
}

/// A 1920×1080 play skin with `fonts` (the `font` array's items) and a
/// literal text in font `t`, or no text when `fonts` is empty, and
/// `sources` drawn as 16×16 images.
fn json(fonts: &str, sources: &[&str]) -> String {
    let src: Vec<String> = sources
        .iter()
        .enumerate()
        .map(|(i, p)| format!(r#"{{"id": "s{i}", "path": "{p}"}}"#))
        .collect();
    let images: Vec<String> = (0..sources.len())
        .map(|i| format!(r#"{{"id": "i{i}", "src": "s{i}", "x": 0, "y": 0, "w": 16, "h": 16}}"#))
        .collect();
    let mut dsts: Vec<String> = (0..sources.len())
        .map(|i| format!(r#"{{"id": "i{i}", "dst": [{{"x": 0, "y": 0, "w": 16, "h": 16}}]}}"#))
        .collect();
    let text = if fonts.is_empty() {
        ""
    } else {
        dsts.push(r#"{"id": "t", "dst": [{"x": 400, "y": 800, "w": 300, "h": 30}]}"#.to_owned());
        r#"{"id": "t", "font": "t", "size": 24, "constantText": "A"}"#
    };
    format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{}], "image": [{}],
            "font": [{fonts}], "text": [{text}], "destination": [{}]}}"#,
        src.join(", "),
        images.join(", "),
        dsts.join(", ")
    )
}

/// Imports `play/s.json` of `root` with its header's defaults.
fn import_play(root: &Path) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "play/s.json")?;
    import(&mut def, &h, &Config::defaults(&h))
}

/// A skin folder `skin` inside a temporary folder, with `play/s.json`
/// holding `json`; the outer folder holds what lies beside the skin.
fn layout(json: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("skin");
    write(&root, "play/s.json", json.as_bytes());
    (outer, root)
}

fn notes(i: &Imported) -> Vec<(Severity, NoteKind)> {
    i.report
        .notes
        .iter()
        .map(|n| (n.severity, n.kind))
        .collect()
}

#[test]
fn a_bitmap_font_and_its_fallback_above_the_entry_load_with_their_pages() {
    let fonts =
        r#"{"id": "t", "path": "../_font/b.fnt", "fallback": [{"path": "..\\_font/./f.fnt"}]}"#;
    let (_outer, root) = layout(&json(fonts, &[]));
    fnt(&root, "_font/b.fnt", "b.png");
    fnt(&root, "_font/f.fnt", "pages/f.png");
    png(&root, "_font/b.png", 64, 32);
    png(&root, "_font/pages/f.png", 64, 32);
    let i = import_play(&root).unwrap();
    assert!(i.report.notes.is_empty(), "{:?}", i.report.notes);
    assert_eq!(i.report.usage.fonts, 2);
    // Both pages, each read from its font's folder.
    assert_eq!(i.report.usage.files, 2);
    assert_eq!(i.report.usage.texts, 1);
}

#[test]
fn a_font_file_above_the_entry_loads() {
    let (_outer, root) = layout(&json(r#"{"id": "t", "path": "../_font/x.ttf"}"#, &[]));
    common::font(&root, "_font/x.ttf");
    let i = import_play(&root).unwrap();
    assert!(i.report.notes.is_empty(), "{:?}", i.report.notes);
    assert_eq!((i.report.usage.fonts, i.report.usage.texts), (1, 1));
}

#[test]
fn an_image_above_the_entry_loads_once_under_every_spelling() {
    // JSON text: `\\` is one backslash.
    let sources = ["../img/a.png", r"..\\img\\a.png", "../play/../img/./a.png"];
    let (_outer, root) = layout(&json("", &sources));
    png(&root, "img/a.png", 16, 16);
    let i = import_play(&root).unwrap();
    assert!(i.report.notes.is_empty(), "{:?}", i.report.notes);
    // One name, so one admitted file.
    assert_eq!(i.report.usage.files, 1);
}

#[test]
fn a_lua_entry_reads_a_script_and_an_image_above_it() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("skin");
    let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"a\", path = dofile(\"../lib/parts.lua\") } }\n\
         skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 16, h = 16 } }\n\
         skin.destination = { { id = \"i\", dst = { { x = 0, y = 0, w = 16, h = 16 } } } }\n\
         return skin\n";
    write(&root, "play/s.luaskin", entry.as_bytes());
    write(&root, "lib/parts.lua", b"return \"../img/a.png\"");
    png(&root, "img/a.png", 16, 16);
    let cancel = Cancel::new();
    let h = read_lua_header(&root, "play/s.luaskin", &cancel).unwrap();
    let LuaImported {
        imported,
        dependencies,
        ..
    } = h.import(&Config::defaults(h.header()), &cancel).unwrap();
    assert!(
        imported.report.notes.is_empty(),
        "{:?}",
        imported.report.notes
    );
    assert_eq!(imported.report.usage.files, 1);
    assert!(
        dependencies.iter().any(|d| d == "lib/parts.lua"),
        "{dependencies:?}"
    );
}

/// Each image name is refused as a missing asset, and nothing admitted,
/// though a file lies where a lenient reading of the name would lead.
#[test]
fn image_names_leaving_the_skin_folder_are_refused() {
    for name in [
        // Above the folder: `x.png` lies beside the skin and inside it.
        "../../x.png",
        r"..\\..\\x.png",
        "../play/../../x.png",
        // Rooted and UNC names: `play/img/a.png` and `play/srv/s/a.png`
        // exist, where joining them to the entry's folder would lead.
        "/img/a.png",
        "\\\\img\\\\a.png",
        "\\\\\\\\srv\\\\s\\\\a.png",
        "//srv/s/a.png",
        // Drive, verbatim and stream names.
        "C:/x.png",
        "C:x.png",
        "\\\\\\\\?\\\\C:\\\\x.png",
        "../img/a.png:s",
        "../img/a.png\\u0000",
        // A junction or link to a folder outside the skin.
        "../link/a.png",
    ] {
        let (outer, root) = layout(&json("", &[name]));
        png(outer.path(), "x.png", 16, 16);
        png(&root, "x.png", 16, 16);
        png(&root, "img/a.png", 16, 16);
        png(&root, "play/img/a.png", 16, 16);
        png(&root, "play/srv/s/a.png", 16, 16);
        png(&outer.path().join("away"), "a.png", 16, 16);
        common::link_dir(&root.join("link"), &outer.path().join("away"));
        let i = import_play(&root).unwrap();
        assert_eq!(
            notes(&i),
            [(Severity::Dropped, NoteKind::MissingAsset)],
            "{name}"
        );
        assert_eq!(i.report.usage.files, 0, "{name}");
    }
}

/// A bitmap font above the folder or behind a junction out of it is
/// refused as a missing asset, though the file exists there.
#[test]
fn bitmap_fonts_leaving_the_skin_folder_are_refused() {
    for name in ["../../f.fnt", "../link/f.fnt", "/_font/f.fnt"] {
        let fonts = format!(r#"{{"id": "t", "path": "{name}"}}"#);
        let (outer, root) = layout(&json(&fonts, &[]));
        for dir in [
            outer.path().to_owned(),
            root.clone(),
            outer.path().join("away"),
            root.join("play/_font"),
        ] {
            fnt(&dir, "f.fnt", "f.png");
            png(&dir, "f.png", 64, 32);
        }
        common::link_dir(&root.join("link"), &outer.path().join("away"));
        let i = import_play(&root).unwrap();
        assert_eq!(
            notes(&i),
            [(Severity::Dropped, NoteKind::MissingAsset)],
            "{name}"
        );
        assert_eq!(
            (i.report.usage.fonts, i.report.usage.files),
            (0, 0),
            "{name}"
        );
    }
}
