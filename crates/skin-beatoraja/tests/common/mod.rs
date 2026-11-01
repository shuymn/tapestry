//! Skin folders written by the tests: self-made definitions only.

#![allow(dead_code)]

pub mod lanes;
pub mod play;

use std::fs;
use std::path::Path;

use skin_beatoraja::{
    Config, Header, ImportError, Imported, InputLimit, NoteKind, Report, import, read_header,
};
use tempfile::TempDir;

/// A skin folder holding `files`, each a name and its text.
pub fn skin(files: &[(&str, &str)]) -> TempDir {
    let d = tempfile::tempdir().unwrap();
    for (name, text) in files {
        write(d.path(), name, text.as_bytes());
    }
    d
}

/// `items` joined by commas: the members of a JSON list.
pub fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(",")
}

pub fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The header of `s.json` with no option enabled.
pub fn header(d: &TempDir) -> Result<Header, ImportError> {
    header_of(d.path(), "s.json")
}

pub fn header_of(root: &Path, file: &str) -> Result<Header, ImportError> {
    read_header(root, file).map(|(_, h)| h)
}

/// The option names of a header.
pub fn names(h: &Header) -> Vec<String> {
    h.options
        .iter()
        .map(|o| o.name.clone().unwrap_or_default())
        .collect()
}

/// The input bound an import stopped at.
pub fn limit(result: Result<Header, ImportError>) -> InputLimit {
    match result {
        Err(ImportError::Input { limit, .. }) => limit,
        other => panic!("{other:?}"),
    }
}

/// A play skin whose fields are `body`, a list of members.
pub fn play(body: &str) -> String {
    format!("{{\"type\": 0, {body}}}")
}

/// `text` padded with spaces to exactly `len` bytes.
pub fn padded(text: &str, len: usize) -> Vec<u8> {
    let mut b = text.as_bytes().to_vec();
    assert!(b.len() <= len);
    b.resize(len, b' ');
    b
}

/// A `w`×`h` PNG of opaque texels at `root/name`.
pub fn png(root: &Path, name: &str, w: u32, h: u32) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w8 = enc.write_header().unwrap();
    let texels = usize::try_from(w * h).unwrap();
    w8.write_image_data(&[255_u8].repeat(texels * 4)).unwrap();
}

/// A file with a font's magic number at `root/name`.
pub fn font(root: &Path, name: &str) {
    write(root, name, &[0, 1, 0, 0, 0]);
}

/// The first code of [`big_fnt`]'s glyphs: two UTF-8 bytes each.
pub const BIG_FNT_CODE: u32 = 0x100;

/// A size-4 bitmap font `name` of `glyphs` 4×4 glyph lines (codes from
/// [`BIG_FNT_CODE`], 256 to a page) and `kernings` pairs, its 64×64 pages
/// `<name>_<n>.png` written beside it, padded with blank lines to `bytes`
/// when it is shorter. With no space line it gains a synthesised one, so
/// it keeps `glyphs + 1` glyphs.
pub fn big_fnt(root: &Path, name: &str, glyphs: usize, kernings: usize, bytes: usize) {
    use std::fmt::Write as _;
    let pages = glyphs.div_ceil(256).max(1);
    let mut s = format!(
        "info face=\"big\" size=4 padding=0,0,0,0\n\
         common lineHeight=4 base=4 scaleW=64 scaleH=64 pages={pages}\n"
    );
    for p in 0..pages {
        writeln!(s, "page id={p} file=\"{name}_{p}.png\"").unwrap();
        png(root, &format!("{name}_{p}.png"), 64, 64);
    }
    writeln!(s, "chars count={glyphs}").unwrap();
    let code = |i: usize| BIG_FNT_CODE + u32::try_from(i).unwrap();
    for i in 0..glyphs {
        writeln!(
            s,
            "char id={} x={} y={} width=4 height=4 xoffset=0 yoffset=0 xadvance=4 page={}",
            code(i),
            i % 16 * 4,
            i / 16 % 16 * 4,
            i / 256
        )
        .unwrap();
    }
    writeln!(s, "kernings count={kernings}").unwrap();
    for k in 0..kernings {
        writeln!(
            s,
            "kerning first={} second={} amount=-1",
            code(k / 64),
            code(k % 64)
        )
        .unwrap();
    }
    if s.len() < bytes {
        s.push_str(&"\n".repeat(bytes - s.len()));
    }
    write(root, name, s.as_bytes());
}

/// `s.json` of 64 texts of 128 glyphs in 16 [`big_fnt`] fonts `b<n>.fnt`,
/// each padded to `bytes`: the font slots and text elements full, the
/// fonts at the page cap and, with their synthesised spaces, together at
/// the sheet's glyph and kerning caps.
pub fn bitmap_fonts(root: &Path, bytes: usize) {
    use skin_model::{MAX_FONTS, MAX_SHEET_GLYPHS, MAX_SHEET_KERNINGS, MAX_TEXT_ELEMENTS};
    for i in 0..MAX_FONTS {
        big_fnt(
            root,
            &format!("b{i}.fnt"),
            MAX_SHEET_GLYPHS / MAX_FONTS - 1,
            MAX_SHEET_KERNINGS / MAX_FONTS,
            bytes,
        );
    }
    let fonts = join((0..MAX_FONTS).map(|i| format!(r#"{{"id": "{i}", "path": "b{i}.fnt"}}"#)));
    let texts = join((0..MAX_TEXT_ELEMENTS).map(|i| {
        let line: String = (0..128)
            .map(|c| char::from_u32(BIG_FNT_CODE + u32::try_from(i * 16 + c).unwrap()).unwrap())
            .collect();
        format!(
            r#"{{"id": "t{i}", "font": "{}", "size": 8, "constantText": "{line}"}}"#,
            i % MAX_FONTS
        )
    }));
    let dsts = join(
        (0..MAX_TEXT_ELEMENTS)
            .map(|i| format!(r#"{{"id": "t{i}", "dst": [{{"w": 1000, "h": 8}}]}}"#)),
    );
    write(
        root,
        "s.json",
        hd(&format!(
            r#""font": [{fonts}], "text": [{texts}], "destination": [{dsts}]"#
        ))
        .as_bytes(),
    );
}

/// A 1920×1080 play skin whose fields are `body`.
pub fn hd(body: &str) -> String {
    format!("{{\"type\": 0, \"w\": 1920, \"h\": 1080, {body}}}")
}

/// Imports `s.json` of `root` with its header's defaults.
pub fn import_dir(root: &Path) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "s.json")?;
    import(&mut def, &h, &Config::defaults(&h))
}

/// Imports `s.json` of `root` with `edit` applied to the defaults.
pub fn import_with(root: &Path, edit: impl FnOnce(&mut Config)) -> Result<Imported, ImportError> {
    let (mut def, h) = read_header(root, "s.json")?;
    let mut config = Config::defaults(&h);
    edit(&mut config);
    import(&mut def, &h, &config)
}

/// A skin folder with `files` (name and size) as PNGs and `json` as
/// `s.json`.
pub fn skin_with(json: &str, files: &[(&str, u32, u32)]) -> TempDir {
    let d = skin(&[("s.json", json)]);
    for &(name, w, h) in files {
        png(d.path(), name, w, h);
    }
    d
}

/// An import and the folder its sheet reads at atlas time.
pub struct Loaded {
    pub dir: TempDir,
    pub result: Result<Imported, ImportError>,
}

impl Loaded {
    #[track_caller]
    pub fn ok(&self) -> &Imported {
        match &self.result {
            Ok(i) => i,
            Err(e) => panic!("{e:?}"),
        }
    }

    #[track_caller]
    pub fn err(self) -> ImportError {
        match self.result {
            Ok(i) => panic!("imported: {:?}", i.report),
            Err(e) => e,
        }
    }

    #[track_caller]
    pub fn report(&self) -> &Report {
        &self.ok().report
    }

    /// The import played over a 100-note chart, its atlas built.
    #[track_caller]
    pub fn drawn(self) -> play::Drawn {
        self.drawn_over(100)
    }

    /// The import played over a chart of `notes` notes.
    #[track_caller]
    pub fn drawn_over(self, notes: u32) -> play::Drawn {
        match self.result {
            Ok(i) => play::Drawn::new(i.skin, notes),
            Err(e) => panic!("{e:?}"),
        }
    }

    /// The import's loading state, its atlas built.
    #[track_caller]
    pub fn loading(self) -> play::Loader {
        match self.result {
            Ok(i) => play::Loader::new(i.skin),
            Err(e) => panic!("{e:?}"),
        }
    }

    /// The quads at `ms` of a 100-note play.
    #[track_caller]
    pub fn quads(self, ms: i64) -> Vec<skin_model::Quad> {
        self.drawn().frame(ms, &[])
    }

    /// The rounded rects at `ms` of a 100-note play.
    #[track_caller]
    pub fn rects(self, ms: i64) -> Vec<[i32; 4]> {
        self.drawn().rects(ms)
    }
}

/// Imports a 1920×1080 skin of `body` (see [`import_sized`]).
pub fn import_body(body: &str) -> Loaded {
    import_sized([1_920, 1_080], body, &[])
}

/// Imports a `size` skin of `body` with source `a` (a 64×64 `a.png`)
/// and source `n` (a 120×20 `n.png`), and `files` written as PNGs.
pub fn import_sized(size: [u32; 2], body: &str, files: &[(&str, u32, u32)]) -> Loaded {
    import_config(size, body, files, |_| {})
}

/// [`import_sized`] with `edit` applied to the header's defaults.
pub fn import_config(
    size: [u32; 2],
    body: &str,
    files: &[(&str, u32, u32)],
    edit: impl FnOnce(&mut Config),
) -> Loaded {
    let [w, h] = size;
    let json = format!(
        r#"{{"type": 0, "w": {w}, "h": {h},
            "source": [{{"id": "a", "path": "a.png"}}, {{"id": "n", "path": "n.png"}}], {body}}}"#
    );
    let dir = skin_with(&json, &[("a.png", 64, 64), ("n.png", 120, 20)]);
    for &(name, w, h) in files {
        png(dir.path(), name, w, h);
    }
    let result = import_with(dir.path(), edit);
    Loaded { dir, result }
}

/// Whether `r` has a note of `kind`.
pub fn has(r: &Report, kind: NoteKind) -> bool {
    r.notes.iter().any(|n| n.kind == kind)
}

/// The note kinds of a report.
pub fn kinds(r: &Report) -> Vec<NoteKind> {
    r.notes.iter().map(|n| n.kind).collect()
}

/// A destination of object `id` with `extra` fields and one key at 0.
pub fn dst(id: &str, extra: &str, key: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(r#"{{"id": "{id}"{sep}{extra}, "dst": [{{{key}}}]}}"#)
}

/// A directory junction on Windows, a symbolic link elsewhere.
pub fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}
