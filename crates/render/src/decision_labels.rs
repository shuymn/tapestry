//! The decision screen's prepared labels (design.md "Play Lifecycle", the
//! decision screen; "Rendering and Skins", the text layer): two label
//! slots per committed decision installation, each a generation of the
//! chosen chart's lines prepared before the choice.
//!
//! **The region.** A decision installation with rasterized text keeps a
//! text layer with no cache: slot `b` owns the layer's half `b` (its
//! band), and each text element that may ask a line has a fixed row in
//! it ([`RowPlan`]): row `e` starts at the sum of the earlier rows'
//! heights, is `pad(min(1024, ⌈2 × em⌉))` tall (`pad(v)` adds both
//! gutters and rounds up to 8) and `min(side, 4096)` wide, and its line is
//! laid out within the row less its gutters. A plan whose rows pass the
//! band is refused ([`RenderError::LabelRegion`]); a line taller than its
//! row is [`TextNote::TooLarge`], its label not drawn. Nothing else
//! writes to the layer, and nothing is evicted.
//!
//! **The work.** A [`LabelWorker`] thread lays out, rasterizes and
//! expands a job's lines to exactly their padded size with mips 1 to 3
//! ([`stage_labels`]) and lays out its bitmap lines, before any write.
//! At frame boundaries the render thread claims a slot for the output
//! ([`LabelStore::claim`]), writes its rows at most
//! [`DECISION_TEXT_WRITE_BUDGET`] bytes at a time, each boundary's writes
//! inside one pair of error scopes ([`write_rows`]), submits once and
//! registers one flag-only completion, and publishes the slot only after a
//! poll that returned saw the flag, the device healthy, the key current
//! and every scope empty ([`settle`]). A preparation without rasterized
//! lines (a skin whose labels are all in bitmap fonts) writes nothing and
//! publishes at its boundary under the same device and key conditions.
//!
//! **Owners.** A slot's state lives in the [`LabelBook`], the one value
//! shared with the game thread, which pins a ready slot at the choice;
//! every change goes through its methods. Its content lives in the
//! render thread's [`LabelStore`], which a decision frame borrows by
//! `(slot, generation)` ([`LabelStore::content`]).
//!
//! **A rebuild.** The resets keep a pinned slot and its generation, but
//! not its content: the store keeps only its strings, so nothing of the
//! old device's layer is drawn, until its lines staged again are written
//! on the new renderer ([`restore_pinned`]).
//!
//! **Invariants.** Each is enforced at one place, named here:
//!
//! - *Supersession* ([`LabelStore::post`]): every post and cancel mints a
//!   new ticket in the worker, and only the newest ticket's work lives. A
//!   post first makes every older output stale, taken or not, then
//!   disposes the store's older waiting output and fill (the fill's slot
//!   cancelled), and only then hands its job to the worker. Publication
//!   ([`settle`]) is fenced by the same ticket, so nothing of an older post
//!   ever publishes.
//! - *Target identity* ([`LabelJob::new`], [`LabelBook::restored`]): a
//!   key's target is the target its rows were planned at ([`RowPlan`]
//!   records it), at a preparation and at a pinned slot's restore alike.
//! - *Device loss* ([`settle`]): `Gpu::lost()` is read after every poll
//!   that returned, before any slot is promoted and before any early
//!   return; a lost device marks the book unhealthy and is reported as
//!   [`RenderError::DeviceLost`].
//!
//! Preparation work allocates; reading a slot's content for a frame
//! allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use skin_model::{
    BitmapLines, DecisionLabels, DecisionStrings, DecisionTiming, FrameUv, GUTTER, Layout,
    MIP_LEVELS, Skin, TextImage, TextTable,
};

use crate::decl::AttemptText;
use crate::gpu::Gpu;
use crate::outline::{LineBounds, LineFit, MAX_LINE_H, MAX_LINE_W, TextNote};
use crate::renderer::RenderError;
use crate::skin_atlas::{self, AtlasTexture, UploadError};
use crate::text_upload::{self, TextScratch};

/// The most bytes the queue stages for a decision preparation's writes at
/// one frame boundary, rows padded as the queue pads them.
pub const DECISION_TEXT_WRITE_BUDGET: u64 = 1 << 20;

/// The label slots of a decision installation.
pub const LABEL_SLOTS: usize = 2;

/// A row's height before its gutters, in ems of its line (design decision:
/// one fixed rule; a taller line is a note, not a refusal).
const ROW_EMS: f32 = 2.0;

/// A label content's name: the generation of its preparation, any value
/// its caller gives it. A rebuild keeps a pinned slot's generation.
/// Supersession never reads it: a job is fenced by the ticket its post
/// minted ([`LabelStore::post`]), so no generation, zero included, can
/// alias a cancellation.
pub type Gen = u64;

/// The decision installation's text layer, as its label region: its side
/// and its layer in the array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LabelRegion {
    side: u32,
    layer: u32,
}

impl LabelRegion {
    pub(crate) const fn new(side: u32, layer: u32) -> Self {
        Self { side, layer }
    }

    /// Where slot `band`'s half of the layer starts.
    fn band_origin(self, band: usize) -> u32 {
        u32::try_from(band).unwrap_or(0) * (self.side / 2)
    }
}

/// A row of the label region: the slot of the text element it holds, the
/// em that element's line is laid out at, and its top and height within
/// a band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelRow {
    /// The text element's slot.
    pub slot: u16,
    /// The em in target pixels.
    pub em: f32,
    /// The row's top within its band.
    pub y: u32,
    /// The row's height, gutters included.
    pub h: u32,
}

/// The rows of a decision installation's label region at one target
/// ([`Renderer::decision_rows`](crate::Renderer::decision_rows)): one per
/// text element that may ask a rasterized line, in slot order. Empty for
/// an installation without rasterized text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowPlan {
    /// The target the rows were planned at: the target of every key whose
    /// lines are rasterized at these rows.
    target: [u32; 2],
    side: u32,
    width: u32,
    rows: Box<[LabelRow]>,
    slots: usize,
}

/// A row's height for a line of `em`: `pad(min(1024, ⌈2 × em⌉))`.
#[must_use]
pub fn row_height(em: f32) -> u32 {
    let v = (ROW_EMS * em).ceil();
    let v = if v.is_finite() && v > 0.0 {
        v.min(MAX_LINE_H as f32) as u32
    } else {
        0
    };
    text_upload::padded_side(v)
}

impl RowPlan {
    /// The rows of `rows` (each text element's `(slot, em)` at `target`)
    /// on a text layer `side` texels square (0 for none), for a skin of
    /// `slots` text slots.
    ///
    /// # Errors
    ///
    /// [`RenderError::LabelRegion`] when the rows pass half the layer.
    pub fn new(
        rows: &[(u16, f32)],
        side: u32,
        slots: usize,
        target: [u32; 2],
    ) -> Result<Self, RenderError> {
        let band = side / 2;
        let mut y = 0_u64;
        let mut out = Vec::with_capacity(rows.len());
        for &(slot, em) in rows {
            let h = row_height(em);
            out.push(LabelRow {
                slot,
                em,
                y: u32::try_from(y).unwrap_or(u32::MAX),
                h,
            });
            y += u64::from(h);
        }
        if y > u64::from(band) {
            return Err(RenderError::LabelRegion { needed: y, band });
        }
        Ok(Self {
            target,
            side,
            width: side.min(MAX_LINE_W),
            rows: out.into_boxed_slice(),
            slots,
        })
    }

    /// The rows, in slot order.
    #[must_use]
    pub fn rows(&self) -> &[LabelRow] {
        &self.rows
    }

    /// The rows' width: `min(side, 4096)`.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The text layer's side the plan was made for.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.side
    }

    /// The target the rows were planned at.
    #[must_use]
    pub const fn target(&self) -> [u32; 2] {
        self.target
    }

    /// The row of text slot `slot`.
    fn row(&self, slot: u16) -> Option<&LabelRow> {
        self.rows.iter().find(|r| r.slot == slot)
    }

    /// The bounds a row's line is laid out within: the row less its
    /// gutters.
    fn bounds(&self, row: &LabelRow) -> LineBounds {
        let gutter = 2 * GUTTER;
        LineBounds {
            w: self.width.saturating_sub(gutter),
            h: row.h.saturating_sub(gutter),
        }
    }

    /// The most bytes one generation's rows take on the CPU, every level:
    /// each row at the plan's width.
    #[must_use]
    pub fn staged_bound(&self) -> u64 {
        let w = u64::from(self.width);
        self.rows
            .iter()
            .map(|r| {
                (0..MIP_LEVELS)
                    .map(|k| (w >> k) * (u64::from(r.h) >> k) * 4)
                    .sum::<u64>()
            })
            .sum()
    }

    /// The most bytes a line's A8 pixels take: the widest line of the
    /// tallest row.
    #[must_use]
    pub fn mask_bound(&self) -> u64 {
        let tallest = self.rows.iter().map(|r| r.h).max().unwrap_or(0);
        let gutter = u64::from(2 * GUTTER);
        u64::from(self.width).saturating_sub(gutter) * u64::from(tallest).saturating_sub(gutter)
    }
}

/// What a decision label preparation is for: the chosen chart, its labels'
/// revision and the target its lines were rasterized at (its rows' target,
/// [`RowPlan::target`]; for a restored pin, the target of its restore).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelKey {
    /// The chart's path.
    pub chart: PathBuf,
    /// The labels' revision.
    pub revision: u64,
    /// The target's size.
    pub target: [u32; 2],
}

/// Why a slot is cooling: its last frame was drawn, or its writes were
/// superseded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoolFrom {
    /// Pinned by a lifetime that ended: ready again after its last frame
    /// completed, if its key is still current.
    Retired,
    /// Superseded after a write: free after its writes completed, never
    /// published.
    Cancelled,
}

/// A label slot's state.
#[derive(Clone, Debug, Default)]
pub enum LabelState {
    /// No content, claimable.
    #[default]
    Free,
    /// Claimed by a preparation being written: `epoch` is the book's when
    /// it was claimed.
    Filling {
        generation: Gen,
        key: LabelKey,
        epoch: u64,
        timing: DecisionTiming,
    },
    /// Published: pinnable, or claimable as the older one.
    Ready {
        generation: Gen,
        key: LabelKey,
        timing: DecisionTiming,
    },
    /// Drawn by a decision lifetime. `restoring` from a
    /// [`reset`](LabelBook::reset) until its content was restored on the
    /// renderer built again ([`restore_pinned`]): no content of it is
    /// drawable meanwhile, and a retire then frees it, never ready again.
    Pinned {
        generation: Gen,
        key: LabelKey,
        timing: DecisionTiming,
        lifetime: u64,
        restoring: bool,
    },
    /// Waiting for `done`, set when the GPU finished the work it watches.
    Cooling {
        generation: Gen,
        key: LabelKey,
        timing: DecisionTiming,
        done: Arc<AtomicBool>,
        from: CoolFrom,
    },
}

impl LabelState {
    /// Its generation, `None` when free.
    #[must_use]
    pub const fn generation(&self) -> Option<Gen> {
        match self {
            Self::Free => None,
            Self::Filling { generation, .. }
            | Self::Ready { generation, .. }
            | Self::Pinned { generation, .. }
            | Self::Cooling { generation, .. } => Some(*generation),
        }
    }
}

/// What [`LabelBook::publish`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish {
    /// The slot is ready.
    Published,
    /// Its key is no longer current (another target, or a rebuild since
    /// the claim): the slot is free.
    Stale,
    /// The device is not healthy: nothing changed.
    Unhealthy,
    /// The slot is not filling that generation: nothing changed.
    NotFilling,
}

/// The label slots' states, shared by the render thread, which prepares
/// them, and the game thread, which pins one at the choice: one mutex, so
/// a pin and a claim never both take a slot.
#[derive(Debug)]
pub struct LabelBook {
    healthy: bool,
    slots: [LabelState; LABEL_SLOTS],
    /// The decision lifetime active now, as the game set it.
    active: Option<u64>,
    /// The target the render thread draws at now.
    target: [u32; 2],
    /// Advanced at each rebuild.
    epoch: u64,
}

/// A completion watch: registers one flag-only completion for the work
/// submitted so far and returns its flag.
pub type Watch<'a> = &'a mut dyn FnMut() -> Arc<AtomicBool>;

impl LabelBook {
    /// Both slots free, the device healthy, at `target`.
    #[must_use]
    pub fn new(target: [u32; 2]) -> Self {
        Self {
            healthy: true,
            slots: [LabelState::Free, LabelState::Free],
            active: None,
            target,
            epoch: 0,
        }
    }

    /// [`new`](Self::new), shared.
    #[must_use]
    pub fn shared(target: [u32; 2]) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::new(target)))
    }

    /// Slot `slot`'s state.
    #[must_use]
    pub fn state(&self, slot: usize) -> Option<&LabelState> {
        self.slots.get(slot)
    }

    /// Whether the device was healthy when last seen.
    #[must_use]
    pub const fn healthy(&self) -> bool {
        self.healthy
    }

    /// The lifetime active now.
    #[must_use]
    pub const fn active(&self) -> Option<u64> {
        self.active
    }

    /// The target and the rebuild epoch the slots are current for.
    #[must_use]
    pub const fn current(&self) -> ([u32; 2], u64) {
        (self.target, self.epoch)
    }

    /// The device was seen lost: no slot is published or pinned until a
    /// [`reset`](Self::reset).
    pub fn set_unhealthy(&mut self) {
        self.healthy = false;
    }

    /// Whether a slot holds `key` already, ready, pinned or retired and
    /// cooling: a request for it needs no job.
    #[must_use]
    pub fn holds(&self, key: &LabelKey) -> bool {
        self.slots.iter().any(|s| match s {
            LabelState::Ready { key: k, .. } | LabelState::Pinned { key: k, .. } => k == key,
            LabelState::Cooling {
                key: k,
                from: CoolFrom::Retired,
                ..
            } => k == key,
            _ => false,
        })
    }

    /// Claims a slot for preparation `generation` of `key`: a free one, else the
    /// older ready one; never a filling, pinned or cooling one.
    pub fn claim(
        &mut self,
        generation: Gen,
        key: LabelKey,
        timing: DecisionTiming,
    ) -> Option<usize> {
        let free = self
            .slots
            .iter()
            .position(|s| matches!(s, LabelState::Free));
        let slot = free.or_else(|| {
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| match s {
                    LabelState::Ready { generation, .. } => Some((i, *generation)),
                    _ => None,
                })
                .min_by_key(|&(_, g)| g)
                .map(|(i, _)| i)
        })?;
        let epoch = self.epoch;
        let s = self.slots.get_mut(slot)?;
        *s = LabelState::Filling {
            generation,
            key,
            epoch,
            timing,
        };
        Some(slot)
    }

    /// Publishes `slot`'s preparation `generation`: ready, the other slot
    /// untouched, only while the device is healthy and its key current
    /// (the book's target, no rebuild since the claim); a stale one is
    /// freed.
    pub fn publish(&mut self, slot: usize, generation: Gen) -> Publish {
        let (target, epoch, healthy) = (self.target, self.epoch, self.healthy);
        let Some(s) = self.slots.get_mut(slot) else {
            return Publish::NotFilling;
        };
        let LabelState::Filling {
            generation: g,
            key,
            epoch: e,
            timing,
        } = s
        else {
            return Publish::NotFilling;
        };
        if *g != generation {
            return Publish::NotFilling;
        }
        if !healthy {
            return Publish::Unhealthy;
        }
        if key.target != target || *e != epoch {
            *s = LabelState::Free;
            return Publish::Stale;
        }
        *s = LabelState::Ready {
            generation,
            key: std::mem::replace(key, LabelKey::empty()),
            timing: *timing,
        };
        Publish::Published
    }

    /// Ends `slot`'s preparation `generation`, superseded or refused: free when
    /// nothing was `wrote` (or every write completed), else cooling until
    /// `watch`'s flag and then always free. Whether it was filling.
    pub fn cancel(&mut self, slot: usize, generation: Gen, wrote: bool, watch: Watch<'_>) -> bool {
        let Some(s) = self.slots.get_mut(slot) else {
            return false;
        };
        let LabelState::Filling {
            generation: g,
            key,
            timing,
            ..
        } = s
        else {
            return false;
        };
        if *g != generation {
            return false;
        }
        *s = if wrote {
            LabelState::Cooling {
                generation,
                key: std::mem::replace(key, LabelKey::empty()),
                timing: *timing,
                done: watch(),
                from: CoolFrom::Cancelled,
            }
        } else {
            LabelState::Free
        };
        true
    }

    /// The game's choice under `lifetime`: pins the ready slot for
    /// `chart` at `revision` and makes `lifetime` the active one, in one
    /// lock; `None` (nothing changed) when none is ready or the device is
    /// not healthy.
    pub fn pin(
        &mut self,
        chart: &Path,
        revision: u64,
        lifetime: u64,
    ) -> Option<(usize, Gen, DecisionTiming)> {
        if !self.healthy {
            return None;
        }
        let (slot, s) = self.slots.iter_mut().enumerate().find(|(_, s)| {
            matches!(s, LabelState::Ready { key, .. } if key.chart == chart && key.revision == revision)
        })?;
        let LabelState::Ready {
            generation,
            key,
            timing,
        } = s
        else {
            return None;
        };
        let (generation, timing) = (*generation, *timing);
        *s = LabelState::Pinned {
            generation,
            key: std::mem::replace(key, LabelKey::empty()),
            timing,
            lifetime,
            restoring: false,
        };
        self.active = Some(lifetime);
        Some((slot, generation, timing))
    }

    /// Makes `lifetime` the active one without a pin: a built-in screen's.
    pub fn begin(&mut self, lifetime: u64) {
        self.active = Some(lifetime);
    }

    /// The game ended `lifetime`: no lifetime is active if it was.
    pub fn end(&mut self, lifetime: u64) {
        if self.active == Some(lifetime) {
            self.active = None;
        }
    }

    /// Retires `slot` if it is pinned at `generation` by `lifetime` and that
    /// lifetime is no longer active: cooling until `watch`'s flag, set when
    /// the last frame that drew it completed; a slot still restoring cools
    /// as a cancelled one, free at its flag. Whether it was retired.
    pub fn retire(
        &mut self,
        slot: usize,
        generation: Gen,
        lifetime: u64,
        watch: Watch<'_>,
    ) -> bool {
        let active = self.active;
        let Some(s) = self.slots.get_mut(slot) else {
            return false;
        };
        let LabelState::Pinned {
            generation: g,
            key,
            timing,
            lifetime: l,
            restoring,
        } = s
        else {
            return false;
        };
        if *g != generation || *l != lifetime || active == Some(lifetime) {
            return false;
        }
        let from = if *restoring {
            CoolFrom::Cancelled
        } else {
            CoolFrom::Retired
        };
        *s = LabelState::Cooling {
            generation,
            key: std::mem::replace(key, LabelKey::empty()),
            timing: *timing,
            done: watch(),
            from,
        };
        true
    }

    /// Retires every pinned slot whose lifetime is not active, at a
    /// boundary.
    pub fn retire_ended(&mut self, watch: Watch<'_>) {
        for slot in 0..LABEL_SLOTS {
            if let Some(LabelState::Pinned {
                generation,
                lifetime,
                ..
            }) = self.slots.get(slot)
            {
                let (generation, lifetime) = (*generation, *lifetime);
                self.retire(slot, generation, lifetime, watch);
            }
        }
    }

    /// Settles every cooling slot whose flag is set (read after a poll
    /// that returned): a retired one with its key current is ready again,
    /// any other free.
    pub fn cool(&mut self) {
        let target = self.target;
        for s in &mut self.slots {
            let LabelState::Cooling {
                generation,
                key,
                timing,
                done,
                from,
            } = s
            else {
                continue;
            };
            if !done.load(Ordering::Acquire) {
                continue;
            }
            *s = if *from == CoolFrom::Retired && key.target == target {
                LabelState::Ready {
                    generation: *generation,
                    key: std::mem::replace(key, LabelKey::empty()),
                    timing: *timing,
                }
            } else {
                LabelState::Free
            };
        }
    }

    /// The target changed to `target`: every ready slot of another target
    /// is freed; a cooling one is freed at its flag, a pinned one kept.
    pub fn invalidate(&mut self, target: [u32; 2]) {
        self.target = target;
        for s in &mut self.slots {
            if matches!(s, LabelState::Ready { key, .. } if key.target != target) {
                *s = LabelState::Free;
            }
        }
    }

    /// The renderer was built again: a new epoch, every slot free but a
    /// pinned one, which keeps its generation and is restoring until its
    /// content is restored ([`restore_pinned`]), the device healthy.
    pub fn reset(&mut self) {
        self.epoch += 1;
        for s in &mut self.slots {
            match s {
                LabelState::Pinned { restoring, .. } => *restoring = true,
                _ => *s = LabelState::Free,
            }
        }
        self.healthy = true;
    }

    /// Whether `slot` is pinned at `generation` and restoring: its content
    /// waits for [`Renderer::restore_decision_labels`](crate::Renderer::restore_decision_labels).
    #[must_use]
    pub fn awaits_restore(&self, slot: usize, generation: Gen) -> bool {
        matches!(
            self.slots.get(slot),
            Some(LabelState::Pinned { generation: g, restoring: true, .. }) if *g == generation
        )
    }

    /// Pinned slot `slot`'s content at `generation` was restored, its
    /// lines rasterized at `target`: no longer restoring, and its key names
    /// `target`, its slot, generation and lifetime kept, so every later
    /// target comparison is against the target it was restored at. Whether
    /// it was restoring.
    pub(crate) fn restored(&mut self, slot: usize, generation: Gen, target: [u32; 2]) -> bool {
        match self.slots.get_mut(slot) {
            Some(LabelState::Pinned {
                generation: g,
                key,
                restoring,
                ..
            }) if *g == generation && *restoring => {
                key.target = target;
                *restoring = false;
                true
            }
            _ => false,
        }
    }
}

impl LabelKey {
    /// A key that names nothing, left behind when a state's key moves.
    const fn empty() -> Self {
        Self {
            chart: PathBuf::new(),
            revision: 0,
            target: [0, 0],
        }
    }
}

/// The book, locked; a poisoned lock is taken as it is, since every
/// change leaves it consistent.
pub fn lock(book: &Mutex<LabelBook>) -> MutexGuard<'_, LabelBook> {
    book.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A preparation asked of the worker. Its key's target is its rows'
/// target ([`RowPlan::target`]) by construction: the key names the target
/// its lines are rasterized at.
#[derive(Debug)]
pub struct LabelJob {
    generation: Gen,
    key: LabelKey,
    strings: DecisionStrings,
    plan: Arc<RowPlan>,
}

impl LabelJob {
    /// `chart`'s labels at `revision`, showing `strings`, laid out at
    /// `plan`'s rows: preparation `generation`, keyed at `plan`'s target.
    #[must_use]
    pub fn new(
        generation: Gen,
        chart: PathBuf,
        revision: u64,
        strings: DecisionStrings,
        plan: Arc<RowPlan>,
    ) -> Self {
        let key = LabelKey {
            chart,
            revision,
            target: plan.target(),
        };
        Self {
            generation,
            key,
            strings,
            plan,
        }
    }

    /// Its generation.
    #[must_use]
    pub const fn generation(&self) -> Gen {
        self.generation
    }

    /// Its key.
    #[must_use]
    pub const fn key(&self) -> &LabelKey {
        &self.key
    }
}

/// One rasterized line staged for its row: its padded size, the line's
/// pixels and metrics, and its levels until they are written.
#[derive(Debug)]
struct StagedLine {
    slot: u16,
    /// Its row's top within the band.
    y: u32,
    /// The rectangle's size: the line's, with its gutters, rounded up.
    rect: [u32; 2],
    /// The line's pixels.
    size: [u32; 2],
    ink: [f32; 4],
    advance: f32,
    levels: [Vec<u8>; MIP_LEVELS],
}

impl StagedLine {
    fn bytes(&self) -> u64 {
        let n: usize = self.levels.iter().map(Vec::capacity).sum();
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

/// A preparation's output: its lines rasterized and expanded, its notes,
/// its bitmap lines and the strings they were made from. It has one
/// owner: the worker, then the store's waiting output, then a filling
/// slot. It is current while its `ticket` is its worker's newest (`fence`):
/// a later post or cancel mints a newer one, and from then on it is
/// neither claimed, kept nor published ([`LabelStore::post`]). Staged
/// without a worker, its fence is its own, and it stays current.
#[must_use]
#[derive(Debug)]
pub struct StagedLabels {
    generation: Gen,
    ticket: u64,
    fence: Arc<AtomicU64>,
    key: LabelKey,
    timing: DecisionTiming,
    strings: DecisionStrings,
    lines: Vec<StagedLine>,
    notes: Vec<(u16, TextNote)>,
    bitmap: BitmapLines,
    slots: usize,
    side: u32,
}

impl StagedLabels {
    /// Its generation.
    #[must_use]
    pub const fn generation(&self) -> Gen {
        self.generation
    }

    /// Its key.
    #[must_use]
    pub const fn key(&self) -> &LabelKey {
        &self.key
    }

    /// Whether no post or cancel followed its own on its worker (always,
    /// staged without one).
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.fence.load(Ordering::Acquire) == self.ticket
    }

    /// One note per line without an image: its slot and why.
    #[must_use]
    pub fn notes(&self) -> &[(u16, TextNote)] {
        &self.notes
    }

    /// The rasterized lines' slots, in order.
    pub fn line_slots(&self) -> impl Iterator<Item = u16> + '_ {
        self.lines.iter().map(|l| l.slot)
    }

    /// The bytes its lines' levels still hold.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.lines.iter().map(StagedLine::bytes).sum()
    }
}

/// Lays out, rasterizes and expands `job`'s lines for its rows, and lays
/// out its bitmap lines, with `skin` and its `producer`, in `scratch`:
/// the worker's work, before any write. `superseded` is asked between
/// lines; `Ok(None)` when it answered yes. `building` counts the levels
/// made so far. A line without ink, or past its row
/// ([`TextNote::TooLarge`]) or the glyph limit, is a note; a skin without
/// a producer stages no rasterized line.
///
/// # Errors
///
/// The producer's refusal of a line ([`RenderError::Skin`]).
pub fn stage_labels(
    skin: &Skin,
    producer: Option<&dyn AttemptText>,
    job: LabelJob,
    scratch: &mut LabelScratch,
    superseded: &dyn Fn() -> bool,
) -> Result<Option<StagedLabels>, RenderError> {
    let plan = &*job.plan;
    let requests = skin.decision_text_requests_for(&job.strings, job.key.target, &Layout::AUTHOR);
    let mut lines = Vec::new();
    let mut notes = Vec::new();
    if let Some(producer) = producer {
        for r in &requests {
            if superseded() {
                return Ok(None);
            }
            let Some(row) = plan.row(r.slot) else {
                continue;
            };
            match producer.layout(r, row.em, plan.bounds(row)) {
                LineFit::Fits(l) => {
                    let (rect, levels) = text_upload::line_levels(producer, &l, &mut scratch.text)
                        .map_err(RenderError::Skin)?;
                    scratch.rasterized = true;
                    let line = StagedLine {
                        slot: r.slot,
                        y: row.y,
                        rect,
                        size: l.size,
                        ink: l.ink,
                        advance: l.advance,
                        levels,
                    };
                    scratch.building.fetch_add(line.bytes(), Ordering::Relaxed);
                    lines.push(line);
                }
                LineFit::Empty => {}
                LineFit::Refused(n) => notes.push((r.slot, n)),
            }
        }
    }
    if superseded() {
        return Ok(None);
    }
    let bitmap = skin.decision_bitmap_lines(&job.strings);
    Ok(Some(StagedLabels {
        generation: job.generation,
        ticket: 0,
        fence: Arc::new(AtomicU64::new(0)),
        key: job.key,
        timing: skin.decision_timing(),
        strings: job.strings,
        lines,
        notes,
        bitmap,
        slots: skin.text_slots(),
        side: plan.side,
    }))
}

/// [`stage_labels`] of `job` on this thread with `bundle`'s skin and
/// producer, never superseded: a rebuild's restaging of a pinned slot's
/// labels ([`Renderer::restore_decision_labels`](crate::Renderer::restore_decision_labels)).
/// Preparation work: it allocates.
///
/// # Errors
///
/// As [`stage_labels`].
pub fn stage_bundle_labels(
    bundle: &crate::SkinBundle,
    job: LabelJob,
) -> Result<Option<StagedLabels>, RenderError> {
    let producer = bundle.text_producer().map(|p| &**p);
    stage_labels(
        bundle.skin(),
        producer,
        job,
        &mut LabelScratch::default(),
        &|| false,
    )
}

/// The worker's own text buffers: a line's A8 pixels, kept, and the
/// levels it is expanding.
#[derive(Debug, Default)]
pub struct LabelScratch {
    text: TextScratch,
    /// Whether a line was rasterized: the producer's mask was live.
    rasterized: bool,
    /// The bytes of levels staged by the job in progress.
    building: Arc<AtomicU64>,
}

/// What the worker returned: its staged labels, or its refusal with the
/// generation it was for.
pub type LabelOutput = Result<StagedLabels, (Gen, RenderError)>;

#[derive(Debug, Default)]
struct Mailbox {
    /// The job and the ticket its post minted.
    job: Option<(u64, LabelJob)>,
    closed: bool,
}

/// Unit tests: what the worker runs at its steps.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct WorkerHooks {
    /// Run with a job's generation once it was staged, before the
    /// worker's last check.
    pub(crate) before_store: Option<Arc<dyn Fn(Gen) + Send + Sync>>,
    /// Run with a job's generation once the worker is done with it, and
    /// whether it stored an output.
    pub(crate) after_job: Option<Arc<dyn Fn(Gen, bool) + Send + Sync>>,
}

struct WorkerShared {
    mailbox: Mutex<Mailbox>,
    posted: Condvar,
    /// The newest ticket: minted by every post and cancel, never reused.
    /// Changed only with `output` locked, which is cleared in the same
    /// lock; the worker's last check before it stores an output is in that
    /// lock too, so `output` only ever holds the newest ticket's output.
    ticket: Arc<AtomicU64>,
    output: Mutex<Option<LabelOutput>>,
    /// The worker's A8 buffer's bytes, and its producer's mask bound once
    /// a line was rasterized.
    a8: AtomicU64,
    mask: AtomicU64,
    building: Arc<AtomicU64>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    hooks: WorkerHooks,
    /// Unit tests: set once the worker's thread was joined.
    #[cfg(test)]
    reaped: Arc<(Mutex<bool>, Condvar)>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One decision installation's preparation thread, with a one-slot
/// mailbox. Jobs reach it only through the store ([`LabelStore::post`],
/// [`LabelStore::cancel`]), which mint a new ticket for each: a job posted
/// replaces the one waiting and supersedes the one in progress, which
/// stops between lines, and an output of a superseded ticket is never
/// stored. Dropping it closes the thread without waiting for it: a thread
/// of its own joins it.
pub struct LabelWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for LabelWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabelWorker")
            .field("ticket", &self.shared.ticket.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl LabelWorker {
    /// The thread preparing `skin`'s labels with its `producer`; `wake`
    /// is called after each output.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub fn spawn(
        skin: Arc<Skin>,
        producer: Option<Arc<dyn AttemptText>>,
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> std::io::Result<Self> {
        Self::spawn_with(
            skin,
            producer,
            wake,
            #[cfg(test)]
            WorkerHooks::default(),
        )
    }

    /// Unit tests: [`spawn`](Self::spawn), with `hooks` run at the
    /// worker's steps.
    #[cfg(test)]
    pub(crate) fn spawn_hooked(
        skin: Arc<Skin>,
        producer: Option<Arc<dyn AttemptText>>,
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
        hooks: WorkerHooks,
    ) -> std::io::Result<Self> {
        Self::spawn_with(skin, producer, wake, hooks)
    }

    fn spawn_with(
        skin: Arc<Skin>,
        producer: Option<Arc<dyn AttemptText>>,
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
        #[cfg(test)] hooks: WorkerHooks,
    ) -> std::io::Result<Self> {
        let building = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(WorkerShared {
            mailbox: Mutex::new(Mailbox::default()),
            posted: Condvar::new(),
            ticket: Arc::new(AtomicU64::new(0)),
            output: Mutex::new(None),
            a8: AtomicU64::new(0),
            mask: AtomicU64::new(0),
            building: Arc::clone(&building),
            wake,
            #[cfg(test)]
            hooks,
            #[cfg(test)]
            reaped: Arc::new((Mutex::new(false), Condvar::new())),
        });
        let s = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("decision labels".to_owned())
            .spawn(move || {
                let mut scratch = LabelScratch {
                    building,
                    ..LabelScratch::default()
                };
                run(&s, &skin, producer.as_deref(), &mut scratch);
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// The thread preparing the labels of `bundle`'s skin with its
    /// producer ([`spawn`](Self::spawn)).
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub fn for_bundle(
        bundle: &crate::SkinBundle,
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> std::io::Result<Self> {
        Self::spawn(
            Arc::clone(bundle.skin()),
            bundle.text_producer().cloned(),
            wake,
        )
    }

    /// Mints the newest ticket and drops the output not taken, in one lock
    /// of the output: from here on every output staged under an older
    /// ticket, taken or not, is stale. The ticket minted.
    fn supersede(&self) -> u64 {
        let mut output = locked(&self.shared.output);
        let ticket = self.shared.ticket.fetch_add(1, Ordering::AcqRel) + 1;
        *output = None;
        ticket
    }

    /// Hands `job` to the thread under `ticket`, replacing a waiting job.
    fn place(&self, ticket: u64, job: LabelJob) {
        locked(&self.shared.mailbox).job = Some((ticket, job));
        self.shared.posted.notify_one();
    }

    /// Unit tests: a post that bypasses the store, so a test can see the
    /// publication fence alone.
    #[cfg(test)]
    pub(crate) fn post_unreconciled(&self, job: LabelJob) {
        let ticket = self.supersede();
        self.place(ticket, job);
    }

    /// Cancels every job, waiting or in progress, and drops the output not
    /// taken; every output taken before is stale.
    fn cancel(&self) {
        self.supersede();
        locked(&self.shared.mailbox).job = None;
    }

    /// The output of the newest job, once returned, `Ok` or `Err`. It is
    /// never an older post's: a post or cancel clears the output in the
    /// lock it mints its ticket in, so after [`LabelStore::post`] or
    /// [`LabelStore::cancel`] nothing older is returned.
    #[must_use]
    pub fn take(&self) -> Option<LabelOutput> {
        locked(&self.shared.output).take()
    }

    /// Unit tests: the generation of the output not taken.
    #[cfg(test)]
    pub(crate) fn output_generation(&self) -> Option<Gen> {
        locked(&self.shared.output).as_ref().map(|o| match o {
            Ok(s) => s.generation,
            Err((g, _)) => *g,
        })
    }

    /// Unit tests: set, with its condition notified, once the thread was
    /// joined after a drop.
    #[cfg(test)]
    pub(crate) fn reaped(&self) -> Arc<(Mutex<bool>, Condvar)> {
        Arc::clone(&self.shared.reaped)
    }

    /// The bytes the worker holds: its staged levels (the job in progress
    /// and the output not taken), and its A8 buffer with its producer's
    /// mask bound.
    #[must_use]
    pub fn bytes(&self) -> (u64, u64) {
        let output = locked(&self.shared.output)
            .as_ref()
            .and_then(|o| o.as_ref().ok())
            .map_or(0, StagedLabels::bytes);
        let staged = output.saturating_add(self.shared.building.load(Ordering::Relaxed));
        let mask = self
            .shared
            .a8
            .load(Ordering::Relaxed)
            .saturating_add(self.shared.mask.load(Ordering::Relaxed));
        (staged, mask)
    }
}

impl Drop for LabelWorker {
    /// Closes the thread without waiting for it: the line it may be
    /// working on is not interrupted, so a thread of its own joins it.
    fn drop(&mut self) {
        self.supersede();
        locked(&self.shared.mailbox).closed = true;
        self.shared.posted.notify_one();
        let Some(t) = self.thread.take() else {
            return;
        };
        #[cfg(test)]
        let reaped = Arc::clone(&self.shared.reaped);
        // Without a joiner the thread is detached: it ends after its line.
        let _ = std::thread::Builder::new()
            .name("decision labels join".to_owned())
            .spawn(move || {
                let _ = t.join();
                #[cfg(test)]
                {
                    *locked(&reaped.0) = true;
                    reaped.1.notify_all();
                }
            });
    }
}

/// The worker's loop: each job posted, until closed.
fn run(
    s: &WorkerShared,
    skin: &Skin,
    producer: Option<&dyn AttemptText>,
    scratch: &mut LabelScratch,
) {
    loop {
        let (ticket, job) = {
            let mut m = locked(&s.mailbox);
            loop {
                if m.closed {
                    return;
                }
                if let Some(job) = m.job.take() {
                    break job;
                }
                m = s.posted.wait(m).unwrap_or_else(PoisonError::into_inner);
            }
        };
        #[cfg(test)]
        let generation = job.generation;
        let stored = stage_one(s, skin, producer, scratch, ticket, job);
        #[cfg(test)]
        if let Some(hook) = &s.hooks.after_job {
            hook(generation, stored);
        }
        if stored && let Some(wake) = &s.wake {
            wake();
        }
    }
}

/// Stages `job`, posted under `ticket`, and stores its output unless a
/// newer ticket was minted meanwhile. Whether it stored one.
fn stage_one(
    s: &WorkerShared,
    skin: &Skin,
    producer: Option<&dyn AttemptText>,
    scratch: &mut LabelScratch,
    ticket: u64,
    job: LabelJob,
) -> bool {
    let generation = job.generation;
    let mask = job.plan.mask_bound();
    let superseded = || s.ticket.load(Ordering::Acquire) != ticket;
    let out = stage_labels(skin, producer, job, scratch, &superseded);
    scratch.building.store(0, Ordering::Relaxed);
    s.a8.store(text_upload::a8_bytes(&scratch.text), Ordering::Relaxed);
    if scratch.rasterized {
        s.mask.store(mask, Ordering::Relaxed);
    }
    let out = match out {
        Ok(Some(mut staged)) => {
            staged.ticket = ticket;
            staged.fence = Arc::clone(&s.ticket);
            Ok(staged)
        }
        Ok(None) => return false,
        Err(e) => Err((generation, e)),
    };
    #[cfg(test)]
    if let Some(hook) = &s.hooks.before_store {
        hook(generation);
    }
    // The last check and the store in the lock a post mints its ticket in.
    let mut output = locked(&s.output);
    if superseded() {
        return false;
    }
    *output = Some(out);
    true
}

/// A slot's content: the strings it was prepared from, its rasterized
/// lines' table and its bitmap lines.
#[derive(Debug)]
pub struct LabelContent {
    strings: DecisionStrings,
    table: TextTable,
    bitmap: BitmapLines,
}

impl LabelContent {
    /// The strings it was prepared from.
    #[must_use]
    pub const fn strings(&self) -> &DecisionStrings {
        &self.strings
    }

    /// Its rasterized lines, one per text slot.
    #[must_use]
    pub const fn table(&self) -> &TextTable {
        &self.table
    }

    fn bytes(&self) -> u64 {
        let table = self.table.len() * size_of::<Option<TextImage>>();
        let n = self.strings.heap_bytes() + table + self.bitmap.heap_bytes();
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

/// What the store holds of a slot: its drawable content, or, for a pinned
/// slot after a reset, only the strings its content is restored from (no
/// table of the old device's layer is kept, so none is drawn).
#[derive(Debug)]
enum Held {
    Drawable(LabelContent),
    Restoring(DecisionStrings),
}

impl Held {
    fn bytes(&self) -> u64 {
        match self {
            Self::Drawable(c) => c.bytes(),
            Self::Restoring(s) => u64::try_from(s.heap_bytes()).unwrap_or(u64::MAX),
        }
    }
}

/// The filling slot's progress: rows written, the first error a write's
/// scope reported, the completion flag once submitted, and the bytes the
/// last boundary staged.
#[derive(Debug, Default)]
pub struct FillProgress {
    /// The lines whose every level was written.
    pub rows_written: usize,
    /// The first error a boundary's scopes reported: the fill writes no
    /// more, submits at the next boundary and is refused once its writes
    /// completed.
    pub scope_error: Option<wgpu::Error>,
    /// Set when the submitted writes completed.
    pub done: Option<Arc<AtomicBool>>,
    /// The bytes the last boundary's writes staged.
    pub upload: u64,
    /// Whether any write was queued.
    pub wrote: bool,
    /// Whether every write was queued (or none was needed).
    pub complete: bool,
}

/// Where the next write starts: a line, its level, its first row.
#[derive(Debug, Default)]
struct Cursor {
    line: usize,
    level: usize,
    row: u32,
}

#[derive(Debug)]
struct Fill {
    slot: usize,
    staged: StagedLabels,
    cursor: Cursor,
    progress: FillProgress,
}

/// An ordered step of a decision preparation's boundary work, as a unit
/// test sees it.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelStep {
    Written,
    Submitted,
    Watched,
    CompletionSeen,
    LostChecked,
    Published,
    Stale,
    Refused,
}

/// The render thread's side of the label slots: each slot's content, the
/// filling slot's progress and the one output waiting for a slot.
#[derive(Debug)]
pub struct LabelStore {
    book: Arc<Mutex<LabelBook>>,
    contents: [Option<(Gen, Held)>; LABEL_SLOTS],
    fill: Option<Fill>,
    waiting: Option<StagedLabels>,
    #[cfg(test)]
    pub(crate) steps: Vec<LabelStep>,
}

/// The bytes the decision labels hold, for the memory witness
/// ([`MemoryWitness::with_decision`](crate::MemoryWitness::with_decision)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionMemory {
    /// The slots' contents.
    pub labels: u64,
    /// Staged levels not yet written: the worker's, the waiting output's
    /// and the filling slot's remainder.
    pub staged: u64,
    /// The worker's A8 buffer and its producer's mask bound.
    pub text_mask: u64,
    /// The bytes the queue staged at the last boundary, until the fill
    /// ends.
    pub upload: u64,
}

/// What a boundary's writes did ([`write_rows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillStep {
    /// No slot is filling.
    Idle,
    /// Some rows were written; more remain.
    Wrote,
    /// Every write is queued: submitted and watched, or none needed.
    Complete,
}

/// What [`settle`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// No slot is filling, or its writes have not completed.
    Pending,
    /// The slot is ready at that generation.
    Published { slot: usize, generation: Gen },
    /// Its key is no longer current: freed, not installed.
    Stale,
    /// The book took the device as unhealthy: nothing published.
    Unhealthy,
}

impl LabelStore {
    /// No content, nothing filling or waiting, on `book`.
    #[must_use]
    pub fn new(book: Arc<Mutex<LabelBook>>) -> Self {
        Self {
            book,
            contents: [None, None],
            fill: None,
            waiting: None,
            #[cfg(test)]
            steps: Vec::new(),
        }
    }

    /// The book it fills.
    #[must_use]
    pub const fn book(&self) -> &Arc<Mutex<LabelBook>> {
        &self.book
    }

    /// Keeps `staged` as the output waiting for a slot, dropping an older
    /// one.
    pub fn offer(&mut self, staged: StagedLabels) {
        self.waiting = Some(staged);
    }

    /// The renderer was built again: the fill and the waiting output are
    /// dropped with the old device's writes (the book's
    /// [`reset`](LabelBook::reset) frees their slots), and no slot's
    /// content is drawable until restored: a pinned slot keeps only its
    /// strings ([`restore_strings`](Self::restore_strings)).
    pub fn reset(&mut self) {
        self.fill = None;
        self.waiting = None;
        for c in self.contents.iter_mut().flatten() {
            if let Held::Drawable(content) = &mut c.1 {
                c.1 = Held::Restoring(std::mem::take(&mut content.strings));
            }
        }
    }

    /// The strings slot `slot`'s content at `generation` is restored from,
    /// while it waits to be restored after a reset.
    #[must_use]
    pub fn restore_strings(&self, slot: usize, generation: Gen) -> Option<&DecisionStrings> {
        match self.contents.get(slot) {
            Some(Some((g, Held::Restoring(s)))) if *g == generation => Some(s),
            _ => None,
        }
    }

    /// Posts `job` to `worker`, superseding every older preparation, in
    /// one order (the supersession invariant): a new ticket is minted, so
    /// every older output, taken or not, is stale and the one not taken is
    /// dropped; then the store's stale waiting output is dropped and its
    /// stale fill disposed of, its remaining levels freed and its slot
    /// cancelled (cooling until `watch`'s flag after a write, never
    /// published); only then is `job` handed to the worker. So no older
    /// preparation's staging is alive once the new one's can exist, and
    /// none publishes after this post ([`settle`] is fenced by the ticket).
    pub fn post(&mut self, worker: &LabelWorker, job: LabelJob, watch: Watch<'_>) {
        let ticket = worker.supersede();
        self.reconcile(watch);
        worker.place(ticket, job);
    }

    /// Cancels every preparation of `worker`'s: as [`post`](Self::post)
    /// with no job. After it, [`LabelWorker::take`] returns nothing older,
    /// `Ok` or `Err` (a request that returns to a key already held cancels
    /// the job in flight here, before it takes).
    pub fn cancel(&mut self, worker: &LabelWorker, watch: Watch<'_>) {
        worker.cancel();
        self.reconcile(watch);
    }

    /// Drops the waiting output and disposes of the fill unless current
    /// ([`StagedLabels::is_current`]): the fill's slot is cancelled,
    /// cooling until `watch`'s flag after a write.
    fn reconcile(&mut self, watch: Watch<'_>) {
        if self.waiting.as_ref().is_some_and(|w| !w.is_current()) {
            self.waiting = None;
        }
        if self.fill.as_ref().is_some_and(|f| !f.staged.is_current())
            && let Some(f) = self.fill.take()
        {
            lock(&self.book).cancel(f.slot, f.staged.generation, f.progress.wrote, watch);
        }
    }

    /// The waiting output's generation.
    #[must_use]
    pub fn waiting(&self) -> Option<Gen> {
        self.waiting.as_ref().map(|w| w.generation)
    }

    /// The filling slot and its generation.
    #[must_use]
    pub fn filling(&self) -> Option<(usize, Gen)> {
        self.fill.as_ref().map(|f| (f.slot, f.staged.generation))
    }

    /// The filling slot's progress.
    #[must_use]
    pub fn progress(&self) -> Option<&FillProgress> {
        self.fill.as_ref().map(|f| &f.progress)
    }

    /// Unit tests: the filling slot's progress, to change.
    #[cfg(test)]
    pub(crate) fn progress_mut(&mut self) -> Option<&mut FillProgress> {
        self.fill.as_mut().map(|f| &mut f.progress)
    }

    /// Claims a slot for the waiting output when none is filling: it
    /// moves into the slot's fill, the slot's old content dropped. A
    /// waiting output no longer current ([`StagedLabels::is_current`]) is
    /// dropped instead. The slot claimed.
    pub fn claim(&mut self) -> Option<usize> {
        if self.fill.is_some() {
            return None;
        }
        if self.waiting.as_ref().is_some_and(|w| !w.is_current()) {
            self.waiting = None;
        }
        let w = self.waiting.as_ref()?;
        let slot = lock(&self.book).claim(w.generation, w.key.clone(), w.timing)?;
        let staged = self.waiting.take()?;
        if let Some(c) = self.contents.get_mut(slot) {
            *c = None;
        }
        let complete = staged.lines.is_empty();
        self.fill = Some(Fill {
            slot,
            staged,
            cursor: Cursor::default(),
            progress: FillProgress {
                complete,
                ..FillProgress::default()
            },
        });
        Some(slot)
    }

    /// Slot `slot`'s content at `generation`, borrowed for a frame. Allocates
    /// nothing.
    #[must_use]
    pub fn content(&self, slot: usize, generation: Gen) -> Option<DecisionLabels<'_>> {
        match self.contents.get(slot) {
            Some(Some((g, Held::Drawable(c)))) if *g == generation => Some(DecisionLabels {
                text: &c.table,
                bitmap: &c.bitmap,
            }),
            _ => None,
        }
    }

    /// Slot `slot`'s content at `generation`, all of it.
    #[must_use]
    pub fn content_of(&self, slot: usize, generation: Gen) -> Option<&LabelContent> {
        match self.contents.get(slot) {
            Some(Some((g, Held::Drawable(c)))) if *g == generation => Some(c),
            _ => None,
        }
    }

    /// Drops the content of every slot the book no longer names at its
    /// generation.
    pub fn prune(&mut self) {
        let book = lock(&self.book);
        for (i, c) in self.contents.iter_mut().enumerate() {
            let keep = matches!(c, Some((g, _)) if book.state(i).and_then(LabelState::generation) == Some(*g));
            if !keep {
                *c = None;
            }
        }
    }

    /// The bytes it holds, with `worker`'s.
    #[must_use]
    pub fn memory(&self, worker: Option<&LabelWorker>) -> DecisionMemory {
        let (working, mask) = worker.map_or((0, 0), LabelWorker::bytes);
        let labels = self.contents.iter().flatten().map(|(_, c)| c.bytes()).sum();
        let staged = working
            .saturating_add(self.waiting.as_ref().map_or(0, StagedLabels::bytes))
            .saturating_add(self.fill.as_ref().map_or(0, |f| f.staged.bytes()));
        DecisionMemory {
            labels,
            staged,
            text_mask: mask,
            upload: self.fill.as_ref().map_or(0, |f| f.progress.upload),
        }
    }
}

/// The device, the decision array and its region a boundary's work writes
/// to.
pub(crate) struct LabelTarget<'a> {
    pub(crate) gpu: &'a Gpu,
    pub(crate) atlas: &'a AtlasTexture,
    pub(crate) region: Option<LabelRegion>,
    /// Run after the submission and the scopes' pop, before the
    /// completion is registered: a unit test makes the device fail there.
    #[cfg(test)]
    pub(crate) after_submit: Option<fn(&Gpu)>,
    /// Run inside a boundary's scopes, after its writes: a unit test makes
    /// a scope report an error.
    #[cfg(test)]
    pub(crate) in_scope: Option<fn(&Gpu)>,
    /// Unit tests: the pairs of error scopes opened so far.
    #[cfg(test)]
    pub(crate) scope_pairs: &'a std::cell::Cell<usize>,
}

/// `work` inside one pair of error scopes on `t`'s device.
fn scoped<T>(t: &LabelTarget<'_>, work: impl FnOnce() -> T) -> (T, Option<wgpu::Error>) {
    #[cfg(test)]
    t.scope_pairs.set(t.scope_pairs.get() + 1);
    skin_atlas::scoped(t.gpu.device(), work)
}

/// Registers one flag-only completion for the work submitted so far on
/// `gpu`'s queue, after submitting what is queued.
pub(crate) fn watch(gpu: &Gpu) -> Arc<AtomicBool> {
    gpu.queue().submit([]);
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    gpu.queue()
        .on_submitted_work_done(move || flag.store(true, Ordering::Release));
    done
}

/// Writes the filling slot's next rows, at most
/// [`DECISION_TEXT_WRITE_BUDGET`] bytes (at least one row), each line's
/// levels freed once written; after the last, submits once. The writes
/// and the submission share the boundary's one pair of error scopes; the
/// one flag-only completion is registered after the scopes settled. A
/// scope's error stops the writes: the next boundary only submits.
pub(crate) fn write_rows(t: &LabelTarget<'_>, store: &mut LabelStore) -> FillStep {
    let Some(fill) = store.fill.as_mut() else {
        return FillStep::Idle;
    };
    if fill.progress.complete {
        return FillStep::Complete;
    }
    let (Some(region), Some(layer)) = (t.region, t.atlas.text_layer()) else {
        // Rasterized lines with no region to write them: nothing is
        // written, and the fill is refused at its settling.
        fill.progress.complete = true;
        return FillStep::Complete;
    };
    let origin = region.band_origin(fill.slot);
    let queue = t.gpu.queue();
    let texture = t.atlas.texture();
    let mut bytes = 0_u64;
    let mut submitted = false;
    let failed = fill.progress.scope_error.is_some();
    let ((), refused) = scoped(t, || {
        let c = &mut fill.cursor;
        while let Some(line) = fill.staged.lines.get_mut(c.line).filter(|_| !failed) {
            let Some(level) = line.levels.get(c.level) else {
                line.levels = Default::default();
                fill.progress.rows_written += 1;
                *c = Cursor {
                    line: c.line + 1,
                    ..Cursor::default()
                };
                continue;
            };
            let k = c.level;
            let [w, h] = line.rect.map(|v| v >> k);
            let row = text_upload::padded(w);
            let room = (DECISION_TEXT_WRITE_BUDGET - bytes) / row.max(1);
            if room == 0 && bytes > 0 {
                break;
            }
            let n = u32::try_from(room.max(1))
                .unwrap_or(u32::MAX)
                .min(h - c.row);
            let stride = usize::try_from(w).unwrap_or(0) * 4;
            let from = usize::try_from(c.row).unwrap_or(0) * stride;
            let to = from + usize::try_from(n).unwrap_or(0) * stride;
            if let Some(data) = level.get(from..to) {
                let y = ((origin + line.y) >> k) + c.row;
                text_upload::write(queue, texture, layer, k, [0, y, w, n], data);
                fill.progress.wrote = true;
                bytes += row * u64::from(n);
            }
            c.row += n;
            if c.row >= h {
                c.level += 1;
                c.row = 0;
            }
        }
        #[cfg(test)]
        if let Some(f) = t.in_scope {
            f(t.gpu);
        }
        if failed || fill.cursor.line >= fill.staged.lines.len() {
            fill.progress.complete = true;
            for l in &mut fill.staged.lines {
                l.levels = Default::default();
            }
            if fill.progress.wrote {
                queue.submit([]);
                submitted = true;
            }
        }
    });
    fill.progress.upload = bytes;
    if let Some(e) = refused {
        fill.progress.scope_error.get_or_insert(e);
    }
    #[cfg(test)]
    if bytes > 0 {
        store.steps.push(LabelStep::Written);
    }
    if !fill.progress.complete {
        return FillStep::Wrote;
    }
    if submitted {
        #[cfg(test)]
        {
            store.steps.push(LabelStep::Submitted);
            if let Some(f) = t.after_submit {
                f(t.gpu);
            }
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        queue.on_submitted_work_done(move || flag.store(true, Ordering::Release));
        fill.progress.done = Some(done);
        #[cfg(test)]
        store.steps.push(LabelStep::Watched);
    }
    FillStep::Complete
}

/// Settles the label slots at a boundary: polls the device without
/// blocking; reads `Gpu::lost()` after that poll, before anything else (the
/// device-loss invariant: a lost device marks the book unhealthy and is
/// reported, and no slot is promoted, cooling ones included); disposes of
/// a fill a newer post superseded (the publication fence,
/// [`LabelStore::post`]); settles the cooling slots; and once the fill's
/// writes are queued and their completion seen by that poll, with every
/// scope empty, installs its content ([`install`]) and publishes it if its
/// key is still current.
///
/// # Errors
///
/// The poll's failure ([`RenderError::Poll`]) or the device lost
/// ([`RenderError::DeviceLost`]): not refusals, the renderer is built
/// again; the fill ends unpublished, its slot left for the rebuild's
/// reset. A write's refusal ([`RenderError::Atlas`]) or rasterized lines
/// with no region: refusals, the fill ends and its slot is free.
pub(crate) fn settle(t: &LabelTarget<'_>, store: &mut LabelStore) -> Result<Settled, RenderError> {
    t.gpu
        .device()
        .poll(wgpu::PollType::Poll)
        .map_err(RenderError::Poll)?;
    let seen = store.fill.as_ref().is_some_and(|f| {
        f.progress.complete
            && f.progress
                .done
                .as_ref()
                .is_none_or(|d| d.load(Ordering::Acquire))
    });
    #[cfg(test)]
    if seen {
        store.steps.push(LabelStep::CompletionSeen);
    }
    let lost = t.gpu.lost();
    #[cfg(test)]
    store.steps.push(LabelStep::LostChecked);
    if lost {
        store.fill = None;
        lock(&store.book).set_unhealthy();
        return Err(RenderError::DeviceLost);
    }
    store.reconcile(&mut || watch(t.gpu));
    lock(&store.book).cool();
    if !seen {
        return Ok(Settled::Pending);
    }
    let Some(fill) = store.fill.take() else {
        return Ok(Settled::Pending);
    };
    let (slot, generation) = (fill.slot, fill.staged.generation);
    let refused = refusal(fill.progress.scope_error, t.region, &fill.staged);
    if let Some(e) = refused {
        lock(&store.book).cancel(slot, generation, false, &mut || {
            Arc::new(AtomicBool::new(true))
        });
        #[cfg(test)]
        store.steps.push(LabelStep::Refused);
        return Err(e);
    }
    let content = install(t.region, slot, fill.staged);
    let published = lock(&store.book).publish(slot, generation);
    match published {
        Publish::Published => {
            if let Some(c) = store.contents.get_mut(slot) {
                *c = Some((generation, Held::Drawable(content)));
            }
            #[cfg(test)]
            store.steps.push(LabelStep::Published);
            Ok(Settled::Published { slot, generation })
        }
        Publish::Stale | Publish::NotFilling => {
            #[cfg(test)]
            store.steps.push(LabelStep::Stale);
            Ok(Settled::Stale)
        }
        Publish::Unhealthy => Ok(Settled::Unhealthy),
    }
}

/// Why a fill whose writes completed is refused: a scope's error, or
/// rasterized lines with no region to write them.
fn refusal(
    scope_error: Option<wgpu::Error>,
    region: Option<LabelRegion>,
    staged: &StagedLabels,
) -> Option<RenderError> {
    match (scope_error, region) {
        (Some(e), _) => Some(RenderError::Atlas(UploadError::Device(e))),
        (None, None) if !staged.lines.is_empty() => {
            Some(RenderError::Atlas(UploadError::Side { side: 0, max: 0 }))
        }
        (None, _) => None,
    }
}

/// Restores pinned slot `slot`'s content from `staged`, its generation's
/// strings ([`LabelStore::restore_strings`]) staged again at the current
/// target, on a renderer built again: every row written at once
/// (unbudgeted: a rebuild blocks already), the writes waited for, then
/// installed under the same generation and drawable again, the slot no
/// longer restoring, its key naming the target `staged` was rasterized at
/// (its rows' target: the target-identity invariant, so a later target
/// change compares against it). Whatever was filling or waiting is
/// dropped first.
/// `Ok(false)`, nothing written, when `slot` is not restoring at that
/// generation.
///
/// # Errors
///
/// As [`settle`]: the poll's failure or the device lost (not refusals),
/// or a write's refusal or rasterized lines with no region (refusals);
/// the slot stays restoring, its content not drawable.
pub(crate) fn restore_pinned(
    t: &LabelTarget<'_>,
    store: &mut LabelStore,
    slot: usize,
    staged: StagedLabels,
) -> Result<bool, RenderError> {
    let (generation, target) = (staged.generation, staged.key.target);
    if store.restore_strings(slot, generation).is_none()
        || !lock(&store.book).awaits_restore(slot, generation)
    {
        return Ok(false);
    }
    store.waiting = None;
    let complete = staged.lines.is_empty();
    store.fill = Some(Fill {
        slot,
        staged,
        cursor: Cursor::default(),
        progress: FillProgress {
            complete,
            ..FillProgress::default()
        },
    });
    while write_rows(t, store) == FillStep::Wrote {}
    let Some(fill) = store.fill.take() else {
        return Ok(false);
    };
    t.gpu
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    if t.gpu.lost() {
        lock(&store.book).set_unhealthy();
        return Err(RenderError::DeviceLost);
    }
    if let Some(e) = refusal(fill.progress.scope_error, t.region, &fill.staged) {
        return Err(e);
    }
    let content = install(t.region, slot, fill.staged);
    if let Some(c) = store.contents.get_mut(slot) {
        *c = Some((generation, Held::Drawable(content)));
    }
    lock(&store.book).restored(slot, generation, target);
    Ok(true)
}

/// The content of `staged` written in slot `slot`'s band of `region`: a
/// table of its lines, each line's image its pixels within its row, the
/// gutters left out, on the region's layer, and its bitmap lines.
fn install(region: Option<LabelRegion>, slot: usize, staged: StagedLabels) -> LabelContent {
    let mut table = vec![None; staged.slots];
    if let Some(region) = region {
        let side = staged.side.max(1) as f32;
        let origin = region.band_origin(slot);
        let layer = u16::try_from(region.layer).unwrap_or(0);
        for l in &staged.lines {
            let [w, h] = l.size;
            let (x, y) = (GUTTER, origin + l.y + GUTTER);
            let uv = FrameUv {
                uv: [x, y, x + w, y + h].map(|v| v as f32 / side),
                layer,
            };
            if let Some(s) = table.get_mut(usize::from(l.slot)) {
                *s = TextImage::new(uv, l.ink, l.advance);
            }
        }
    }
    LabelContent {
        strings: staged.strings,
        table: TextTable::new(table),
        bitmap: staged.bitmap,
    }
}

#[cfg(test)]
mod tests {
    //! The book's transitions, without a GPU: every completion flag is
    //! the test's own.

    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    use skin_model::DecisionTiming;

    use super::{CoolFrom, LabelBook, LabelKey, LabelState, Publish, lock};

    const T: [u32; 2] = [1_920, 1_080];

    fn key(chart: &str) -> LabelKey {
        LabelKey {
            chart: chart.into(),
            revision: 1,
            target: T,
        }
    }

    fn timing() -> DecisionTiming {
        DecisionTiming::default()
    }

    /// Flags the test sets itself, one per watch, in order.
    #[derive(Default)]
    struct Flags(Vec<Arc<AtomicBool>>);

    impl Flags {
        fn watch(&mut self) -> Arc<AtomicBool> {
            let f = Arc::new(AtomicBool::new(false));
            self.0.push(Arc::clone(&f));
            f
        }

        fn fire(&self, i: usize) {
            self.0[i].store(true, Ordering::Release);
        }
    }

    /// `chart`'s preparation `generation` claimed and published in `book`.
    fn ready(book: &mut LabelBook, generation: u64, chart: &str) -> usize {
        let slot = book.claim(generation, key(chart), timing()).unwrap();
        assert_eq!(book.publish(slot, generation), Publish::Published);
        slot
    }

    fn is_ready(book: &LabelBook, slot: usize, generation: u64) -> bool {
        matches!(book.state(slot), Some(LabelState::Ready { generation: g, .. }) if *g == generation)
    }

    fn is_pinned(book: &LabelBook, slot: usize, generation: u64, l: u64) -> bool {
        matches!(book.state(slot), Some(LabelState::Pinned { generation: g, lifetime, .. }) if *g == generation && *lifetime == l)
    }

    fn is_free(book: &LabelBook, slot: usize) -> bool {
        matches!(book.state(slot), Some(LabelState::Free))
    }

    fn is_cooling(book: &LabelBook, slot: usize, from: CoolFrom) -> bool {
        matches!(book.state(slot), Some(LabelState::Cooling { from: f, .. }) if *f == from)
    }

    /// A claim and a pin racing for the one ready slot (the other cooling)
    /// never both take it, in either order and under a race.
    #[test]
    fn pin_and_claim_exclude_each_other() {
        let setup = |flags: &mut Flags| {
            let mut b = LabelBook::new(T);
            let a = ready(&mut b, 1, "a");
            let other = b.claim(2, key("b"), timing()).unwrap();
            assert_ne!(a, other);
            assert!(b.cancel(other, 2, true, &mut || flags.watch()));
            (b, a)
        };
        let mut flags = Flags::default();
        // The pin first: the claim finds no slot.
        let (mut b, a) = setup(&mut flags);
        assert_eq!(b.pin(Path::new("a"), 1, 7).map(|p| p.0), Some(a));
        assert_eq!(b.claim(3, key("c"), timing()), None);
        assert!(is_pinned(&b, a, 1, 7));
        // The claim first: the pin finds nothing ready.
        let (mut b, a) = setup(&mut flags);
        assert_eq!(b.claim(3, key("c"), timing()), Some(a));
        assert_eq!(b.pin(Path::new("a"), 1, 7), None);
        // Raced from two threads behind one barrier.
        for _ in 0..64 {
            let (b, _) = setup(&mut flags);
            let book = Arc::new(Mutex::new(b));
            let gate = Arc::new(Barrier::new(2));
            let (pb, pg) = (Arc::clone(&book), Arc::clone(&gate));
            let pinner = std::thread::spawn(move || {
                pg.wait();
                lock(&pb).pin(Path::new("a"), 1, 7).is_some()
            });
            gate.wait();
            let claimed = lock(&book).claim(3, key("c"), timing()).is_some();
            let pinned = pinner.join().unwrap();
            assert!(claimed ^ pinned, "claimed {claimed}, pinned {pinned}");
        }
    }

    /// A retire naming an older lifetime leaves a newer pin of the slot.
    #[test]
    fn stale_retire_leaves_a_newer_pin() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 1).unwrap();
        b.end(1);
        assert!(b.retire(s, 1, 1, &mut || flags.watch()));
        flags.fire(0);
        b.cool();
        assert!(is_ready(&b, s, 1));
        b.pin(Path::new("a"), 1, 2).unwrap();
        // The first lifetime's retire again, after the second pinned.
        assert!(!b.retire(s, 1, 1, &mut || flags.watch()));
        assert!(is_pinned(&b, s, 1, 2));
        // Nor a retire of the right lifetime at another generation.
        b.end(2);
        assert!(!b.retire(s, 9, 2, &mut || flags.watch()));
        assert!(is_pinned(&b, s, 1, 2));
    }

    /// A new generation completes into the other slot while one is
    /// pinned: one pinned, one ready.
    #[test]
    fn new_generation_completes_while_another_is_pinned() {
        let mut b = LabelBook::new(T);
        let a = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 1).unwrap();
        let s = b.claim(2, key("b"), timing()).unwrap();
        assert_ne!(s, a);
        assert_eq!(b.publish(s, 2), Publish::Published);
        assert!(is_pinned(&b, a, 1, 1));
        assert!(is_ready(&b, s, 2));
    }

    /// The cursor resting on A, B, then C with no choice: C's job claims
    /// the older ready slot.
    #[test]
    fn cursor_rests_on_three_charts_keeps_preparing() {
        let mut b = LabelBook::new(T);
        let a = ready(&mut b, 1, "a");
        let bb = ready(&mut b, 2, "b");
        assert_eq!(b.claim(3, key("c"), timing()), Some(a));
        assert_eq!(b.publish(a, 3), Publish::Published);
        assert!(is_ready(&b, bb, 2));
        assert!(b.holds(&key("c")) && b.holds(&key("b")) && !b.holds(&key("a")));
    }

    /// A preparation superseded after a write cools until its flag, then
    /// is free; one superseded before any write is free at once.
    #[test]
    fn superseded_after_a_partial_write_cools_then_frees() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = b.claim(1, key("a"), timing()).unwrap();
        assert!(b.cancel(s, 1, true, &mut || flags.watch()));
        assert!(is_cooling(&b, s, CoolFrom::Cancelled));
        b.cool();
        assert!(is_cooling(&b, s, CoolFrom::Cancelled));
        // Not claimable while cooling: the other slot is taken instead.
        let other = b.claim(2, key("b"), timing()).unwrap();
        assert_ne!(other, s);
        flags.fire(0);
        b.cool();
        assert!(is_free(&b, s));
        assert!(b.cancel(other, 2, false, &mut || flags.watch()));
        assert!(is_free(&b, other));
        assert_eq!(flags.0.len(), 1);
    }

    /// A's preparation cancelled after a write, the cursor back on A
    /// before its flag: the flag frees it, never publishes it.
    #[test]
    fn cancelled_slot_never_publishes_when_its_key_returns() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = b.claim(1, key("a"), timing()).unwrap();
        assert!(b.cancel(s, 1, true, &mut || flags.watch()));
        let other = b.claim(2, key("b"), timing()).unwrap();
        assert!(b.cancel(other, 2, false, &mut || flags.watch()));
        // A's key current again, and A's old flag fires.
        assert!(!b.holds(&key("a")));
        flags.fire(0);
        b.cool();
        assert!(is_free(&b, s));
        assert!(!b.holds(&key("a")));
        assert_eq!(b.pin(Path::new("a"), 1, 1), None);
    }

    /// A pin sets the active lifetime in its own lock: a boundary's
    /// sweep between the pin and the hook's return retires nothing.
    #[test]
    fn fresh_pin_not_retired_before_its_mirror() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 5).unwrap();
        assert_eq!(b.active(), Some(5));
        b.retire_ended(&mut || flags.watch());
        assert!(is_pinned(&b, s, 1, 5));
        assert!(flags.0.is_empty());
        b.end(5);
        b.retire_ended(&mut || flags.watch());
        assert!(is_cooling(&b, s, CoolFrom::Retired));
    }

    /// A retired slot waits for its last frame's flag: a target change
    /// before it leaves it cooling and unclaimable; its flag then frees
    /// it, its key no longer current.
    #[test]
    fn retired_slot_waits_for_its_last_draw() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 1).unwrap();
        b.end(1);
        b.retire_ended(&mut || flags.watch());
        b.invalidate([1_280, 720]);
        assert!(is_cooling(&b, s, CoolFrom::Retired));
        let other = b.claim(2, key("b"), timing()).unwrap();
        assert_ne!(other, s);
        assert_eq!(b.claim(3, key("c"), timing()), None);
        assert!(is_cooling(&b, s, CoolFrom::Retired));
        flags.fire(0);
        b.cool();
        assert!(is_free(&b, s));
    }

    /// A retired slot whose key is still current is ready again at its
    /// flag, and pins again.
    #[test]
    fn retired_slot_is_ready_again_at_its_flag() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 1).unwrap();
        b.end(1);
        b.retire_ended(&mut || flags.watch());
        assert!(b.holds(&key("a")));
        b.cool();
        assert!(is_cooling(&b, s, CoolFrom::Retired));
        flags.fire(0);
        b.cool();
        assert!(is_ready(&b, s, 1));
        assert_eq!(b.pin(Path::new("a"), 1, 2).map(|p| p.0), Some(s));
    }

    /// With the device not healthy, nothing pins and the active lifetime
    /// is not set.
    #[test]
    fn pin_refused_while_unhealthy() {
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.set_unhealthy();
        assert_eq!(b.pin(Path::new("a"), 1, 1), None);
        assert_eq!(b.active(), None);
        assert!(is_ready(&b, s, 1));
        b.reset();
        assert!(is_free(&b, s));
    }

    /// A pinned slot reset is restoring: retired before its content was
    /// restored, it is freed at its flag, never ready again; once
    /// restored, it is ready again at its flag.
    #[test]
    fn restoring_pin_never_ready_again_before_its_restore() {
        let mut flags = Flags::default();
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 1).unwrap();
        assert!(!b.awaits_restore(s, 1));
        b.reset();
        assert!(b.awaits_restore(s, 1));
        assert!(!b.awaits_restore(s, 2));
        b.end(1);
        b.retire_ended(&mut || flags.watch());
        assert!(is_cooling(&b, s, CoolFrom::Cancelled));
        flags.fire(0);
        b.cool();
        assert!(is_free(&b, s));
        assert_eq!(b.pin(Path::new("a"), 1, 2), None);
        // Restored: no longer restoring, and ready again after its retire.
        let s = ready(&mut b, 2, "a");
        b.pin(Path::new("a"), 1, 3).unwrap();
        b.reset();
        assert!(b.restored(s, 2, T));
        assert!(!b.awaits_restore(s, 2));
        assert!(!b.restored(s, 2, T));
        b.end(3);
        b.retire_ended(&mut || flags.watch());
        flags.fire(1);
        b.cool();
        assert!(is_ready(&b, s, 2));
    }

    /// The row rule at each of its edges, from the formula's own numbers:
    /// `pad(min(1024, ceil(2 em)))` (gutters of 8 on both sides, rounded
    /// up to 8), rows stacked from 0, `min(side, 4096)` wide, laid out
    /// within the row less its gutters, and a plan refused only past half
    /// the layer.
    #[test]
    fn row_formula_at_its_edges() {
        use super::{RowPlan, row_height};
        use crate::renderer::RenderError;
        // ceil(2 × 18) = 36, + 16 = 52, up to 56.
        assert_eq!(row_height(18.0), 56);
        // 2 × 20 = 40 + 16 = 56 exactly; 20.01 rounds 40.02 up to 41: 64.
        assert_eq!(row_height(20.0), 56);
        assert_eq!(row_height(20.01), 64);
        // 2 × 16.1 = 32.2 → 33 → 49 → 56; 2 × 16 = 32 → 48.
        assert_eq!(row_height(16.0), 48);
        assert_eq!(row_height(16.1), 56);
        // The cap: 2 × 512 = 1 024 → 1 040; more stays at 1 040.
        assert_eq!(row_height(512.0), 1_040);
        assert_eq!(row_height(512.5), 1_040);
        assert_eq!(row_height(4_000.0), 1_040);
        // No line: only the gutters.
        assert_eq!(row_height(0.0), 16);
        assert_eq!(row_height(-3.0), 16);
        assert_eq!(row_height(f32::NAN), 16);

        let plan = RowPlan::new(&[(3, 18.0), (5, 24.0), (9, 20.01)], 2_048, 12, T).unwrap();
        let rows: Vec<_> = plan.rows().iter().map(|r| (r.slot, r.y, r.h)).collect();
        // 24: 48 + 16 = 64.
        assert_eq!(rows, [(3, 0, 56), (5, 56, 64), (9, 120, 64)]);
        assert_eq!(plan.width(), 2_048);
        let b = plan.bounds(&plan.rows()[1]);
        assert_eq!((b.w, b.h), (2_032, 48));
        // The width's cap.
        let wide = RowPlan::new(&[(0, 18.0)], 8_192, 1, T).unwrap();
        assert_eq!(wide.width(), 4_096);
        assert_eq!(wide.bounds(&wide.rows()[0]).w, 4_080);
        // The fit: exactly half the layer fits, one row more does not.
        // Side 2 080: band 1 040, one capped row.
        assert!(RowPlan::new(&[(0, 600.0)], 2_080, 1, T).is_ok());
        let e = RowPlan::new(&[(0, 600.0), (1, 0.0)], 2_080, 2, T).unwrap_err();
        assert!(
            matches!(
                e,
                RenderError::LabelRegion {
                    needed: 1_056,
                    band: 1_040
                }
            ),
            "{e:?}"
        );
    }

    /// A target change or a rebuild between the claim and the publish:
    /// the slot is freed, never ready; a pinned slot survives a reset.
    #[test]
    fn stale_key_never_publishes() {
        let mut b = LabelBook::new(T);
        let s = b.claim(1, key("a"), timing()).unwrap();
        b.invalidate([1_280, 720]);
        assert_eq!(b.publish(s, 1), Publish::Stale);
        assert!(is_free(&b, s));
        b.invalidate(T);
        let p = ready(&mut b, 2, "p");
        b.pin(Path::new("p"), 1, 9).unwrap();
        let s = b.claim(3, key("a"), timing()).unwrap();
        b.reset();
        assert_eq!(b.publish(s, 3), Publish::NotFilling);
        assert!(is_free(&b, s));
        assert!(is_pinned(&b, p, 2, 9));
        let s = b.claim(4, key("a"), timing()).unwrap();
        let (_, epoch) = b.current();
        assert_eq!(epoch, 1);
        b.reset();
        assert!(is_free(&b, s));
    }

    /// A pin made at T and restored at U names U: back at T, nothing holds
    /// T's key, and its retire then never makes it ready for T; it keeps
    /// its slot, generation and lifetime.
    #[test]
    fn restored_pin_keys_the_target_it_was_restored_at() {
        const U: [u32; 2] = [1_280, 720];
        let mut b = LabelBook::new(T);
        let s = ready(&mut b, 1, "a");
        b.pin(Path::new("a"), 1, 7).unwrap();
        b.reset();
        b.invalidate(U);
        assert!(b.restored(s, 1, U));
        assert!(is_pinned(&b, s, 1, 7));
        assert!(b.holds(&LabelKey {
            target: U,
            ..key("a")
        }));
        b.invalidate(T);
        assert!(!b.holds(&key("a")), "T's labels are prepared afresh");
        b.end(7);
        b.retire_ended(&mut || Arc::new(AtomicBool::new(true)));
        b.cool();
        assert!(is_free(&b, s));
        assert_eq!(b.pin(Path::new("a"), 1, 8), None);
    }
}
