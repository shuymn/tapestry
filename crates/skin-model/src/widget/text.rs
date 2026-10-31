//! The `text` widget: one line of a chart field or a literal, rasterized
//! before play into an image whose metrics are in em, and drawn as one
//! quad.
//!
//! The element's sampled rect `[x, y, w, h]` is the box. The em size is
//! `h`, and the box's top is the font's cap height, which the rasterizer
//! folds into the image's ink rect (relative to the box top). The line's
//! width is `tw = width·h`; [`Overflow::Shrink`] squeezes it horizontally
//! to `w` when it is wider, and never enlarges it. The drawn width
//! `d = tw·sx` sits at the box's left edge, centre or right edge by
//! [`Align`]; spilled text overflows the box, centred text both ways. The
//! quad is the ink rect `[x + off + (ink.x − left)·h·sx, y + ink.y·h,
//! ink.w·h·sx, ink.h·h]`, turned about the element's pivot on the box.
//!
//! A text in a bitmap font ([`FontRef::Bitmap`]) is laid out before play
//! instead ([`glyphs`](super::glyphs)) and drawn as one quad per glyph,
//! by the same width, squeeze and alignment.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::borrow::Cow;

use super::Brush;
use super::digits::Align;
use super::glyphs::GlyphLine;
use crate::atlas::uv::{FrameUv, UvTable};
use crate::quad::{Filter, QuadBatch};

/// The most text elements one skin may hold.
pub const MAX_TEXT_ELEMENTS: usize = 64;
/// The longest literal, in bytes; a chart string is cut to it at a
/// character boundary when the requests are made.
pub const MAX_TEXT_BYTES: usize = 256;
/// The largest em a request asks for, in pixels.
pub const MAX_TEXT_EM_PX: f32 = 256.0;
/// The largest magnitude of a [`TextImage`]'s metrics, in em.
pub const MAX_TEXT_EM: f32 = 4_096.0;

/// A string of the chart's header: the static top-level record that the
/// select screen shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextField {
    Title,
    Subtitle,
    /// The title, then a space and the subtitle when it is not empty.
    FullTitle,
    Artist,
    Subartist,
    /// The artist, then a space and the subartist when it is not empty.
    FullArtist,
    Genre,
}

/// What a text element shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextValue {
    Field(TextField),
    /// 1 to [`MAX_TEXT_BYTES`] bytes with no control character.
    Literal(String),
}

/// The font a text element is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRef {
    /// The player's text font.
    Default,
    /// The sheet's font file at this index
    /// ([`Sheet::with_fonts`](crate::Sheet::with_fonts)).
    File(u32),
    /// The sheet's bitmap font at this index
    /// ([`SheetBuilder::add_bitmap_font`](crate::SheetBuilder::add_bitmap_font)),
    /// laid out by [`Skin::attach_bitmap_text`](crate::Skin::attach_bitmap_text).
    Bitmap(u32),
}

/// What a line wider than its box does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overflow {
    /// It overflows the box.
    Spill,
    /// It is squeezed horizontally to the box's width.
    Shrink,
}

/// A text widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextDef {
    pub value: TextValue,
    pub font: FontRef,
    /// Which edge of the box the line sits on: left, centre or right.
    pub align: Align,
    pub overflow: Overflow,
}

impl TextDef {
    /// The quads one frame of this text can push at worst: 1, or with a
    /// bitmap font one per character, a literal's or a field's
    /// [`MAX_TEXT_BYTES`].
    pub(crate) fn quads(&self) -> u32 {
        match (self.font, &self.value) {
            (FontRef::Bitmap(_), TextValue::Literal(s)) => {
                u32::try_from(s.chars().count()).unwrap_or(u32::MAX)
            }
            (FontRef::Bitmap(_), TextValue::Field(_)) => {
                u32::try_from(MAX_TEXT_BYTES).unwrap_or(u32::MAX)
            }
            (FontRef::Default | FontRef::File(_), _) => 1,
        }
    }
}

/// Why a text element was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("the text is bound to lanes")]
    Bound,
    #[error("the text fills the canvas")]
    Fill,
    #[error("a key's height is not positive or its width is negative")]
    Size,
    #[error("the literal is empty, longer than {MAX_TEXT_BYTES} bytes or has a control character")]
    Literal,
    #[error("the text's font does not exist")]
    UnknownFont,
}

/// The chart's header strings a text element may show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChartText<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    pub artist: &'a str,
    pub subartist: &'a str,
    pub genre: &'a str,
}

/// One line to rasterize for a text element's slot
/// ([`Skin::text_requests`](crate::Skin::text_requests)).
#[derive(Clone, Debug, PartialEq)]
pub struct TextRequest {
    /// The element's slot: text elements are numbered in draw order.
    pub slot: u16,
    /// Not empty, at most [`MAX_TEXT_BYTES`], no control character.
    pub text: String,
    pub font: FontRef,
    /// The em in target pixels, at most [`MAX_TEXT_EM_PX`].
    pub em_px: f32,
}

/// A rasterized line: where it lies in the atlas and its metrics in em,
/// relative to the pen origin on the box's top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextImage {
    uv: FrameUv,
    /// The image's left, top, width and height.
    ink: [f32; 4],
    /// The line's layout extent: its left edge (at most 0) and width,
    /// covering the advance and the ink.
    left: f32,
    width: f32,
}

impl TextImage {
    /// A line whose image is `uv`, with the ink rect `ink` (left, top,
    /// width, height) and the pen advance `advance`, both in em. The
    /// line's extent is derived: from `min(0, ink.x)` to
    /// `max(advance, ink.x + ink.w)`.
    ///
    /// `None` for a non-finite value, an ink width or height of 0 or less,
    /// a negative advance, or any value or derived extent beyond
    /// ±[`MAX_TEXT_EM`].
    #[must_use]
    pub fn new(uv: FrameUv, ink: [f32; 4], advance: f32) -> Option<Self> {
        let [x, _, w, h] = ink;
        let left = x.min(0.0);
        let width = advance.max(x + w) - left;
        let values = ink.into_iter().chain([advance, x + w, left, width]);
        let bounded = values
            .chain(uv.uv)
            .all(|v| v.is_finite() && v.abs() <= MAX_TEXT_EM);
        (bounded && w > 0.0 && h > 0.0 && advance >= 0.0).then_some(Self {
            uv,
            ink,
            left,
            width,
        })
    }

    /// Where the image lies in the atlas.
    #[must_use]
    pub fn uv(&self) -> FrameUv {
        self.uv
    }

    /// The line's width in em, ink and advance together.
    #[must_use]
    pub fn width(&self) -> f32 {
        self.width
    }
}

/// One attempt's rasterized lines, one per text slot; `None` draws
/// nothing for its slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextTable(Box<[Option<TextImage>]>);

impl TextTable {
    /// The lines, by slot.
    #[must_use]
    pub fn new(images: Vec<Option<TextImage>>) -> Self {
        Self(images.into_boxed_slice())
    }

    /// The number of slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the table has no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(&self, slot: u16) -> Option<&TextImage> {
        self.0.get(usize::from(slot)).and_then(Option::as_ref)
    }
}

/// Why [`Skin::attach_text`](crate::Skin::attach_text) refused a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextTableError {
    #[error("the table has {got} slots for {expected} text elements")]
    Len { expected: usize, got: usize },
}

/// Whether a literal is 1 to [`MAX_TEXT_BYTES`] bytes with no control
/// character (C0, DEL or C1).
pub(crate) fn literal_ok(s: &str) -> bool {
    (1..=MAX_TEXT_BYTES).contains(&s.len()) && !s.chars().any(char::is_control)
}

/// `s` without control characters (C0, DEL or C1), cut to
/// [`MAX_TEXT_BYTES`] at a character boundary.
pub(crate) fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT_BYTES));
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

impl TextValue {
    /// The string this value shows for `chart`, before cleaning: a
    /// literal or a single field borrowed, a joined pair owned.
    pub(crate) fn resolve<'a>(&'a self, chart: &ChartText<'a>) -> Cow<'a, str> {
        let join = |a: &'a str, b: &str| {
            if b.is_empty() {
                Cow::Borrowed(a)
            } else {
                Cow::Owned(format!("{a} {b}"))
            }
        };
        match self {
            Self::Literal(s) => Cow::Borrowed(s),
            Self::Field(f) => match f {
                TextField::Title => Cow::Borrowed(chart.title),
                TextField::Subtitle => Cow::Borrowed(chart.subtitle),
                TextField::FullTitle => join(chart.title, chart.subtitle),
                TextField::Artist => Cow::Borrowed(chart.artist),
                TextField::Subartist => Cow::Borrowed(chart.subartist),
                TextField::FullArtist => join(chart.artist, chart.subartist),
                TextField::Genre => Cow::Borrowed(chart.genre),
            },
        }
    }
}

/// How a text element lays out its line.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Text {
    pub(crate) slot: u16,
    pub(crate) align: Align,
    pub(crate) overflow: Overflow,
    /// The bitmap font, for a bitmap line.
    pub(crate) bitmap: Option<u32>,
}

impl Text {
    /// Pushes `image` laid out in the box `[x, y, w, h]`; nothing when the
    /// line is drawn 0 wide. Allocates nothing.
    pub(crate) fn draw(
        &self,
        image: &TextImage,
        [x, y, w, h]: [f32; 4],
        brush: &Brush,
        out: &mut QuadBatch,
    ) {
        let tw = image.width * h;
        let sx = if self.overflow == Overflow::Shrink && tw > w {
            w / tw
        } else {
            1.0
        };
        let d = tw * sx;
        if d <= 0.0 {
            return;
        }
        let off = match self.align {
            Align::Left => 0.0,
            Align::Center => (w - d) / 2.0,
            Align::Right => w - d,
        };
        let [ix, iy, iw, ih] = image.ink;
        let rect = [
            x + off + (ix - image.left) * h * sx,
            y + iy * h,
            iw * h * sx,
            ih * h,
        ];
        let [fx, fy] = brush.pivot;
        let pivot = [x + fx * w, y + fy * h];
        brush.push_about(out, rect, pivot, image.uv, Filter::Linear);
    }

    /// Pushes the glyphs of bitmap line `line` laid out in the box
    /// `[x, y, w, h]`, `h` the font's size: the line's width
    /// `tw = width·h`, squeezed to `w` by `sx` under [`Overflow::Shrink`]
    /// when wider, sits at the box's left, centre or right; each glyph's
    /// quad is `[x + off + ink.x·h·sx, y + ink.y·h, ink.w·h·sx,
    /// ink.h·h]`, turned about the element's pivot on the box. Each
    /// glyph's uv is found in its page's frame now, so a line never
    /// depends on how an atlas packed the pages; a missing frame draws
    /// nothing. Allocates nothing.
    pub(crate) fn draw_glyphs(
        &self,
        line: &GlyphLine,
        [x, y, w, h]: [f32; 4],
        brush: &Brush,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) {
        let tw = line.width_em * h;
        let sx = if self.overflow == Overflow::Shrink && tw > w {
            w / tw
        } else {
            1.0
        };
        let d = tw * sx;
        let off = match self.align {
            Align::Left => 0.0,
            Align::Center => (w - d) / 2.0,
            Align::Right => w - d,
        };
        let [fx, fy] = brush.pivot;
        let pivot = [x + fx * w, y + fy * h];
        for q in &line.quads {
            let Some(f) = uvs.get(q.frame) else {
                continue;
            };
            // A page is a whole frame: `(u1 − u0) / width` is exactly one
            // over the atlas side, so each uv is exact.
            let [u0, v0, u1, v1] = f.uv;
            let [pw, ph] = q.page.map(f32::from);
            let (ku, kv) = ((u1 - u0) / pw, (v1 - v0) / ph);
            let [gx, gy, gw, gh] = q.src.map(f32::from);
            let uv = FrameUv {
                uv: [
                    u0 + gx * ku,
                    v0 + gy * kv,
                    u0 + (gx + gw) * ku,
                    v0 + (gy + gh) * kv,
                ],
                layer: f.layer,
            };
            let [ix, iy, iw, ih] = q.ink;
            let rect = [x + off + ix * h * sx, y + iy * h, iw * h * sx, ih * h];
            brush.push_about(out, rect, pivot, uv, Filter::Linear);
        }
    }
}
