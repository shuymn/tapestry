//! Quads and the per-frame batch a skin fills.
//!
//! The batch keeps one ordered list, so an alpha quad pushed after an
//! additive one is drawn after it, and splits it into runs wherever the
//! blend, the clip or the inline clip rect changes; a renderer draws the
//! runs in order, each under both of its clips.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

#[cfg(feature = "dev-tools")]
use crate::observation::{ElementObservation, Outcome};

/// One quad in canvas pixels.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Quad {
    /// Left, top, width, height.
    pub rect: [f32; 4],
    /// Atlas coordinates: left, top, right, bottom.
    pub uv: [f32; 4],
    /// Color multiplied with the atlas sample, RGBA.
    pub color: [f32; 4],
    /// The point on the canvas the rect rotates about.
    pub pivot: [f32; 2],
    /// The clockwise rotation about `pivot`, in radians in `[0, 2π)`;
    /// applied last, to the rect as placed.
    pub angle: f32,
    /// The atlas texture array's layer the uv points into.
    pub layer: u16,
    /// How the atlas is sampled.
    pub filter: Filter,
}

// The renderer's sprite carries the same 64 bytes.
const _: () = assert!(std::mem::size_of::<Quad>() == 64);

/// How an image is sampled. Both use the atlas's box-filtered mip levels
/// when shrinking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    /// Bilinear within a level, blending between levels.
    #[default]
    Linear,
    /// The nearest texel of the nearest level, for pixel art.
    Nearest,
}

/// How a quad is blended over what is under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// Ordinary alpha blending.
    Alpha,
    /// Added on top.
    Add,
}

/// An opaque clip: an index into the clip table of the skin that pushes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipId(pub u16);

/// What a run shares: neighbouring quads with equal keys draw together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunKey {
    pub blend: Blend,
    pub clip: Option<ClipId>,
}

/// A clip rect a quad carries itself, in canvas pixels: left, top, width
/// and height, every value finite and the width and height positive.
///
/// A renderer draws a run under the intersection of its key's clip
/// ([`RunKey::clip`]) and this rect; an empty intersection draws nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipRect([f32; 4]);

impl ClipRect {
    /// The rect `[x, y, w, h]`; `None` unless every value is finite and
    /// the width and height are positive.
    #[must_use]
    pub fn new(rect: [f32; 4]) -> Option<Self> {
        let [.., w, h] = rect;
        (rect.iter().all(|v| v.is_finite()) && w > 0.0 && h > 0.0).then_some(Self(rect))
    }

    /// Left, top, width, height.
    #[must_use]
    pub fn rect(self) -> [f32; 4] {
        self.0
    }
}

/// The quads `start..end` of a batch, all with `key` and `clip_rect`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub key: RunKey,
    /// The rect every quad of the run is clipped to besides `key.clip`.
    pub clip_rect: Option<ClipRect>,
    pub start: u32,
    pub end: u32,
}

/// The quads of one frame, in draw order, at most `quads` of them in at
/// most `runs` runs, and after them, once the batch's owner opens it
/// ([`open_reserve`](QuadBatch::open_reserve)), its reserve.
///
/// Both lists are allocated once, at full capacity, the reserve included,
/// so pushing never allocates. A push beyond either capacity in force is
/// dropped and counted.
#[derive(Debug)]
pub struct QuadBatch {
    quads: Vec<Quad>,
    runs: Vec<Run>,
    /// The capacities in force: `own`'s until the reserve opens, then
    /// `all`'s.
    quad_cap: u32,
    run_cap: u32,
    /// The capacities of the skin that fills the batch: quads, runs.
    own: (u32, u32),
    /// Those with the reserve: quads, runs.
    all: (u32, u32),
    dropped: u32,
    /// What each element's draw evaluated, allocated once for the most
    /// elements a skin has.
    #[cfg(feature = "dev-tools")]
    observations: Vec<ElementObservation>,
    /// Whether the last observation takes what the draw finds next.
    #[cfg(feature = "dev-tools")]
    observing: bool,
    /// Observations with no room.
    #[cfg(feature = "dev-tools")]
    observations_dropped: u32,
    /// Where the skin's elements ended: what follows is its owner's.
    #[cfg(feature = "dev-tools")]
    observation_end: Option<u32>,
}

/// `n` as a `Vec` capacity, and the cap the batch then enforces (0 on a
/// target whose `usize` cannot hold it).
fn reserve(n: u32) -> (usize, u32) {
    usize::try_from(n).map_or((0, 0), |c| (c, n))
}

impl QuadBatch {
    /// An empty batch holding up to `quads` quads in up to `runs` runs,
    /// with no reserve.
    #[must_use]
    pub fn with_capacity(quads: u32, runs: u32) -> Self {
        Self::with_reserve(quads, runs, 0, 0)
    }

    /// An empty batch holding up to `quads` quads in up to `runs` runs for
    /// the skin that fills it, and `reserved_quads` more quads in
    /// `reserved_runs` more runs that only its owner fills, after the
    /// skin, once it opens them ([`open_reserve`](Self::open_reserve)).
    /// Totals past `u32::MAX` are cut to it.
    #[must_use]
    pub fn with_reserve(quads: u32, runs: u32, reserved_quads: u32, reserved_runs: u32) -> Self {
        let (quad_len, quad_all) = reserve(quads.saturating_add(reserved_quads));
        let (run_len, run_all) = reserve(runs.saturating_add(reserved_runs));
        let own = (quads.min(quad_all), runs.min(run_all));
        Self {
            quads: Vec::with_capacity(quad_len),
            runs: Vec::with_capacity(run_len),
            quad_cap: own.0,
            run_cap: own.1,
            own,
            all: (quad_all, run_all),
            dropped: 0,
            #[cfg(feature = "dev-tools")]
            observations: Vec::with_capacity(crate::MAX_ELEMENTS),
            #[cfg(feature = "dev-tools")]
            observing: false,
            #[cfg(feature = "dev-tools")]
            observations_dropped: 0,
            #[cfg(feature = "dev-tools")]
            observation_end: None,
        }
    }

    /// Empties the batch and its drop count for the next frame, keeping its
    /// capacity, and closes the reserve again.
    pub fn clear(&mut self) {
        self.quads.clear();
        self.runs.clear();
        (self.quad_cap, self.run_cap) = self.own;
        self.dropped = 0;
        #[cfg(feature = "dev-tools")]
        {
            self.observations.clear();
            self.observing = false;
            self.observations_dropped = 0;
            self.observation_end = None;
        }
    }

    /// Opens element `element`'s observation, at the quads held so far. Past
    /// the capacity it counts, and nothing is observed until the next one
    /// fits. Never allocates.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn observe(&mut self, element: u32) {
        self.observing = self.observations.len() < self.observations.capacity();
        if !self.observing {
            self.observations_dropped = self.observations_dropped.saturating_add(1);
            return;
        }
        let quad_start = self.len();
        self.observations.push(ElementObservation {
            element,
            quad_start,
            outcome: Outcome::Sampled,
            value: None,
            choice: None,
            t: None,
        });
    }

    /// The open observation, if any.
    #[cfg(feature = "dev-tools")]
    fn observing(&mut self) -> Option<&mut ElementObservation> {
        if self.observing {
            self.observations.last_mut()
        } else {
            None
        }
    }

    /// The draw stopped with the open observation's element.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn observed(&mut self, outcome: Outcome) {
        if let Some(o) = self.observing() {
            o.outcome = outcome;
        }
    }

    /// The open observation's element's track was sampled at `t` µs.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn observed_time(&mut self, t: i64) {
        if let Some(o) = self.observing() {
            o.t = Some(bms_model::TimeUs(t));
        }
    }

    /// The open observation's number shows `value`.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn observed_value(&mut self, value: i64) {
        if let Some(o) = self.observing() {
            o.value = Some(value);
        }
    }

    /// The open observation's image choice selected entry `choice`.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn observed_choice(&mut self, choice: Option<u32>) {
        if let Some(o) = self.observing() {
            o.choice = choice;
        }
    }

    /// The skin's elements are done: quads from here are not theirs.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn end_observations(&mut self) {
        self.observing = false;
        self.observation_end = Some(self.len());
    }

    /// This frame's observations, in draw order.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn observations(&self) -> &[ElementObservation] {
        &self.observations
    }

    /// Observations with no room since the last [`clear`](Self::clear).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub const fn observations_dropped(&self) -> u32 {
        self.observations_dropped
    }

    /// Where the skin's elements ended among the quads, once they have.
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub const fn observation_end(&self) -> Option<u32> {
        self.observation_end
    }

    /// Opens the reserve until the next [`clear`](Self::clear): later
    /// pushes may fill it, after whatever the skin pushed, however full
    /// the skin left its own capacities. The batch's owner calls it once
    /// the skin's build is over.
    pub fn open_reserve(&mut self) {
        (self.quad_cap, self.run_cap) = self.all;
    }

    /// The skin's capacities, without the reserve: quads, runs.
    #[must_use]
    pub const fn capacity(&self) -> (u32, u32) {
        self.own
    }

    /// The capacities with the reserve: quads, runs.
    #[must_use]
    pub const fn total_capacity(&self) -> (u32, u32) {
        self.all
    }

    /// The number of quads held: the end of the last run.
    fn len(&self) -> u32 {
        self.runs.last().map_or(0, |r| r.end)
    }

    fn drop_one(&mut self) -> bool {
        self.count_drop();
        false
    }

    /// Counts work a widget left undrawn because its budget ran out.
    pub(crate) fn count_drop(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Adds `quad` with `key` and no clip rect, extending the last run when
    /// its key is equal and it has no clip rect. Returns `false` and counts
    /// the quad as dropped when the quad list is full, or when it needs a
    /// new run and the run list is full.
    pub fn push(&mut self, key: RunKey, quad: Quad) -> bool {
        self.push_in(key, None, quad)
    }

    /// [`push`](Self::push) with the quad clipped to `rect` as well: the
    /// last run is extended only when its key and its clip rect are both
    /// equal.
    pub fn push_clipped(&mut self, key: RunKey, quad: Quad, rect: ClipRect) -> bool {
        self.push_in(key, Some(rect), quad)
    }

    fn push_in(&mut self, key: RunKey, clip_rect: Option<ClipRect>, quad: Quad) -> bool {
        let len = self.len();
        if len >= self.quad_cap {
            return self.drop_one();
        }
        match self.runs.last_mut() {
            Some(run) if run.key == key && run.clip_rect == clip_rect => run.end = len + 1,
            _ => {
                let runs_full = u32::try_from(self.runs.len()).map_or(true, |n| n >= self.run_cap);
                if runs_full {
                    return self.drop_one();
                }
                self.runs.push(Run {
                    key,
                    clip_rect,
                    start: len,
                    end: len + 1,
                });
            }
        }
        self.quads.push(quad);
        true
    }

    /// The quads, in push order.
    #[must_use]
    pub fn quads(&self) -> &[Quad] {
        &self.quads
    }

    /// The runs, in push order; they cover the quads contiguously, and
    /// neighbouring runs differ in key or clip rect.
    #[must_use]
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// Quads dropped since the last [`clear`](Self::clear).
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Whether the quad list is full, so every push would be dropped.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len() >= self.quad_cap
    }
}
