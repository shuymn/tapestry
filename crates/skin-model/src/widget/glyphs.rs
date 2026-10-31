//! Bitmap text: a line laid out with a bitmap font before play, as the
//! source player's libGDX 1.9.9 lays out one run without tight bounds,
//! in em of the primary font's size. It allocates; the per-frame draw is
//! [`Text::draw_glyphs`](super::text::Text::draw_glyphs).
//!
//! The pen starts at 0 and moves by each glyph's advance, plus the
//! kerning to the next character when the glyph is the primary file's own
//! basic-plane glyph (a substituted glyph is a copy without kerning). The
//! line's width is the pen after the last glyph's advance
//! (`BitmapFont$BitmapFontData.getGlyphs`, gdx 1.9.9 jar lines 801-813).
//! A glyph's ink is `[pen + xoffset, yoffset − adjust − ascent, width,
//! height]` over the size, top-origin from the box's top: the source draws
//! a glyph's top `ascent − yoffset` above the line's top
//! (`BitmapFontCache.addText`, jar line 512, and `addGlyph`, jar lines
//! 389-391), where a fallback glyph's offset moves by `adjust`, its
//! representative offset minus the primary's (`SkinTextBitmap.java:693,
//! 761` at `0.8.9`).
//!
//! A character the primary lacks is looked up in order
//! (`SkinTextBitmap.java:611-765`): each fallback file; for the wave dash
//! and the fullwidth tilde, the other one in the primary, then in each
//! fallback; `□ ▢ ☐ ■ ?` in the primary, then in each fallback; for a
//! basic-plane character, the primary's missing glyph. Otherwise it is
//! skipped.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use crate::canvas::f32_of;
use crate::image::Sheet;
use crate::image::fnt::{Fnt, FntGlyph};
use crate::widget::text::MAX_TEXT_EM;

/// The highest code of the basic plane.
const MAX_BMP: u32 = 0xFFFF;
const WAVE_DASH: u32 = 0x301C;
const FULLWIDTH_TILDE: u32 = 0xFF5E;
/// `□ ▢ ☐ ■ ?` (`SkinTextBitmap.java:350`).
const MISSING_CANDIDATES: [u32; 5] = [0x25A1, 0x25A2, 0x2610, 0x25A0, '?' as u32];

/// One glyph of a laid-out line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GlyphQuad {
    /// Left, top, width and height in em, from the pen origin on the
    /// box's top.
    pub(crate) ink: [f32; 4],
    /// The uv-table index of its page's frame.
    pub(crate) frame: u32,
    /// Its rectangle in its page, in pixels.
    pub(crate) src: [u16; 4],
    /// Its page's width and height, in pixels.
    pub(crate) page: [u16; 2],
}

/// A laid-out line: its width and its glyphs, in em.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GlyphLine {
    pub(crate) width_em: f32,
    pub(crate) quads: Box<[GlyphQuad]>,
}

/// A glyph found for a character: from which file, with which y adjust,
/// and whether it keeps its kerning.
#[derive(Clone, Copy)]
struct Pick<'a> {
    glyph: FntGlyph,
    font: &'a Fnt,
    adjust: i64,
    kerns: bool,
}

/// The glyph for `code` by the missing-glyph chain.
fn pick<'a>(primary: &'a Fnt, fallbacks: &[&'a Fnt], code: u32) -> Option<Pick<'a>> {
    let bmp = code <= MAX_BMP;
    let own = |code: u32| primary.file.glyph(code).copied();
    let copy = |glyph, font: &'a Fnt| Pick {
        glyph,
        font,
        adjust: i64::from(font.file.rep()) - i64::from(primary.file.rep()),
        kerns: false,
    };
    let in_fallbacks = |code: u32| {
        fallbacks
            .iter()
            .find_map(|f| f.file.glyph(code).map(|&g| copy(g, f)))
    };
    let primary_copy = |glyph| Pick {
        glyph,
        font: primary,
        adjust: 0,
        kerns: false,
    };
    if let Some(glyph) = own(code) {
        return Some(Pick {
            kerns: bmp,
            ..primary_copy(glyph)
        });
    }
    if let Some(p) = in_fallbacks(code) {
        return Some(p);
    }
    if bmp {
        let equivalent = match code {
            WAVE_DASH => Some(FULLWIDTH_TILDE),
            FULLWIDTH_TILDE => Some(WAVE_DASH),
            _ => None,
        };
        if let Some(e) = equivalent
            && let Some(p) = own(e).map(primary_copy).or_else(|| in_fallbacks(e))
        {
            return Some(p);
        }
    }
    if let Some(p) = MISSING_CANDIDATES
        .iter()
        .find_map(|&c| own(c).map(primary_copy))
    {
        return Some(p);
    }
    if let Some(p) = fallbacks.iter().find_map(|f| {
        MISSING_CANDIDATES
            .iter()
            .find_map(|&c| f.file.glyph(c).map(|&g| copy(g, f)))
    }) {
        return Some(p);
    }
    if bmp {
        return primary.file.missing().map(primary_copy);
    }
    None
}

/// `v` pixels in em of `size`, if within ±[`MAX_TEXT_EM`].
fn em(v: i64, size: f64) -> Option<f32> {
    // Every value is below 2^40: exact in f64.
    let e = v as f64 / size;
    (e.abs() <= f64::from(MAX_TEXT_EM)).then_some(f32_of(e))
}

/// Lays out `text` with bitmap font `font` of `sheet`; `None` for an
/// unknown font, or a line with a value beyond ±[`MAX_TEXT_EM`].
pub(crate) fn layout(sheet: &Sheet, font: u32, text: &str) -> Option<GlyphLine> {
    let chain = sheet.bitmap_fonts().get(usize::try_from(font).ok()?)?;
    let fonts = chain
        .iter()
        .map(|&f| sheet.fnts().get(usize::from(f)))
        .collect::<Option<Vec<_>>>()?;
    let (&primary, fallbacks) = fonts.split_first()?;
    let size = f64::from(primary.file.size());
    let ascent = i64::from(primary.file.ascent());
    let mut quads = Vec::with_capacity(text.chars().count());
    let mut pen = 0_i64;
    let mut last: Option<Pick<'_>> = None;
    for c in text.chars() {
        let code = u32::from(c);
        let Some(p) = pick(primary, fallbacks, code) else {
            continue;
        };
        if let Some(prev) = last {
            let kern = if prev.kerns && code <= MAX_BMP {
                i64::from(primary.file.kerning(prev.glyph.code, code))
            } else {
                0
            };
            pen += i64::from(prev.glyph.adv) + kern;
        }
        last = Some(p);
        let g = p.glyph;
        let [_, _, w, h] = g.src;
        if w == 0 || h == 0 {
            continue;
        }
        let page = p.font.pages.get(usize::from(g.page))?;
        let frame = sheet.images().get(usize::try_from(page.image).ok()?)?.first;
        let ink = [
            em(pen + i64::from(g.xoff), size)?,
            em(i64::from(g.raw_yoff) - p.adjust - ascent, size)?,
            em(i64::from(w), size)?,
            em(i64::from(h), size)?,
        ];
        quads.push(GlyphQuad {
            ink,
            frame,
            src: g.src,
            page: page.dims,
        });
    }
    let width = last.map_or(0, |l| pen + i64::from(l.glyph.adv));
    Some(GlyphLine {
        width_em: em(width, size)?,
        quads: quads.into_boxed_slice(),
    })
}
