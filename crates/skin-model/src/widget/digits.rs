//! Glyph strips: the `number` widget and the judge's combo draw a value as
//! a row of digit glyphs from one image.
//!
//! The layout follows the importers' rule. Cells fill from the right with
//! the magnitude's digits (`unsigned_abs`, so `i32::MIN` never
//! overflows); a value longer than the strip keeps its low digits. Under
//! [`GlyphSet::TwentyFour`] a negative value takes the second half of the
//! glyphs and the sign takes cell 0 with zero or glyph padding, or the
//! first blank left of the digits with blank padding (none when the digits
//! fill the strip). Without it, a negative value shows its magnitude.
//! Blank cells are always leading; the alignment moves the drawn cells
//! left by all of them (left) or half of them (centre).
//!
//! A strip may carry per-cell paint deltas ([`DigitOffset`]): constant
//! moves and resizes of each fixed cell's rect in target pixels, added
//! after the cell is placed and before its native size and pivot. They
//! change neither the pitch, the alignment, the drawn width a judge's
//! shift reads, nor the strip's logical box.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Brush, Frame, ImageRef};
use crate::expr::Program;
use crate::place::{Bounds, union};
use crate::quad::QuadBatch;
use crate::skin::MAX_COORD;
use crate::track::Track;

/// The digits of `u32::MAX`, which covers `|i32::MIN|`.
pub const I32_DIGITS: u8 = 10;
/// The most cells a strip holds: every digit of an `i32` and a sign.
pub const MAX_CELLS: u8 = I32_DIGITS + 1;

/// The most digit offsets a skin holds across its strips: every number
/// definition and every judge combo, referenced or not.
pub const MAX_DIGIT_OFFSETS: usize = 65_536;

/// A paint delta of one cell of a strip, in target pixels, y down: added
/// to the cell's placed rect `[x, y, w, h]` before its native size and
/// pivot. Constant paint data, never sampled or keyed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DigitOffset {
    pub dx: i32,
    pub dy: i32,
    pub dw: i32,
    pub dh: i32,
}

impl DigitOffset {
    /// Whether every component is within ±[`MAX_COORD`].
    fn in_range(self) -> bool {
        [self.dx, self.dy, self.dw, self.dh]
            .iter()
            .all(|v| (-MAX_COORD..=MAX_COORD).contains(v))
    }
}

/// The glyphs of one set, in frame order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlyphSet {
    /// The digits 0–9.
    Ten,
    /// The digits and a padding glyph (10).
    Eleven,
    /// Two halves of 12, for values ≥ 0 and < 0: each the digits, a
    /// padding glyph (10) and a sign (11, "+" then "−").
    TwentyFour,
}

impl GlyphSet {
    /// The glyphs per set.
    #[must_use]
    pub const fn glyphs(self) -> u16 {
        match self {
            Self::Ten => 10,
            Self::Eleven => 11,
            Self::TwentyFour => 24,
        }
    }
}

/// Where the drawn cells sit when some are blank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Right,
    Left,
    /// Half the blank cells' width left of `Right`, half a cell when their
    /// count is odd.
    Center,
}

/// What a cell left of the digits shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pad {
    Blank,
    Zero,
    /// The padding glyph (10); not with [`GlyphSet::Ten`].
    Glyph,
}

/// A glyph strip. Its element's sampled rect is cell 0, and cell `j` is
/// `j · (w + space)` to its right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigitsDef {
    /// The sheet image: `k · glyphs` frames hold `k` sets, cycled by the
    /// image's own cycle.
    pub image: u32,
    pub set: GlyphSet,
    /// 1 to [`MAX_CELLS`], a sign cell included.
    pub cells: u8,
    pub align: Align,
    pub pad: Pad,
    /// Reference pixels between cells, within ±[`MAX_COORD`].
    pub space: i32,
    /// Entry `j` moves cell `j`, counted from the left before the
    /// alignment, blank or not; a cell past the end does not move. At
    /// most `cells` entries, each component within ±[`MAX_COORD`].
    pub offsets: Vec<DigitOffset>,
}

/// A `number` element's strip and value, an `int` over the play schema.
#[derive(Clone, Debug)]
pub struct NumberDef {
    pub digits: DigitsDef,
    pub value: Program,
}

/// Why a glyph strip was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NumberError {
    #[error("the value is not an int")]
    NotInt,
    #[error("the image's frames are not a positive multiple of the set's glyphs")]
    Glyphs,
    #[error("the cells are not 1 to {MAX_CELLS}")]
    Cells,
    #[error("ten glyphs have no padding glyph")]
    PadGlyph,
    #[error("a signed strip with padding needs a cell besides the sign")]
    SignCells,
    #[error("the space leaves ±{MAX_COORD}")]
    Space,
    #[error("the image does not exist")]
    UnknownImage,
    #[error("{n} digit offsets for fewer cells")]
    OffsetCount { n: usize },
    #[error("digit offset {cell} leaves ±{MAX_COORD}")]
    OffsetRange { cell: usize },
}

impl DigitsDef {
    /// The image's checks: it exists with `frames` frames, a positive
    /// multiple of the set's glyphs.
    pub(crate) fn check_image(&self, frames: Option<u16>) -> Result<(), NumberError> {
        let frames = frames.ok_or(NumberError::UnknownImage)?;
        let g = self.set.glyphs();
        if frames == 0 || !frames.is_multiple_of(g) {
            return Err(NumberError::Glyphs);
        }
        Ok(())
    }

    /// The parameters' checks: cells, padding, sign cells, space.
    pub(crate) fn check(&self) -> Result<(), NumberError> {
        if !(1..=MAX_CELLS).contains(&self.cells) {
            return Err(NumberError::Cells);
        }
        if self.pad == Pad::Glyph && self.set == GlyphSet::Ten {
            return Err(NumberError::PadGlyph);
        }
        if self.set == GlyphSet::TwentyFour && self.pad != Pad::Blank && self.cells < 2 {
            return Err(NumberError::SignCells);
        }
        if !(-MAX_COORD..=MAX_COORD).contains(&self.space) {
            return Err(NumberError::Space);
        }
        let n = self.offsets.len();
        if n > usize::from(self.cells) {
            return Err(NumberError::OffsetCount { n });
        }
        if let Some(cell) = self.offsets.iter().position(|o| !o.in_range()) {
            return Err(NumberError::OffsetRange { cell });
        }
        Ok(())
    }

    /// The box of the strip whose cell 0 follows `track`: per key, the
    /// union of cell 0's and cell `cells − 1`'s rects, which holds the
    /// drawn cells of every value and alignment. Within ±2^25.
    pub(crate) fn bounds(&self, track: &Track) -> Option<Bounds> {
        track.rects().fold(None, |acc, [x, y, w, h]| {
            let (x, y, w, h) = (i64::from(x), i64::from(y), i64::from(w), i64::from(h));
            let c = (i64::from(self.cells) - 1) * (w + i64::from(self.space));
            let ends = [x, x + w, x + c, x + c + w];
            let x0 = ends.iter().copied().min().unwrap_or(x);
            let x1 = ends.iter().copied().max().unwrap_or(x);
            Some(union(acc, [x0, y.min(y + h), x1, y.max(y + h)]))
        })
    }

    /// The widest the strip whose cell 0 follows `track` can be drawn, as
    /// a magnitude: the most over its keys of `cells · |w + space|`. The
    /// sampled width lies between two keys', so this bounds every frame.
    pub(crate) fn max_width(&self, track: &Track) -> i64 {
        track
            .rects()
            .map(|[_, _, w, _]| {
                i64::from(self.cells) * (i64::from(w) + i64::from(self.space)).abs()
            })
            .max()
            .unwrap_or(0)
    }
}

/// The glyph of each cell of one value, the blank count and the half of
/// a twenty-four-glyph set it draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cells {
    pub(crate) glyph: [Option<u8>; MAX_CELLS as usize],
    pub(crate) blanks: u8,
    pub(crate) half: u8,
}

/// The sign's glyph within a twenty-four-glyph half.
const SIGN: u8 = 11;
/// The padding glyph.
const PAD: u8 = 10;

/// A strip as a build reads it.
#[derive(Clone, Debug)]
pub(crate) struct Digits {
    pub(crate) image: ImageRef,
    pub(crate) set: GlyphSet,
    pub(crate) cells: u8,
    pub(crate) align: Align,
    pub(crate) pad: Pad,
    pub(crate) space: f32,
    /// Per fixed cell; empty when no cell moves.
    pub(crate) offsets: Box<[DigitOffset]>,
}

impl Digits {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the space is within ±2^20, exact in f32"
    )]
    /// The strip of `d`, its offsets moved in, its image from `image`.
    pub(crate) fn new(d: DigitsDef, image: impl Fn(u32) -> ImageRef) -> Self {
        Self {
            image: image(d.image),
            set: d.set,
            cells: d.cells,
            align: d.align,
            pad: d.pad,
            space: d.space as f32,
            offsets: d.offsets.into_boxed_slice(),
        }
    }

    /// `v`'s cells.
    pub(crate) fn lay(&self, v: i32) -> Cells {
        let n = usize::from(self.cells.min(MAX_CELLS));
        let signed = self.set == GlyphSet::TwentyFour;
        let sign_cell = signed && self.pad != Pad::Blank;
        let mut out = Cells {
            glyph: [None; MAX_CELLS as usize],
            blanks: 0,
            half: if signed && v < 0 { 12 } else { 0 },
        };
        let mut m = v.unsigned_abs();
        // Whether the sign still waits for the first blank.
        let mut sign = signed;
        for j in (0..n).rev() {
            let glyph = if sign_cell && j == 0 {
                Some(SIGN)
            } else if m > 0 || j + 1 == n {
                let d = u8::try_from(m % 10).unwrap_or(0);
                m /= 10;
                Some(d)
            } else {
                match self.pad {
                    Pad::Zero => Some(0),
                    Pad::Glyph => Some(PAD),
                    Pad::Blank if sign => {
                        sign = false;
                        Some(SIGN)
                    }
                    Pad::Blank => None,
                }
            };
            if glyph.is_none() {
                out.blanks += 1;
            }
            if let Some(g) = out.glyph.get_mut(j) {
                *g = glyph;
            }
        }
        out
    }

    /// Draws `v` with cell 0 at `rect` through `brush`, the glyph set its
    /// image's cycle shows at `now`, each cell moved by its offset. A
    /// cell whose frame is missing is skipped. Returns the drawn width
    /// `(w + space) · drawn cells`, the offsets aside, or `None` when the
    /// batch refused a push, which stops the strip.
    pub(crate) fn draw(
        &self,
        v: i32,
        [x, y, w, h]: [f32; 4],
        brush: &Brush,
        at: &Frame<'_>,
        out: &mut QuadBatch,
    ) -> Option<f32> {
        let cells = self.lay(v);
        let g = self.set.glyphs();
        let sets = self.image.frames / g;
        let set = self.image.group(at.play, at.now, sets);
        let base = self
            .image
            .first
            .saturating_add(set.saturating_mul(u32::from(g)))
            .saturating_add(u32::from(cells.half));
        let p = f64::from(w) + f64::from(self.space);
        let b = f64::from(cells.blanks);
        let s = match self.align {
            Align::Right => 0.0,
            Align::Left => p * b,
            Align::Center => p * b / 2.0,
        };
        let n = usize::from(self.cells.min(MAX_CELLS));
        for (j, glyph) in (0_u8..).zip(cells.glyph.iter().take(n)) {
            let Some(glyph) = *glyph else {
                continue;
            };
            let Some(f) = at.uvs.get(base.saturating_add(u32::from(glyph))) else {
                continue;
            };
            let cx = crate::canvas::f32_of(f64::from(x) + p * f64::from(j) - s);
            let d = self
                .offsets
                .get(usize::from(j))
                .copied()
                .unwrap_or_default();
            if !brush.push_offset(out, [cx, y, w, h], d, f, self.image.filter) {
                return None;
            }
        }
        Some(crate::canvas::f32_of(p * (f64::from(self.cells) - b)))
    }
}

/// A loaded `number`.
#[derive(Clone, Debug)]
pub(crate) struct Number {
    pub(crate) digits: Digits,
    pub(crate) value: Program,
}
