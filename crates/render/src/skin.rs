//! The skin boundary (section 2, "Skins"): what a skin is given each frame
//! and what it must do.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, TimeUs};
use eval::{JudgeEvent, Section};
use game::PlayView;
use skin_model::{
    BuildError, Canvas, ChartText, CoverSettings, Fit, FrameReadouts, LaneView, Layout, LoadState,
    LoadingGauge, PlayContext,
};

use crate::{Gpu, MAX_SPRITES, SpriteBatch};

// The producer's bound is the model's (design.md "Play readouts").
const _: () = assert!(game::readouts::READOUT_MAX == skin_model::READOUT_MAX);

/// What one frame draws from.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// Chart time when the frame's work starts, read from QPC.
    pub now: TimeUs,
    /// The display latency offset: how much later than `now` the frame is
    /// expected to reach the screen (design.md "Rendering and Skins"), as
    /// the presentation snapshot holds it.
    pub display_offset: TimeUs,
    /// The state clock: µs since the play state was entered, sampled with
    /// `now` ([`SkinFrame::state_us`](skin_model::SkinFrame::state_us)).
    pub state_us: i64,
    /// The lanes: the exact original-lane gain and the effective covers
    /// (design.md "Play readouts"), from the frame's presentation
    /// snapshot ([`presentation::frame`](crate::presentation::frame)).
    pub lanes: LaneView,
    /// The readouts, from the same snapshot as `lanes`.
    pub readouts: FrameReadouts,
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
    /// The turntable's angle on the play state's clock
    /// ([`skin_model::SkinFrame::turntable`]).
    pub turntable: i32,
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
    /// The effective covers; a loading frame has no gain.
    pub lanes: LaneView,
    /// The stored cover settings `lanes` follows.
    pub covers: CoverSettings,
    /// The display offset ([`skin_model::LoadingFrame::display_offset`]).
    pub display_offset: TimeUs,
    /// The speed policy's hi-speed fix
    /// ([`skin_model::LoadingFrame::fix_hispeed`]).
    pub fix_hispeed: u8,
    /// The shown gauge's descriptor; `None` leaves what depends on it
    /// undrawn.
    pub gauge: Option<LoadingGauge>,
    /// The chosen chart's play level; `None` leaves what depends on it
    /// undrawn.
    pub play_level: Option<i32>,
    /// The chosen chart's difficulty (`skin_model::LoadingFrame::difficulty`);
    /// `None` leaves what depends on it undrawn.
    pub difficulty: Option<u8>,
    /// Whether the chosen chart has a BGA; `None` leaves what depends on it
    /// undrawn.
    pub bga: Option<bool>,
    /// Whether the play loaded is autoplay.
    pub autoplay: bool,
    /// The revision of the loading text installed for this lifetime; 0
    /// with none.
    pub text: u64,
    /// The turntable's angle on the play state's clock
    /// ([`skin_model::LoadingFrame::turntable`]).
    pub turntable: i32,
}

impl<'a> LoadingFrame<'a> {
    /// The model's frame of it.
    #[must_use]
    pub const fn model(&self) -> skin_model::LoadingFrame<'a> {
        skin_model::LoadingFrame {
            difficulty: self.difficulty,
            bga: self.bga,
            autoplay: self.autoplay,
            now: self.now,
            load: self.load,
            target: self.target,
            layout: self.layout,
            turntable: self.turntable,
            lanes: self.lanes,
            covers: self.covers,
            display_offset: self.display_offset,
            fix_hispeed: self.fix_hispeed,
            gauge: self.gauge,
            play_level: self.play_level,
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
    /// What the attempt takes over from the earlier attempts of its play
    /// state; [`Continuation::Fresh`](skin_model::Continuation::Fresh)
    /// for a play state's first.
    pub continuation: skin_model::Continuation<'a>,
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
    /// What `batch`, this skin's last build, holds as a list; `None` for a
    /// skin that keeps no observations (dev-tools: a scenario's captures).
    #[cfg(feature = "dev-tools")]
    fn display_list(
        &self,
        batch: &skin_model::QuadBatch,
        clips: &crate::ClipTable,
    ) -> Option<skin_model::DisplayList> {
        let _ = (batch, clips);
        None
    }

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

    /// The prepared attempt's READY candidate was rearmed (design.md "Play
    /// Lifecycle", the start gate): its runtime state (what the effects
    /// built up, the timers) starts over in place, its chart and text
    /// resources kept, allocating nothing. Nothing by default.
    fn rearm(&mut self) {}

    /// What the prepared attempt gives the next attempt of its play state
    /// ([`skin_model::Play::carry`]); `None` for a skin with no presentation
    /// program state, the default.
    fn carry(&self) -> Option<skin_model::Carry<'_>> {
        None
    }

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
    /// lifetime and was built starts it afresh, allocating nothing. The
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

/// The stored cover settings a frame reports, from the adjustments' one
/// snapshot (design.md "Play readouts", "Play Adjustments", publication):
/// the configured amounts and switches, and `adjusting` (condition 270,
/// `lanecover1_changing`) true while START or SELECT is held, so an
/// imported skin's 270 reads what the source tests.
#[must_use]
pub const fn cover_settings(s: &game::adjustment::PresentationSnapshot) -> CoverSettings {
    let covers = s.options.covers();
    CoverSettings {
        cover: covers.sudden.amount,
        lift: covers.lift.amount,
        hidden: covers.hidden.amount,
        cover_enabled: covers.sudden.on,
        lift_enabled: covers.lift.on,
        hidden_enabled: covers.hidden.on,
        adjusting: s.adjusting,
    }
}

#[cfg(test)]
mod tests {
    use game::adjustment::{
        Adjustment, ControlId, ControlRoles, Event, NoTempo, Observation, Role,
    };
    use game::{Cover, Covers, HiSpeedSettings, PlayOptions};

    use super::cover_settings;
    use bms_model::TimeUs;

    /// Condition 270 reads START or SELECT held from the snapshot, with
    /// the configured covers beside it.
    #[test]
    fn adjusting_follows_start_or_select_held_in_the_snapshot() {
        let covers = Covers {
            sudden: Cover {
                amount: 250,
                on: true,
            },
            lift: Cover {
                amount: 100,
                on: false,
            },
            hidden: Cover::default(),
        };
        let options = PlayOptions::new(HiSpeedSettings::default(), covers).unwrap();
        let mut roles = ControlRoles::default();
        roles.set(ControlId(0), Role::Start);
        roles.set(ControlId(1), Role::Select);
        let mut a = Adjustment::new(options, roles, TimeUs(-1_000_000), 0);
        let at = |t, seq, event| Observation {
            time: TimeUs(t),
            seq,
            event,
        };
        let c = cover_settings(&a.snapshot());
        assert!(!c.adjusting);
        assert_eq!(
            (c.cover, c.cover_enabled, c.lift, c.lift_enabled),
            (250, true, 100, false)
        );
        a.observe(at(10, 0, Event::Press(ControlId(1))), &NoTempo);
        assert!(cover_settings(&a.snapshot()).adjusting, "SELECT held");
        a.observe(at(20, 1, Event::Release(ControlId(1))), &NoTempo);
        a.observe(at(30, 2, Event::Press(ControlId(0))), &NoTempo);
        assert!(cover_settings(&a.snapshot()).adjusting, "START held");
        a.observe(at(40, 3, Event::Release(ControlId(0))), &NoTempo);
        assert!(!cover_settings(&a.snapshot()).adjusting);
    }
}
