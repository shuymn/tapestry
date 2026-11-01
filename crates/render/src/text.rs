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
//! [`MenuText`] holds the screen's glyph cache: a glyph already in its atlas
//! is not rasterized again by later prepares, and the cache goes when the
//! screen drops its [`MenuText`]. When the atlas grows, glyphon rasterizes
//! the glyphs in it once more, on the menu.
//!
//! A [`MenuText`] belongs to the [`Renderer`] it was made for and to the
//! [`Fonts`] it first prepared with. After the renderer is replaced, the
//! screen makes a new [`MenuText`] and prepares it again; using the old one
//! is an error, never a GPU fault.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// The screen's text was made for another renderer, one since replaced.
    #[error("text: made for another renderer")]
    OtherRenderer,
    /// A block, or the screen's atlas, belongs to other [`Fonts`] than the
    /// ones given.
    #[error("text: laid out with other fonts")]
    OtherFonts,
}

/// Identifies each [`Fonts`], so a screen's atlas never mixes glyphs of
/// two font systems.
static FONTS: AtomicU64 = AtomicU64::new(0);

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
    id: u64,
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
            id: FONTS.fetch_add(1, Ordering::Relaxed),
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
    /// The [`Fonts`] it was laid out in.
    fonts: u64,
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
        Self {
            buffer,
            style,
            fonts: fonts.id,
        }
    }

    /// Sets the color's alpha, for the next [`MenuText::prepare`]: a fade
    /// without laying the text out again.
    pub const fn set_alpha(&mut self, alpha: u8) {
        self.style.color[3] = alpha;
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

/// The text of one menu screen and its glyph cache: the atlas its glyphs
/// are rasterized into and drawn from for as long as the screen keeps it.
/// It works only with the renderer it was made for (see the module
/// documentation).
pub struct MenuText {
    atlas: TextAtlas,
    renderer: TextRenderer,
    viewport: Viewport,
    swash: SwashCache,
    /// The [`Renderer::id`] it was made for.
    owner: u64,
    /// The [`Fonts`] its atlas holds glyphs of, from the first prepare.
    fonts: Option<u64>,
}

impl MenuText {
    /// An empty screen's text, for drawing with `renderer` only.
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
            owner: renderer.id(),
            fonts: None,
        }
    }

    /// The [`Renderer::id`] it was made for.
    pub(crate) const fn owner(&self) -> u64 {
        self.owner
    }

    /// Makes `blocks` what [`Renderer::menu_frame`] draws. A glyph already
    /// in this screen's atlas is not rasterized again; glyphs of earlier
    /// blocks stay there and are evicted only when the atlas needs their
    /// room. When the atlas has to grow, every glyph in it is rasterized
    /// once more.
    ///
    /// # Errors
    ///
    /// Returns an error when `renderer` is not the one this was made for,
    /// when `fonts` or a block's fonts are not the ones the screen first
    /// prepared with, or when the atlas cannot grow to hold the glyphs.
    pub fn prepare(
        &mut self,
        renderer: &Renderer,
        fonts: &mut Fonts,
        blocks: &[&TextBlock],
    ) -> Result<(), TextError> {
        if renderer.id() != self.owner {
            return Err(TextError::OtherRenderer);
        }
        // Glyphs in the atlas are rasterized again from `fonts` when it
        // grows, so it holds glyphs of one font system only.
        // A refused prepare binds nothing.
        let own = self.fonts.unwrap_or(fonts.id);
        if own != fonts.id || blocks.iter().any(|b| b.fonts != own) {
            return Err(TextError::OtherFonts);
        }
        self.fonts = Some(own);
        let gpu = renderer.gpu();
        let (width, height) = renderer.size();
        // The whole target is the viewport, so glyphs land on its pixels
        // exactly; the layout is placed and clipped where the letterbox
        // puts the sprites.
        let [x, y, w, h] = letterbox(width, height);
        let scale = w / layout::WIDTH;
        self.viewport
            .update(gpu.queue(), Resolution { width, height });
        let bounds = TextBounds {
            left: first_covered(x),
            top: first_covered(y),
            right: first_covered(x + w),
            bottom: first_covered(y + h),
        };
        // Glyphs the previous blocks used may now be evicted for room.
        self.atlas.trim();
        let areas = blocks.iter().map(|b| {
            let [r, g, bl, a] = b.style.color;
            TextArea {
                buffer: &b.buffer,
                left: b.style.left.mul_add(scale, x),
                top: b.style.top.mul_add(scale, y),
                scale,
                bounds,
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

    /// Draws the prepared blocks into `pass`, whose viewport is the whole
    /// target.
    pub(crate) fn render(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> Result<(), glyphon::RenderError> {
        self.renderer.render(&self.atlas, &self.viewport, pass)
    }
}

/// The first pixel whose center is at or past the edge `edge`: as the
/// rasterizer covers pixels by their centers, with a center on a left or
/// top edge inside and one on a right or bottom edge outside, so text is
/// clipped to the pixels the letterboxed sprites cover.
fn first_covered(edge: f32) -> i32 {
    (edge - 0.5).ceil() as i32
}

#[cfg(test)]
mod tests {
    use std::fs;

    use glyphon::FontSystem;
    use glyphon::cosmic_text::fontdb;

    use super::{FONT_ORDER, Fonts, MenuText, TextBlock, TextError, TextStyle, choose_font};
    use crate::atlas::solid_uv;
    use crate::{AdapterChoice, Blend, RenderError, Renderer, Sprite, Target};

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

    fn warp(width: u32, height: u32) -> Renderer {
        Renderer::new(Target::Offscreen { width, height }, AdapterChoice::Warp).unwrap()
    }

    fn white(left: f32, top: f32) -> TextStyle {
        TextStyle {
            left,
            top,
            size: 96.0,
            color: [255, 255, 255, 255],
        }
    }

    /// Takes away every font of `fonts`: from here on no glyph can be
    /// rasterized, so a glyph drawn must come from a screen's atlas.
    fn unrasterizable(fonts: &mut Fonts) {
        fonts.system =
            FontSystem::new_with_locale_and_db("ja-JP".to_owned(), fontdb::Database::new());
    }

    #[test]
    fn glyphs_are_cached_per_screen() {
        // A glyph already in the screen's atlas is not rasterized again by
        // a later prepare. Two glyphs fit the initial atlas, so it never
        // grows here (`atlas_growth_keeps_drawn_glyphs` covers growth).
        //
        // 480×270: the layout at 1/4 scale, so 4 layout pixels are one
        // target pixel and a block moved by a multiple of 4 lands on the
        // same subpixel position, with the same glyph images.
        let mut r = warp(480, 270);
        let mut fonts = Fonts::runtime().unwrap();
        let first = TextBlock::new(&mut fonts, "譜面", white(100.0, 100.0));
        let again = TextBlock::new(&mut fonts, "譜面", white(100.0, 500.0));
        let mut text = MenuText::new(&r);
        text.prepare(&r, &mut fonts, &[&first]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let once = lit(&r.read_back(), 480);
        assert!(once.len() > 100, "{} lit pixels", once.len());
        assert!(once.iter().all(|&(_, y)| y < 100));

        // The screen still draws both blocks, from its atlas.
        unrasterizable(&mut fonts);
        text.prepare(&r, &mut fonts, &[&first, &again]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let twice = lit(&r.read_back(), 480);
        let (top, moved): (Vec<_>, Vec<_>) = twice.into_iter().partition(|&(_, y)| y < 100);
        assert_eq!(top, once);
        // 400 layout pixels lower: 100 target rows.
        let shifted: Vec<_> = once.iter().map(|&(x, y)| (x, y + 100)).collect();
        assert_eq!(moved, shifted);
    }

    #[test]
    fn atlas_growth_keeps_drawn_glyphs() {
        // 1920×1080, so a 96 px glyph is about 90×90 target pixels: the 64
        // distinct glyphs below need about 500 000 atlas texels, more than
        // the initial 256×256 atlas and its first doubling hold, so the
        // atlas grows while they are prepared, rasterizing the glyphs
        // already in it again.
        const MANY: &str = "一二三四五六七八九十百千万円年月日時分秒\n\
                            春夏秋冬東西南北上下左右前後内外大小高低\n\
                            山川海空雨雪風雲花草木森林石金銀銅鉄火水\n\
                            土光音楽";
        let mut r = warp(1920, 1080);
        let mut fonts = Fonts::runtime().unwrap();
        let first = TextBlock::new(&mut fonts, "譜面", white(100.0, 50.0));
        let many = TextBlock::new(&mut fonts, MANY, white(100.0, 300.0));
        let mut text = MenuText::new(&r);
        text.prepare(&r, &mut fonts, &[&first]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let before = lit(&r.read_back(), 1920);
        assert!(before.len() > 1_000, "{} lit pixels", before.len());

        text.prepare(&r, &mut fonts, &[&first, &many]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        let grown = lit(&r.read_back(), 1920);
        let (top, rest): (Vec<_>, Vec<_>) = grown.iter().partition(|&&(_, y)| y < 250);
        // The glyphs drawn before the growth draw the same after it.
        assert_eq!(top, before);
        // Every line of the many glyphs is drawn: lines are 120 px apart.
        for line in 0..4 {
            let rows = 300 + line * 120..300 + (line + 1) * 120;
            let n = rest.iter().filter(|(_, y)| rows.contains(y)).count();
            assert!(n > 2_000, "line {line}: {n} lit pixels");
        }

        // The grown atlas holds them all: drawn again without rasterizing,
        // the screen is unchanged.
        unrasterizable(&mut fonts);
        text.prepare(&r, &mut fonts, &[&first, &many]).unwrap();
        r.menu_frame(|_| {}, &text).unwrap();
        assert_eq!(lit(&r.read_back(), 1920), grown);
    }

    #[test]
    fn text_of_another_renderer_or_fonts_is_refused() {
        let mut old = warp(480, 270);
        let mut fonts = Fonts::runtime().unwrap();
        let block = TextBlock::new(&mut fonts, "譜面", white(100.0, 100.0));
        let mut text = MenuText::new(&old);
        text.prepare(&old, &mut fonts, &[&block]).unwrap();

        // The renderer is replaced, as after a lost surface: the old text
        // uses the old device, so it is refused, not drawn.
        let mut new = warp(480, 270);
        assert!(matches!(
            new.menu_frame(|_| {}, &text),
            Err(RenderError::OtherRenderersText)
        ));
        assert!(matches!(
            text.prepare(&new, &mut fonts, &[&block]),
            Err(TextError::OtherRenderer)
        ));
        let mut remade = MenuText::new(&new);
        remade.prepare(&new, &mut fonts, &[&block]).unwrap();
        new.menu_frame(|_| {}, &remade).unwrap();
        assert!(!lit(&new.read_back(), 480).is_empty());
        old.menu_frame(|_| {}, &text).unwrap();

        // Other fonts, or a block laid out in them, are refused too.
        let mut other = Fonts::runtime().unwrap();
        let theirs = TextBlock::new(&mut other, "譜面", white(100.0, 100.0));
        assert!(matches!(
            text.prepare(&old, &mut other, &[&theirs]),
            Err(TextError::OtherFonts)
        ));
        assert!(matches!(
            text.prepare(&old, &mut fonts, &[&theirs]),
            Err(TextError::OtherFonts)
        ));
        text.prepare(&old, &mut fonts, &[&block]).unwrap();
    }

    #[test]
    fn text_lines_up_with_sprites_off_16_9() {
        // 1366×768 is scaled to the height, by 32/45: a 1365.33 px wide
        // layout 0.33 px from the left. 1125 layout pixels are exactly 800
        // target pixels, so a sprite and a glyph moved that far land 800
        // pixels on, at the same subpixel position. A text pass projected
        // against a rounded viewport drifts from the sprites across the
        // screen.
        let mut r = warp(1366, 768);
        let mut fonts = Fonts::runtime().unwrap();
        let near = TextBlock::new(&mut fonts, "譜", white(90.0, 100.0));
        let far = TextBlock::new(&mut fonts, "譜", white(1_215.0, 100.0));
        let mut text = MenuText::new(&r);
        text.prepare(&r, &mut fonts, &[&near, &far]).unwrap();
        r.menu_frame(
            |b| {
                for left in [90.0, 1_215.0] {
                    b.push(
                        Blend::Alpha,
                        Sprite {
                            rect: [left, 600.0, 45.0, 45.0],
                            uv: solid_uv(),
                            color: [1.0, 0.0, 0.0, 1.0],
                            ..Sprite::default()
                        },
                    );
                }
            },
            &text,
        )
        .unwrap();
        let on = lit(&r.read_back(), 1366);
        let (text_on, sprite_on): (Vec<_>, Vec<_>) = on.into_iter().partition(|&(_, y)| y < 300);
        let first_column = |pixels: &[(usize, usize)], right: bool| {
            pixels
                .iter()
                .filter(|&&(x, _)| (x >= 683) == right)
                .map(|&(x, _)| x)
                .min()
                .unwrap()
        };
        // The sprites' left edges: 0.33 + 64 and 0.33 + 864.
        assert_eq!(
            (
                first_column(&sprite_on, false),
                first_column(&sprite_on, true)
            ),
            (64, 864)
        );
        // The far glyph is the near one 800 pixels on, as the sprites are.
        let (text_near, text_far): (Vec<_>, Vec<_>) =
            text_on.into_iter().partition(|&(x, _)| x < 683);
        assert!(!text_near.is_empty());
        let shifted: Vec<_> = text_near.iter().map(|&(x, y)| (x + 800, y)).collect();
        assert_eq!(text_far, shifted);
    }

    #[test]
    fn text_is_clipped_where_sprites_are() {
        // 1921×1080 is scaled by 1 and letterboxed at x = 0.5…1920.5: pixel
        // centers 0.5 and 1920.5 lie on its edges, so the covered columns
        // are 0…1919. A glyph and a sprite cross each edge.
        let mut r = warp(1921, 1080);
        let mut fonts = Fonts::runtime().unwrap();
        let left = TextBlock::new(&mut fonts, "譜", white(-40.0, 100.0));
        let right = TextBlock::new(&mut fonts, "譜", white(1_880.0, 100.0));
        let mut text = MenuText::new(&r);
        text.prepare(&r, &mut fonts, &[&left, &right]).unwrap();
        r.menu_frame(
            |b| {
                for x in [-40.0, 1_880.0] {
                    b.push(
                        Blend::Alpha,
                        Sprite {
                            rect: [x, 600.0, 100.0, 50.0],
                            uv: solid_uv(),
                            color: [1.0, 0.0, 0.0, 1.0],
                            ..Sprite::default()
                        },
                    );
                }
            },
            &text,
        )
        .unwrap();
        let on = lit(&r.read_back(), 1921);
        let columns = |rows: std::ops::Range<usize>| {
            let xs: Vec<_> = on
                .iter()
                .filter(|(_, y)| rows.contains(y))
                .map(|&(x, _)| x)
                .collect();
            (*xs.iter().min().unwrap(), *xs.iter().max().unwrap())
        };
        let sprites = columns(500..1080);
        assert_eq!(sprites, (0, 1919));
        assert_eq!(columns(0..500), sprites);
    }

    #[test]
    fn refused_first_prepare_binds_no_fonts() {
        let r = warp(480, 270);
        let mut a = Fonts::runtime().unwrap();
        let mut b = Fonts::runtime().unwrap();
        let of_b = TextBlock::new(&mut b, "譜面", white(100.0, 100.0));
        let mut text = MenuText::new(&r);
        assert!(matches!(
            text.prepare(&r, &mut a, &[&of_b]),
            Err(TextError::OtherFonts)
        ));
        text.prepare(&r, &mut b, &[&of_b]).unwrap();
        let of_a = TextBlock::new(&mut a, "譜面", white(100.0, 100.0));
        assert!(matches!(
            text.prepare(&r, &mut a, &[&of_a]),
            Err(TextError::OtherFonts)
        ));
    }
}
