//! The skin boundary (section 2, "Skins"): what a skin is given each frame
//! and what it must do.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, TimeUs};
use eval::{JudgeEvent, Section};
use game::PlayView;
use skin_model::{
    BuildError, Canvas, ChartText, CoverSettings, Fit, LaneView, Layout, LoadState, LoadingGauge,
    PlayContext,
};

use crate::{Gpu, MAX_SPRITES, SpriteBatch};

/// What one frame draws from.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// Chart time when the frame's work starts, read from QPC.
    pub now: TimeUs,
    /// The display latency offset: how much later than `now` the frame is
    /// expected to reach the screen (design.md "Rendering and Skins").
    pub display_offset: TimeUs,
    /// Hi-speed as the visible time (design.md "Algorithms"): how long a note's
    /// center takes from the top of the lane to the judgment line while the
    /// chart scrolls at its initial BPM. The note's edge shows slightly
    /// earlier.
    pub visible_time: TimeUs,
    /// The newest play state.
    pub view: &'a PlayView,
    /// The effects received since the previous frame, in emission order.
    pub effects: &'a [JudgeEvent],
    /// Index into [`PlayView::gauges`] of the gauge to show.
    pub gauge: usize,
    /// Lines of the measurement overlay (design.md "Latency Figures"), drawn
    /// over the play after the skin's sprites, by the staging and not the
    /// skin ([`Staging::build`](crate::Staging::build)); empty when it is
    /// hidden.
    pub overlay: &'a [&'a str],
    /// The target's size in pixels as the renderer last applied it
    /// ([`Renderer::target`](crate::Renderer::target)), never zero: the
    /// size the frame is projected onto ([`PlaySkin::canvas`]).
    /// [`Renderer::build`](crate::Renderer::build) sets it so.
    pub target: [u32; 2],
}

impl FrameInput<'_> {
    /// The chart time the frame is expected to appear on screen: `now` plus
    /// the display latency offset. Note positions follow it.
    #[must_use]
    pub const fn display_time(&self) -> TimeUs {
        TimeUs(self.now.0.saturating_add(self.display_offset.0))
    }
}

/// What one loading frame draws from (design.md "Rendering and Skins",
/// loading state): the model's loading frame
/// ([`skin_model::LoadingFrame`]) with the loading lifetime it belongs to
/// and the revision of the loading text shown with it.
#[derive(Clone, Copy, Debug)]
pub struct LoadingFrame<'a> {
    /// The loading lifetime the frame belongs to: a skin's loading state
    /// starts afresh at the first frame of another
    /// ([`PlaySkin::enter_loading`]).
    pub lifetime: u64,
    /// The frame's time on the loading clock: timers age on it.
    pub now: TimeUs,
    /// The load: its origin on the loading clock and its progress.
    pub load: LoadState,
    /// The target's size in pixels as the renderer last applied it, never
    /// zero; [`Renderer::loading_frame`](crate::Renderer::loading_frame)
    /// sets it so.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a Layout,
    /// The hi-speed and the effective covers.
    pub lanes: LaneView,
    /// The stored cover settings `lanes` follows.
    pub covers: CoverSettings,
    /// The shown gauge's descriptor; `None` leaves what depends on it
    /// undrawn.
    pub gauge: Option<LoadingGauge>,
    /// The revision of the loading text installed for this lifetime; 0
    /// with none.
    pub text: u64,
}

impl LoadingFrame<'static> {
    /// A frame of lifetime `lifetime` at `now` on the loading clock, from
    /// its origin `since`, with the interim values the render thread gives
    /// until the player's bridge (docs/stage3.md S10-03c-3): the author's
    /// layout, every cover 0 and off and not being adjusted, the lanes'
    /// `visible_time` from the tuning snapshot, no gauge descriptor,
    /// progress 0 and no text. The renderer applies its own target.
    #[must_use]
    pub const fn interim(lifetime: u64, now: TimeUs, since: TimeUs, visible_time: TimeUs) -> Self {
        Self {
            lifetime,
            now,
            load: LoadState { since, percent: 0 },
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes: LaneView {
                visible_time,
                cover: 0,
                lift: 0,
                hidden: 0,
            },
            covers: CoverSettings {
                cover: 0,
                lift: 0,
                hidden: 0,
                cover_enabled: false,
                lift_enabled: false,
                hidden_enabled: false,
                adjusting: false,
            },
            gauge: None,
            text: 0,
        }
    }
}

impl<'a> LoadingFrame<'a> {
    /// The model's frame of it.
    #[must_use]
    pub const fn model(&self) -> skin_model::LoadingFrame<'a> {
        skin_model::LoadingFrame {
            now: self.now,
            load: self.load,
            target: self.target,
            layout: self.layout,
            lanes: self.lanes,
            covers: self.covers,
            gauge: self.gauge,
        }
    }
}

/// What one attempt's preparation gives a skin (design.md "Play
/// Lifecycle"): the chart, the attempt's context made from the refreshed
/// play preset, the chart's labels from the ingestion that read it, and
/// the target's size as the renderer applied it. Every borrow lasts the
/// one call.
#[derive(Clone, Copy, Debug)]
pub struct AttemptInput<'a> {
    /// The chart.
    pub chart: &'a Chart,
    /// The attempt: the preset's gauges and rules, the whole chart or a
    /// section, the scratch side and the notes' end.
    pub context: &'a PlayContext<'a>,
    /// The chart's labels, for the skin's text.
    pub text: &'a ChartText<'a>,
    /// The target's size in pixels as last applied, never zero
    /// ([`Renderer::target`](crate::Renderer::target)); the renderer sets
    /// it.
    pub target: [u32; 2],
}

impl AttemptInput<'_> {
    /// The section a practice round draws only the targets of; `None` for
    /// the whole chart.
    #[must_use]
    pub const fn section(&self) -> Option<&Section> {
        match self.context.attempt {
            skin_model::Attempt::Full { .. } => None,
            skin_model::Attempt::Section(s) => Some(s),
        }
    }
}

/// A skin that cannot draw a chart.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct SkinError {
    /// Why, for the player.
    pub reason: String,
}

/// A play screen layout. The renderer calls `load_chart` when a chart is
/// chosen, `prepare` before each attempt's READY state and `build` once per
/// frame. `build` must not allocate: everything it needs is sized in
/// `prepare`.
pub trait PlaySkin: Send {
    /// Takes what the skin needs from the attempt `a` and the GPU, and
    /// starts the attempt's state fresh, so each retry and practice round
    /// calls it again. With a practice round's section, only its targets
    /// are drawn ([`Section::is_target`]).
    ///
    /// # Errors
    ///
    /// Returns why the skin cannot draw this attempt.
    fn prepare(&mut self, gpu: &Gpu, a: &AttemptInput<'_>) -> Result<(), SkinError>;

    /// A chart was chosen, before its attempt's preset is known: takes
    /// what the skin needs of `chart` alone (`section`, a practice's first
    /// round, the targets it draws), and nothing that depends on the play
    /// preset. The default takes nothing.
    ///
    /// # Errors
    ///
    /// Returns why the skin cannot draw this chart.
    fn load_chart(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), SkinError> {
        let _ = (chart, section);
        Ok(())
    }

    /// Fills `out`, which arrives empty, with the frame's sprites, or
    /// refuses the frame with the runtime model's error. A refused frame
    /// is drawn by nothing: the renderer drops whatever was pushed, submits
    /// nothing, and calls `build` no more until the next
    /// [`prepare`](Self::prepare), so the attempt stops. Allocation-free,
    /// the error included.
    ///
    /// # Errors
    ///
    /// The model's [`BuildError`] for a frame it cannot build.
    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch) -> Result<(), BuildError>;

    /// The most sprites a frame of the skin pushes: the renderer's batch,
    /// staging and instance buffer hold this many, and never fewer than
    /// [`MAX_SPRITES`], which the built-in screens use, and the
    /// measurement overlay's reserve after them
    /// ([`OVERLAY_SPRITES`](crate::OVERLAY_SPRITES)). The default is
    /// [`MAX_SPRITES`].
    fn capacity(&self) -> u32 {
        MAX_SPRITES as u32
    }

    /// The most runs a frame of the skin's sprites take, at least one per
    /// sprite when it may change blend or clip at every sprite; the
    /// overlay's run is reserved apart. The default is
    /// [`capacity`](Self::capacity).
    fn run_capacity(&self) -> u32 {
        self.capacity()
    }

    /// The bytes the prepared attempt's own state holds, for the memory
    /// witness ([`Renderer::memory`](crate::Renderer::memory)). The default
    /// is 0: a skin whose preparation keeps nothing per attempt.
    fn attempt_bytes(&self) -> u64 {
        0
    }

    /// The canvas the skin draws on for a `target`-pixel target, which the
    /// renderer projects its frames and clips onto. The default is the
    /// fixed 1920×1080 layout, letterboxed.
    fn canvas(&self, target: [u32; 2]) -> Canvas {
        Canvas::new(target, Fit::Fixed)
    }

    /// The loading lifetime `lifetime` is about to draw its frame,
    /// between frames: a skin whose loading state belongs to another
    /// lifetime and was built starts it afresh, which may allocate. The
    /// default keeps nothing per lifetime.
    fn enter_loading(&mut self, lifetime: u64) {
        let _ = lifetime;
    }

    /// Fills `out`, which arrives empty, with the play screen's frame `f`
    /// while a chart loads before READY (design.md "Play
    /// Lifecycle"): the lanes and the rest of the screen without notes,
    /// and nothing of any chart: the skin may still hold the previous
    /// chart's preparation, whose values the loading frame must not show.
    /// The renderer draws the load's text, the chosen chart's, over it.
    /// Allocation-free, the error included. The default draws nothing.
    ///
    /// # Errors
    ///
    /// The model's [`BuildError`] for a loading frame it cannot build: the
    /// renderer drops what was pushed, submits nothing, and builds that
    /// loading state no more within its lifetime.
    fn build_loading(
        &mut self,
        f: &LoadingFrame<'_>,
        out: &mut SpriteBatch,
    ) -> Result<(), BuildError> {
        let _ = (f, out);
        Ok(())
    }
}
