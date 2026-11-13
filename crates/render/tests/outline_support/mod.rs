//! Self-made imported skins with rasterized text, for the producer and
//! renderer tests: a JSON definition naming generated TrueType fonts
//! ([`crate::font_support`]) and distance-field `.fnt` records (D13), its
//! texts drawn over a black canvas, through the real importer
//! (`read_header`, `Config::defaults`, `import`), the font files read by a
//! `FontReader`, the producer built from them and a generated default
//! font, and the atlas built with its text layer and bundled as production
//! bundles are.

#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use render::{
    AttemptText, ClipTable, DefaultFont, OutlineError, OutlineText, Renderer, SkinBundle,
};
use skin_model::{FontReader, Layout, Side, Skin};
use tempfile::TempDir;

use super::font_support::{self, Spec};

/// The default font's family in the tests.
pub const DEFAULT_FAMILY: &str = "Test Default";

/// The generated default font: [`font_support::cjk`] with a cap height of
/// 600, so its lines sit lower than a 700-cap face's, and a `.notdef` of
/// 800 × 600, unlike a Latin face's.
pub fn default_spec() -> Spec {
    let mut s = font_support::cjk(DEFAULT_FAMILY);
    s.os2 = font_support::Os2::V4 { cap_height: 600 };
    s.glyphs[0].rect = Some([50, 0, 850, 600]);
    s
}

/// The generated default font, loaded as the runtime one is.
pub fn default_font() -> Arc<DefaultFont> {
    Arc::new(DefaultFont::from_data(font_support::build(&default_spec()), DEFAULT_FAMILY).unwrap())
}

/// What a text shows.
#[derive(Clone, Debug)]
pub enum Value {
    /// The chart field the source's `ref` names (10 the title).
    Ref(i32),
    Literal(String),
}

/// A text of the definition: its font record's id, size and box,
/// `[x, y, w, h]` in canvas pixels, y down.
#[derive(Clone, Debug)]
pub struct TextSpec {
    pub font: &'static str,
    pub size: i32,
    pub value: Value,
    pub rect: [i32; 4],
}

/// A font record: its id and file, and its `type` (1 a distance-field
/// font, D13).
#[derive(Clone, Debug)]
pub struct FontRec {
    pub id: &'static str,
    pub path: &'static str,
    pub kind: Option<i32>,
}

/// The skin's folder: `files` written in it, and a definition with
/// `fonts` and `texts`.
pub fn folder(files: &[(&str, Vec<u8>)], fonts: &[FontRec], texts: &[TextSpec]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for (name, bytes) in files {
        fs::write(d.join(name), bytes).unwrap();
    }
    let fonts: Vec<String> = fonts
        .iter()
        .map(|f| match f.kind {
            Some(k) => format!(r#"{{"id": "{}", "path": "{}", "type": {k}}}"#, f.id, f.path),
            None => format!(r#"{{"id": "{}", "path": "{}"}}"#, f.id, f.path),
        })
        .collect();
    let records: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let value = match &t.value {
                Value::Ref(r) => format!(r#""ref": {r}"#),
                Value::Literal(s) => format!(r#""constantText": "{s}""#),
            };
            format!(
                r#"{{"id": "t{i}", "font": "{}", "size": {}, {value}}}"#,
                t.font, t.size
            )
        })
        .collect();
    let destinations: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let [x, y, w, h] = t.rect;
            let y = 1_080 - y - h;
            format!(r#"{{"id": "t{i}", "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#)
        })
        .collect();
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "font": [{}],
            "text": [{}],
            "destination": [{}]}}"#,
        fonts.join(", "),
        records.join(", "),
        destinations.join(", ")
    );
    fs::write(d.join("s.json"), json).unwrap();
    dir
}

/// The skin of `dir` imported with its header's defaults.
pub fn import(dir: &Path) -> skin_beatoraja::Imported {
    let (mut def, header) = skin_beatoraja::read_header(dir, "s.json").unwrap();
    let config = skin_beatoraja::Config::defaults(&header);
    skin_beatoraja::import(&mut def, &header, &config, skin_model::ScreenKind::Play).unwrap()
}

/// The font files `skin`'s texts are set in, read for installation.
pub fn read_fonts(skin: &Skin) -> Vec<(u32, Vec<u8>)> {
    let mut reader = FontReader::new(skin.sheet());
    skin.outline_fonts()
        .into_iter()
        .map(|i| (i, reader.read(i).unwrap()))
        .collect()
}

/// `skin`'s producer with `default`.
pub fn producer(skin: &Skin, default: &Arc<DefaultFont>) -> Result<OutlineText, OutlineError> {
    OutlineText::new(skin, read_fonts(skin), default)
}

/// The skin of `dir` as a bundle for `r`, with its producer: imported, its
/// atlas with the text layer for a 1920×1080 target.
pub fn bundle_in(r: &Renderer, dir: &Path, default: &Arc<DefaultFont>) -> Arc<SkinBundle> {
    bundle_with(r, dir, |skin| Arc::new(producer(skin, default).unwrap()))
}

/// [`bundle_in`] with the producer `text` makes of the skin.
pub fn bundle_with(
    r: &Renderer,
    dir: &Path,
    text: impl FnOnce(&Skin) -> Arc<dyn AttemptText>,
) -> Arc<SkinBundle> {
    let imported = import(dir);
    let skin = imported.skin;
    let text = text(&skin);
    let variant = skin.variant([1_920, 1_080], &Layout::AUTHOR);
    let atlas = skin.atlas(variant, r.atlas_limits(), &|| false).unwrap();
    let scratch = imported.scratch.unwrap_or(Side::Left);
    let bundle = SkinBundle::new(Arc::new(skin), atlas, ClipTable::default(), scratch).unwrap();
    Arc::new(bundle.with_text(text).unwrap())
}

/// A TrueType font record `f` of `ttf.ttf`.
pub fn ttf() -> FontRec {
    FontRec {
        id: "f",
        path: "ttf.ttf",
        kind: None,
    }
}

/// A distance-field record `d` of `d.fnt`, whose file need not exist.
pub fn distance_field() -> FontRec {
    FontRec {
        id: "d",
        path: "d.fnt",
        kind: Some(1),
    }
}

/// A text of font record `font`, size `size`, at `rect`, showing `value`.
pub fn text(font: &'static str, size: i32, value: Value, rect: [i32; 4]) -> TextSpec {
    TextSpec {
        font,
        size,
        value,
        rect,
    }
}

/// [`folder`] as a decision skin's (the source's type 6).
pub fn decision_folder(
    files: &[(&str, Vec<u8>)],
    fonts: &[FontRec],
    texts: &[TextSpec],
) -> TempDir {
    let dir = folder(files, fonts, texts);
    let path = dir.path().join("s.json");
    let json = fs::read_to_string(&path).unwrap();
    fs::write(&path, json.replacen(r#""type": 0"#, r#""type": 6"#, 1)).unwrap();
    dir
}

/// The decision skin of `dir` as a bundle for `r`: imported into the
/// decision slot, its atlas for a 1920×1080 target, with a producer of
/// `default` when it has rasterized text.
pub fn decision_bundle_in(r: &Renderer, dir: &Path, default: &Arc<DefaultFont>) -> Arc<SkinBundle> {
    let (mut def, header) = skin_beatoraja::read_header(dir, "s.json").unwrap();
    let config = skin_beatoraja::Config::defaults(&header);
    let imported =
        skin_beatoraja::import(&mut def, &header, &config, skin_model::ScreenKind::Decision)
            .unwrap();
    let skin = imported.skin;
    let variant = skin.variant([1_920, 1_080], &Layout::AUTHOR);
    let atlas = skin.atlas(variant, r.atlas_limits(), &|| false).unwrap();
    let outline = skin.kept_layers().text;
    let text: Option<Arc<dyn AttemptText>> =
        outline.then(|| Arc::new(producer(&skin, default).unwrap()) as Arc<dyn AttemptText>);
    let bundle = SkinBundle::new(Arc::new(skin), atlas, ClipTable::default(), Side::Left).unwrap();
    Arc::new(match text {
        Some(t) => bundle.with_text(t).unwrap(),
        None => bundle,
    })
}

/// A bitmap font record `g` of `g.fnt`.
pub fn bitmap() -> FontRec {
    FontRec {
        id: "g",
        path: "g.fnt",
        kind: None,
    }
}

/// The bitmap font [`bitmap`] names and its page: size 8, `A` and `B`
/// each an opaque white 8 × 8 cell of a 16 × 8 page.
pub fn bitmap_files() -> Vec<(&'static str, Vec<u8>)> {
    let mut fnt = String::from(
        "info face=\"g\" size=8 padding=0,0,0,0\n\
         common lineHeight=8 base=8 scaleW=16 scaleH=8 pages=1\n\
         page id=0 file=\"g.png\"\n\
         chars count=2\n",
    );
    for (i, code) in [65, 66].into_iter().enumerate() {
        fnt += &format!(
            "char id={code} x={} y=0 width=8 height=8 xoffset=0 yoffset=0 xadvance=8 page=0\n",
            i * 8
        );
    }
    let mut png_bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut png_bytes, 16, 8);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header()
            .unwrap()
            .write_image_data(&[255; 16 * 8 * 4])
            .unwrap();
    }
    vec![("g.fnt", fnt.into_bytes()), ("g.png", png_bytes)]
}
