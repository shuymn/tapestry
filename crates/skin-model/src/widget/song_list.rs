//! The `song list` widget (docs/stage3.md R6, S10-09d): the selection
//! screen's rows over the snapshot's visible window, one placement per
//! slot, the cursor's row in the selected placement at the center slot.
//!
//! Slot `i` shows row `cursor + i − center` of the window. A slot whose
//! row lies outside the list draws nothing; the list never wraps. A slot
//! whose placement does not draw draws none of its parts. The parts draw
//! in passes over every slot: the bars, then the row texts, the lamps, the
//! levels and the labels.
//!
//! A part's track is an offset from its placement's anchor: the placement's
//! sampled rect's bottom-left corner, `(x, y + h)` with y down, as the
//! source draws every part at its bar's bottom-left in its y-up
//! coordinates (`select/BarRenderer.java:141-142`,
//! `skin/SkinImage.java:153-159`). A part's keys are written relative to
//! that corner: x as authored, y the top of the part below the corner
//! (`−(y + h)` of the source's y-up values).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;

use super::digits::{Align, Digits, DigitsDef};
use super::glyphs::GlyphLine;
use super::text::{FontRef, Overflow, Text, TextTable};
use super::{Brush, Frame, ImageRef, Starts, sample};
use crate::atlas::uv::UvTable;
use crate::expr::{Program, State};
use crate::layout::Anchor;
use crate::quad::{Blend, QuadBatch, RunKey};
use crate::selection::{OwnLamp, RowKind, RowScore, RowView};
use crate::skin::TimerRef;
use crate::track::Track;
use crate::widget::gauge::GaugeLook;

/// The most placements a song list has: the source's own
/// (`select/SkinBar.java:31`).
pub const MAX_PLACEMENTS: usize = 60;

/// The level styles: by `#DIFFICULTY` 1 to 5, and 0 without one.
pub const LEVEL_STYLES: usize = 6;

/// The kinds of row a song list draws, which choose a placement's image
/// and a row's text style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowStyle {
    Chart,
    Folder,
    /// The "all charts" row.
    All,
}

impl RowStyle {
    pub const COUNT: usize = 3;

    /// The style of a row of `kind`; none for a heading, which a skin's
    /// list never shows.
    #[must_use]
    pub const fn of(kind: RowKind) -> Option<Self> {
        match kind {
            RowKind::Chart => Some(Self::Chart),
            RowKind::Folder { .. } => Some(Self::Folder),
            RowKind::All { .. } => Some(Self::All),
            RowKind::Heading => None,
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

/// A row's own lamp, as a skin's lamp images are chosen: unplayed, failed,
/// a cleared gauge's look, or a full combo. A hazard clear shows as a full
/// combo, as the source's clear types do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LampKey {
    NoPlay,
    Failed,
    AssistEasy,
    Easy,
    Normal,
    Hard,
    ExHard,
    FullCombo,
}

impl LampKey {
    pub const COUNT: usize = 8;
    /// Every key, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::NoPlay,
        Self::Failed,
        Self::AssistEasy,
        Self::Easy,
        Self::Normal,
        Self::Hard,
        Self::ExHard,
        Self::FullCombo,
    ];

    /// The lamp a row's score shows: `NoPlay` without a best.
    #[must_use]
    pub const fn of(score: RowScore) -> Self {
        match score {
            RowScore::Unplayed | RowScore::NoBest { .. } => Self::NoPlay,
            RowScore::Best { lamp, .. } => match lamp {
                OwnLamp::Failed => Self::Failed,
                OwnLamp::FullCombo => Self::FullCombo,
                OwnLamp::Cleared { look, .. } => match look {
                    GaugeLook::AssistEasy => Self::AssistEasy,
                    GaugeLook::Easy => Self::Easy,
                    GaugeLook::Normal => Self::Normal,
                    GaugeLook::Hard => Self::Hard,
                    GaugeLook::ExHard => Self::ExHard,
                    GaugeLook::Hazard => Self::FullCombo,
                },
            },
        }
    }

    /// The key's index, its place in [`ALL`](Self::ALL).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// A chart row's labels, in the order they draw
/// (`select/BarRenderer.java:389-406`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowLabel {
    LongNotes,
    Mines,
    Random,
}

impl RowLabel {
    pub const COUNT: usize = 3;
    /// Every label, in index order.
    pub const ALL: [Self; Self::COUNT] = [Self::LongNotes, Self::Mines, Self::Random];
}

/// What a song list's part shares with an element: its track, its timer,
/// its visibility (a bool over the selection schema), its blend and its
/// pivot. Its keys are offsets from its placement's anchor.
#[derive(Clone, Debug)]
pub struct PartDef {
    pub track: Track,
    pub timer: TimerRef,
    pub visible: Option<Program>,
    pub blend: Blend,
    pub pivot: Anchor,
}

/// A slot's placement: the bar drawn behind its row, the sheet image for
/// each row style, `None` drawing no bar for that style. Its keys are the
/// slot's place, not an offset.
#[derive(Clone, Debug)]
pub struct PlacementDef {
    pub part: PartDef,
    pub images: [Option<u32>; RowStyle::COUNT],
}

/// A row text style: the row's text in a font.
#[derive(Clone, Debug)]
pub struct RowTextDef {
    pub part: PartDef,
    pub font: FontRef,
    pub align: Align,
    pub overflow: Overflow,
}

/// A level style: a chart row's level in a glyph strip.
#[derive(Clone, Debug)]
pub struct LevelDef {
    pub part: PartDef,
    pub digits: DigitsDef,
}

/// A lamp or label style: one sheet image.
#[derive(Clone, Debug)]
pub struct ImagePartDef {
    pub part: PartDef,
    pub image: u32,
}

/// A song list as a front end builds it.
#[derive(Clone, Debug)]
pub struct SongListDef {
    /// Per slot, `[unselected, selected]`: 1 to [`MAX_PLACEMENTS`].
    pub placements: Vec<[PlacementDef; 2]>,
    /// The cursor's slot, below the placements' count.
    pub center: u8,
    /// By [`RowStyle`].
    pub text: [Option<RowTextDef>; RowStyle::COUNT],
    /// By `#DIFFICULTY`, 0 without one.
    pub level: [Option<LevelDef>; LEVEL_STYLES],
    /// By [`LampKey::index`].
    pub lamp: [Option<ImagePartDef>; LampKey::COUNT],
    /// By [`RowLabel`].
    pub label: [Option<ImagePartDef>; RowLabel::COUNT],
}

impl SongListDef {
    /// Every part, placements first.
    pub(crate) fn parts(&self) -> impl Iterator<Item = &PartDef> {
        self.placements
            .iter()
            .flat_map(|p| p.iter().map(|p| &p.part))
            .chain(self.text.iter().flatten().map(|t| &t.part))
            .chain(self.level.iter().flatten().map(|l| &l.part))
            .chain(self.lamp.iter().flatten().map(|l| &l.part))
            .chain(self.label.iter().flatten().map(|l| &l.part))
    }

    /// Every sheet image a part names.
    pub(crate) fn images(&self) -> impl Iterator<Item = u32> + '_ {
        self.placements
            .iter()
            .flat_map(|p| p.iter().flat_map(|p| p.images.iter().flatten().copied()))
            .chain(self.lamp.iter().flatten().map(|l| l.image))
            .chain(self.label.iter().flatten().map(|l| l.image))
    }

    /// The quads one slot can push at worst: its bar, its text (one, or a
    /// field's glyphs in a bitmap font), its lamp, its level's cells and
    /// its labels.
    pub(crate) fn quads(&self) -> u32 {
        let text = self
            .text
            .iter()
            .flatten()
            .map(|t| match t.font {
                FontRef::Bitmap(_) => {
                    u32::try_from(super::text::MAX_TEXT_BYTES).unwrap_or(u32::MAX)
                }
                FontRef::Default | FontRef::File(_) => 1,
            })
            .max()
            .unwrap_or(0);
        let cells = self
            .level
            .iter()
            .flatten()
            .map(|l| u32::from(l.digits.cells))
            .max()
            .unwrap_or(0);
        let per_slot = 2_u32
            .saturating_add(text)
            .saturating_add(cells)
            .saturating_add(u32::try_from(RowLabel::COUNT).unwrap_or(u32::MAX));
        per_slot.saturating_mul(u32::try_from(self.placements.len()).unwrap_or(u32::MAX))
    }
}

/// Why a song list was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SongListError {
    #[error("the song list has no placement")]
    NoPlacements,
    #[error("the song list has {n} placements, more than {MAX_PLACEMENTS}")]
    TooManyPlacements { n: usize },
    #[error("the song list's center slot {center} is past its placements")]
    Center { center: u8 },
    #[error("a song list part's image does not exist")]
    UnknownImage,
    #[error("a song list part's timer does not exist")]
    UnknownTimer,
    #[error("a song list part's visibility is not a bool")]
    VisibleNotBool,
    #[error("a song list part's keys leave the coordinate range")]
    Coordinates,
    #[error("a song list row text's font does not exist")]
    UnknownFont,
    #[error("a song list row text has a key whose height is not positive or width is negative")]
    TextSize,
    #[error("a song list level")]
    Level(#[source] super::digits::NumberError),
}

/// A part as a build draws it.
#[derive(Clone, Debug)]
pub(crate) struct Part<T> {
    pub(crate) track: Track,
    pub(crate) slot: usize,
    pub(crate) visible: Option<Program>,
    pub(crate) blend: Blend,
    pub(crate) pivot: [f32; 2],
    pub(crate) content: T,
}

impl<T> Part<T> {
    pub(crate) fn new(def: PartDef, slot: usize, content: T) -> Self {
        Self {
            track: def.track,
            slot,
            visible: def.visible,
            blend: def.blend,
            pivot: def.pivot.fractions(),
            content,
        }
    }
}

/// A loaded placement: its part, and its image by row style.
pub(crate) type Placement = Part<[Option<ImageRef>; RowStyle::COUNT]>;

/// A loaded song list.
#[derive(Clone, Debug)]
pub(crate) struct SongList {
    /// Per slot, `[unselected, selected]`.
    pub(crate) placements: Box<[[Placement; 2]]>,
    pub(crate) center: usize,
    /// Each style's layout and font.
    pub(crate) text: [Option<Part<(Text, FontRef)>>; RowStyle::COUNT],
    pub(crate) level: [Option<Part<Digits>>; LEVEL_STYLES],
    pub(crate) lamp: [Option<Part<ImageRef>>; LampKey::COUNT],
    pub(crate) label: [Option<Part<ImageRef>>; RowLabel::COUNT],
}

/// What one slot shows: its row's style, lamp, level and labels, set when
/// the view changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowBind {
    pub(crate) style: Option<RowStyle>,
    pub(crate) lamp: Option<LampKey>,
    /// The level's value and its style's index.
    pub(crate) level: Option<(i32, usize)>,
    /// By [`RowLabel::ALL`]'s order: long notes, mines, random.
    pub(crate) labels: [bool; RowLabel::COUNT],
}

impl RowBind {
    /// What `row` shows; a heading shows nothing.
    pub(crate) fn of(row: &RowView) -> Self {
        let Some(style) = RowStyle::of(row.kind) else {
            return Self::default();
        };
        if style != RowStyle::Chart {
            return Self {
                style: Some(style),
                ..Self::default()
            };
        }
        let features = row.features;
        Self {
            style: Some(style),
            lamp: Some(LampKey::of(row.score)),
            level: Some((
                row.level_value,
                row.difficulty.map_or(0, usize::from).min(LEVEL_STYLES - 1),
            )),
            labels: [
                features.is_some_and(|f| f.long_notes),
                features.is_some_and(|f| f.mines),
                row.random,
            ],
        }
    }
}

/// What a song list's frame reads: the timers, the state its visibilities
/// read, the rows per slot, the row texts per slot, the uv table and a
/// brush carrying the element's region, color and clip.
pub(crate) struct ListDraw<'a> {
    pub(crate) starts: Starts<'a>,
    pub(crate) now: TimeUs,
    pub(crate) state: &'a State<'a>,
    pub(crate) rows: &'a [RowBind],
    pub(crate) text: &'a TextTable,
    pub(crate) bitmap: &'a [Option<GlyphLine>],
    pub(crate) uvs: &'a UvTable,
    pub(crate) brush: Brush,
}

impl SongList {
    /// Each slot's placement as this frame draws it, its brush and rect,
    /// sampled once: its visibility runs once per slot, as
    /// [`Skin::new_selection`](crate::Skin::new_selection) charges it. `None`
    /// for a slot without a row or whose placement does not draw.
    fn placed(&self, d: &ListDraw<'_>) -> [Option<(Brush, [f32; 4])>; MAX_PLACEMENTS] {
        let mut out = [None; MAX_PLACEMENTS];
        for (i, (placed, p)) in out.iter_mut().zip(self.placements.iter()).enumerate() {
            if d.rows.get(i).and_then(|r| r.style).is_none() {
                continue;
            }
            let [off, on] = p;
            let p = if i == self.center { on } else { off };
            *placed = self.place(d, p, None);
        }
        out
    }

    /// Draws every slot's parts, pass by pass. Allocates nothing.
    pub(crate) fn draw(&self, d: &ListDraw<'_>, out: &mut QuadBatch) {
        let placed = self.placed(d);
        let at = Frame {
            starts: d.starts,
            now: d.now,
            uvs: d.uvs,
        };
        // A slot's anchor: its placement's bottom-left corner.
        let slots = || {
            placed
                .iter()
                .zip(d.rows.iter())
                .enumerate()
                .filter_map(|(i, (p, r))| p.map(|(_, [x, y, _, h])| (i, [x, y + h], r)))
        };
        // The bars.
        for (i, (p, row)) in placed.iter().zip(d.rows.iter()).enumerate() {
            let (Some((brush, rect)), Some(style)) = (p, row.style) else {
                continue;
            };
            let Some([off, on]) = self.placements.get(i) else {
                continue;
            };
            let p = if i == self.center { on } else { off };
            let Some(image) = p.content.get(style.index()).copied().flatten() else {
                continue;
            };
            if let Some(f) = image.frame(d.starts, d.now, d.uvs) {
                brush.push(out, *rect, f, image.filter);
            }
        }
        // The row texts.
        for (i, anchor, row) in slots() {
            let Some(t) = row
                .style
                .and_then(|s| self.text.get(s.index()))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            let Some((brush, rect)) = self.place(d, t, Some(anchor)) else {
                continue;
            };
            let slot = u16::try_from(i).unwrap_or(u16::MAX);
            if t.content.0.bitmap.is_some() {
                if let Some(Some(line)) = d.bitmap.get(i) {
                    t.content.0.draw_glyphs(line, rect, &brush, d.uvs, out);
                }
            } else if let Some(image) = d.text.get(slot) {
                t.content.0.draw(image, rect, &brush, out);
            }
        }
        // The lamps.
        for (_, anchor, row) in slots() {
            let Some(l) = row
                .lamp
                .and_then(|k| self.lamp.get(k.index()))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            self.image(d, l, anchor, out);
        }
        // The levels.
        for (_, anchor, row) in slots() {
            let Some((v, style)) = row.level else {
                continue;
            };
            let Some(l) = self.level.get(style).and_then(Option::as_ref) else {
                continue;
            };
            if let Some((brush, rect)) = self.place(d, l, Some(anchor)) {
                l.content.draw(v, rect, &brush, &at, out);
            }
        }
        // The labels.
        for (_, anchor, row) in slots() {
            for (shown, l) in row.labels.iter().zip(self.label.iter()) {
                if let (true, Some(l)) = (*shown, l) {
                    self.image(d, l, anchor, out);
                }
            }
        }
    }

    /// Draws image part `p` at `anchor`.
    fn image(&self, d: &ListDraw<'_>, p: &Part<ImageRef>, anchor: [f32; 2], out: &mut QuadBatch) {
        if let Some((brush, rect)) = self.place(d, p, Some(anchor))
            && let Some(f) = p.content.frame(d.starts, d.now, d.uvs)
        {
            brush.push(out, rect, f, p.content.filter);
        }
    }

    /// Part `p`'s brush and rect this frame, moved to `anchor` when it is
    /// an offset; `None` while it does not draw.
    fn place<T>(
        &self,
        d: &ListDraw<'_>,
        p: &Part<T>,
        anchor: Option<[f32; 2]>,
    ) -> Option<(Brush, [f32; 4])> {
        let (_, s) = sample(
            d.starts,
            d.now,
            d.state,
            p.slot,
            p.visible.as_ref(),
            &p.track,
        )?;
        let [x, y, w, h] = s.rect;
        let [ax, ay] = anchor.unwrap_or([0.0, 0.0]);
        let brush = Brush {
            key: RunKey {
                blend: p.blend,
                clip: d.brush.key.clip,
            },
            pivot: p.pivot,
            angle: crate::skin::radians(s.angle),
            color: super::tint(s.color, d.brush.color),
            ..d.brush
        };
        Some((brush, [x + ax, y + ay, w, h]))
    }
}
