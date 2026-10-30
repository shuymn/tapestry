//! Japanese text on menu screens (design.md "Rendering and Skins"):
//! `cosmic-text` lays it out, and `glyphon` keeps the glyph atlas and draws
//! it in its own pass after the sprite pass.
//!
//! Everything here runs on menus, where allocation is allowed; the play
//! screen keeps the bitmap font and never calls into this module.
//!
//! A screen loads [`Fonts`] once, lays out each string as a [`TextBlock`],
//! hands its blocks to its [`MenuText`] with [`MenuText::prepare`] whenever
//! they change, and draws them with [`Renderer::menu_frame`]. The
//! [`MenuText`] holds the screen's glyph cache: a glyph is rasterized the
//! first time the screen draws it and taken from the atlas after that, and
//! the cache goes when the screen drops its [`MenuText`].

use std::path::{Path, PathBuf};

use glyphon::cosmic_text::fontdb;
use glyphon::{
    Attrs, Buffer, Cache, Color, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};

use crate::Renderer;
use crate::renderer::letterbox;
use crate::simple::layout;

/// A runtime font: a family and the file under the Windows fonts folder that
/// holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeFont {
    /// The family name text is laid out in.
    pub family: &'static str,
    /// The file name in the fonts folder.
    pub file: &'static str,
}

/// The runtime fonts in the order they are tried: the first present is the
/// one used, and only its file is read.
pub const FONT_ORDER: [RuntimeFont; 3] = [
    RuntimeFont {
        family: "Yu Gothic UI",
        file: "YuGothM.ttc",
    },
    RuntimeFont {
        family: "Meiryo",
        file: "meiryo.ttc",
    },
    RuntimeFont {
        family: "MS Gothic",
        file: "msgothic.ttc",
    },
];

/// Why text could not be set up or prepared.
#[derive(Debug, thiserror::Error)]
pub enum TextError {
    /// None of [`FONT_ORDER`] is in the fonts folder.
    #[error("font: none of Yu Gothic UI, Meiryo and MS Gothic is in {}", dir.display())]
    NoFont {
        /// The folder searched.
        dir: PathBuf,
    },
    /// The chosen font file could not be read.
    #[error("font: {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The font data has no face of the family.
    #[error("font: no {family} face in the font data")]
    NoFamily {
        /// The family looked for.
        family: String,
    },
    /// The screen's glyph atlas cannot take its text.
    #[error("text")]
    Prepare(#[source] glyphon::PrepareError),
}

/// The Windows fonts folder (`%WINDIR%\Fonts`, the system one also under
/// Remote Desktop).
#[must_use]
pub fn fonts_dir() -> PathBuf {
    let mut buf = [0_u16; 260];
    // SAFETY: the call writes at most `buf.len()` characters into `buf`.
    let n = unsafe {
        windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(Some(&mut buf))
    };
    let windows = buf.get(..n as usize).filter(|_| n != 0).map_or_else(
        || PathBuf::from(r"C:\Windows"),
        |s| PathBuf::from(String::from_utf16_lossy(s)),
    );
    windows.join("Fonts")
}

/// The first of [`FONT_ORDER`] whose file is in `dir`, and its path. Only
/// the presence of each file is checked; none is opened.
#[must_use]
pub fn choose_font(dir: &Path) -> Option<(RuntimeFont, PathBuf)> {
    FONT_ORDER.iter().find_map(|f| {
        let path = dir.join(f.file);
        path.is_file().then_some((*f, path))
    })
}

/// The font text is laid out and rasterized with: one font file, never a
/// scan of the system's fonts.
pub struct Fonts {
    system: FontSystem,
    family: String,
}

impl Fonts {
    /// The runtime font: the first of [`FONT_ORDER`] in the Windows fonts
    /// folder.
    ///
    /// # Errors
    ///
    /// Returns an error when none is there, or the chosen file cannot be
    /// read or lacks its family.
    pub fn runtime() -> Result<Self, TextError> {
        let dir = fonts_dir();
        let (font, path) = choose_font(&dir).ok_or(TextError::NoFont { dir })?;
        let mut db = fontdb::Database::new();
        db.load_font_file(&path)
            .map_err(|source| TextError::Read { path, source })?;
        Self::with_db(db, font.family)
    }

    /// `family` from font data in memory (a TTF, OTF or collection).
    ///
    /// # Errors
    ///
    /// Returns an error when the data has no face of `family`.
    pub fn from_data(data: Vec<u8>, family: &str) -> Result<Self, TextError> {
        let mut db = fontdb::Database::new();
        db.load_font_data(data);
        Self::with_db(db, family)
    }

    fn with_db(db: fontdb::Database, family: &str) -> Result<Self, TextError> {
        let has = db
            .faces()
            .any(|f| f.families.iter().any(|(name, _)| name == family));
        if !has {
            return Err(TextError::NoFamily {
                family: family.to_owned(),
            });
        }
        Ok(Self {
            system: FontSystem::new_with_locale_and_db("ja-JP".to_owned(), db),
            family: family.to_owned(),
        })
    }

    /// The family text is laid out in.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }
}

/// How a [`TextBlock`] is placed, in layout pixels (1920×1080, origin top
/// left, as sprites are).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    /// The left edge of the text.
    pub left: f32,
    /// The top of the first line.
    pub top: f32,
    /// The font size; lines are 1.25 times it apart.
    pub size: f32,
    /// The color, straight RGBA8.
    pub color: [u8; 4],
}

/// A string laid out in [`Fonts`], ready for [`MenuText::prepare`]. Lines
/// break only at line breaks in the string.
pub struct TextBlock {
    buffer: Buffer,
    style: TextStyle,
}

impl TextBlock {
    /// Lays out `text` with `style` in `fonts`.
    #[must_use]
    pub fn new(fonts: &mut Fonts, text: &str, style: TextStyle) -> Self {
        let metrics = Metrics::new(style.size, style.size * 1.25);
        let mut buffer = Buffer::new(&mut fonts.system, metrics);
        buffer.set_size(None, None);
        let attrs = Attrs::new().family(Family::Name(&fonts.family));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut fonts.system, false);
        Self { buffer, style }
    }

    /// The laid-out width of the widest line, in layout pixels.
    #[must_use]
    pub fn width(&self) -> f32 {
        self.buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
    }
}

/// The text of one menu screen and its glyph cache: the atlas a glyph is
/// rasterized into once and drawn from for as long as the screen keeps it.
pub struct MenuText {
    atlas: TextAtlas,
    renderer: TextRenderer,
    viewport: Viewport,
    swash: SwashCache,
}

impl MenuText {
    /// An empty screen's text, for drawing with `renderer`.
    #[must_use]
    pub fn new(renderer: &Renderer) -> Self {
        let gpu = renderer.gpu();
        let device = gpu.device();
        let cache = Cache::new(device);
        // The targets are linear formats holding sRGB values, as the
        // sprites' colors are.
        let mut atlas = TextAtlas::with_color_mode(
            device,
            gpu.queue(),
            &cache,
            renderer.format(),
            ColorMode::Web,
        );
        let text = TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        Self {
            atlas,
            renderer: text,
            viewport: Viewport::new(device, &cache),
            swash: SwashCache::new(),
        }
    }

    /// Makes `blocks` what [`Renderer::menu_frame`] draws, rasterizing only
    /// the glyphs this screen has not drawn yet. Glyphs of earlier
    /// blocks stay cached; they are evicted only when the atlas needs
    /// their room.
    ///
    /// # Errors
    ///
    /// Returns an error when the atlas cannot grow to hold the blocks'
    /// glyphs.
    pub fn prepare(
        &mut self,
        renderer: &Renderer,
        fonts: &mut Fonts,
        blocks: &[&TextBlock],
    ) -> Result<(), TextError> {
        let gpu = renderer.gpu();
        let (width, height) = renderer.size();
        let [_, _, w, h] = letterbox(width, height);
        let scale = w / layout::WIDTH;
        self.viewport.update(
            gpu.queue(),
            Resolution {
                width: w.round() as u32,
                height: h.round() as u32,
            },
        );
        // Glyphs the previous blocks used may now be evicted for room.
        self.atlas.trim();
        let areas = blocks.iter().map(|b| {
            let [r, g, bl, a] = b.style.color;
            TextArea {
                buffer: &b.buffer,
                left: b.style.left * scale,
                top: b.style.top * scale,
                scale,
                bounds: TextBounds::default(),
                default_color: Color::rgba(r, g, bl, a),
                custom_glyphs: &[],
            }
        });
        self.renderer
            .prepare(
                gpu.device(),
                gpu.queue(),
                &mut fonts.system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash,
            )
            .map_err(TextError::Prepare)
    }

    /// Draws the prepared blocks into `pass`, whose viewport is the
    /// letterboxed layout.
    pub(crate) fn render(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> Result<(), glyphon::RenderError> {
        self.renderer.render(&self.atlas, &self.viewport, pass)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use glyphon::FontSystem;
    use glyphon::cosmic_text::fontdb;

    use super::{FONT_ORDER, Fonts, MenuText, TextBlock, TextStyle, choose_font};
    use crate::{AdapterChoice, Renderer, Target};

    #[test]
    fn runtime_font_falls_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = || choose_font(dir.path()).map(|(f, p)| (f.family, p));
        let touch = |file: &str| fs::write(dir.path().join(file), b"").unwrap();
        assert_eq!(chosen(), None);
        touch("msgothic.ttc");
        assert_eq!(
            chosen(),
            Some(("MS Gothic", dir.path().join("msgothic.ttc")))
        );
        touch("meiryo.ttc");
        assert_eq!(chosen(), Some(("Meiryo", dir.path().join("meiryo.ttc"))));
        touch("YuGothM.ttc");
        assert_eq!(
            chosen(),
            Some(("Yu Gothic UI", dir.path().join("YuGothM.ttc")))
        );
        // Only files count: a folder of the first name is passed over.
        fs::remove_file(dir.path().join("YuGothM.ttc")).unwrap();
        fs::create_dir(dir.path().join("YuGothM.ttc")).unwrap();
        assert_eq!(chosen(), Some(("Meiryo", dir.path().join("meiryo.ttc"))));
        assert_eq!(FONT_ORDER.len(), 3);
    }

    /// Lit pixels (any channel above 64) of an RGBA8 image `width` wide,
    /// as (x, y).
    fn lit(pixels: &[u8], width: usize) -> Vec<(usize, usize)> {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, p)| p[..3].iter().any(|&c| c > 64))
            .map(|(i, _)| (i % width, i / width))
            .collect()
    }

    #[test]
    fn glyphs_are_cached_per_screen() {
        // 480×270: the layout at 1/4 scale, so 4 layout pixels are one
        // target pixel and a block moved by a multiple of 4 lands on the
        // same subpixel position, with the same glyph images.
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 480,
                height: 270,
            },
            AdapterChoice::Warp,
        )
        .unwrap();
        let mut fonts = Fonts::runtime().unwrap();
        let style = |top| TextStyle {
            left: 100.0,
            top,
            size: 96.0,
            color: [255, 255, 255, 255],
        };
        let first = TextBlock::new(&mut fonts, "譜面", style(100.0));
        let again = TextBlock::new(&mut fonts, "譜面", style(500.0));
        let mut text = MenuText::new(&r);
        text.prepare(&r, &mut fonts, &[&first]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let once = lit(&r.read_back(), 480);
        assert!(once.len() > 100, "{} lit pixels", once.len());
        assert!(once.iter().all(|&(_, y)| y < 100));

        // From here on no glyph can be rasterized: the font system has no
        // font. The screen still draws both blocks, from its atlas.
        let family = fonts.family.clone();
        fonts.system =
            FontSystem::new_with_locale_and_db("ja-JP".to_owned(), fontdb::Database::new());
        assert_eq!(fonts.family, family);
        text.prepare(&r, &mut fonts, &[&first, &again]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let twice = lit(&r.read_back(), 480);
        let (top, moved): (Vec<_>, Vec<_>) = twice.into_iter().partition(|&(_, y)| y < 100);
        assert_eq!(top, once);
        // 400 layout pixels lower: 100 target rows.
        let shifted: Vec<_> = once.iter().map(|&(x, y)| (x, y + 100)).collect();
        assert_eq!(moved, shifted);
    }
}
