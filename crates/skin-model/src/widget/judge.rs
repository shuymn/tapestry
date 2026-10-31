//! The `judge` widget: the latest judgment's grade image and, beside it,
//! the combo as a glyph strip placed relative to the grade's sampled rect.
//!
//! The reducer's latest judgment picks the slot; a PERFECT while
//! `gauge_max` is on resolves the grade image and the combo each to their
//! full-gauge slot, each falling back to PERFECT's on its own. Each part
//! runs its own track on its own timer, with its own pivot, blend and
//! visibility. The combo is the snapshot's `combo` and shows only above
//! 0. With `shift`, the grade moves left by half the combo's drawn width.
//! The element's own track gives color and alpha, multiplied into every
//! part's.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::digits::{Digits, DigitsDef, NumberError};
use super::{Brush, Frame, ImageRef, tint};
use crate::expr::{Program, State};
use crate::layout::Anchor;
use crate::place::{Affine, Bounds, LaneMap, union};
use crate::play::schema::{Grade, field, timer};
use crate::play::timers::elapsed_us;
use crate::quad::{Blend, ClipId, QuadBatch, RunKey};
use crate::skin::{MAX_COORD, TimerRef, radians};
use crate::track::Track;

/// A judge's slots: the grades, the empty POOR, and PERFECT at a full
/// gauge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeSlot {
    Perfect,
    Great,
    Good,
    Bad,
    Poor,
    EmptyPoor,
    /// A PERFECT while `gauge_max` is on; PERFECT's part when empty.
    PerfectAtMax,
}

impl JudgeSlot {
    /// The number of slots.
    pub const COUNT: usize = 7;
    /// Every slot, in index order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Perfect,
        Self::Great,
        Self::Good,
        Self::Bad,
        Self::Poor,
        Self::EmptyPoor,
        Self::PerfectAtMax,
    ];

    /// The slot's index into [`JudgeDef::grades`] and [`JudgeDef::combos`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    const fn of(g: Grade) -> Self {
        match g {
            Grade::Perfect => Self::Perfect,
            Grade::Great => Self::Great,
            Grade::Good => Self::Good,
            Grade::Bad => Self::Bad,
            Grade::Poor => Self::Poor,
            Grade::EmptyPoor => Self::EmptyPoor,
        }
    }
}

/// Which edge of the grade's sampled rect the combo's relative y is
/// measured from; x is always from its left edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach {
    Top,
    Bottom,
}

/// A slot's grade image.
#[derive(Clone, Debug)]
pub struct GradePart {
    pub image: u32,
    /// Corner-origin, like an element's.
    pub track: Track,
    pub timer: TimerRef,
    pub pivot: Anchor,
    pub blend: Blend,
    /// Drawn only while this is true; compiled against the play schema.
    pub visible: Option<Program>,
}

/// A slot's combo strip.
#[derive(Clone, Debug)]
pub struct ComboPart {
    pub digits: DigitsDef,
    /// Its `(x, y)` is relative to the grade's sampled rect before the
    /// shift: x to its left edge, y to the `attach` edge.
    pub track: Track,
    pub timer: TimerRef,
    pub attach: Attach,
    pub pivot: Anchor,
    pub blend: Blend,
    /// Drawn only while this is true; compiled against the play schema.
    pub visible: Option<Program>,
}

/// A judge: per [`JudgeSlot::index`], a grade image and a combo, each
/// optional.
#[derive(Clone, Debug)]
pub struct JudgeDef {
    pub grades: [Option<GradePart>; JudgeSlot::COUNT],
    pub combos: [Option<ComboPart>; JudgeSlot::COUNT],
    /// Moves the grade left by half the combo's drawn width.
    pub shift: bool,
}

/// A part of a judge, for errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgePart {
    Grade(JudgeSlot),
    Combo(JudgeSlot),
}

/// Why a judge was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    #[error("the judge has no grade image")]
    Empty,
    #[error("the judge element is bound to lanes")]
    Bound,
    #[error("the {slot:?} grade image does not exist")]
    UnknownImage { slot: JudgeSlot },
    #[error("the {part:?} timer does not exist")]
    UnknownTimer { part: JudgePart },
    #[error("the {part:?} visibility is not a bool")]
    VisibleNotBool { part: JudgePart },
    #[error("the {part:?} keys leave ±{MAX_COORD}")]
    Coordinates { part: JudgePart },
    #[error("the {slot:?} combo")]
    Combo {
        slot: JudgeSlot,
        #[source]
        error: NumberError,
    },
}

/// `parts`' entry for `slot`, PERFECT's standing in for an empty
/// full-gauge slot.
fn resolve<T>(parts: &[Option<T>; JudgeSlot::COUNT], slot: JudgeSlot) -> Option<&T> {
    let own = parts.get(slot.index()).and_then(Option::as_ref);
    if slot == JudgeSlot::PerfectAtMax {
        own.or_else(|| {
            parts
                .get(JudgeSlot::Perfect.index())
                .and_then(Option::as_ref)
        })
    } else {
        own
    }
}

impl JudgeDef {
    /// Every part's track with its part.
    pub(crate) fn tracks(&self) -> impl Iterator<Item = (JudgePart, &Track)> + '_ {
        let grades = JudgeSlot::ALL
            .into_iter()
            .zip(&self.grades)
            .filter_map(|(s, g)| g.as_ref().map(|g| (JudgePart::Grade(s), &g.track)));
        let combos = JudgeSlot::ALL
            .into_iter()
            .zip(&self.combos)
            .filter_map(|(s, c)| c.as_ref().map(|c| (JudgePart::Combo(s), &c.track)));
        grades.chain(combos)
    }

    /// Every part's timer with its part.
    pub(crate) fn timers(&self) -> impl Iterator<Item = (JudgePart, TimerRef)> + '_ {
        let grades = JudgeSlot::ALL
            .into_iter()
            .zip(&self.grades)
            .filter_map(|(s, g)| g.as_ref().map(|g| (JudgePart::Grade(s), g.timer)));
        let combos = JudgeSlot::ALL
            .into_iter()
            .zip(&self.combos)
            .filter_map(|(s, c)| c.as_ref().map(|c| (JudgePart::Combo(s), c.timer)));
        grades.chain(combos)
    }

    /// Every part's visibility with its part.
    pub(crate) fn visibles(&self) -> impl Iterator<Item = (JudgePart, &Program)> + '_ {
        let grades = JudgeSlot::ALL
            .into_iter()
            .zip(&self.grades)
            .filter_map(|(s, g)| Some((JudgePart::Grade(s), g.as_ref()?.visible.as_ref()?)));
        let combos = JudgeSlot::ALL
            .into_iter()
            .zip(&self.combos)
            .filter_map(|(s, c)| Some((JudgePart::Combo(s), c.as_ref()?.visible.as_ref()?)));
        grades.chain(combos)
    }

    /// The effective states: per slot without a full gauge, then PERFECT
    /// at a full gauge, the grade image and the combo each resolved.
    fn states(&self) -> impl Iterator<Item = (&GradePart, Option<&ComboPart>)> + '_ {
        JudgeSlot::ALL.into_iter().filter_map(|s| {
            let grade = resolve(&self.grades, s)?;
            Some((grade, resolve(&self.combos, s)))
        })
    }

    /// The quads one frame can push: the most over the effective states of
    /// the grade and its combo's cells.
    pub(crate) fn quads(&self) -> u32 {
        self.states()
            .map(|(_, c)| 1 + c.map_or(0, |c| u32::from(c.digits.cells)))
            .max()
            .unwrap_or(0)
    }

    /// The logical box: over the effective states, the grade's keys, the
    /// combo's strip boxes moved by the range of the grade's corner on the
    /// attach edge, and with `shift` the grade's box widened on both sides
    /// by half the widest strip.
    pub(crate) fn bounds(&self) -> Option<Bounds> {
        let mut out = None;
        for (grade, combo) in self.states() {
            let Some(mut g) = grade.track.bounds() else {
                continue;
            };
            if let Some(c) = combo {
                // The range of the grade's corner on the attach edge.
                let mut corner: Option<Bounds> = None;
                for [x, y, _, h] in grade.track.rects() {
                    let (x, y, h) = (i64::from(x), i64::from(y), i64::from(h));
                    let cy = match c.attach {
                        Attach::Top => y,
                        Attach::Bottom => y + h,
                    };
                    corner = Some(union(corner, [x, cy, x, cy]));
                }
                let strip = c.digits.bounds(&c.track);
                if let (Some([cx0, cy0, cx1, cy1]), Some([sx0, sy0, sx1, sy1])) = (corner, strip) {
                    out = Some(union(out, [cx0 + sx0, cy0 + sy0, cx1 + sx1, cy1 + sy1]));
                }
                if self.shift {
                    // Half the widest strip, rounded up.
                    let e = (c.digits.max_width(&c.track) + 1) / 2;
                    g = [g[0] - e, g[1], g[2] + e, g[3]];
                }
            }
            out = Some(union(out, g));
        }
        out
    }
}

/// A grade image as a build reads it.
#[derive(Clone, Debug)]
pub(crate) struct GradeImage {
    pub(crate) image: ImageRef,
    pub(crate) track: Track,
    pub(crate) slot: usize,
    pub(crate) pivot: [f32; 2],
    pub(crate) blend: Blend,
    pub(crate) visible: Option<Program>,
}

/// A combo as a build reads it.
#[derive(Clone, Debug)]
pub(crate) struct ComboStrip {
    pub(crate) digits: Digits,
    pub(crate) track: Track,
    pub(crate) slot: usize,
    pub(crate) attach: Attach,
    pub(crate) pivot: [f32; 2],
    pub(crate) blend: Blend,
    pub(crate) visible: Option<Program>,
}

/// A loaded judge.
#[derive(Clone, Debug)]
pub(crate) struct Judge {
    pub(crate) grades: [Option<GradeImage>; JudgeSlot::COUNT],
    pub(crate) combos: [Option<ComboStrip>; JudgeSlot::COUNT],
    pub(crate) shift: bool,
}

/// What a judge draws from this frame.
pub(crate) struct JudgeDraw<'a> {
    pub(crate) at: Frame<'a>,
    pub(crate) state: &'a State<'a>,
    pub(crate) affine: Affine,
    pub(crate) map: LaneMap,
    /// The element's color and clip.
    pub(crate) color: [f32; 4],
    pub(crate) clip: Option<ClipId>,
}

/// A part's track at `now`, when its timer is on and it is visible.
fn sample(
    d: &JudgeDraw<'_>,
    track: &Track,
    slot: usize,
    visible: Option<&Program>,
) -> Option<crate::track::Sample> {
    let start = d.at.play.timer_at(slot)?;
    if visible.is_some_and(|v| !v.eval_bool(d.state)) {
        return None;
    }
    track.sample(elapsed_us(d.at.now, start))
}

impl Judge {
    /// Draws the latest judgment's slot: the combo's cells first, then the
    /// grade. Nothing before the first judgment, for an empty slot, or
    /// while the grade is not drawn.
    pub(crate) fn draw(&self, d: &JudgeDraw<'_>, out: &mut QuadBatch) {
        let Some(grade) = d.at.play.judge() else {
            return;
        };
        let mut slot = JudgeSlot::of(grade);
        if slot == JudgeSlot::Perfect && d.at.play.timer_at(usize::from(timer::GAUGE_MAX)).is_some()
        {
            slot = JudgeSlot::PerfectAtMax;
        }
        let Some(g) = resolve(&self.grades, slot) else {
            return;
        };
        let Some(gs) = sample(d, &g.track, g.slot, g.visible.as_ref()) else {
            return;
        };
        let [gx, gy, gw, gh] = gs.rect;
        let combo = d
            .state
            .fields
            .get(usize::from(field::COMBO))
            .copied()
            .unwrap_or(0);
        let mut shift = 0.0_f32;
        if combo > 0
            && let Some(c) = resolve(&self.combos, slot)
            && let Some(cs) = sample(d, &c.track, c.slot, c.visible.as_ref())
        {
            let [cx, cy, cw, ch] = cs.rect;
            let top = match c.attach {
                Attach::Top => gy,
                Attach::Bottom => gy + gh,
            };
            let brush = Brush {
                key: RunKey {
                    blend: c.blend,
                    clip: d.clip,
                },
                affine: d.affine,
                map: d.map,
                pivot: c.pivot,
                angle: radians(cs.angle),
                color: tint(cs.color, d.color),
            };
            let rect = [gx + cx, top + cy, cw, ch];
            let Some(width) = c.digits.draw(combo, rect, &brush, &d.at, out) else {
                return;
            };
            if self.shift {
                shift = width / 2.0;
            }
        }
        let Some(f) = g.image.frame(d.at.play, d.at.now, d.at.uvs) else {
            return;
        };
        let brush = Brush {
            key: RunKey {
                blend: g.blend,
                clip: d.clip,
            },
            affine: d.affine,
            map: d.map,
            pivot: g.pivot,
            angle: radians(gs.angle),
            color: tint(gs.color, d.color),
        };
        brush.push(out, [gx - shift, gy, gw, gh], f, g.image.filter);
    }
}
