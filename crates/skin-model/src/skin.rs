//! A loaded skin: its elements in draw order, its `when` timers and its
//! images, and the per-frame build into a [`QuadBatch`].

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::Chart;

use crate::atlas::uv::{Cycle, UvTable};
use crate::atlas::{Atlas, AtlasError, AtlasLimits, Variant};
use crate::expr::{CompileError, Program, SkinOps};
use crate::image::Sheet;
use crate::play::schema::timer::BUILTIN;
use crate::play::timers::elapsed_us;
use crate::play::{Play, PlayContext, PrepareError, SkinFrame};
use crate::quad::{Blend, ClipId, Filter, Quad, QuadBatch, RunKey};
use crate::schema::Ty;
use crate::track::Track;

/// The most elements one skin may hold.
pub const MAX_ELEMENTS: usize = 16_384;
/// The most `when` timers one skin may hold; with the built-in timers,
/// every slot fits a `u16`.
pub const MAX_WHEN_TIMERS: usize = 1_024;
/// The most track keys across one skin's elements.
pub const MAX_SKIN_KEYS: usize = 65_536;

/// What an element draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The sheet's image at this index, at the frame its cycle shows.
    Image(u32),
}

/// The timer an element's track or an image's cycle runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerRef {
    /// A built-in timer's slot (`play::timer::*`, `lane_timer`).
    Builtin(u16),
    /// The skin's `when` timer at this index.
    When(u16),
}

/// One element as a front end builds it. Every element names its timer;
/// a front end gives an element without one `ready`.
#[derive(Clone, Debug)]
pub struct ElementDef {
    pub source: Source,
    pub track: Track,
    pub timer: TimerRef,
    /// Drawn only while this is true; compiled against the play schema.
    pub visible: Option<Program>,
    pub blend: Blend,
    pub clip: Option<ClipId>,
}

/// Why [`Skin::new`] refused a skin; `element` and `when` are indices.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("the skin has {n} elements, more than {MAX_ELEMENTS}")]
    TooManyElements { n: usize },
    #[error("the skin has {n} `when` timers, more than {MAX_WHEN_TIMERS}")]
    TooManyWhenTimers { n: usize },
    #[error("the skin's tracks have {n} keys, more than {MAX_SKIN_KEYS}")]
    TooManyKeys { n: usize },
    #[error("`when` timer {when}'s condition is not a bool")]
    WhenNotBool { when: usize },
    #[error("element {element}'s visibility is not a bool")]
    VisibleNotBool { element: usize },
    #[error("element {element}'s timer does not exist")]
    UnknownTimer { element: usize },
    #[error("element {element}'s track rotates, which is not drawn yet")]
    Rotation { element: usize },
    #[error("element {element}'s image does not exist")]
    UnknownImage { element: usize },
    #[error("image {image}'s cycle timer does not exist")]
    UnknownImageTimer { image: usize },
    #[error("the skin's expressions")]
    Ops(#[source] CompileError),
}

#[derive(Clone, Debug)]
struct Element {
    /// The image's first frame in the uv table, its frame count, cycle and
    /// filter, copied so a build never reads the sheet.
    first: u32,
    frames: u16,
    cycle: Option<Cycle>,
    filter: Filter,
    track: Track,
    /// The timer's slot: built-in, or `BUILTIN` plus the `when` index.
    slot: usize,
    visible: Option<Program>,
    key: RunKey,
}

/// A loaded skin.
#[derive(Clone, Debug)]
pub struct Skin {
    elements: Box<[Element]>,
    whens: Box<[Program]>,
    ops: SkinOps,
    sheet: Sheet,
}

/// The slot `timer` reads with `whens` `when` timers: a built-in slot, or
/// `BUILTIN` plus the `when` index.
fn slot_of(timer: TimerRef, whens: usize) -> Option<usize> {
    let builtin = usize::from(BUILTIN);
    match timer {
        TimerRef::Builtin(s) if usize::from(s) < builtin => Some(usize::from(s)),
        TimerRef::When(i) if usize::from(i) < whens => Some(builtin + usize::from(i)),
        TimerRef::Builtin(_) | TimerRef::When(_) => None,
    }
}

impl Skin {
    /// Loads a skin from `when` conditions, the sheet its elements draw
    /// from, and elements in draw order.
    ///
    /// Checks, in order: the counts (elements, `when` timers, keys across
    /// all tracks); every `when` condition and visibility is a bool; every
    /// timer exists; no track rotates; every element's image exists; every
    /// image's cycle timer exists; and the total operations, each
    /// `when` charged once however many elements share it, then each
    /// visibility in element order. So the per-frame cost is known here.
    ///
    /// These counts check what was built. A front end (an importer, the
    /// authored format's loader) enforces the same element, `when` and key
    /// limits cumulatively while it builds the input, before it compiles
    /// or allocates each item.
    ///
    /// # Errors
    ///
    /// The first check that fails.
    pub fn new(
        whens: Vec<Program>,
        sheet: Sheet,
        elements: Vec<ElementDef>,
    ) -> Result<Self, LoadError> {
        if elements.len() > MAX_ELEMENTS {
            return Err(LoadError::TooManyElements { n: elements.len() });
        }
        if whens.len() > MAX_WHEN_TIMERS {
            return Err(LoadError::TooManyWhenTimers { n: whens.len() });
        }
        let keys = elements
            .iter()
            .fold(0_usize, |n, e| n.saturating_add(e.track.key_count()));
        if keys > MAX_SKIN_KEYS {
            return Err(LoadError::TooManyKeys { n: keys });
        }
        if let Some(when) = whens.iter().position(|w| w.ty() != Ty::Bool) {
            return Err(LoadError::WhenNotBool { when });
        }
        if let Some(element) = elements
            .iter()
            .position(|e| e.visible.as_ref().is_some_and(|v| v.ty() != Ty::Bool))
        {
            return Err(LoadError::VisibleNotBool { element });
        }
        let mut slots = Vec::with_capacity(elements.len());
        for (element, e) in elements.iter().enumerate() {
            let slot = slot_of(e.timer, whens.len()).ok_or(LoadError::UnknownTimer { element })?;
            slots.push(slot);
        }
        if let Some(element) = elements.iter().position(|e| e.track.has_rotation()) {
            return Err(LoadError::Rotation { element });
        }
        let images = sheet.images();
        if let Some(element) = elements.iter().position(|e| {
            let Source::Image(i) = e.source;
            usize::try_from(i).map_or(true, |i| i >= images.len())
        }) {
            return Err(LoadError::UnknownImage { element });
        }
        let mut cycles = Vec::with_capacity(images.len());
        for (image, def) in images.iter().enumerate() {
            let cycle = match def.cycle {
                Some(c) => Some(Cycle {
                    period: c.period.0,
                    slot: slot_of(c.timer, whens.len())
                        .ok_or(LoadError::UnknownImageTimer { image })?,
                }),
                None => None,
            };
            cycles.push(cycle);
        }
        let mut ops = SkinOps::default();
        for program in whens
            .iter()
            .chain(elements.iter().filter_map(|e| e.visible.as_ref()))
        {
            ops.charge(program).map_err(LoadError::Ops)?;
        }
        let elements = elements
            .into_iter()
            .zip(slots)
            .map(|(e, slot)| {
                let Source::Image(i) = e.source;
                let i = usize::try_from(i).unwrap_or(usize::MAX);
                let image = images.get(i);
                Element {
                    first: image.map_or(0, |m| m.first),
                    frames: image.map_or(0, |m| m.frames),
                    cycle: cycles.get(i).copied().flatten(),
                    filter: image.map_or(Filter::Linear, |m| m.filter),
                    track: e.track,
                    slot,
                    visible: e.visible,
                    key: RunKey {
                        blend: e.blend,
                        clip: e.clip,
                    },
                }
            })
            .collect();
        Ok(Self {
            elements,
            whens: whens.into_boxed_slice(),
            ops,
            sheet,
        })
    }

    /// The frames of the sheet's images: the length of the uv table
    /// [`build`](Self::build) takes.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.sheet.frame_count()
    }

    /// The sheet the elements draw from.
    #[must_use]
    pub fn sheet(&self) -> &Sheet {
        &self.sheet
    }

    /// Builds the sheet's atlas for `variant` ([`Sheet::atlas`]).
    ///
    /// # Errors
    ///
    /// As [`Sheet::atlas`].
    pub fn atlas(&self, variant: Variant, limits: AtlasLimits) -> Result<Atlas, AtlasError> {
        self.sheet.atlas(variant, limits)
    }

    /// The operations every frame's expressions run.
    #[must_use]
    pub fn ops(&self) -> SkinOps {
        self.ops
    }

    /// The quads a frame can push: one per element.
    #[must_use]
    pub fn quad_capacity(&self) -> u32 {
        u32::try_from(self.elements.len()).unwrap_or(u32::MAX)
    }

    /// The runs a frame can push: at worst, one per quad.
    #[must_use]
    pub fn run_capacity(&self) -> u32 {
        self.quad_capacity()
    }

    /// A fresh [`Play`] for one attempt of `chart`; a retry or the next
    /// practice round calls this again and starts clean.
    ///
    /// # Errors
    ///
    /// A negative lead-in, a countdown start outside the time range, or a
    /// section whose initial gauges do not match the context's gauges.
    pub fn prepare(&self, chart: &Chart, ctx: &PlayContext<'_>) -> Result<Play, PrepareError> {
        Play::new(chart, ctx, self.whens.len())
    }

    /// Builds one frame: applies the drained effects, reads the snapshot,
    /// starts the timers, then appends each drawn element's quad to `out`
    /// in draw order (the caller clears `out`). Allocates nothing.
    ///
    /// `play` must come from this skin's [`prepare`](Self::prepare); one
    /// with another number of timers draws nothing. `uvs` must come from an
    /// atlas of this skin's sheet; a table of another sheet, or of another
    /// length, draws nothing.
    ///
    /// Each element shows its image's frame at the image's cycle timer:
    /// frame 0 while that timer is off, without a cycle, or with one frame.
    ///
    /// Conditions sample once per frame, after the effects. The snapshot
    /// can be newer than a drain the effect buffer split, so a frame may
    /// show a newer combo beside an older judgment. `gauge_max` reads off
    /// while the snapshot's gauge is below its maximum; if a below-maximum
    /// effect is dropped and the gauge later refills, it keeps the earlier
    /// start.
    pub fn build(
        &self,
        play: &mut Play,
        frame: &SkinFrame<'_>,
        uvs: &UvTable,
        out: &mut QuadBatch,
    ) {
        if play.slots() != usize::from(BUILTIN) + self.whens.len()
            || uvs.sheet_id() != self.sheet.id()
            || uvs.frames().len() != self.sheet.frame_count()
        {
            return;
        }
        play.fill(frame, &self.whens);
        let state = play.state();
        for e in &self.elements {
            let Some(start) = play.timer_at(e.slot) else {
                continue;
            };
            if e.visible.as_ref().is_some_and(|v| !v.eval_bool(&state)) {
                continue;
            }
            let Some(s) = e.track.sample(elapsed_us(frame.now, start)) else {
                continue;
            };
            let shown = e.cycle.map_or(0, |c| {
                play.timer_at(c.slot)
                    .map_or(0, |at| c.frame(elapsed_us(frame.now, at), e.frames))
            });
            let Some(f) = e.first.checked_add(shown).and_then(|i| uvs.get(i)) else {
                continue;
            };
            out.push(
                e.key,
                Quad {
                    rect: s.rect,
                    uv: f.uv,
                    color: s.color,
                    layer: f.layer,
                    filter: e.filter,
                },
            );
        }
    }
}
