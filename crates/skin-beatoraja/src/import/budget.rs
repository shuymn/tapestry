//! The importer's budgets: every table the import grows is charged before
//! the work it guards, against the skin model's own limits, so the model
//! never sees more than it accepts and nothing grows past its cap.

use std::fmt;

use skin_model::{
    Charge, MAX_BAR_GROUPS, MAX_DIGIT_OFFSETS, MAX_ELEMENTS, MAX_FILES, MAX_FONTS,
    MAX_IMAGE_CHOICE_REFS, MAX_IMAGES, MAX_PLACEMENTS, MAX_SHEET_GLYPHS, MAX_SHEET_KERNINGS,
    MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS, MAX_TEXT_ELEMENTS, MAX_TOTAL_FILE_BYTES,
    MAX_TOTAL_FONT_BYTES, MAX_TRACK_KEYS, MAX_WHEN_TIMERS, MAX_WIDGET_QUADS, Program, SkinOps,
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
    /// More than [`MAX_IMAGE_CHOICE_REFS`] image choice entries, holes and
    /// repeats included.
    ChoiceRefs,
    /// More than [`MAX_BAR_GROUPS`] bar line groups, dropped ones
    /// included.
    BarGroups,
    /// More than [`MAX_DIGIT_OFFSETS`] digit offsets across the numbers
    /// and combos, each copy counted.
    DigitOffsets,
    /// More than [`MAX_PLACEMENTS`] song list placements, the row text
    /// table's slots.
    Placements,
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
            Self::ChoiceRefs => write!(f, "more than {MAX_IMAGE_CHOICE_REFS} image choice entries"),
            Self::BarGroups => write!(f, "more than {MAX_BAR_GROUPS} bar line groups"),
            Self::DigitOffsets => write!(f, "more than {MAX_DIGIT_OFFSETS} digit offsets"),
            Self::Placements => write!(f, "more than {MAX_PLACEMENTS} song list placements"),
        }
    }
}

/// The running counters, each checked as it is charged.
#[derive(Debug, Default)]
pub(crate) struct Budget {
    pub(crate) usage: Usage,
    ops: SkinOps,
    /// The bytes charged for bitmap font pages, low and high variant,
    /// failed admissions included: never refunded.
    page_bytes: [u64; 2],
    /// The bytes charged for `.fnt` files, failed ones included.
    font_bytes: u64,
}

/// `n` more bytes on `total`, at most `cap`; unchanged when it would pass.
fn charge_bytes(total: &mut u64, n: u64, cap: u64, limit: ImportLimit) -> Result<(), ImportLimit> {
    let next = total.checked_add(n).filter(|&t| t <= cap).ok_or(limit)?;
    *total = next;
    Ok(())
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

    /// A grade attachment track's `n` keys, emitted beside the keys its
    /// destination was charged as written, before its rects, keys or
    /// track are built: ordinary keys, also counted on their own.
    pub(crate) fn grade_anchor_keys(&mut self, n: usize) -> Result<(), ImportLimit> {
        charge(&mut self.usage.keys, n, MAX_SKIN_KEYS, ImportLimit::Keys)?;
        // At most the keys charged, so within `u32`.
        self.usage.grade_anchor_keys = self
            .usage
            .grade_anchor_keys
            .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        Ok(())
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
        self.ops_times(program, 1)
    }

    /// A program's operations `n` times over: a song list element
    /// evaluates a part's visibility once per slot.
    pub(crate) fn ops_times(&mut self, program: &Program, n: usize) -> Result<(), ImportLimit> {
        for _ in 0..n {
            self.ops.charge(program).map_err(|_| ImportLimit::Ops)?;
        }
        self.usage.ops = u32::try_from(self.ops.total()).unwrap_or(u32::MAX);
        Ok(())
    }

    /// The song list's `n` placements, the row text table's slots, before
    /// any placement is read.
    pub(crate) fn placements(&mut self, n: usize) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.placements,
            n,
            MAX_PLACEMENTS,
            ImportLimit::Placements,
        )
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

    /// A charge of a bitmap font's admission: its attempts, its file's
    /// bytes, its pages' bytes, images and frames. Every charge stays
    /// spent when the admission later fails; the bytes are kept here
    /// against the sheet's totals, since the builder counts only what it
    /// keeps.
    pub(crate) fn fnt_charge(&mut self, c: Charge) -> Result<(), ImportLimit> {
        match c {
            Charge::FileAttempt => self.file_attempt(),
            Charge::Image => charge(&mut self.usage.images, 1, MAX_IMAGES, ImportLimit::Images),
            Charge::Frames(n) => self.frames(n),
            Charge::FontBytes(n) => charge_bytes(
                &mut self.font_bytes,
                n,
                MAX_TOTAL_FONT_BYTES,
                ImportLimit::Fonts,
            ),
            Charge::ImageBytes { lo, hi } => {
                let [mut l, mut h] = self.page_bytes;
                let cap = MAX_TOTAL_FILE_BYTES;
                charge_bytes(&mut l, lo, cap, ImportLimit::FileTotals)?;
                charge_bytes(&mut h, hi, cap, ImportLimit::FileTotals)?;
                self.page_bytes = [l, h];
                Ok(())
            }
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

    /// `quads` more widget quads a frame; a gauge charges its segments and
    /// flicker tip before its nodes are looked up or admitted.
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

    /// An image choice table's `n` entries, holes and repeats included,
    /// before the table is reserved or its images admitted.
    pub(crate) fn choice_refs(&mut self, n: usize) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.choice_refs,
            n,
            MAX_IMAGE_CHOICE_REFS,
            ImportLimit::ChoiceRefs,
        )
    }

    /// The lanes' `n` bar line groups, before any is read.
    pub(crate) fn bar_groups(&mut self, n: usize) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.bar_groups,
            n,
            MAX_BAR_GROUPS,
            ImportLimit::BarGroups,
        )
    }

    /// A strip's `n` retained digit offsets, before they are allocated or
    /// copied into its definition; spent even when the element is later
    /// refused.
    pub(crate) fn digit_offsets(&mut self, n: usize) -> Result<(), ImportLimit> {
        charge(
            &mut self.usage.digit_offsets,
            n,
            MAX_DIGIT_OFFSETS,
            ImportLimit::DigitOffsets,
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

    #[test]
    fn digit_offsets_are_capped() {
        let mut b = Budget::default();
        b.digit_offsets(MAX_DIGIT_OFFSETS - 1).unwrap();
        b.digit_offsets(1).unwrap();
        assert_eq!(b.digit_offsets(1), Err(ImportLimit::DigitOffsets));
        assert_eq!(b.usage.digit_offsets as usize, MAX_DIGIT_OFFSETS);
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
    fn bar_groups_are_capped() {
        let mut b = Budget::default();
        b.bar_groups(MAX_BAR_GROUPS).unwrap();
        assert_eq!(b.bar_groups(1), Err(ImportLimit::BarGroups));
        assert_eq!(b.usage.bar_groups as usize, MAX_BAR_GROUPS);
    }

    #[test]
    fn a_track_over_the_key_cap_is_charged_and_flagged() {
        let mut b = Budget::default();
        assert_eq!(b.keys(MAX_TRACK_KEYS), Ok(true));
        assert_eq!(b.keys(MAX_TRACK_KEYS + 1), Ok(false));
        assert_eq!(b.usage.keys as usize, 2 * MAX_TRACK_KEYS + 1);
    }

    #[test]
    fn grade_anchor_keys_share_the_key_cap() {
        let mut b = Budget::default();
        b.keys(MAX_SKIN_KEYS - 3).unwrap();
        b.grade_anchor_keys(3).unwrap();
        assert_eq!(b.grade_anchor_keys(1), Err(ImportLimit::Keys));
        assert_eq!(b.usage.keys as usize, MAX_SKIN_KEYS);
        assert_eq!(b.usage.grade_anchor_keys, 3);
    }

    #[test]
    fn ops_are_charged_through_the_model() {
        let mut b = Budget::default();
        let p = compile("combo > 1", &Scope::new(&PLAY), Ty::Bool).unwrap();
        b.ops(&p).unwrap();
        assert_eq!(b.usage.ops as usize, p.op_count());
    }
}
