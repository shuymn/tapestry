//! The importer's budgets: every table the import grows is charged before
//! the work it guards, against the skin model's own limits, so the model
//! never sees more than it accepts and nothing grows past its cap.

use std::fmt;

use skin_model::{
    Charge, MAX_ELEMENTS, MAX_FILES, MAX_FONTS, MAX_IMAGES, MAX_SHEET_GLYPHS, MAX_SHEET_KERNINGS,
    MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS, MAX_TEXT_ELEMENTS, MAX_TRACK_KEYS,
    MAX_WHEN_TIMERS, MAX_WIDGET_QUADS, Program, SkinOps,
};

use super::report::Usage;

/// A budget or aggregate limit an import reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportLimit {
    /// More than [`MAX_ELEMENTS`] destinations.
    Elements,
    /// More than [`MAX_SKIN_KEYS`] track keys across the destinations.
    Keys,
    /// More than [`MAX_SKIN_OPS`] expression operations.
    Ops,
    /// More than [`MAX_WHEN_TIMERS`] `when` timers.
    Whens,
    /// More than [`MAX_IMAGES`] images.
    Images,
    /// More than [`MAX_SKIN_FRAMES`] frames across the images.
    Frames,
    /// More than [`MAX_FILES`] file admissions, failed ones included.
    FileAttempts,
    /// A variant's encoded bytes or texels past the sheet's totals.
    FileTotals,
    /// More than [`MAX_FONTS`] fonts, or their bytes past the total.
    Fonts,
    /// More than [`MAX_SHEET_GLYPHS`] glyphs across the bitmap fonts.
    Glyphs,
    /// More than [`MAX_SHEET_KERNINGS`] kerning pairs across the bitmap
    /// fonts.
    Kernings,
    /// More than [`MAX_TEXT_ELEMENTS`] text elements.
    Texts,
    /// More than [`MAX_WIDGET_QUADS`] widget quads a frame.
    WidgetQuads,
}

impl fmt::Display for ImportLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elements => write!(f, "more than {MAX_ELEMENTS} destinations"),
            Self::Keys => write!(f, "more than {MAX_SKIN_KEYS} track keys"),
            Self::Ops => write!(f, "more than {MAX_SKIN_OPS} expression operations"),
            Self::Whens => write!(f, "more than {MAX_WHEN_TIMERS} `when` timers"),
            Self::Images => write!(f, "more than {MAX_IMAGES} images"),
            Self::Frames => write!(f, "more than {MAX_SKIN_FRAMES} image frames"),
            Self::FileAttempts => write!(f, "more than {MAX_FILES} image file admissions"),
            Self::FileTotals => f.write_str("the image files' total bytes or texels"),
            Self::Fonts => write!(f, "more than {MAX_FONTS} fonts, or their total bytes"),
            Self::Glyphs => write!(f, "more than {MAX_SHEET_GLYPHS} bitmap glyphs"),
            Self::Kernings => write!(f, "more than {MAX_SHEET_KERNINGS} bitmap kerning pairs"),
            Self::Texts => write!(f, "more than {MAX_TEXT_ELEMENTS} text elements"),
            Self::WidgetQuads => write!(f, "more than {MAX_WIDGET_QUADS} widget quads"),
        }
    }
}

/// The running counters, each checked as it is charged.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    pub(crate) usage: Usage,
    ops: SkinOps,
}

/// `n` more on `count`, at most `cap`; unchanged when it would pass.
fn charge(count: &mut u32, n: usize, cap: usize, limit: ImportLimit) -> Result<(), ImportLimit> {
    let next = usize::try_from(*count)
        .unwrap_or(usize::MAX)
        .saturating_add(n);
    if next > cap {
        return Err(limit);
    }
    *count = u32::try_from(next).map_err(|_| limit)?;
    Ok(())
}

impl Budget {
    /// One destination.
    pub(crate) fn element(&mut self) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.elements,
            1,
            MAX_ELEMENTS,
            ImportLimit::Elements,
        )
    }

    /// A destination's `n` keys as written, at most [`MAX_TRACK_KEYS`]
    /// each, checked before the track is built: `Ok(false)` when this
    /// track alone has too many.
    pub(crate) fn keys(&mut self, n: usize) -> Result<bool, ImportLimit> {
        charge(&mut self.usage.keys, n, MAX_SKIN_KEYS, ImportLimit::Keys)?;
        Ok(n <= MAX_TRACK_KEYS)
    }

    /// One `when` timer, before its condition is compiled.
    pub(crate) fn when(&mut self) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.whens,
            1,
            MAX_WHEN_TIMERS,
            ImportLimit::Whens,
        )
    }

    /// A compiled program's operations, before it is stored.
    pub(crate) fn ops(&mut self, program: &Program) -> Result<(), ImportLimit> {
        self.ops.charge(program).map_err(|_| ImportLimit::Ops)?;
        self.usage.ops = u32::try_from(self.ops.total()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// One image of `frames` frames, before its file is admitted.
    pub(crate) fn image(&mut self, frames: u32) -> Result<(), ImportLimit> {
        charge(&mut self.usage.images, 1, MAX_IMAGES, ImportLimit::Images)?;
        self.frames(frames)
    }

    /// `frames` more frames.
    fn frames(&mut self, frames: u32) -> Result<(), ImportLimit> {
        let n = usize::try_from(frames).unwrap_or(usize::MAX);
        charge(
            &mut self.usage.frames,
            n,
            MAX_SKIN_FRAMES,
            ImportLimit::Frames,
        )
    }

    /// A charge of a bitmap font's admission: its attempts, its pages'
    /// images and frames. Its bytes are bounded by the sheet builder's
    /// totals, the same ones every font and image file counts against.
    pub(crate) fn fnt_charge(&mut self, c: Charge) -> Result<(), ImportLimit> {
        match c {
            Charge::FileAttempt => self.file_attempt(),
            Charge::Image => charge(&mut self.usage.images, 1, MAX_IMAGES, ImportLimit::Images),
            Charge::Frames(n) => self.frames(n),
            Charge::FontBytes(_) | Charge::ImageBytes { .. } => Ok(()),
        }
    }

    /// One file admission, failed ones and refused names included.
    pub(crate) fn file_attempt(&mut self) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.file_attempts,
            1,
            MAX_FILES,
            ImportLimit::FileAttempts,
        )
    }

    /// `quads` more widget quads a frame.
    pub(crate) fn widget_quads(&mut self, quads: u32) -> Result<(), ImportLimit> {
        let n = usize::try_from(quads).unwrap_or(usize::MAX);
        let cap = usize::try_from(MAX_WIDGET_QUADS).unwrap_or(usize::MAX);
        charge(
            &mut self.usage.widget_quads,
            n,
            cap,
            ImportLimit::WidgetQuads,
        )
    }

    /// One text element and its `quads` at worst.
    pub(crate) fn text(&mut self, quads: u32) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.texts,
            1,
            MAX_TEXT_ELEMENTS,
            ImportLimit::Texts,
        )?;
        self.widget_quads(quads)
    }
}

#[cfg(test)]
mod tests {
    use skin_model::{MAX_WHEN_TIMERS, PLAY, Scope, Ty, compile};

    use super::*;

    #[test]
    fn when_timers_are_capped() {
        let mut b = Budget::default();
        for _ in 0..MAX_WHEN_TIMERS {
            b.when().unwrap();
        }
        assert_eq!(b.when(), Err(ImportLimit::Whens));
        assert_eq!(b.usage.whens as usize, MAX_WHEN_TIMERS);
    }

    /// The image cap equals the element cap, so the mapping meets the
    /// element cap first; the image cap is charged here directly.
    #[test]
    fn images_are_capped() {
        let mut b = Budget::default();
        for _ in 0..MAX_IMAGES {
            b.image(1).unwrap();
        }
        assert_eq!(b.image(1), Err(ImportLimit::Images));
        assert_eq!(b.usage.images as usize, MAX_IMAGES);
        assert_eq!(b.usage.frames as usize, MAX_IMAGES);
    }

    #[test]
    fn a_track_over_the_key_cap_is_charged_and_flagged() {
        let mut b = Budget::default();
        assert_eq!(b.keys(MAX_TRACK_KEYS), Ok(true));
        assert_eq!(b.keys(MAX_TRACK_KEYS + 1), Ok(false));
        assert_eq!(b.usage.keys as usize, 2 * MAX_TRACK_KEYS + 1);
    }

    #[test]
    fn ops_are_charged_through_the_model() {
        let mut b = Budget::default();
        let p = compile("combo > 1", &Scope::new(&PLAY), Ty::Bool).unwrap();
        b.ops(&p).unwrap();
        assert_eq!(b.usage.ops as usize, p.op_count());
    }
}
