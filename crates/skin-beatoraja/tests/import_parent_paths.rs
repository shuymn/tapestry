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
    import(
        &mut def,
        &h,
        &Config::defaults(&h),
        skin_model::ScreenKind::Play,
    )
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
fn an_image_above_the_entry_loads_once_under_each_lexical_spelling() {
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
    } = common::lua_play(&h, &Config::defaults(h.header()), &cancel).unwrap();
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

/// `name` as JSON string text: `\` and NUL escaped.
fn esc(name: &str) -> String {
    name.replace('\\', r"\\").replace('\0', r"\u0000")
}

/// A skin folder whose `play/s.json` draws `name` as an image, with
/// files where a lenient reading of an escaping name would lead:
/// `x.png` beside the skin and inside it, `img/a.png`, `play/a.png`,
/// `play/img/a.png` and `play/srv/s/a.png`, and the junction or link
/// `link` to the folder `away` beside the skin, which holds `a.png`.
fn image_layout(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let (outer, root) = layout(&json("", &[&esc(name)]));
    png(outer.path(), "x.png", 16, 16);
    for file in [
        "x.png",
        "img/a.png",
        "play/a.png",
        "play/img/a.png",
        "play/srv/s/a.png",
    ] {
        png(&root, file, 16, 16);
    }
    png(&outer.path().join("away"), "a.png", 16, 16);
    common::link_dir(&root.join("link"), &outer.path().join("away"));
    (outer, root)
}

/// What importing image `name` from [`image_layout`] noted and admitted.
fn image_outcome(name: &str) -> (Vec<(Severity, NoteKind)>, u32) {
    let (_outer, root) = image_layout(name);
    let i = import_play(&root).unwrap();
    (notes(&i), i.report.usage.files)
}

/// The names of `cases` whose outcome is not `expected`, each with what
/// it gave: every case is checked, so each form fails on its own.
fn mismatches<T: PartialEq + std::fmt::Debug>(
    cases: &[&str],
    expected: &T,
    outcome: impl Fn(&str) -> T,
) -> Vec<String> {
    cases
        .iter()
        .filter_map(|&name| {
            let got = outcome(name);
            (got != *expected).then(|| format!("{name:?}: {got:?}"))
        })
        .collect()
}

/// The outcome of a refused image: one dropped element, no file.
fn refused_image() -> (Vec<(Severity, NoteKind)>, u32) {
    (vec![(Severity::Dropped, NoteKind::MissingAsset)], 0)
}

/// Each image name leaving the skin folder is refused as a missing
/// asset, and nothing admitted.
#[test]
fn image_names_leaving_the_skin_folder_are_refused() {
    let names = [
        // Above the folder.
        "../../x.png",
        r"..\..\x.png",
        "../play/../../x.png",
        // Rooted and UNC names, which joining to the entry's folder
        // would lead to `play/img/a.png` and `play/srv/s/a.png`.
        "/img/a.png",
        r"\img\a.png",
        r"\\srv\s\a.png",
        "//srv/s/a.png",
        // Drive, device, verbatim and stream names.
        "C:/x.png",
        "C:x.png",
        r"\\.\C:\x.png",
        "//./C:/x.png",
        r"\\?\C:\x.png",
        "//?/C:/x.png",
        r"\\.\CON",
        "../img/a.png:s",
        "a.png::$DATA",
        "../img/a.png\0",
        // A junction or link to a folder outside the skin.
        "../link/a.png",
    ];
    let bad = mismatches(&names, &refused_image(), image_outcome);
    assert!(bad.is_empty(), "{bad:#?}");
}

/// Windows device names, with extensions and from a subfolder, and
/// names ending in dots or spaces are refused as missing assets, and
/// nothing admitted.
#[cfg(windows)]
#[test]
fn windows_device_and_trailing_names_are_refused() {
    let names = [
        "CON.png",
        "con",
        "COM1.txt",
        "NUL",
        "CONIN$",
        "sub/CON.png",
        "../CON.png",
        // `play/a.png` exists.
        "a.png.",
        "a.png ",
        "a.png. .",
    ];
    let bad = mismatches(&names, &refused_image(), image_outcome);
    assert!(bad.is_empty(), "{bad:#?}");
}

/// A case alias and an 8.3 alias of the in-root `play/longfilename.png`
/// are admitted: they reach that file, inside the folder. Normalising
/// is lexical, so each alias is a cache entry of its own.
#[cfg(windows)]
#[test]
fn windows_aliases_of_a_file_inside_the_folder_are_admitted() {
    use std::os::windows::process::CommandExt as _;

    let (_outer, root) = layout(&json("", &[]));
    png(&root, "play/longfilename.png", 16, 16);
    let file = root.join("play").join("longfilename.png");
    // The file's 8.3 name, when the volume makes one; cmd takes the
    // quoted path as written.
    let out = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/C for %I in (\"{}\") do @echo %~snxI",
            file.display()
        ))
        .output()
        .unwrap();
    let short = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let mut names = vec![
        "LongFileName.PNG".to_owned(),
        "../PLAY/longfilename.png".to_owned(),
    ];
    if short.eq_ignore_ascii_case("longfilename.png") {
        eprintln!("no 8.3 name on this volume: the 8.3 case is skipped");
    } else {
        names.push(short);
    }
    let cases: Vec<&str> = names.iter().map(String::as_str).collect();
    let admitted = (Vec::new(), 1);
    let bad = mismatches(&cases, &admitted, |name| {
        write(&root, "play/s.json", json("", &[&esc(name)]).as_bytes());
        let i = import_play(&root).unwrap();
        (notes(&i), i.report.usage.files)
    });
    assert!(bad.is_empty(), "{bad:#?}");
}

/// A bitmap font leaving the skin folder, or a device name, is refused
/// as a missing asset, though a font lies where a lenient reading would
/// lead.
#[test]
fn bitmap_fonts_leaving_the_skin_folder_are_refused() {
    let mut names = vec![
        "../../f.fnt",
        "../link/f.fnt",
        "/_font/f.fnt",
        r"\\.\C:\f.fnt",
    ];
    if cfg!(windows) {
        names.extend(["nul.fnt", "CON.fnt", "f.fnt."]);
    }
    let refused = (vec![(Severity::Dropped, NoteKind::MissingAsset)], (0, 0));
    let bad = mismatches(&names, &refused, |name| {
        let fonts = format!(r#"{{"id": "t", "path": "{}"}}"#, esc(name));
        let (outer, root) = layout(&json(&fonts, &[]));
        for dir in [
            outer.path().to_owned(),
            root.clone(),
            outer.path().join("away"),
            root.join("play"),
            root.join("play/_font"),
        ] {
            fnt(&dir, "f.fnt", "f.png");
            png(&dir, "f.png", 64, 32);
        }
        common::link_dir(&root.join("link"), &outer.path().join("away"));
        let i = import_play(&root).unwrap();
        (notes(&i), (i.report.usage.fonts, i.report.usage.files))
    });
    assert!(bad.is_empty(), "{bad:#?}");
}
