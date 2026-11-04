//! The play screen's rasterized text (design.md "Rendering and Skins"):
//! [`OutlineText`], the producer of a skin's lines in its font files and
//! in the default font, built at installation and asked at each attempt's
//! preparation, before READY and outside every frame build.
//!
//! **Faces.** Only face 0 of a skin font is registered, after its
//! collection header is checked (a `ttcf` declares at least one face and
//! its offsets fit the data) and skrifa parses it; a face with a `VARC`
//! table, no `glyf`, `CFF ` or `CFF2` outlines, or zero units per em is
//! refused. Each face is registered with `fontdb::Database::push_face_info`
//! under a synthetic family, never through a bulk loader, and its source
//! is always `Source::Binary` over the immutable snapshot (`Arc`) read at
//! installation, never a file or a shared mapping: fontdb's mapping of a
//! file stays unreachable.
//!
//! **Font systems.** A skin font's system holds its face and the default
//! face, with empty fallback lists, so a glyph the skin font lacks comes
//! from the default font (D16), and one neither has is the skin face's
//! `.notdef`. A further system holds the default face alone, for
//! [`FontRef::Default`] (D13 included). Each face is materialised at
//! installation (cosmic-text parses fonts lazily), and each system lays out
//! and rasterizes one probe line there, so a line laid out later parses
//! nothing.
//!
//! **Raster safety.** A glyph's outline is extracted with swash
//! (`scale_outline_into`, unhinted), and every point is checked finite and
//! inside the line's pixels plus [`RASTER_MARGIN`] before anything reaches
//! the rasterizer; the translated points and the mask's size are checked
//! again right before `zeno::Mask` renders them. zeno converts each point
//! to fixed point unchecked, so nothing unchecked may reach it. One helper
//! serves layout, the probes and rendering.
//!
//! What the font libraries allocate while they shape and expand outlines
//! is not bounded here (design.md "Threat Model").

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use glyphon::cosmic_text::fontdb;
use glyphon::cosmic_text::{
    Attrs, AttrsList, Fallback, Family, FontSystem, Hinting, LayoutLine, ShapeLine, Shaping, Wrap,
};
use skin_model::{FontRef, Skin, TextRequest};
use skrifa::MetadataProvider as _;
use skrifa::raw::TableProvider as _;
use swash::scale::ScaleContext;
use swash::scale::outline::Outline;
use swash::zeno::{self, Point, Verb};

use crate::text::{choose_font, fonts_dir};

/// The widest line, before the atlas side caps it ([`LineBounds::for_side`]).
pub const MAX_LINE_W: u32 = 4_096;
/// The tallest line, before the atlas side caps it.
pub const MAX_LINE_H: u32 = 1_024;
/// The most glyphs a line may lay out for rendering: past it the line is
/// refused ([`TextNote::TooComplex`]).
pub const MAX_SHAPED_GLYPHS: usize = 1_024;
/// How far a glyph's outline may reach past its line's pixels, in pixels.
pub const RASTER_MARGIN: f32 = 64.0;
/// The largest default font file read.
pub const MAX_DEFAULT_FONT_BYTES: u64 = 32 << 20;
/// How many times a line too wide for its bounds is laid out again at a
/// smaller em.
const MAX_REFITS: u32 = 2;
/// The largest pen or outline coordinate layout accepts, in pixels, before
/// a line is fitted to its bounds.
const MAX_LAYOUT_COORD: f32 = 1_048_576.0;
/// The em the probes are laid out at.
const PROBE_EM: f32 = 64.0;
/// The skin face's family in its system: synthetic, never a font's own.
const SKIN_FAMILY: &str = "tapestry skin face";
/// The default face's family.
const DEFAULT_FAMILY: &str = "tapestry default face";

/// The pixel box a line must fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineBounds {
    /// The widest line.
    pub w: u32,
    /// The tallest line.
    pub h: u32,
}

impl LineBounds {
    /// The bounds of a text layer `side` texels square: at most
    /// [`MAX_LINE_W`] and [`MAX_LINE_H`], less the zero gutter on both
    /// sides, so a line's rectangle never passes `min(4096, side)` wide.
    #[must_use]
    pub fn for_side(side: u32) -> Self {
        let gutter = 2 * skin_model::GUTTER;
        Self {
            w: side.min(MAX_LINE_W).saturating_sub(gutter),
            h: side.saturating_sub(gutter).min(MAX_LINE_H),
        }
    }
}

/// Why a line has no image: one report line, nothing drawn for its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextNote {
    /// The text layer had no room for it after every round.
    LayerFull,
    /// Its pixels do not fit its bounds, after the refits.
    TooLarge,
    /// It lays out more than [`MAX_SHAPED_GLYPHS`] glyphs.
    TooComplex,
}

/// One glyph of a laid-out line: its face in the line's font system (0 the
/// line's own, 1 the fallback), its glyph id, and its pen position in the
/// line image's pixels, y down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedGlyph {
    /// The face.
    pub face: u8,
    /// The glyph.
    pub glyph: u16,
    /// The pen's x.
    pub x: f32,
    /// The baseline's y.
    pub y: f32,
}

/// A line laid out and fitted, with no pixels yet.
#[derive(Clone, Debug, PartialEq)]
pub struct LineLayout {
    /// The font it was laid out in.
    pub font: FontRef,
    /// The image's pixels, width and height, both at least 1.
    pub size: [u32; 2],
    /// The image's left, top, width and height in em, relative to the pen
    /// origin on the box's top (the cap height).
    pub ink: [f32; 4],
    /// The pen advance, in em.
    pub advance: f32,
    /// The em in pixels the line is rasterized at.
    pub raster_em: f32,
    /// What its producer renders.
    pub glyphs: Box<[PlacedGlyph]>,
}

/// What laying out a request gave.
#[derive(Clone, Debug, PartialEq)]
pub enum LineFit {
    /// A line to rasterize.
    Fits(LineLayout),
    /// Nothing to draw: no ink.
    Empty,
    /// No image, for this reason.
    Refused(TextNote),
}

/// The bytes a producer keeps resident: its skin fonts' snapshots, counted
/// with its bundle, and the shared default font, counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextResidency {
    /// The skin font files read at installation.
    pub skin_fonts: u64,
    /// The default font file, shared by every producer.
    pub default_font: u64,
}

/// Why a producer was not made: its skin's installation is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutlineError {
    /// A font cannot be used: `font` is the sheet's font index, `None` the
    /// default font.
    #[error("{}: {reason}", match font { Some(i) => format!("font {i}"), None => "the default font".to_owned() })]
    Font {
        /// The font.
        font: Option<u32>,
        /// Why.
        reason: String,
    },
    /// None of the runtime fonts is installed.
    #[error("no default font: none of Yu Gothic UI, Meiryo and MS Gothic is installed")]
    NoDefaultFont,
}

impl OutlineError {
    fn font(font: Option<u32>, reason: impl Into<String>) -> Self {
        Self::Font {
            font,
            reason: reason.into(),
        }
    }
}

/// The default font: the runtime font file, read once per process, and
/// the face whose family is the menus'.
pub struct DefaultFont {
    data: Arc<Vec<u8>>,
    index: u32,
}

impl fmt::Debug for DefaultFont {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultFont")
            .field("bytes", &self.data.len())
            .field("index", &self.index)
            .finish()
    }
}

/// The runtime default font, once read.
static RUNTIME: OnceLock<Arc<DefaultFont>> = OnceLock::new();

impl DefaultFont {
    /// The runtime font (`render::text::FONT_ORDER`'s first present),
    /// read once per process, at most [`MAX_DEFAULT_FONT_BYTES`]; a
    /// failure is not kept, so a later call reads again.
    ///
    /// # Errors
    ///
    /// [`OutlineError::NoDefaultFont`] without a runtime font, and
    /// [`OutlineError::Font`] when it cannot be read or used.
    pub fn runtime() -> Result<Arc<Self>, OutlineError> {
        if let Some(f) = RUNTIME.get() {
            return Ok(Arc::clone(f));
        }
        let f = Arc::new(Self::from_dir(&fonts_dir())?);
        Ok(Arc::clone(RUNTIME.get_or_init(|| f)))
    }

    /// The first runtime font in `dir`, read now, at most
    /// [`MAX_DEFAULT_FONT_BYTES`].
    ///
    /// # Errors
    ///
    /// As [`runtime`](Self::runtime).
    pub fn from_dir(dir: &Path) -> Result<Self, OutlineError> {
        let (font, path) = choose_font(dir).ok_or(OutlineError::NoDefaultFont)?;
        let fail = |e: std::io::Error| OutlineError::font(None, e.to_string());
        let file = File::open(&path).map_err(fail)?;
        let len = file.metadata().map_err(fail)?.len();
        if len > MAX_DEFAULT_FONT_BYTES {
            return Err(OutlineError::font(None, "larger than 32 MiB"));
        }
        let mut data = Vec::new();
        let exact = usize::try_from(len).map_err(|_| OutlineError::font(None, "too large"))?;
        data.try_reserve_exact(exact)
            .map_err(|_| OutlineError::font(None, "too large"))?;
        file.take(MAX_DEFAULT_FONT_BYTES)
            .read_to_end(&mut data)
            .map_err(fail)?;
        Self::from_data(data, font.family)
    }

    /// The face of `family` in `data` (a font or a collection), checked as
    /// a skin font's face is.
    ///
    /// # Errors
    ///
    /// [`OutlineError::Font`] when no face is of `family` or it is refused.
    pub fn from_data(data: Vec<u8>, family: &str) -> Result<Self, OutlineError> {
        let count = face_count(&data).map_err(|r| OutlineError::font(None, r))?;
        let index = (0..count)
            .find(|&i| {
                skrifa::FontRef::from_index(&data, i).is_ok_and(|f| {
                    let names = [
                        skrifa::string::StringId::TYPOGRAPHIC_FAMILY_NAME,
                        skrifa::string::StringId::FAMILY_NAME,
                    ];
                    names.into_iter().any(|id| {
                        f.localized_strings(id)
                            .any(|s| s.chars().eq(family.chars()))
                    })
                })
            })
            .ok_or_else(|| OutlineError::font(None, format!("no {family} face")))?;
        check_face(&data, index).map_err(|r| OutlineError::font(None, r))?;
        Ok(Self {
            data: Arc::new(data),
            index,
        })
    }

    /// Its file's bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        u64::try_from(self.data.capacity()).unwrap_or(u64::MAX)
    }
}

/// The faces a font file declares: 1, or a collection's count once its
/// header is checked (at least one face, and its offsets inside the data).
fn face_count(data: &[u8]) -> Result<u32, &'static str> {
    if data.get(..4) != Some(b"ttcf") {
        return Ok(1);
    }
    let count = data
        .get(8..12)
        .and_then(|b| b.first_chunk::<4>())
        .map(|b| u32::from_be_bytes(*b))
        .ok_or("the collection header is cut short")?;
    let table = 12 + 4 * u64::from(count);
    if count == 0 || table > u64::try_from(data.len()).unwrap_or(u64::MAX) {
        return Err("the collection declares no face or more than it holds");
    }
    Ok(count)
}

/// Checks face `index` of `data`: skrifa parses it, it has `glyf`, `CFF `
/// or `CFF2` outlines and no `VARC` table, and nonzero units per em.
fn check_face(data: &[u8], index: u32) -> Result<(), String> {
    let font = skrifa::FontRef::from_index(data, index).map_err(|e| e.to_string())?;
    let has = |tag: &[u8; 4]| font.table_data(skrifa::Tag::new(tag)).is_some();
    if has(b"VARC") {
        return Err("a VARC table".to_owned());
    }
    if !(has(b"glyf") || has(b"CFF ") || has(b"CFF2")) {
        return Err("no glyf, CFF or CFF2 outlines".to_owned());
    }
    let upem = font.head().map_err(|e| e.to_string())?.units_per_em();
    if upem == 0 {
        return Err("zero units per em".to_owned());
    }
    Ok(())
}

/// Font systems with no fallback lists: a glyph comes from the faces the
/// system holds, in their order, and nothing else.
struct NoFallback;

impl Fallback for NoFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &[]
    }

    fn script_fallback(&self, _: unicode_script::Script, _: &str) -> &[&'static str] {
        &[]
    }
}

/// A face of a font system: its id, its data, where swash finds it, the
/// variation coordinates it is shaped and drawn at, and its cap height in
/// em there.
struct Face {
    id: fontdb::ID,
    data: Arc<Vec<u8>>,
    offset: u32,
    key: swash::CacheKey,
    coords: Vec<swash::NormalizedCoord>,
    cap_em: f32,
}

impl Face {
    fn swash(&self) -> swash::FontRef<'_> {
        swash::FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

/// One font system and its faces: the line's own first, the fallback
/// second.
struct System {
    fonts: FontSystem,
    family: &'static str,
    faces: Vec<Face>,
}

/// What a producer locks for each call.
struct Inner {
    /// The skin fonts' systems, by font index.
    skin: Vec<(u32, System)>,
    /// The default font's.
    default: System,
    scale: ScaleContext,
    outline: Outline,
    points: Vec<Point>,
}

/// Materialises face `id` of `fonts`, as cosmic-text would on first use.
pub(crate) type Materialise = fn(&mut FontSystem, fontdb::ID) -> bool;

fn get_font(fonts: &mut FontSystem, id: fontdb::ID) -> bool {
    fonts.get_font(id, fontdb::Weight::NORMAL).is_some()
}

/// The rasterized-text producer of one skin: its fonts' systems and a
/// scaler, behind a lock.
pub struct OutlineText {
    inner: Mutex<Inner>,
    residency: TextResidency,
}

impl fmt::Debug for OutlineText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutlineText")
            .field("residency", &self.residency)
            .finish_non_exhaustive()
    }
}

impl OutlineText {
    /// The producer of `skin`'s text, from the bytes of the font files its
    /// texts are set in (`fonts`, by index: what a
    /// `skin_model::FontReader` read for [`Skin::outline_fonts`]) and the
    /// default font. Installation work: it parses, materialises and probes
    /// every face.
    ///
    /// # Errors
    ///
    /// [`OutlineError::Font`] for a font missing from `fonts`, refused, or
    /// failing its materialisation or probe.
    pub fn new(
        skin: &Skin,
        fonts: Vec<(u32, Vec<u8>)>,
        default: &Arc<DefaultFont>,
    ) -> Result<Self, OutlineError> {
        Self::with_materialise(skin, fonts, default, get_font)
    }

    /// [`new`](Self::new) with `materialise` loading each face.
    pub(crate) fn with_materialise(
        skin: &Skin,
        fonts: Vec<(u32, Vec<u8>)>,
        default: &Arc<DefaultFont>,
        materialise: Materialise,
    ) -> Result<Self, OutlineError> {
        let mut snapshots: Vec<(u32, Arc<Vec<u8>>)> = Vec::with_capacity(fonts.len());
        for (i, data) in fonts {
            face_count(&data).map_err(|r| OutlineError::font(Some(i), r))?;
            check_face(&data, 0).map_err(|r| OutlineError::font(Some(i), r))?;
            snapshots.push((i, Arc::new(data)));
        }
        let mut skin_systems = Vec::with_capacity(snapshots.len());
        for i in skin.outline_fonts() {
            let data = snapshots
                .iter()
                .find(|(j, _)| *j == i)
                .map(|(_, d)| Arc::clone(d))
                .ok_or_else(|| OutlineError::font(Some(i), "not read"))?;
            let faces = [
                (SKIN_FAMILY, data, 0),
                (DEFAULT_FAMILY, Arc::clone(&default.data), default.index),
            ];
            let system =
                build_system(&faces, materialise).map_err(|r| OutlineError::font(Some(i), r))?;
            skin_systems.push((i, system));
        }
        let faces = [(DEFAULT_FAMILY, Arc::clone(&default.data), default.index)];
        let default_system =
            build_system(&faces, materialise).map_err(|r| OutlineError::font(None, r))?;
        let skin_fonts = skin_systems
            .iter()
            .filter_map(|(_, s)| s.faces.first())
            .map(|f| u64::try_from(f.data.capacity()).unwrap_or(u64::MAX))
            .sum();
        let text = Self {
            inner: Mutex::new(Inner {
                skin: skin_systems,
                default: default_system,
                scale: ScaleContext::new(),
                outline: Outline::new(),
                points: Vec::new(),
            }),
            residency: TextResidency {
                skin_fonts,
                default_font: default.bytes(),
            },
        };
        text.probe()?;
        Ok(text)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lays out and rasterizes each system's probe line: `H`, a Latin
    /// letter, and a CJK character when only the fallback has it.
    fn probe(&self) -> Result<(), OutlineError> {
        let fonts: Vec<FontRef> = {
            let inner = self.lock();
            inner
                .skin
                .iter()
                .map(|(i, _)| FontRef::File(*i))
                .chain([FontRef::Default])
                .collect()
        };
        for font in fonts {
            let index = match font {
                FontRef::File(i) => Some(i),
                FontRef::Default | FontRef::Bitmap(_) => None,
            };
            let text = {
                let inner = self.lock();
                let Some(system) = inner.system(font) else {
                    continue;
                };
                let has =
                    |face: Option<&Face>, c| face.is_some_and(|f| f.swash().charmap().map(c) != 0);
                let cjk = !has(system.faces.first(), 'あ') && has(system.faces.get(1), 'あ');
                if cjk { "Haあ" } else { "Ha" }
            };
            let req = TextRequest {
                slot: 0,
                text: text.to_owned(),
                font,
                em_px: PROBE_EM,
            };
            let bounds = LineBounds {
                w: MAX_LINE_W,
                h: MAX_LINE_H,
            };
            let LineFit::Fits(line) = self.layout_line(&req, PROBE_EM, bounds) else {
                return Err(OutlineError::font(index, "its probe line does not lay out"));
            };
            let [w, h] = line.size;
            let mut out = vec![0; usize::try_from(u64::from(w) * u64::from(h)).unwrap_or(0)];
            self.raster_line(&line, &mut out)
                .map_err(|_| OutlineError::font(index, "its probe line does not rasterize"))?;
        }
        Ok(())
    }

    /// The bytes it keeps resident.
    #[must_use]
    pub fn residency(&self) -> TextResidency {
        self.residency
    }

    /// Lays out `req` at `em_px` within `bounds`: a line wider than them is
    /// laid out again at the em that fits, at most twice; one taller, or
    /// still wider, is [`TextNote::TooLarge`]. More than
    /// [`MAX_SHAPED_GLYPHS`] glyphs is [`TextNote::TooComplex`]. A request
    /// in a bitmap font, or one with no ink, is [`LineFit::Empty`].
    #[must_use]
    pub fn layout_line(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        if !(em_px.is_finite() && em_px > 0.0) {
            return LineFit::Empty;
        }
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(system) = system_mut(&mut inner.skin, &mut inner.default, req.font) else {
            return LineFit::Empty;
        };
        // A paragraph separator would split the line into paragraphs that
        // cosmic-text requires to share one direction.
        let text: String = req
            .text
            .chars()
            .map(|c| if c == '\u{2029}' { ' ' } else { c })
            .collect();
        let attrs = Attrs::new().family(Family::Name(system.family));
        let shape = ShapeLine::new(
            &mut system.fonts,
            &text,
            &AttrsList::new(&attrs),
            Shaping::Advanced,
            8,
        );
        let mut em = em_px;
        for refit in 0..=MAX_REFITS {
            let lines = shape.layout(em, None, Wrap::None, None, None, Hinting::Disabled);
            let count: usize = lines.iter().map(|l| l.glyphs.len()).sum();
            if count > MAX_SHAPED_GLYPHS {
                return LineFit::Refused(TextNote::TooComplex);
            }
            let fitted = match measure(
                system,
                &mut inner.scale,
                &mut inner.outline,
                &mut inner.points,
                &lines,
                em,
            ) {
                Ok(m) => m,
                Err(note) => return LineFit::Refused(note),
            };
            let Some(m) = fitted else {
                return LineFit::Empty;
            };
            let [x0, y0, x1, y1] = m.ink;
            let (w, h) = (x1 - x0, y1 - y0);
            if h > i64::from(bounds.h) {
                return LineFit::Refused(TextNote::TooLarge);
            }
            if w > i64::from(bounds.w) {
                if refit == MAX_REFITS {
                    break;
                }
                // Both are below 2^21 pixels: exact enough in f32.
                em *= bounds.w as f32 / w as f32;
                continue;
            }
            let cap = system.faces.first().map_or(0.0, |f| f.cap_em) * em;
            let to_em = |v: i64| v as f32 / em;
            let glyphs = m
                .glyphs
                .into_iter()
                .map(|g| PlacedGlyph {
                    x: g.x - x0 as f32,
                    y: g.y - y0 as f32,
                    ..g
                })
                .collect();
            let (Ok(sw), Ok(sh)) = (u32::try_from(w), u32::try_from(h)) else {
                return LineFit::Refused(TextNote::TooLarge);
            };
            return LineFit::Fits(LineLayout {
                font: req.font,
                size: [sw, sh],
                ink: [to_em(x0), (y0 as f32 + cap) / em, to_em(w), to_em(h)],
                advance: m.advance / em,
                raster_em: em,
                glyphs,
            });
        }
        LineFit::Refused(TextNote::TooLarge)
    }

    /// Rasterizes `line` into `out`: A8 coverage, `line.size` row by row,
    /// zeroed by the caller.
    ///
    /// # Errors
    ///
    /// When `out` is not the line's size or a glyph's outline leaves the
    /// line's pixels plus [`RASTER_MARGIN`]: nothing reached the
    /// rasterizer for that glyph.
    pub fn raster_line(&self, line: &LineLayout, out: &mut [u8]) -> Result<(), RasterError> {
        let [w, h] = line.size;
        if u64::try_from(out.len()).ok() != Some(u64::from(w) * u64::from(h)) {
            return Err(RasterError::Size);
        }
        let mut guard = self.lock();
        let inner = &mut *guard;
        let Some(system) = system_mut(&mut inner.skin, &mut inner.default, line.font) else {
            return Err(RasterError::Size);
        };
        let window = Window::image([w, h]);
        let mut face_of = None;
        let mut scaler = None;
        for g in &line.glyphs {
            if face_of != Some(g.face) || scaler.is_none() {
                let face = system
                    .faces
                    .get(usize::from(g.face))
                    .ok_or(RasterError::Size)?;
                scaler = Some(
                    inner
                        .scale
                        .builder(face.swash())
                        .normalized_coords(&face.coords)
                        .size(line.raster_em)
                        .hint(false)
                        .build(),
                );
                face_of = Some(g.face);
            }
            let Some(s) = scaler.as_mut() else {
                continue;
            };
            // A glyph with no outline (a space) draws nothing.
            if !s.scale_outline_into(swash::GlyphId::from(g.glyph), &mut inner.outline)
                || inner.outline.points().is_empty()
            {
                continue;
            }
            let Some(b) = place_points(
                inner.outline.points(),
                [g.x, g.y],
                window,
                &mut inner.points,
            ) else {
                return Err(RasterError::Outline);
            };
            composite(&inner.points, inner.outline.verbs(), b, [w, h], out)?;
        }
        Ok(())
    }
}

/// Why a line was not rasterized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RasterError {
    /// The buffer or the line does not match its producer.
    #[error("the line's buffer is not its size")]
    Size,
    /// An outline left its line's pixels or was not finite.
    #[error("a glyph's outline leaves its line")]
    Outline,
}

impl Inner {
    fn system(&self, font: FontRef) -> Option<&System> {
        match font {
            FontRef::Default => Some(&self.default),
            FontRef::File(i) => self.skin.iter().find(|(j, _)| *j == i).map(|(_, s)| s),
            FontRef::Bitmap(_) => None,
        }
    }
}

fn system_mut<'a>(
    skin: &'a mut [(u32, System)],
    default: &'a mut System,
    font: FontRef,
) -> Option<&'a mut System> {
    match font {
        FontRef::Default => Some(default),
        FontRef::File(i) => skin.iter_mut().find(|(j, _)| *j == i).map(|(_, s)| s),
        FontRef::Bitmap(_) => None,
    }
}

/// The ink and glyphs of laid-out lines at `em`.
struct Measured {
    /// Left, top, right, bottom in pixels from the pen origin, y down.
    ink: [i64; 4],
    advance: f32,
    glyphs: Vec<PlacedGlyph>,
}

/// Measures `lines` of `system` at `em`: each glyph's outline extracted and
/// checked; `None` without ink.
fn measure(
    system: &System,
    scale: &mut ScaleContext,
    outline: &mut Outline,
    points: &mut Vec<Point>,
    lines: &[LayoutLine],
    em: f32,
) -> Result<Option<Measured>, TextNote> {
    let window = Window::layout();
    let mut ink: Option<[f32; 4]> = None;
    let mut advance = 0.0_f32;
    let mut glyphs = Vec::new();
    let mut scaler: Option<(usize, swash::scale::Scaler<'_>)> = None;
    for line in lines {
        if !line.w.is_finite() || line.w.abs() > MAX_LAYOUT_COORD {
            return Err(TextNote::TooLarge);
        }
        advance = advance.max(line.w);
        for g in &line.glyphs {
            let x = g.x + g.font_size * g.x_offset;
            let y = g.y - g.font_size * g.y_offset;
            if !(x.is_finite()
                && y.is_finite()
                && x.abs() <= MAX_LAYOUT_COORD
                && y.abs() <= MAX_LAYOUT_COORD)
            {
                return Err(TextNote::TooLarge);
            }
            let Some(face) = system.faces.iter().position(|f| f.id == g.font_id) else {
                continue;
            };
            // One scaler per run of glyphs of one face: building one
            // parses the face.
            if scaler.as_ref().is_none_or(|(f, _)| *f != face) {
                let Some(f) = system.faces.get(face) else {
                    continue;
                };
                let builder = scale.builder(f.swash()).normalized_coords(&f.coords);
                scaler = Some((face, builder.size(em).hint(false).build()));
            }
            let Some((_, s)) = scaler.as_mut() else {
                continue;
            };
            // A glyph with no outline (a space) has no ink.
            if !s.scale_outline_into(swash::GlyphId::from(g.glyph_id), outline)
                || outline.points().is_empty()
            {
                continue;
            }
            let Some(b) = place_points(outline.points(), [x, y], window, points) else {
                return Err(TextNote::TooLarge);
            };
            ink = Some(ink.map_or(b, |i| {
                [
                    i[0].min(b[0]),
                    i[1].min(b[1]),
                    i[2].max(b[2]),
                    i[3].max(b[3]),
                ]
            }));
            glyphs.push(PlacedGlyph {
                face: u8::try_from(face).unwrap_or(u8::MAX),
                glyph: g.glyph_id,
                x,
                y,
            });
        }
    }
    let Some([l, t, r, b]) = ink else {
        return Ok(None);
    };
    // Inside ±2^20 pixels, so exact as integers.
    let ink = [
        l.floor() as i64,
        t.floor() as i64,
        r.ceil() as i64,
        b.ceil() as i64,
    ];
    if ink[2] <= ink[0] || ink[3] <= ink[1] {
        return Ok(None);
    }
    Ok(Some(Measured {
        ink,
        advance,
        glyphs,
    }))
}

/// Where outline points may lie, in pixels, y down: inclusive bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    min: [f32; 2],
    max: [f32; 2],
}

impl Window {
    /// Layout's: any coordinate within ±[`MAX_LAYOUT_COORD`].
    fn layout() -> Self {
        Self {
            min: [-MAX_LAYOUT_COORD; 2],
            max: [MAX_LAYOUT_COORD; 2],
        }
    }

    /// An image of `size` pixels plus [`RASTER_MARGIN`] on every side.
    pub(crate) fn image([w, h]: [u32; 2]) -> Self {
        Self {
            min: [-RASTER_MARGIN; 2],
            max: [w as f32 + RASTER_MARGIN, h as f32 + RASTER_MARGIN],
        }
    }

    /// Whether `p` is finite and inside: NaN fails every comparison.
    fn holds(&self, x: f32, y: f32) -> bool {
        x >= self.min[0] && x <= self.max[0] && y >= self.min[1] && y <= self.max[1]
    }
}

/// `points`, a glyph's outline in font orientation (y up) with its pen at
/// `pen` (y down), into `out` in pixels, y down, and their bounds (left,
/// top, right, bottom); `None`, with `out` left partial, when the pen or a
/// point is not finite or leaves `window`.
pub(crate) fn place_points(
    points: &[Point],
    [px, py]: [f32; 2],
    window: Window,
    out: &mut Vec<Point>,
) -> Option<[f32; 4]> {
    out.clear();
    out.reserve(points.len());
    let mut b = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for p in points {
        let (x, y) = (px + p.x, py - p.y);
        if !window.holds(x, y) {
            return None;
        }
        b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        out.push(Point::new(x, y));
    }
    b[0].is_finite().then_some(b)
}

/// Renders the glyph whose checked points (in image pixels, bounds `b`)
/// are `points` with `verbs` and adds its coverage into the A8 image `out`
/// of `size`: only the part inside the image.
fn composite(
    points: &[Point],
    verbs: &[Verb],
    b: [f32; 4],
    [w, h]: [u32; 2],
    out: &mut [u8],
) -> Result<(), RasterError> {
    // The bounds lie within the image plus the margin: small integers.
    let x0 = (b[0].floor().max(0.0)) as u32;
    let y0 = (b[1].floor().max(0.0)) as u32;
    let x1 = (b[2].ceil().min(w as f32)) as u32;
    let y1 = (b[3].ceil().min(h as f32)) as u32;
    if x1 <= x0 || y1 <= y0 {
        return Ok(());
    }
    let (mw, mh) = (x1 - x0, y1 - y0);
    let local: Vec<Point> = points
        .iter()
        .map(|p| Point::new(p.x - x0 as f32, p.y - y0 as f32))
        .collect();
    let mut mask =
        vec![0_u8; usize::try_from(u64::from(mw) * u64::from(mh)).map_err(|_| RasterError::Size)?];
    rasterize_checked(&local, verbs, [mw, mh], &mut mask)?;
    let (row, mw_us) = (
        usize::try_from(w).unwrap_or(0),
        usize::try_from(mw).unwrap_or(0),
    );
    for (my, src) in (0..).zip(mask.chunks_exact(mw_us.max(1))) {
        let start = usize::try_from(u64::from(y0 + my) * u64::from(w) + u64::from(x0))
            .unwrap_or(usize::MAX);
        let Some(dst) = out.get_mut(start..start.saturating_add(mw_us)) else {
            continue;
        };
        for (d, s) in dst.iter_mut().zip(src) {
            *d = d.saturating_add(*s);
        }
        let _ = row;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The calls that reached the rasterizer on this thread.
    pub(crate) static RASTER_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Renders `points` with `verbs` into the A8 `mask` of `size`, after
/// checking, right before zeno, that the mask is that size and every
/// point is finite and inside it plus [`RASTER_MARGIN`]: zeno converts each
/// to fixed point unchecked.
pub(crate) fn rasterize_checked(
    points: &[Point],
    verbs: &[Verb],
    [w, h]: [u32; 2],
    mask: &mut [u8],
) -> Result<(), RasterError> {
    let max = u64::from(MAX_LINE_W) * u64::from(MAX_LINE_H);
    let area = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || area > max || u64::try_from(mask.len()).ok() != Some(area) {
        return Err(RasterError::Size);
    }
    let window = Window::image([w, h]);
    if !points.iter().all(|p| window.holds(p.x, p.y)) {
        return Err(RasterError::Outline);
    }
    #[cfg(test)]
    RASTER_CALLS.with(|c| c.set(c.get() + 1));
    zeno::Mask::new((points, verbs))
        .size(w, h)
        .format(zeno::Format::Alpha)
        .render_into(mask, None);
    Ok(())
}

/// Builds a font system holding `faces` (family, data, face index), in
/// order: each registered under its synthetic family as
/// `Source::Binary` over its snapshot, then materialised, then given its
/// cap height.
fn build_system(
    faces: &[(&'static str, Arc<Vec<u8>>, u32)],
    materialise: Materialise,
) -> Result<System, String> {
    let mut db = fontdb::Database::new();
    let mut ids = Vec::with_capacity(faces.len());
    for (family, data, index) in faces {
        // Only ever `Source::Binary` over the immutable snapshot: never a
        // file or a shared mapping, so fontdb's unsafe mapping of a file
        // (`make_shared_face_data`) has nothing to map.
        let source: Arc<dyn AsRef<[u8]> + Sync + Send> = Arc::clone(data) as _;
        ids.push(db.push_face_info(fontdb::FaceInfo {
            id: fontdb::ID::dummy(),
            source: fontdb::Source::Binary(source),
            index: *index,
            families: vec![((*family).to_owned(), fontdb::Language::English_UnitedStates)],
            post_script_name: (*family).to_owned(),
            style: fontdb::Style::Normal,
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            monospaced: false,
        }));
    }
    let mut fonts =
        FontSystem::new_with_locale_and_db_and_fallback("ja-JP".to_owned(), db, NoFallback);
    let mut out = Vec::with_capacity(faces.len());
    for ((_, data, index), id) in faces.iter().zip(ids) {
        if !materialise(&mut fonts, id) {
            return Err("its face does not load".to_owned());
        }
        let font = swash::FontRef::from_index(data, usize::try_from(*index).unwrap_or(usize::MAX))
            .ok_or("swash cannot read its face")?;
        let coords = shaping_coords(data, *index)?;
        let cap_em = cap_height(data, *index, font, &coords)
            .ok_or("no cap height: no OS/2 cap height, H or ascender")?;
        out.push(Face {
            id,
            data: Arc::clone(data),
            offset: font.offset,
            key: font.key,
            coords,
            cap_em,
        });
    }
    let family = faces.first().map_or(DEFAULT_FAMILY, |f| f.0);
    Ok(System {
        fonts,
        family,
        faces: out,
    })
}

/// The normalized variation coordinates face `index` of `data` is shaped
/// at: cosmic-text shapes every face at a `wght` of 400
/// (`fontdb::Weight::NORMAL`, the weight its faces are registered and
/// asked for at), so its outlines, metrics and cap height are taken there
/// too. Empty for a face without variations.
fn shaping_coords(data: &[u8], index: u32) -> Result<Vec<swash::NormalizedCoord>, String> {
    let font = skrifa::FontRef::from_index(data, index).map_err(|e| e.to_string())?;
    let weight = f32::from(fontdb::Weight::NORMAL.0);
    let location = font.axes().location([(skrifa::Tag::new(b"wght"), weight)]);
    Ok(location.coords().iter().map(|c| c.to_bits()).collect())
}

/// Face `index` of `data`'s cap height in em at `coords`: a positive OS/2
/// `sCapHeight` (none before version 2), else the top of its own `H` above
/// the baseline, else a positive ascender.
///
/// The table metrics come from skrifa, its `MVAR` deltas applied at
/// `coords`; swash 0.2.10 reads `MVAR`'s delta-set indices swapped.
fn cap_height(
    data: &[u8],
    index: u32,
    font: swash::FontRef<'_>,
    coords: &[swash::NormalizedCoord],
) -> Option<f32> {
    let face = skrifa::FontRef::from_index(data, index).ok()?;
    let location: Vec<skrifa::instance::NormalizedCoord> = coords
        .iter()
        .map(|&c| skrifa::instance::NormalizedCoord::from_bits(c))
        .collect();
    let m = face.metrics(
        skrifa::instance::Size::unscaled(),
        skrifa::instance::LocationRef::new(&location),
    );
    let upem = f32::from(m.units_per_em);
    if upem <= 0.0 {
        return None;
    }
    let positive = |v: f32| (v.is_finite() && v > 0.0).then_some(v / upem);
    if let Some(c) = m.cap_height.and_then(positive) {
        return Some(c);
    }
    let h = font.charmap().map('H');
    if h != 0 {
        let mut ctx = ScaleContext::new();
        let mut scaler = ctx
            .builder(font)
            .normalized_coords(coords)
            .hint(false)
            .build();
        let mut outline = Outline::new();
        if scaler.scale_outline_into(h, &mut outline) {
            let top = outline
                .points()
                .iter()
                .map(|p| p.y)
                .fold(f32::NEG_INFINITY, f32::max);
            if let Some(c) = positive(top) {
                return Some(c);
            }
        }
    }
    positive(m.ascent)
}

#[cfg(test)]
#[path = "outline_tests.rs"]
mod tests;
