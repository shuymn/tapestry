//! The game thread's rig: the play lifecycle of `game::live` on the real
//! threads (design.md "Overall Architecture", "Play Lifecycle").
//!
//! Each play has its own timeline: the input thread opens recording at a
//! boundary and puts chart time 0 `lead_in` after it on the QPC clock. The
//! source set moves every recorded time onto the play's timeline, the logic
//! step advances to the clock minus that origin, the audio engine and the
//! key sounds' origins sit on the clock with the play's origin, and the
//! render thread draws at the clock minus the origin. This thread runs the
//! logic step, woken by the input thread after each publication.

use std::thread;
use std::time::{Duration, Instant};

use audio::OutputError;
use bms_model::{TimeSource, TimeUs};
use eval::JudgeEvent;
use game::{
    Advance, AudioQueue, AudioWaitError, Chain, Closing, Command, InputWaitError, Loaded,
    MenuCommand, Opening, PlayReport, Preparation, RenderSide, RestoreError, Restored,
    ResultScreen, Rig, Settings, Tuning,
};
use input::{QpcClock, SourceReader};

use crate::attempt_context::{AttemptContext, SIMPLE_SCRATCH};
use crate::audio_side::AudioLink;
use crate::controls::Controls;
use crate::end_screen::{self, Hints, LoadTimes, QueueLine};
use crate::input_side::{InputLink, Recorded, Request};
use crate::loading_state::{LoadInput, shown_gauge};
use crate::measure::{
    AudioBase, Figures, PlayMeasure, Step, StepSample, judgment_samples, limits_line, measure_path,
    process_page_faults, to_toml,
};
use crate::render_side::{
    AttemptSkin, Declared, EndPage, RenderEnd, RenderLink, RenderMsg, ScreenError,
};

use crate::result_input::ResultInput;
use crate::tuning::{Delivered, SettingsFiles};

/// How long the logic step parks at most without a wake from the input
/// thread; also the pace of the tail.
const PARK: Duration = Duration::from_millis(2);

/// How long the game thread parks at most outside a play and its tail
/// (the end screen): until the window or the system unparks it
/// ([`Controls`]), or the next look at what has no wake of its own (the
/// screen's failure, a timed quit).
const IDLE_PARK: Duration = Duration::from_micros(game::IDLE_WAIT.0.unsigned_abs());

/// How long a wait for the input thread's marker parks at most
/// ([`game::InputPump::park`]).
pub(crate) const MARKER_PARK: Duration = Duration::from_millis(1);

/// How long the result's idle wait parks between looks while a layout
/// job's outcome was received and is not yet taken: the job wakes the game
/// thread right after its send, but its outcome can be taken only once its
/// thread has ended ([`crate::imports::Job::poll`]), so the look is
/// repeated in short slices instead of waiting for the next deadline (up
/// to `IDLE_WAIT`, 250 ms).
const LAYOUT_RELOOK: Duration = Duration::from_millis(1);

/// How long an operation waits for the input thread's markers in all, in
/// awake time, before the input thread counts as failed (design.md "Input
/// thread failure": 2 s of awake time, a suspend not counted).
const MARKER_TIMEOUT: Duration = Duration::from_secs(2);

/// The input thread as the adjustment driver needs it
/// ([`game::InputPump`]): fences through its link, wakes, and the display
/// offset the window steps.
pub(crate) struct PlayerPump<'a> {
    pub(crate) input: &'a InputLink,
    pub(crate) tuning: &'a crate::tuning::LiveTuning,
    /// The controls whose watched decision screen each slice of a wait
    /// looks at ([`Controls::look`]); none: no look.
    pub(crate) look: Option<&'a Controls>,
    /// When this pump's waits began, in awake time: they end
    /// [`MARKER_TIMEOUT`] of awake time later, a suspend not counted.
    pub(crate) since: Option<Duration>,
}

impl game::InputPump for PlayerPump<'_> {
    fn request_fence(&mut self, n: u64) {
        self.input.request_fence(n);
    }

    fn wake(&mut self) {
        self.input.wake_input();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn display_offset_us(&self) -> i64 {
        self.tuning.display_offset_us()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn with_ingress(&mut self, f: &mut game::IngressDrain<'_>) {
        self.tuning
            .with_ingress(|word| f(Some(word), &between_ingress));
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn look(&mut self) -> game::Verdict {
        self.look
            .and_then(Controls::look)
            .unwrap_or(game::Verdict::Continue)
    }

    fn park(&mut self) -> Result<(), InputWaitError> {
        if self.input.gone() {
            return Err(InputWaitError::Gone);
        }
        if let Some(e) = self.input.failure() {
            return Err(e);
        }
        let now = self.input.awake_now();
        let since = *self.since.get_or_insert(now);
        if now.saturating_sub(since) >= MARKER_TIMEOUT {
            return Err(InputWaitError::Timeout);
        }
        thread::park_timeout(MARKER_PARK);
        Ok(())
    }
}

/// Between a drain's validation of an observation and its action, the step
/// lock held: nothing outside tests.
#[cfg(not(test))]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
const fn between_ingress() {}

#[cfg(test)]
thread_local! {
    /// A test's meeting point between a drain's validation and its action.
    pub(crate) static BETWEEN_INGRESS: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Between a drain's validation of an observation and its action, the step
/// lock held: the test's meeting point, if it set one on this thread.
#[cfg(test)]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn between_ingress() {
    BETWEEN_INGRESS.with_borrow(|f| {
        if let Some(f) = f {
            f();
        }
    });
}

/// One drain of the adjustments' driver, if there is one: what the tap
/// observed since reaches the reducer and its snapshot (design.md "Play
/// Adjustments"). Allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn serve_adjustments(
    adjust: Option<&mut game::AdjustDriver>,
    input: &InputLink,
    tuning: &crate::tuning::LiveTuning,
) {
    if let Some(driver) = adjust {
        driver.pump(&mut PlayerPump {
            input,
            tuning,
            look: None,
            since: None,
        });
    }
}

/// Each saved play's record and figures, in order (dev-tools).
#[cfg(feature = "dev-tools")]
pub type MeasuredPlays = std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, Figures)>>>;

/// The game's threads as a rig.
pub struct PlayerRig<'a> {
    /// The input and audio threads' failures, as one state every reader
    /// asks ([`crate::failures::Failures`]).
    pub failures: crate::failures::Failures,
    /// The input thread.
    pub input: InputLink,
    /// The audio thread.
    pub audio: AudioLink,
    /// The render thread.
    pub render: RenderLink,
    /// The play skin's installation: charts are admitted once it is
    /// ready.
    pub skin: crate::render_side::SkinState,
    /// The pending play skin, when startup marked it pending: each
    /// attempt is prepared with its specialization for the attempt's
    /// facts ([`crate::imports::PendingSkin`]).
    pub pending: Option<crate::imports::PendingSkin>,
    /// The chart's own images' worker (S10-12): the run's one owner.
    pub chart_images: crate::chart_images::ChartImageWorker,
    /// The page side of the committed play skin's chart layer, if it keeps
    /// one ([`crate::imports::Started::play_chart`]).
    pub play_chart: Option<u32>,
    /// The window's controls.
    pub controls: &'a Controls,
    /// Whether the window was shown: recording opens once it is. `None`
    /// without a window (headless).
    pub window: Option<crate::window::WindowShown>,
    /// The clock, started with the process.
    pub clock: QpcClock,
    /// What each play records.
    pub recorded: Recorded,
    /// The gauge the settings show (`displayed_gauge`), by its index in
    /// a play preset, never changed by an attempt: each preset resolves it
    /// afresh, the select screen's loading gauge included
    /// ([`loading_gauge`]).
    pub gauge: usize,
    /// The gauge the current attempt shows, as it resolved the settings'
    /// ([`game::Loaded::displayed`]): its loading gauge and its frames.
    pub attempt_gauge: usize,
    /// Whether the session plays generated input (design.md "Autoplay"):
    /// no recommended input offset, and "not saved" when nothing was.
    pub autoplay: bool,
    /// The run's play states, for the turntable (docs/stage3.md, offset
    /// 1).
    pub play_states: game::PlayStates,
    /// READY's length: from the opening to chart time 0, as the
    /// current play's opening was given it.
    pub lead_in: TimeUs,
    /// What restores a practice round's audio: the engine's seek source and
    /// the round's schedule; `None` without practice, which refuses a round.
    pub restorer: Option<game::Restorer>,
    /// The current round's restore, planned at its opening, until its start
    /// sends it.
    pub restore: Option<Box<game::Restore>>,
    /// Where the current round's audio is restored, in its chart time.
    pub restore_at: Option<TimeUs>,
    /// From recording's open to the end of the play's audio tail: the
    /// game thread parks [`PARK`] at most, else [`IDLE_PARK`].
    pub in_play: bool,
    /// The input offset the current play uses (a device play; autoplay
    /// plays with 0 whatever it is).
    pub input_offset_us: i64,
    /// The settings files.
    pub files: SettingsFiles,
    /// The settings keys' values last acted on.
    pub seen: Delivered,
    /// The hi-speed line the end screen was last sent with.
    pub shown_speed: end_screen::SpeedLine,
    /// The end screen's page while it shows: the summary's lines and the
    /// view's graphs; the settings lines are added each time it is sent.
    pub end_page: Option<EndPage>,
    /// The screen's failure a chart's load hid behind
    /// [`crate::selection::ChartLoadError::ScreenFailed`], for the flow
    /// ([`game::AppRig::screen_load_failed`]).
    pub screen_load_failure: Option<ScreenError>,
    /// The end screen's key names.
    pub hints: Hints,
    /// The result screen's input.
    pub result_input: ResultInput,
    /// The view last printed: its entry and line ([`crate::end_screen::view_log`]).
    pub viewed: Option<(game::EntryId, String)>,
    /// The evaluations the shown result had made when last printed.
    pub evaluated: u64,
    /// The result screen's input, scripted, without selection (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub result_script: Option<crate::dev::SelectScript>,
    /// What the first play's load time runs from: the process start (the
    /// clock's 0), or the end of what the command set up first (a
    /// measurement's PresentMon capture).
    pub load_start: TimeUs,
    /// The latest retry taken, by its receipt time: the next play starts
    /// from it.
    pub pending_retry: Option<TimeUs>,
    /// The retry the current play started from.
    pub retry_from: Option<TimeUs>,
    /// Where the play state was entered: each play frame's state clock
    /// runs from it.
    pub(crate) state_clock: crate::state_clock::StateClock,
    /// The retries a result has taken since the play was prepared, by
    /// receipt: only the one its flow acts on becomes the next play's
    /// retry, whose load time runs from it.
    pub(crate) result_retries: ResultRetries,
    /// Load times shown on the end screen.
    pub times: LoadTimes,
    /// When the current play's READY state started, on the clock.
    pub ready_state_at: Option<TimeUs>,
    /// Where the current play's chart time 0 lies on the clock.
    pub origin: Option<TimeUs>,
    /// What the play skin declares, from the current attempt's commit
    /// receipt ([`commit_ready`]); none declared before it.
    pub declared: Declared,
    /// Where each logic step's measurement goes (the render thread).
    pub steps: rtrb::Producer<StepSample>,
    /// Step samples of the current play the full queue refused.
    pub steps_dropped: u64,
    /// The figures of the play just over, until they are written.
    pub figures: Option<Figures>,
    /// The current play's interval, from its start to its end: no screen
    /// runtime runs while it is open (design.md "beatoraja Lua Skins",
    /// Quiescence acknowledged).
    pub interval: Option<crate::screens::PlayInterval>,
    /// The current play's number, as the audio thread knows it.
    pub generation: u64,
    /// The periods the next play queues on the audio output.
    pub queue: game::QueueChoice,
    /// The most periods the output's queue holds, when it says.
    pub queue_most: Option<u32>,
    /// The periods the output keeps for the current play, when it says.
    pub queued_now: Option<u32>,
    /// The last ended play's audio queue, for its end screen.
    pub queue_line: Option<QueueLine>,
    /// Each saved play's record and figures, kept for the display
    /// capture of `measure play` (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub measured_plays: Option<MeasuredPlays>,
    /// The scenario the run plays (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub scenario: Option<crate::dev::scenario::State>,
    /// Quit once the end screen has shown this long and its page has been
    /// drawn (scripted runs; [`scripted_end_over`]).
    #[cfg(feature = "dev-tools")]
    pub quit_after_end: Option<Duration>,
    #[cfg(feature = "dev-tools")]
    pub end_shown: Option<std::time::Instant>,
    /// Retries still to send by itself at an end screen (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub auto_retries: u32,
    /// A loading script is to be fed before the first opening
    /// (`--dev-loading`), until it is started.
    #[cfg(feature = "dev-tools")]
    pub loading_script: bool,
    /// Settings keys pressed by themselves when the first play reaches
    /// chart time `adjust_at` (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub adjust_in_play: Vec<game::Adjust>,
    /// Speed keys pressed by themselves on the window when an autoplay
    /// session's first play reaches chart time `adjust_at` (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub speed_in_play: Vec<u32>,
    #[cfg(feature = "dev-tools")]
    pub adjust_at: TimeUs,
    /// The select screen, with song selection.
    pub selection: Option<crate::selection::Selection>,
    /// The plays' gain without song selection (`tapestry play`): measured
    /// at the load, else the headroom.
    pub gain: audio::Gain,
    /// Settings keys pressed by themselves at the first end screen
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub adjust_on_end: Vec<game::Adjust>,
    /// The start gate's player side (design.md "Play Lifecycle", the start
    /// gate), installed with the adjustments' driver by every launcher:
    /// `None` runs the plays ungated, every first READY the play.
    pub gate: Option<crate::start_gate::PlayerGate>,
    /// The adjustments' driver (design.md "Play Adjustments"): the game
    /// thread's one writer of the presentation, installed with the gate.
    pub adjust: Option<game::AdjustDriver>,
    /// START and SELECT held as the input thread publishes them for the
    /// select screen's panels (design.md "Input", Menu input), installed
    /// with the adjustments' tap.
    pub panels: Option<input::PanelLevel>,
    /// The result skin (design D10-D13): its image stage in each
    /// attempt's Preparing, its layout on the result and each switch,
    /// published on the CPU.
    pub result_skin: crate::result_skin::ResultSkin,
    /// The attempt's chart facts a result view carries.
    pub result_chart: Option<skin_model::ResultChart>,
    /// The attempt's preparation begun last, until the next.
    pub prep: Option<AttemptPrep>,
    /// The reducer's count of accepted adjustments when the loading
    /// state's opening took its starting settings, read after a fence
    /// ([`game::LoadingRig::apply_snapshot`]): what the revalidation
    /// compares.
    pub accepted_at_o1: Option<u64>,
    /// The window's focus-loss generation the loading state's tuning
    /// acceptance closed with, while it is closed: the opening asked for
    /// carries it, and the input thread defers it when focus was lost
    /// since.
    pub accepted_focus: Option<u64>,
    /// The preparations begun so far: each one's ticket.
    pub preps: u64,
    /// The result's presentation (the S10-09i design's D1): its lifetime,
    /// the desired view and what was published.
    pub presenter: crate::result_present::Presenter,
    /// The chart's strings the attempt's result shows.
    pub result_labels: Option<skin_model::ResultLabels>,
    /// The attempt's result upload was withdrawn by a quit, a close or a
    /// thread's failure: the end of loading does not wait on the render
    /// thread, which may still be in it.
    pub result_withdrawn: bool,
    /// The facts behind the result strip's lines that do not depend on the
    /// view, while a result lifetime is current.
    pub result_facts: Option<crate::result_present::OverlayFacts>,
    /// The input offset last seen while the result shows, and whether the
    /// offset keys changed it in this lifetime (O6).
    pub result_offset: Option<(i64, bool)>,
    /// The flow served the result's leaving command: the next screen's
    /// preparation leaves its lifetime. Until then a screen built again
    /// keeps it, an outro playing included.
    pub result_leaving: bool,
    /// The views the result has changed to so far, each printed once
    /// (dev-tools: a scripted view command is over once its view shows).
    #[cfg(feature = "dev-tools")]
    pub views_shown: u64,
    /// When the accepted leaving command's outro began (the view pinned),
    /// while the lifetime is current: no view's layout is asked for.
    pub result_outro: Option<TimeUs>,
    /// Development builds: G2's start (the S10-09i design's §8.1), when
    /// the attempt's `commit_ready` returned, and what its result step
    /// did, until the opening prints it.
    #[cfg(feature = "dev-tools")]
    pub probe_ready: Option<(std::time::Instant, &'static str)>,
    /// Tests: set when the result's idle wait found a layout's outcome
    /// received and not yet taken (its thread still ending), before it
    /// looks again.
    #[cfg(test)]
    pub relooked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Tests: the audio output reads as lost while set, from any thread.
    #[cfg(all(test, feature = "dev-tools"))]
    pub audio_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Tests: where the loading opener's steps wait while a test holds
    /// them.
    #[cfg(test)]
    pub opening_gates: std::sync::Arc<loading::OpeningGates>,
    /// Tests: each ended play's end and record, in order.
    #[cfg(test)]
    pub ended_plays: Vec<(game::PlayEnd, Option<replay::PlayRecord>)>,
}

/// An attempt's preparation begun on the player's rig
/// ([`Rig::begin_preparation`]): what it was begun with, and where it
/// stands. Dropped, an unanswered render request is discarded.
pub struct AttemptPrep {
    /// The handle's ticket.
    ticket: u64,
    /// The attempt, until the render thread is asked.
    attempt: Option<game::Attempt>,
    /// The loading gauge's descriptor, from the refreshed play preset.
    loading_gauge: Option<skin_model::LoadingGauge>,
    /// The gauge the attempt shows, by its index in the preset.
    displayed: usize,
    /// The attempt's chart facts a result view carries.
    chart: skin_model::ResultChart,
    /// The attempt's judgment class.
    class: Option<skin_beatoraja::JudgeClass>,
    /// The chart's strings the attempt's result shows, until the result
    /// skin's stage takes them.
    labels: Option<skin_model::ResultLabels>,
    /// The chart's folder and its stage file and back image names, which
    /// the attempt's images are read from (S10-12).
    images: (std::path::PathBuf, [String; 2]),
    /// Autoplay's scratch notes for the turntable, built with the attempt;
    /// none for a play by hand.
    scratch_notes: Vec<game::ScratchNote>,
    /// Prepared with a pending play skin's specialization.
    specialized: bool,
    /// The play state and chart the attempt belongs to.
    carry_key: Option<crate::carry::CarryKey>,
    /// When the render thread was asked.
    staging: Instant,
    stage: PrepStage,
}

/// Where an attempt's preparation stands.
enum PrepStage {
    /// The pending play skin's specialization runs.
    Skin(Box<crate::imports::Specializing>),
    /// The result skin's image stage runs, the play skin known.
    Image(AttemptSkin, Box<crate::result_skin::ImageJob>),
    /// The render thread prepares the attempt.
    Screen(crate::render_side::PendingAttempt),
    /// Answered ready: the chart's images are settled and published for
    /// the attempt's target before its final readiness (S10-12 design C4):
    /// the request answered, and the publication's request, `Some` for a
    /// staging with an array of its own.
    Images(u64, Option<u64>),
    /// It ended before the render thread answered ready.
    Ended(Preparation),
    /// Answered: with the render request answered ready, which the final
    /// readiness and the commit name; `None` when it failed.
    Answered(Option<u64>),
}

/// How the play skin's stage began.
enum SkinStep {
    /// Known at once.
    Ready(AttemptSkin),
    /// The specialization's job runs.
    Running(Box<crate::imports::Specializing>),
}

/// How a run's rig ended ([`PlayerRig::shut_down`]).
#[must_use]
#[derive(Debug)]
pub struct Shutdown {
    /// How the render thread ended.
    pub rendered: RenderEnd,
    /// A panic of a practice page job a Start was responsible for, found
    /// as the run ended: the run ends with it.
    pub page: Result<(), game::coord::LoadFailure>,
}

impl PlayerRig<'_> {
    /// The result skin's image stage for the attempt (design D11), begun
    /// after the play skin's specialization and before READY with what the
    /// attempt's preparation began with, and its play skin `skin`: decided
    /// at once, or its job running, waited for in the preparation's polls
    /// ([`poll_image`](Self::poll_image)).
    fn after_skin(&mut self, prep: &mut AttemptPrep, skin: AttemptSkin) -> PrepStage {
        self.result_chart = Some(prep.chart);
        self.result_labels = prep.labels.take();
        // Once the run disabled the result skin, no skin work is done: the
        // built-in result shows the chart and labels taken above.
        if self.result_run_disabled() {
            return self.send_screen(prep, skin);
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::stage_began(crate::dev::SpecializingStage::ResultImage);
        match self.result_skin.begin_image_stage(prep.chart, prep.class) {
            crate::result_skin::ImageStage::Ready(staged) => self.after_image(prep, skin, staged),
            crate::result_skin::ImageStage::Running(job) => PrepStage::Image(skin, job),
        }
    }

    /// The result skin's image stage `job`, waited for until `until`: the
    /// quit key cancels its job and waits for its thread; the window's
    /// close and a failure of the input or audio thread end it within the
    /// stop bound. `None` at `until` with the job still running.
    fn poll_image(
        &mut self,
        job: &mut crate::result_skin::ImageJob,
        until: Instant,
    ) -> Option<crate::result_skin::Staged> {
        let mut result = std::mem::take(&mut self.result_skin);
        let rig = &*self;
        let staged = result.poll_image_stage(
            job,
            Some(until),
            || rig.controls.leaving(),
            || rig.run_ends(),
        );
        self.result_skin = result;
        staged
    }

    /// What follows the image stage's end `staged`: a cancelled attempt
    /// ends, a ready one is asked of the render thread with `skin`.
    fn after_image(
        &mut self,
        prep: &mut AttemptPrep,
        skin: AttemptSkin,
        staged: crate::result_skin::Staged,
    ) -> PrepStage {
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: result image stage: {}",
            self.result_skin.staged_line()
        );
        match staged {
            crate::result_skin::Staged::Cancelled => PrepStage::Ended(Preparation::Cancelled),
            crate::result_skin::Staged::Ready => self.send_screen(prep, skin),
        }
    }

    /// Asks the render thread to prepare the attempt from its owned
    /// context with play skin `skin`, and returns at once. The loading
    /// lifetime's gauge descriptor is replaced from the preset the play
    /// refreshed, with the gauge the attempt shows.
    fn send_screen(&mut self, prep: &mut AttemptPrep, skin: AttemptSkin) -> PrepStage {
        let Some(attempt) = prep.attempt.take() else {
            return PrepStage::Ended(Preparation::Cancelled);
        };
        prep.specialized = matches!(skin, AttemptSkin::Specialized(_));
        // The images the attempt draws, fitted for its skin's chart layer.
        let side = match &skin {
            AttemptSkin::InUse => self.play_chart,
            AttemptSkin::Specialized(bundle) => bundle.chart_layer().map(|_| bundle.side()),
        };
        let (dir, names) = prep.images.clone();
        self.chart_images
            .want(crate::chart_images::Want { dir, names, side });
        self.attempt_gauge = prep.displayed;
        refresh_loading_gauge(&self.render, self.loading_active(), prep.loading_gauge);
        // The attempt attached last is the one prepared: its epoch is
        // the frames'.
        let epoch = self.adjust.as_mut().and_then(game::AdjustDriver::prepared);
        let attempt =
            AttemptContext::new(attempt, SIMPLE_SCRATCH, epoch.unwrap_or(0), self.autoplay)
                .with_scratch_notes(std::mem::take(&mut prep.scratch_notes))
                .with_carry_key(prep.carry_key);
        prep.staging = Instant::now();
        PrepStage::Screen(self.render.begin_attempt(attempt, skin))
    }

    /// The render thread's answer to the attempt's preparation `pending`,
    /// waited for until `until`, the adjustments served meanwhile, so an
    /// adjustment then is published before the preparation ends; a quit
    /// key, a close, or a failure of the input or audio thread stops the
    /// wait ([`preparation_stops`]), and the game looks at the threads for
    /// a failure's cause. `None` at `until` with no answer.
    fn poll_screen(
        &mut self,
        prep: &AttemptPrep,
        pending: &mut crate::render_side::PendingAttempt,
        until: Instant,
    ) -> Option<Result<crate::render_side::Answered, ScreenError>> {
        let mut adjust = self.adjust.take();
        let rig = &*self;
        let answered = pending.wait_until(Some(until), || {
            #[cfg(feature = "dev-tools")]
            crate::dev::press_due(rig.controls, crate::dev::LoadPhase::Preparing);
            serve_adjustments(adjust.as_mut(), &rig.input, rig.controls.tuning());
            preparation_stops(
                rig.quitting(),
                rig.controls.leaving(),
                rig.input_failure(),
                rig.audio_failure(),
            )
        });
        self.adjust = adjust;
        let answered = answered?;
        // The render thread's staging and upload of the specialization,
        // apart from the job's time.
        if prep.specialized {
            crate::imports::log_staged(
                prep.staging.elapsed(),
                answered.as_ref().map(|a| &a.preparation),
            );
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::print_adjustments("preparing", self.adjust.as_ref());
        Some(answered)
    }

    /// The chart's images when `loaded` is loaded (S10-12 design §5.3),
    /// in the generation its choice or Start began
    /// ([`crate::chart_images::ChartImageWorker::renew`]): with the play
    /// skin in use known (no pending skin), the images' job begins now and
    /// publishes them into the skin in use as soon as they are read, so
    /// the loading frames draw them while the chart's sounds load.
    pub(crate) fn chart_images_at_choice(&mut self, loaded: &Loaded) {
        if self.pending.is_some() {
            return;
        }
        let labels = loaded.labels();
        self.chart_images.want(crate::chart_images::Want {
            dir: crate::chart_images::folder(loaded.path()),
            names: [labels.stagefile.clone(), labels.backbmp.clone()],
            side: self.play_chart,
        });
        let coord = self.selection.as_ref().map(|s| (&s.coord, s.spawner));
        // No coordinator (no song selection): the preparation reads them.
        if coord.is_some() {
            let publisher = self.render.chart_publisher();
            self.chart_images.step(coord, &|| false, Some(&publisher));
        }
    }

    /// The chart's images for the attempt, until `until`: the worker
    /// stepped, and the set published into the attempt's target (`target`
    /// the staging's request when it has an array of its own, else the
    /// committed installation), the adjustments served meanwhile. `Some`
    /// true once no image worker is alive and the publication returned
    /// (S10-12 design C4), whatever it left; [`ImagesPoll::Stopped`] when
    /// the wait stops ([`preparation_stops`]), the worker kept;
    /// [`ImagesPoll::Expired`] when the publication's answer did not come
    /// within the render link's bound; `None` at `until`.
    fn poll_images(&mut self, target: Option<u64>, until: Instant) -> Option<ImagesPoll> {
        let worker = self.chart_images.clone();
        let mut adjust = self.adjust.take();
        let out = loop {
            let stops = || {
                preparation_stops(
                    self.quitting(),
                    self.controls.leaving(),
                    self.input_failure(),
                    self.audio_failure(),
                )
            };
            let coord = self.selection.as_ref().map(|s| (&s.coord, s.spawner));
            worker.step(coord, &stops, None);
            // A stop wins over images that settled meanwhile: the attempt
            // is not readied after the quit key or a close.
            if stops() {
                break Some(ImagesPoll::Stopped);
            }
            if worker.idle() {
                match worker.publish(&self.render, target) {
                    crate::chart_images::Publish::Answered(_) => break Some(ImagesPoll::Ready),
                    crate::chart_images::Publish::Expired => break Some(ImagesPoll::Expired),
                    crate::chart_images::Publish::Waiting => {}
                }
            }
            #[cfg(feature = "dev-tools")]
            crate::dev::press_due(self.controls, crate::dev::LoadPhase::Preparing);
            serve_adjustments(adjust.as_mut(), &self.input, self.controls.tuning());
            let now = Instant::now();
            if now >= until {
                break None;
            }
            std::thread::park_timeout(IMAGE_SLICE.min(until - now));
        };
        self.adjust = adjust;
        out
    }

    /// Whether the render thread disabled the result skin for the run (the
    /// S10-09i design's D7), as the presenter now knows too.
    fn result_run_disabled(&mut self) -> bool {
        let status = self.render.result_status();
        self.presenter.status(&status);
        self.presenter.disabled().is_some()
    }

    /// Whether the result's skin work is suppressed: the attempt's
    /// fallback or the run's (no image stage, layout or upload; the
    /// built-in result is still presented).
    fn result_skin_suppressed(&mut self) -> bool {
        self.presenter.attempt_fallen().is_some() || self.result_run_disabled()
    }

    /// The preparation begun last dropped (design.md "Play Lifecycle",
    /// preparing each attempt: the handle owns its subjobs), on its
    /// replacement or its discard, which do not end the run: a job still
    /// running is settled as the attempt's quit settles it, waited for to
    /// its thread's end, while the run's end ([`run_ends`](Self::run_ends))
    /// is still asked and, once it came, bounds the wait by the stop bound.
    /// The specialization's outcome is kept from a successful run, the
    /// image stage's images handed back unless it was abandoned. An
    /// unanswered render request is discarded as it drops. `None` when no
    /// preparation was begun.
    fn drop_prep(&mut self) -> Option<crate::imports::Settled> {
        let mut prep = self.prep.take()?;
        let settled = match std::mem::replace(&mut prep.stage, PrepStage::Answered(None)) {
            PrepStage::Skin(mut job) => match self.pending.take() {
                Some(mut pending) => {
                    let settled = pending.settle(&mut job, || self.run_ends());
                    self.pending = Some(pending);
                    settled
                }
                None => crate::imports::Settled::Ended,
            },
            PrepStage::Image(_, mut job) => {
                let mut result = std::mem::take(&mut self.result_skin);
                let settled = result.settle_image_stage(&mut job, || self.run_ends());
                self.result_skin = result;
                settled
            }
            // The images' worker is the rig's: it is never settled here.
            PrepStage::Screen(_)
            | PrepStage::Images(..)
            | PrepStage::Ended(_)
            | PrepStage::Answered(_) => crate::imports::Settled::Ended,
        };
        Some(settled)
    }

    /// The preparation begun last dropped as the run ends (its shut-down):
    /// a job still running is cancelled and given up within the stop
    /// bound, the specialization's outcome kept from a successful run and
    /// the image stage's images handed back unless it was abandoned.
    fn end_prep(&mut self) {
        let Some(mut prep) = self.prep.take() else {
            return;
        };
        match std::mem::replace(&mut prep.stage, PrepStage::Answered(None)) {
            PrepStage::Skin(mut job) => {
                if let Some(pending) = self.pending.as_mut() {
                    let _ = pending.settle(&mut job, || true);
                }
            }
            PrepStage::Image(_, mut job) => {
                let _ = self.result_skin.settle_image_stage(&mut job, || true);
            }
            PrepStage::Screen(_)
            | PrepStage::Images(..)
            | PrepStage::Ended(_)
            | PrepStage::Answered(_) => {}
        }
    }

    /// Whether the run ends: the window's close, or a failure of the input
    /// or audio thread. A script's step on a specialization's job
    /// (dev-tools) acts at every look.
    fn run_ends(&self) -> bool {
        #[cfg(feature = "dev-tools")]
        crate::dev::specialization_signal(self.controls, || self.input.fail_for_script());
        self.controls.quitting() || self.input_failure().is_some() || self.audio_failure().is_some()
    }

    /// The atlases the render thread retains for the result go to the
    /// result skin (the S10-09i design's §2.1 Q3), then its finished job's
    /// outcome is taken; while a result lifetime is current, the desired
    /// view's payload is published once its layout is (D1), and a fallback
    /// the render thread reported applies (D7). Returns whether the latest
    /// request is published.
    fn land_result(&mut self) -> bool {
        let status = self.render.result_status();
        self.result_skin.set_retained(&status.retained);
        let ready = self.result_skin.poll();
        if self.presenter.current() {
            if self.presenter.status(&status) || ready {
                self.presenter
                    .land(&mut self.render, self.result_skin.shown());
            }
            self.present_overlay();
        }
        ready
    }

    /// The result's view of `screen` after `report`, as the end page just
    /// built shows it, becomes the desired one: its snapshot is kept until
    /// its layout lands. A result lifetime begins with the result's first
    /// show. Nothing without a result skin; once the run disabled it, each
    /// view is still presented, as the built-in result with the reason.
    pub(crate) fn present_view(&mut self, report: &PlayReport, screen: &ResultScreen) {
        // The page exactly as the end screen is sent it, its settings
        // lines included: the render side's page base for the lifetime's
        // first frame, before that page arrives.
        let (Some(chart), Some(page)) = (
            self.result_chart,
            self.end_page.as_ref().map(|base| {
                end_screen::with_settings(base, self.controls.tuning().get(), self.speed_line())
            }),
        ) else {
            return;
        };
        if !self.result_skin.has_skin() {
            return;
        }
        if !self.presenter.current() {
            self.presenter.begin(&mut self.render, self.clock.now());
            self.result_leaving = false;
            self.result_offset = Some((self.controls.tuning().get().input_offset_us, false));
        }
        self.presenter.want(crate::result_present::Snapshot {
            view: crate::result_present::view_of(screen, chart),
            labels: self.result_labels.clone().unwrap_or_default(),
            lines: crate::result_present::view_lines(report, screen),
            page,
        });
        self.result_facts = Some(crate::result_present::OverlayFacts::of(
            report,
            screen,
            self.autoplay,
        ));
        self.land_result();
    }

    /// The strip's lines that do not depend on the view, as they stand:
    /// the offset guidance once the offset keys changed the offset in this
    /// lifetime, and a thread's failure.
    fn present_overlay(&mut self) {
        let Some(facts) = &self.result_facts else {
            return;
        };
        let now = self.controls.tuning().get().input_offset_us;
        let used = match &mut self.result_offset {
            Some((seen, used)) => {
                *used |= *seen != now;
                *seen = now;
                *used
            }
            None => false,
        };
        let failure = self
            .input_failure()
            .map(|e| format!("INPUT: {e}"))
            .or_else(|| self.audio_failure().map(|e| format!("AUDIO: {e}")));
        let lines = facts.lines(used.then_some(now), failure);
        self.presenter.lines(&mut self.render, lines);
    }

    /// The result's lifetime ends: nothing of it is drawn any more, and
    /// the input offset keys stop working (design.md "Play Lifecycle",
    /// tuning while loading) whether selection, the practice page or a
    /// retry's preparation comes next.
    pub(crate) fn leave_result(&mut self) {
        let _ = self.controls.tuning().close_end_screen();
        #[cfg(feature = "dev-tools")]
        if self.presenter.current() {
            crate::dev::print_result_work("left");
        }
        #[cfg(feature = "dev-tools")]
        if let Some(at) = self.result_outro {
            println!(
                "dev: result left {} us after its outro began",
                self.clock.now().0 - at.0
            );
        }
        self.presenter.end(&mut self.render);
        self.result_facts = None;
        self.result_offset = None;
        self.result_leaving = false;
        self.result_outro = None;
    }

    /// The attempt's result is uploaded after its commit and before
    /// recording opens (the S10-09i design's D2): a fixed skin reinstalled
    /// when play's admission released it, a chart-only specialization, or
    /// a prepared atlas, unless its atlas is installed. The wait looks at
    /// the quit key, the close and the input and audio threads between
    /// slices: on any of them the upload is withdrawn, so a late staging
    /// installs nothing, and the end of loading does not wait on the
    /// render thread. A refusal shows the built-in result for the attempt.
    pub(crate) fn upload_result(&mut self) {
        self.result_withdrawn = false;
        let status = self.render.result_status();
        if status.disabled.is_some() {
            return;
        }
        let Some(upload) = self.result_skin.upload() else {
            return;
        };
        // Development builds: `cold` asks for every upload (G1), and G2's
        // sample says what the step did: `none` (no skin, or the kind
        // disabled), `reused` (installed) or `uploaded` (asked for).
        #[cfg(feature = "dev-tools")]
        let installed = status.installed == Some(upload.atlas()) && !crate::dev::result_cold();
        #[cfg(not(feature = "dev-tools"))]
        let installed = status.installed == Some(upload.atlas());
        #[cfg(feature = "dev-tools")]
        if let Some((_, step)) = &mut self.probe_ready {
            *step = if installed { "reused" } else { "uploaded" };
        }
        if installed {
            return;
        }
        let mut pending = self.render.upload_result(upload);
        loop {
            if preparation_stops(
                self.quitting(),
                self.controls.leaving(),
                self.input_failure(),
                self.audio_failure(),
            ) {
                // Withdrawn as it drops: a completion after it installs
                // nothing.
                drop(pending);
                self.result_withdrawn = true;
                #[cfg(feature = "dev-tools")]
                println!("dev: result upload withdrawn");
                return;
            }
            let why = match pending.poll(crate::render_side::SELECT_SLICE) {
                None => continue,
                Some(Ok(Preparation::Ready)) => return,
                Some(Ok(Preparation::Refused(why))) => why.to_string(),
                Some(Ok(Preparation::Cancelled)) => "the upload was withdrawn".to_owned(),
                Some(Err(e)) => Chain(&e).to_string(),
            };
            println!("skin: result: built-in for this attempt: {why}");
            self.presenter.attempt_fallback(why);
            return;
        }
    }

    /// The result skin's running layout job cancelled and waited for; the
    /// run's end bounds the wait ([`imports::STOP_WAIT`]).
    fn settle_result(&mut self) {
        let mut result = std::mem::take(&mut self.result_skin);
        let rig = &*self;
        // When the run's end was first seen (dev-tools witness).
        #[cfg(feature = "dev-tools")]
        let ended: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
        result.settle(|| {
            let ends = rig.run_ends();
            #[cfg(feature = "dev-tools")]
            if ends && ended.get().is_none() {
                ended.set(Some(std::time::Instant::now()));
            }
            ends
        });
        self.result_skin = result;
        #[cfg(feature = "dev-tools")]
        if let Some(at) = ended.get() {
            println!(
                "dev: result skin settled {} ms after the run's end",
                at.elapsed().as_millis()
            );
        }
        // The job's thread has ended, and its VMs with it, unless the
        // run's end gave it up (dev-tools witness).
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: result skin jobs: live {} joined {} of {}",
            skin_beatoraja::vm_hooks::live(),
            crate::imports::jobs_joined(),
            crate::imports::jobs_started()
        );
    }

    /// The play skin the attempt at `loaded` is prepared with, begun: the
    /// skin in use, or the pending play skin's specialization for the
    /// attempt's facts ([`crate::imports::PendingSkin::begin`]), reused
    /// while the facts it read hold, else its job started and waited for in
    /// the preparation's polls ([`poll_skin`](Self::poll_skin)). A job's
    /// wait shows the stage line on the loading state, or the preparation
    /// page without one (a retry, a round).
    fn begin_skin(&mut self, loaded: &Loaded) -> Result<SkinStep, Preparation> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(SkinStep::Ready(AttemptSkin::InUse));
        };
        let facts = crate::imports::facts::attempt_facts(loaded, self.autoplay);
        let rig = &*self;
        let wait = || {
            println!("skin: play: building for the chart");
            match rig.selection.as_ref() {
                Some(sel) if sel.loading.is_some() => crate::selection::show_stage(
                    &rig.render,
                    sel.loading.as_ref(),
                    crate::loading_text::Stage::Skin,
                ),
                // The wait in the page's notice line.
                Some(sel) => {
                    let mut page = sel.screen.page("");
                    if let Some(notice) = page
                        .lines
                        .iter_mut()
                        .find(|l| l.style == crate::menu_screen::LineStyle::Notice)
                    {
                        crate::ui_text::Label::SkinPreparing
                            .text()
                            .clone_into(&mut notice.text);
                    }
                    rig.render.send(RenderMsg::Menu(Box::new(page)));
                }
                None => rig.render.send(RenderMsg::Menu(Box::new(
                    crate::loading_text::skin_wait_page(),
                ))),
            }
            #[cfg(feature = "dev-tools")]
            crate::dev::specialization_began();
        };
        let begun = pending.begin(facts, wait);
        self.pending = Some(pending);
        match begun {
            crate::imports::Begun::Ready(skin, how) => {
                self.skin_ended(skin, &how).map(SkinStep::Ready)
            }
            crate::imports::Begun::Running(job) => Ok(SkinStep::Running(job)),
        }
    }

    /// The pending play skin's specialization `job`, waited for until
    /// `until`: the quit key cancels the job and waits for its thread's
    /// end; the window's close and a failure of the input or audio thread
    /// end it within the stop bound. The adjustments and the dev presses
    /// are served meanwhile. `None` at `until` with the job still running.
    ///
    /// # Errors
    ///
    /// The attempt's preparation instead: cancelled once a signal came,
    /// or refused with the reason the skin cannot be built.
    fn poll_skin(
        &mut self,
        job: &mut crate::imports::Specializing,
        until: Instant,
    ) -> Option<Result<AttemptSkin, Preparation>> {
        let mut adjust = self.adjust.take();
        let rig = &*self;
        let quit = || {
            #[cfg(feature = "dev-tools")]
            crate::dev::press_due(rig.controls, crate::dev::LoadPhase::Preparing);
            serve_adjustments(adjust.as_mut(), &rig.input, rig.controls.tuning());
            rig.controls.leaving()
        };
        // A script's step on the specialization (dev-tools) is asked at
        // every look of the wait, an attempt's quit included.
        let end_run = || rig.run_ends();
        let ran = job.wait_until(Some(until), quit, end_run);
        self.adjust = adjust;
        let ran = ran?;
        let mut pending = self.pending.take()?;
        let skin = pending.finish(job, ran);
        self.pending = Some(pending);
        Some(self.skin_ended(skin, &crate::imports::Specialized::Ran))
    }

    /// The attempt at `loaded` prepared as the flow prepares it: begun,
    /// polled to its end, and, ready, its final readiness taken (tests).
    ///
    /// # Errors
    ///
    /// The screen's failure, or [`ScreenError::Lost`] for a `Stale`
    /// readiness.
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn prepare_attempt(&mut self, loaded: &Loaded) -> Result<Preparation, ScreenError> {
        use game::Rig as _;
        let mut handle = self.begin_preparation(loaded, loaded.prep_key());
        let ended = loop {
            let until = Instant::now() + std::time::Duration::from_millis(10);
            if let game::PrepPoll::Done(ended) = self.poll_preparation(&mut handle, until)? {
                break ended;
            }
        };
        if ended != Preparation::Ready {
            return Ok(ended);
        }
        match self.ready_attempt(1, &handle)? {
            game::Readiness::Ready => Ok(Preparation::Ready),
            game::Readiness::Stale(stale) => Err(stale.lost),
        }
    }

    /// The attempt's play skin once its specialization ended `skin`, as it
    /// came `how`, reported.
    ///
    /// # Errors
    ///
    /// The attempt's preparation instead: cancelled once a signal came,
    /// or refused with the reason the skin cannot be built.
    fn skin_ended(
        &self,
        skin: crate::imports::ForAttempt,
        how: &crate::imports::Specialized,
    ) -> Result<AttemptSkin, Preparation> {
        #[cfg(feature = "dev-tools")]
        println!("dev: specialization wait left");
        skin.report(how);
        // The job's thread has ended, and its VMs with it, before the
        // render thread prepares the attempt (dev-tools witness).
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: specialization ended: live {} joined {} of {}",
            skin_beatoraja::vm_hooks::live(),
            crate::imports::jobs_joined(),
            crate::imports::jobs_started()
        );
        match skin {
            crate::imports::ForAttempt::Ready(bundle) => Ok(AttemptSkin::Specialized(bundle)),
            crate::imports::ForAttempt::Cancelled => Err(Preparation::Cancelled),
            crate::imports::ForAttempt::Refused(reason) => {
                Err(Preparation::Refused(game::PreparationRefusal {
                    location: None,
                    reason,
                }))
            }
        }
    }

    /// The Lua sandbox's counters at `when` (dev-tools): its VMs open, the
    /// jobs started, the passes and the instruction steps run, and the
    /// screens' runtime frames and forced collections, which all stay
    /// still from recording's open to the play's end.
    #[cfg(feature = "dev-tools")]
    fn print_lua(when: &str) {
        use skin_beatoraja::vm_hooks;
        println!(
            "dev: lua at {when}: live {} jobs {} passes {} steps {} frames {} collections {}",
            vm_hooks::live(),
            crate::imports::jobs_started(),
            vm_hooks::passes(),
            vm_hooks::steps(),
            crate::screens::runtime_frames(),
            crate::screens::collections()
        );
    }

    /// One drain of the adjustments' driver ([`serve_adjustments`]).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn serve_adjustments(&mut self) {
        serve_adjustments(self.adjust.as_mut(), &self.input, self.controls.tuning());
    }

    /// The adjustments as the play left them, on the console: the active
    /// hi-speed, the mode, the target green number and the covers.
    fn print_adjustments(&self) {
        let Some(driver) = &self.adjust else {
            return;
        };
        let s = driver.snapshot();
        let h = s
            .hispeed
            .map(|h| {
                // Hundredths, a half rounded up.
                let (n, d) = (u128::from(h.num()), u128::from(h.den()));
                (200 * n + d) / (2 * d)
            })
            .map_or_else(|| "-".to_owned(), |x| format!("{}.{:02}", x / 100, x % 100));
        let o = s.options;
        let c = o.covers();
        let cover = |c: game::Cover| {
            if c.on {
                c.amount.to_string()
            } else {
                "off".to_owned()
            }
        };
        println!(
            "adjust: hi-speed {h} {} target {} sudden {} lift {} hidden {} out-of-order {}",
            match o.hispeed().mode {
                game::SpeedMode::Floating => "floating",
                game::SpeedMode::Classic => "classic",
            },
            o.hispeed().green_target,
            cover(c.sudden),
            cover(c.lift),
            cover(c.hidden),
            driver.diagnostics().out_of_order,
        );
    }

    /// The window's scene-scoped state moves on: the settings keys' scene
    /// epoch ([`Controls::next_scene`]), so a held F3 to F8 repeats no
    /// action. Every scene change and attempt entry calls it, beside the
    /// adjustments' context change.
    pub(crate) fn next_window_scene(&self) {
        self.controls.next_scene();
    }

    /// Every scene change outside an attempt's entry (a play's result,
    /// selection, the practice page) goes through here: the window's scene
    /// epoch moves on, and the driver fences the input thread and changes
    /// the adjustments' context ([`game::AdjustDriver::scene_changed`]).
    /// An attempt's entry goes through [`enter_attempt`](Self::enter_attempt),
    /// which does both too. A failed fence is latched in the driver and
    /// ends the run ([`Rig::input_failure`]).
    pub(crate) fn enter_scene(&mut self) {
        self.next_window_scene();
        if let Some((driver, mut pump)) = self.adjustments() {
            let _ = driver.scene_changed(&mut pump);
        }
    }

    /// The coming attempt's entry, at a loading state's start: the
    /// window's scene epoch moves on and the adjustments enter the attempt
    /// ([`game::AdjustDriver::enter_attempt`]).
    pub(crate) fn enter_attempt(&mut self) {
        self.next_window_scene();
        if let Some((driver, mut pump)) = self.adjustments() {
            let _ = driver.enter_attempt(None, &mut pump);
        }
    }

    /// The adjustments' first failed wait on the input thread, latched by
    /// the driver.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn adjustments_failure(&self) -> Option<InputWaitError> {
        self.adjust.as_ref().and_then(game::AdjustDriver::failure)
    }

    /// The driver and its pump, for a wait on the input thread; `None` once
    /// a wait failed, so nothing more is asked of the input thread.
    pub(crate) fn adjustments(&mut self) -> Option<(&mut game::AdjustDriver, PlayerPump<'_>)> {
        if self.adjustments_failure().is_some() {
            return None;
        }
        let pump = PlayerPump {
            input: &self.input,
            tuning: self.controls.tuning(),
            look: None,
            since: None,
        };
        self.adjust.as_mut().map(|d| (d, pump))
    }

    /// [`adjustments`](Self::adjustments), the pump's waits looking at the
    /// watched decision screen ([`Controls::look`]).
    pub(crate) fn adjustments_watched(
        &mut self,
    ) -> Option<(&mut game::AdjustDriver, PlayerPump<'_>)> {
        let controls = self.controls;
        self.adjustments().map(|(driver, mut pump)| {
            pump.look = Some(controls);
            (driver, pump)
        })
    }

    /// The practice page's jobs end with the run, within the bound: what
    /// their settle found that the run ends with.
    pub(crate) fn page_at_end(&mut self) -> Result<(), game::coord::LoadFailure> {
        self.selection
            .as_mut()
            .and_then(|sel| {
                sel.page
                    .leave(Some(&mut sel.shelf), &mut || {
                        Some(crate::page_jobs::Terminal)
                    })
                    .failure()
            })
            .map_or(Ok(()), Err)
    }

    /// Writes changed settings, then ends the render and audio threads;
    /// how the render thread ended (when [`RenderEnd::Stuck`], the window
    /// must stay open until the process exits), and what the practice
    /// page's jobs left that the run ends with.
    pub fn shut_down(mut self) -> Shutdown {
        // The run ends: a result or selection shown is left, and its
        // captures whose frame was not taken fail (dev-tools).
        #[cfg(feature = "dev-tools")]
        {
            self.finish_result(crate::dev::scenario::Leaving::Ended);
            self.finish_selection(crate::dev::scenario::Leaving::Ended);
        }
        // A panic a Start was responsible for ends the run with it, after the
        // rest of the cleanup.
        let page = self.page_at_end();
        // A preparation still running settles its job before the render
        // thread and the skins go.
        self.end_prep();
        // The images' worker, stopped within its bound; its panic is never
        // the run's.
        self.chart_images.end();
        self.persist();
        let rendered = self.render.exit();
        self.audio.exit();
        Shutdown { rendered, page }
    }

    /// Writes the settings keys' values to `device.toml` and
    /// `player.toml`, each atomically and only if it changed. Called when a
    /// play ends and when the run ends, never during a play.
    fn persist(&mut self) {
        if let Some(driver) = &self.adjust {
            self.files.set_options(driver.options());
        }
        self.files
            .write(self.controls.tuning().get(), self.queue.periods());
    }

    /// Counts the ended play's underruns after its READY state, raises the
    /// next play's queue on any, and keeps the end screen's line.
    fn judge_queue(&mut self) {
        let steady = self
            .audio
            .control
            .counters()
            .steady_underruns(self.generation);
        self.queue.play_ended(steady);
        // What the next play will keep: never more than the output holds.
        let next = end_screen::next_queue(self.queue.periods(), self.queue_most);
        let line = QueueLine {
            played: self.queued_now,
            steady_underruns: steady,
            next,
        };
        println!(
            "audio: queued {} periods, {steady} underruns after READY{}",
            self.queued_now
                .map_or_else(|| "an unknown number of".to_owned(), |q| q.to_string()),
            if self.queued_now == Some(next) {
                String::new()
            } else {
                format!("; the next play queues {next}")
            }
        );
        self.queue_line = Some(line);
    }

    /// Acts on changed settings keys' values: the volume goes to the
    /// audio thread, and a shown end screen is drawn again.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn sync(&mut self) {
        let tuned = self.seen.take_new(self.controls.tuning());
        if tuned.is_some() {
            // Always within 0 to unity: loading and `Tuning` keep it there.
            let _ = self
                .audio
                .control
                .set_volume(self.controls.tuning().volume());
        }
        // The end screen's hi-speed line follows the adjustments too.
        let speed = self.speed_line();
        if tuned.is_some() || speed != self.shown_speed {
            self.shown_speed = speed;
            self.send_end(tuned.unwrap_or_else(|| self.controls.tuning().get()));
        }
    }

    /// The hi-speed the end screen shows: the adjustments' newest state,
    /// its green number at the attempt's reference tempo.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn speed_line(&self) -> end_screen::SpeedLine {
        let Some(driver) = &self.adjust else {
            return end_screen::SpeedLine::default();
        };
        let s = driver.snapshot();
        let h = s.options.hispeed();
        let reference = driver.tempo().and_then(|t| {
            let (k, d) = t.speed().ratio();
            let speed = game::Ratio::new(u64::from(k), u64::from(d))?;
            let bpm = t.references().ok()?.get(h.reference);
            bpm.times(speed).ok()
        });
        end_screen::SpeedLine::of(&s, reference)
    }

    /// Hands the end screen's page, with the settings lines of `t` and the
    /// view's graphs, to the render thread, while the end screen shows.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn send_end(&self, t: Tuning) {
        if let Some(base) = &self.end_page {
            self.render
                .show_end(end_screen::with_settings(base, t, self.speed_line()));
        }
    }

    /// Takes the ended play's figures from the render thread and writes
    /// them beside its saved record.
    /// Writes the figures frozen when the play was over beside its saved
    /// record.
    fn write_measure(&mut self, report: &PlayReport) {
        let Some(mut fig) = self.figures.take() else {
            eprintln!("measure: no figures for play {}", report.attempt);
            return;
        };
        fig.steps_dropped = std::mem::take(&mut self.steps_dropped);
        println!("{}", limits_line(&fig));
        let Some(Ok(saved)) = &report.saved else {
            return;
        };
        let path = measure_path(&saved.path);
        let ms = |d: Option<Duration>| d.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let text = to_toml(&fig, (ms(self.times.first), ms(self.times.retry)));
        match game::write_atomic(&path, text.as_bytes()) {
            Ok(()) => println!("measure: {}", path.display()),
            Err(e) => eprintln!("measure: {}: {e}", path.display()),
        }
        #[cfg(feature = "dev-tools")]
        if let Some(plays) = &self.measured_plays
            && let Ok(mut plays) = plays.lock()
        {
            plays.push((saved.path.clone(), fig));
        }
        #[cfg(not(feature = "dev-tools"))]
        let _: Figures = fig;
    }

    /// How the ended round's audio was restored, if it was a round.
    fn print_restored(&mut self) {
        let Some(at) = self.restore_at.take() else {
            return;
        };
        let ms = |t: TimeUs| t.0 / 1_000;
        match self.audio.control.counters().restored(self.generation) {
            Some(Restored::First) => println!("audio: restored at {} ms (first)", ms(at)),
            Some(Restored::Fallback) => println!(
                "audio: restored at {} ms (fallback)",
                ms(TimeUs(at.0.saturating_add(game::RESTORE_RETRY.0)))
            ),
            Some(Restored::Refused(why)) => {
                println!("audio: not restored ({why:?}); the background music was silent");
            }
            None => println!("audio: the restore was not taken"),
        }
    }

    fn print(&self, report: &PlayReport) {
        let result = report.result.as_ref();
        let counts = result
            .and_then(|r| r.evaluation.as_ref().ok())
            .map(|e| e.tally.counts);
        match counts {
            Some(c) => println!(
                "play {}: {:?}; perfect {} great {} good {} bad {} poor {}; re-evaluation matches live: {}",
                report.attempt,
                report.end,
                c.perfect,
                c.great,
                c.good,
                c.bad,
                c.poor,
                result.is_some_and(|r| r.matches_live)
            ),
            None => println!(
                "play {}: {:?}; nothing recorded",
                report.attempt, report.end
            ),
        }
        if let Some(c) = &report.closing {
            println!("{}", closing_line(c));
        }
        match &report.saved {
            Some(Ok(r)) => println!("saved: {}", r.path.display()),
            Some(Err(e)) => eprintln!("not saved: {}", Chain(e)),
            None if self.autoplay => println!("autoplay: not saved"),
            None => {}
        }
        let ms = |d: Duration| format!("{} ms", d.as_millis());
        println!(
            "load: first {}, retry {}",
            self.times.first.map_or_else(|| "-".to_owned(), ms),
            self.times.retry.map_or_else(|| "-".to_owned(), ms)
        );
    }
}

/// The report's line for a play's Closing (design.md "Play Lifecycle",
/// Closing): its start on the play clock, how long it lasted and how it
/// ended.
pub(crate) fn closing_line(c: &game::ClosingReport) -> String {
    let end = match c.end {
        game::ClosingEnd::RanOut => "ran out".to_owned(),
        game::ClosingEnd::Retry => "retried".to_owned(),
        game::ClosingEnd::Cut(cause) => format!("cut by {cause:?}"),
    };
    format!(
        "closing: from {} us, {} us, {end}",
        c.started_at.0,
        c.ended_at.0.saturating_sub(c.started_at.0)
    )
}

/// The render thread's commit of the attempt prepared: while a loading
/// lifetime runs (`loading`), it is ready first, so its progress reaches
/// 100 before the commit and the end of loading. `declared` becomes the
/// commit's receipt: the declared Closing duration of the declarative
/// installation the committed attempt draws with, the one it staged or
/// the one already in use; an error clears it.
pub(crate) fn commit_ready(
    render: &RenderLink,
    loading: bool,
    declared: &mut Declared,
) -> Result<(), ScreenError> {
    *declared = Declared::default();
    if loading {
        render.loading().report(LoadInput::Ready, None);
    }
    *declared = render.commit_attempt()?;
    Ok(())
}

/// While a loading lifetime runs (`loading`), replaces its gauge
/// descriptor with gauge `gauge` of `preset`, the preset the play
/// refreshed.
pub(crate) fn refresh_loading_gauge(
    render: &RenderLink,
    loading: bool,
    gauge: Option<skin_model::LoadingGauge>,
) {
    if loading {
        render.loading().report(LoadInput::Gauge(gauge), None);
    }
}

/// The loading gauge descriptor of the gauge the settings show,
/// `gauge`, in `preset`, before an attempt resolves it: resolved as the
/// attempt will be ([`game::resolve_displayed`], through the player's
/// policy), so a gauge the preset does not have shows its first from the
/// lifetime's first frame.
pub(crate) fn loading_gauge(
    preset: &rules::RulePreset,
    gauge: usize,
) -> Option<skin_model::LoadingGauge> {
    let policy = game::DefaultPolicy {
        displayed: gauge,
        on_failure: game::FailureAction::default(),
    };
    shown_gauge(preset, game::resolve_displayed(&policy, preset).0)
}

/// A round's restore at chart time `at`, its lead-in's start, planned on
/// the line the audio thread last published for a play whose chart time 0
/// lies at `origin`, the opening's: `at` is then `RESTORE_AHEAD` in the
/// output's future (design.md "Mid-Chart Start and Section Repeat").
fn plan_restore(
    restorer: &game::Restorer,
    counters: &game::AudioCounters,
    origin: TimeUs,
    at: TimeUs,
) -> Result<Box<game::Restore>, RestoreError> {
    restorer.plan(counters.frame_line(origin), at)
}

pub(crate) fn since(from: TimeUs, to: TimeUs) -> Duration {
    Duration::from_micros(u64::try_from(to.0 - from.0).unwrap_or(0))
}

/// The command of a scenario's `until`, once it is due at `now` for the
/// play whose chart time 0 lies at `origin`: taken, so it is sent once.
#[cfg(feature = "dev-tools")]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn until_fired<C: Copy>(
    until: &mut Option<(TimeUs, C)>,
    origin: Option<TimeUs>,
    now: TimeUs,
) -> Option<C> {
    let (Some(origin), Some((at, command))) = (origin, *until) else {
        return None;
    };
    if !until_due(origin, at, now) {
        return None;
    }
    *until = None;
    Some(command)
}

/// Whether chart time `at` has come at `now` for a play whose chart time 0
/// lies at `origin`, both on the clock.
#[cfg(feature = "dev-tools")]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) const fn until_due(origin: TimeUs, at: TimeUs, now: TimeUs) -> bool {
    now.0 - origin.0 >= at.0
}

/// How long a loading wait parks until `until` from `now` on the clock:
/// the idle wake at most, never past `until` (design.md "Play Lifecycle",
/// the loading state's wait); the idle wake without one.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn park_for(until: Option<TimeUs>, now: TimeUs) -> Duration {
    until.map_or(IDLE_PARK, |u| {
        IDLE_PARK.min(Duration::from_micros(
            u.0.saturating_sub(now.0).max(0).unsigned_abs(),
        ))
    })
}

impl PlayerRig<'_> {
    /// Parks for `park` at most (the input thread unparks it earlier),
    /// then serves what a step's wait serves ([`game::Rig::wait`]).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait_parked(&mut self, park: Duration) {
        thread::park_timeout(park);
        self.serve_adjustments();
        // A scenario's `until`: the command, sent once, as its key would.
        #[cfg(feature = "dev-tools")]
        if let Some(sc) = &mut self.scenario
            && let Some(command) = until_fired(&mut sc.until, self.origin, self.clock.now())
        {
            let at = TimeUs(self.clock.now().0 - self.origin.map_or(0, |o| o.0));
            sc.sent = Some((command, at));
            self.controls.command(command.command());
        }
        #[cfg(feature = "dev-tools")]
        if let Some(origin) = self.origin
            && !self.adjust_in_play.is_empty()
            && until_due(origin, self.adjust_at, self.clock.now())
        {
            for a in self.adjust_in_play.drain(..) {
                self.controls.tuning().adjust(a);
            }
        }
        // An autoplay session's speed keys come from the window's queue.
        #[cfg(feature = "dev-tools")]
        if let Some(origin) = self.origin
            && self.autoplay
            && !self.speed_in_play.is_empty()
            && until_due(origin, self.adjust_at, self.clock.now())
        {
            for code in self.speed_in_play.drain(..) {
                let key = u8::from(code != game::SPEED_KEYS[0]);
                self.controls
                    .window_key(key, input::WindowKeyEvent::Press, false);
                self.controls
                    .window_key(key, input::WindowKeyEvent::Release, false);
            }
        }
        self.sync();
    }

    /// A loading wait's park until `until` ([`park_for`]).
    pub(crate) fn loading_park(&mut self, until: Option<TimeUs>) {
        let park = park_for(until, self.clock.now());
        self.wait_parked(park);
    }
}

impl Rig for PlayerRig<'_> {
    type Reader = SourceReader;
    type ScreenError = ScreenError;
    type ResultPrep = ();

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn now(&self) -> TimeUs {
        let now = self.clock.now();
        #[cfg(test)]
        self.opening_gates.read_clock(now);
        now
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait(&mut self) {
        self.wait_parked(if self.in_play { PARK } else { IDLE_PARK });
    }

    fn idle_wait(&mut self, deadline: TimeUs) {
        let left = deadline.0 - self.clock.now().0;
        if left > 0 {
            thread::park_timeout(Duration::from_micros(left.unsigned_abs()));
        }
        // A switch's layout lands when its job is done: the job wakes this
        // thread once its outcome is in, and while its thread still ends
        // the look is repeated every LAYOUT_RELOOK, within the deadline. A
        // close leaves the looks at once, and the wait within the stop
        // bound.
        while !self.land_result() && self.result_skin.landing() && !self.controls.quitting() {
            // Tests: a look found the outcome received, not yet taken.
            #[cfg(test)]
            self.relooked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let left = deadline.0 - self.clock.now().0;
            if left <= 0 {
                break;
            }
            thread::park_timeout(Duration::from_micros(left.unsigned_abs()).min(LAYOUT_RELOOK));
        }
        if self.controls.quitting() && self.result_skin.has_job() {
            self.result_skin.settle(|| true);
        }
        self.serve_adjustments();
        self.sync();
        // The result's own path, after the adjustments: an offset changed
        // by them reaches the strip's guidance.
        self.present_overlay();
        // The result's scripted steps, at each wake: one waiting on a
        // capture is polled (dev-tools).
        #[cfg(feature = "dev-tools")]
        if self.end_page.is_some() {
            self.poll_result_script();
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take(&mut self) -> Option<Command> {
        let c = self.controls.take();
        if c == Some(Command::Retry) {
            self.pending_retry = self.controls.retry_at();
        }
        c
    }

    /// The current attempt's commit receipt: Closing's duration.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn declared_close(&self) -> Option<TimeUs> {
        self.declared.close
    }

    /// The current attempt's commit receipt: the play's ending.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn declared_ending(&self) -> game::PlayEnding {
        self.declared.ending
    }

    /// As [`take`](Self::take), with the quit key taken without dropping a
    /// retry pressed with it ([`Controls::take_closing`]).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take_closing(&mut self) -> Option<Command> {
        let c = self.controls.take_closing();
        if c == Some(Command::Retry) {
            self.pending_retry = self.controls.retry_at();
        }
        c
    }

    /// The retry and quit keys first (a suspend means nothing on a result),
    /// then, while a result shows, its own keys and controller events. A
    /// retry taken here is only offered ([`ResultRetries`]): the result's
    /// flow drops some (gated, queued behind the command it accepts, or
    /// while its outro plays), and only the one it acts on becomes the
    /// next play's. A controller's retry carries no receipt of its own: it
    /// is received when taken here.
    fn take_menu(&mut self) -> Option<MenuCommand> {
        loop {
            match self.controls.take() {
                Some(Command::Retry) => {
                    if let Some(at) = self.controls.retry_at() {
                        self.result_retries.offer(at);
                    }
                    return Some(MenuCommand::Retry);
                }
                Some(Command::Quit) => return Some(MenuCommand::Back),
                Some(Command::Suspend) => {}
                None => break,
            }
        }
        let c = self.result_input.take();
        if c == Some(MenuCommand::Retry) {
            self.result_retries.offer(self.clock.now());
        }
        c
    }

    /// A withdrawn preparation cancels the result skin's layout job and
    /// waits for its thread (design D11, "Cancellation"); the run's end
    /// bounds the wait. Withdrawn by a retry, that retry is the next
    /// play's.
    fn withdraw_result(&mut self, by: Option<MenuCommand>) {
        // The first command taken withdraws the preparation, and those
        // queued behind it are dropped: a withdrawing retry is the first
        // offered, and a retry queued behind a back is none.
        let first = self.result_retries.first.take();
        if by == Some(MenuCommand::Retry)
            && let Some(at) = first
        {
            self.pending_retry = Some(at);
        }
        self.settle_result();
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settled: withdrawn");
    }

    /// A leaving command the Result state acts on cancels a switch's
    /// layout job and waits for its thread before it is served (D11); the
    /// run's end bounds the wait.
    fn accept_leaving(&mut self, cmd: MenuCommand) {
        // The accepted command is the one just taken.
        if cmd == MenuCommand::Retry
            && let Some(at) = self.result_retries.last
        {
            self.pending_retry = Some(at);
        }
        self.settle_result();
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settled: accepted");
    }

    /// The presented result skin's timing (the S10-09i design's D6):
    /// `None` on any built-in result, so a command pending is served at
    /// once.
    fn result_timing(&self) -> Option<game::ResultTiming> {
        self.presenter.timing(&self.render).map(flow_timing)
    }

    /// The barrier is set at `at` (D4), for every accepted command: the
    /// view presented then stays, and timer 2 starts, before the command
    /// settles the result skin. The pinned view's timing, read with it; a
    /// pressed command that view's input time still gates is refused under
    /// the same lock, nothing set.
    fn begin_outro(&mut self, at: TimeUs, gate: Option<TimeUs>) -> game::Outro {
        let Some((pinned, timing)) = self.presenter.pin(&mut self.render, at, gate) else {
            return game::Outro::Gated;
        };
        self.result_outro = Some(at);
        #[cfg(feature = "dev-tools")]
        println!("dev: result outro began, view {pinned:?} pinned");
        #[cfg(not(feature = "dev-tools"))]
        let _ = pinned;
        game::Outro::Begun(timing.map(flow_timing))
    }

    /// The flow serves the leaving command: the result is left from here.
    fn serve_leaving(&mut self, _cmd: MenuCommand) {
        self.result_leaving = true;
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn quitting(&self) -> bool {
        #[cfg(feature = "dev-tools")]
        if let (Some(after), Some(shown)) = (self.quit_after_end, self.end_shown)
            && scripted_end_over(after, shown.elapsed(), &self.render)
        {
            return true;
        }
        self.controls.quitting()
    }

    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        // With selection, the scan has acknowledged its pause before
        // recording opens and READY starts; so has every screen runtime its
        // parking (a render thread that ended holds none).
        self.pause_scan();
        if !self.render.park_scripts() {
            eprintln!("render: the screens' runtimes were not acknowledged parked");
        }
        self.audio.control.ready()?;
        // Between plays, before recording opens: the change never lands
        // inside a play or its READY state.
        self.queued_now = self
            .audio
            .control
            .set_queued_periods(self.queue.periods())?;
        // A volume changed before the first play (during startup) or on
        // the end screen reaches the engine before the play starts.
        self.sync();
        Ok(())
    }

    /// The render thread answers once it draws no loading frame any more;
    /// a retry has no loading state to end.
    /// The render thread installs what the attempt's preparation staged:
    /// on every play, a retry and a practice round included.
    ///
    /// A loading lifetime is ready first: everything before the commit
    /// passed, so its progress reaches 100.
    fn commit_attempt(&mut self) -> Result<(), ScreenError> {
        let loading = self.loading_active();
        commit_ready(&self.render, loading, &mut self.declared)?;
        self.chart_images.committed();
        #[cfg(feature = "dev-tools")]
        {
            self.probe_ready = Some((std::time::Instant::now(), "none"));
        }
        self.upload_result();
        #[cfg(feature = "dev-tools")]
        crate::dev::print_result_work("commit");
        Ok(())
    }

    fn discard_attempt(&mut self) {
        let _ = self.drop_prep();
        self.render.discard_attempt();
    }

    /// A gated attempt pauses its loading lifetime instead, so a hold at
    /// the opening returns to the same loading state ([`defer`](Rig::defer));
    /// the opening ends it.
    fn end_loading(&mut self) -> Result<(), ScreenError> {
        #[cfg(test)]
        self.opening_gates.ending();
        let loading = self
            .selection
            .as_mut()
            .and_then(|s| s.loading.take())
            .is_some();
        // A withdrawn upload may still hold the render thread: the loading
        // state ends without its answer, and recording does not open.
        if std::mem::take(&mut self.result_withdrawn) {
            self.render.clear_loading();
            return Ok(());
        }
        match &mut self.gate {
            Some(gate) => gate.end_loading(&self.render, loading),
            None if loading => self.render.end_loading(),
            None => Ok(()),
        }
    }

    fn prepare_screen(&mut self) -> Result<(), ScreenError> {
        self.before_rebuild();
        // The screen's rebuild after a loss goes through its one owner: one
        // under way (a decision screen's, left) is taken over and waited
        // for, a failure saved there is reported here, and a lost screen
        // with neither is built again there. Nothing of it: a preparation.
        loop {
            match self.render.serve_recovery(
                Some(std::time::Instant::now() + std::time::Duration::from_millis(10)),
                true,
            ) {
                crate::render_side::Recovered::Nothing => return self.render.prepare(),
                crate::render_side::Recovered::Running => {}
                crate::render_side::Recovered::Answered(answered) => return answered,
            }
        }
    }

    /// The attempt's preparation begun (design.md "Play Lifecycle",
    /// preparing each attempt): a pending play skin's specialization
    /// started, or, without one, the result skin's image stage, or the
    /// render thread asked; it returns at once, and the polls drive the
    /// rest ([`poll_preparation`](Rig::poll_preparation)). A preparation
    /// begun before is dropped first ([`drop_prep`](Self::drop_prep)), its
    /// job settled and its unanswered request discarded. Once that settle
    /// is over, the run's end is looked at again: when it came, however
    /// the settle ended, nothing new begins and the handle is cancelled.
    fn begin_preparation(&mut self, loaded: &Loaded, key: game::PrepKey) -> game::PrepHandle {
        if self.drop_prep().is_some() && self.run_ends() {
            self.preps += 1;
            self.prep = Some(AttemptPrep {
                ticket: self.preps,
                attempt: None,
                loading_gauge: None,
                displayed: loaded.displayed().0,
                chart: crate::result_skin::attempt_chart(loaded),
                class: crate::result_skin::attempt_class(loaded),
                labels: None,
                images: (std::path::PathBuf::new(), [String::new(), String::new()]),
                scratch_notes: Vec::new(),
                carry_key: None,
                specialized: false,
                staging: Instant::now(),
                stage: PrepStage::Ended(Preparation::Cancelled),
            });
            return game::PrepHandle::new(key, self.preps);
        }
        self.result_skin.next_attempt();
        self.presenter.next_attempt();
        self.preps += 1;
        let attempt = loaded.attempt();
        let labels = crate::result_present::labels_of(&attempt.labels);
        let images = (
            crate::chart_images::folder(loaded.path()),
            [
                attempt.labels.stagefile.clone(),
                attempt.labels.backbmp.clone(),
            ],
        );
        let mut prep = AttemptPrep {
            ticket: self.preps,
            attempt: Some(attempt),
            loading_gauge: shown_gauge(loaded.preset(), loaded.displayed().0),
            displayed: loaded.displayed().0,
            chart: crate::result_skin::attempt_chart(loaded),
            class: crate::result_skin::attempt_class(loaded),
            labels: Some(labels),
            images,
            // Autoplay's turntable presses its scratch notes.
            scratch_notes: if self.autoplay {
                game::scratch_notes(loaded.chart(), loaded.chart().meta.ln_mode)
            } else {
                Vec::new()
            },
            specialized: false,
            carry_key: self.play_states.current().map(|s| crate::carry::CarryKey {
                play_state: s.number,
                chart: key.chart(),
            }),
            staging: Instant::now(),
            stage: PrepStage::Answered(None),
        };
        prep.stage = match self.begin_skin(loaded) {
            Ok(SkinStep::Ready(skin)) => self.after_skin(&mut prep, skin),
            Ok(SkinStep::Running(job)) => PrepStage::Skin(job),
            Err(ended) => PrepStage::Ended(ended),
        };
        let handle = game::PrepHandle::new(key, prep.ticket);
        self.prep = Some(prep);
        handle
    }

    /// Drives the preparation until `deadline`: the specialization's job,
    /// then the result skin's image stage, then the render thread's
    /// answer, each waited for in slices that serve the player. Ready, the
    /// handle holds the screen's structural inputs it was staged with.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn poll_preparation(
        &mut self,
        handle: &mut game::PrepHandle,
        deadline: Instant,
    ) -> Result<game::PrepPoll<Preparation>, ScreenError> {
        let mut prep = match self.prep.take() {
            Some(p) if p.ticket == handle.ticket() => p,
            // Another handle's: it stays, for its own polls or its drop.
            other => {
                self.prep = other;
                return Err(ScreenError::Lost);
            }
        };
        // The skin in use's images, published as soon as they are read, so
        // the loading frames draw them while the attempt is prepared.
        if self.pending.is_none() && self.selection.is_some() {
            let coord = self.selection.as_ref().map(|s| (&s.coord, s.spawner));
            let publisher = self.render.chart_publisher();
            self.chart_images.step(coord, &|| false, Some(&publisher));
            let _ = self.chart_images.publish(&self.render, None);
        }
        let polled = loop {
            match std::mem::replace(&mut prep.stage, PrepStage::Answered(None)) {
                PrepStage::Skin(mut job) => match self.poll_skin(&mut job, deadline) {
                    None => {
                        prep.stage = PrepStage::Skin(job);
                        break Ok(game::PrepPoll::Pending);
                    }
                    Some(Ok(skin)) => prep.stage = self.after_skin(&mut prep, skin),
                    Some(Err(ended)) => prep.stage = PrepStage::Ended(ended),
                },
                PrepStage::Image(skin, mut job) => match self.poll_image(&mut job, deadline) {
                    None => {
                        prep.stage = PrepStage::Image(skin, job);
                        break Ok(game::PrepPoll::Pending);
                    }
                    Some(staged) => prep.stage = self.after_image(&mut prep, skin, staged),
                },
                PrepStage::Screen(mut pending) => {
                    match self.poll_screen(&prep, &mut pending, deadline) {
                        None => {
                            prep.stage = PrepStage::Screen(pending);
                            break Ok(game::PrepPoll::Pending);
                        }
                        Some(Ok(answered)) => {
                            if answered.preparation != Preparation::Ready {
                                break Ok(game::PrepPoll::Done(answered.preparation));
                            }
                            handle.staged_with(answered.screen);
                            let request = pending.request();
                            let target = prep.specialized.then_some(request);
                            prep.stage = PrepStage::Images(request, target);
                        }
                        Some(Err(e)) => break Err(e),
                    }
                }
                PrepStage::Images(request, target) => match self.poll_images(target, deadline) {
                    None => {
                        prep.stage = PrepStage::Images(request, target);
                        break Ok(game::PrepPoll::Pending);
                    }
                    Some(ImagesPoll::Ready) => {
                        prep.stage = PrepStage::Answered(Some(request));
                        break Ok(game::PrepPoll::Done(Preparation::Ready));
                    }
                    Some(ImagesPoll::Stopped) => {
                        // Stopped: what the request staged is dropped, as a
                        // stopped Screen stage's is.
                        self.render.discard_attempt();
                        prep.stage = PrepStage::Ended(Preparation::Cancelled);
                        break Ok(game::PrepPoll::Done(Preparation::Cancelled));
                    }
                    Some(ImagesPoll::Expired) => {
                        // The render thread holds the publication: what the
                        // request staged is dropped and the attempt refused,
                        // never readied without its images settled.
                        self.render.discard_attempt();
                        let refused = Preparation::Refused(game::PreparationRefusal {
                            location: None,
                            reason: "the chart's images were not published in time".to_owned(),
                        });
                        prep.stage = PrepStage::Ended(refused.clone());
                        break Ok(game::PrepPoll::Done(refused));
                    }
                },
                // Ended or answered: it stays so, for its final readiness
                // and its commit, however often it is polled again.
                PrepStage::Ended(ended) => {
                    prep.stage = PrepStage::Ended(ended.clone());
                    break Ok(game::PrepPoll::Done(ended));
                }
                PrepStage::Answered(request) => {
                    prep.stage = PrepStage::Answered(request);
                    break Err(ScreenError::Lost);
                }
            }
        };
        self.prep = Some(prep);
        polled
    }

    /// The render thread stamps the adjustment epoch on what the request
    /// staged once it still fits the screen's device, target size and play
    /// skin selection, and answers `Stale` with them otherwise; it prepares
    /// nothing. The attempt's number is the flow's: nothing staged on the
    /// render thread holds it. Nothing staged at all answers `Stale` with
    /// the inputs the handle was staged with, which ends the run.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn ready_attempt(
        &mut self,
        number: u32,
        handle: &game::PrepHandle,
    ) -> Result<game::Readiness<ScreenError>, ScreenError> {
        let _ = number;
        let lost = |screen| {
            game::Readiness::Stale(game::Stale {
                screen,
                lost: ScreenError::Lost,
            })
        };
        let Some(request) = self
            .prep
            .as_ref()
            .filter(|p| p.ticket == handle.ticket())
            .and_then(|p| match p.stage {
                PrepStage::Answered(request) => request,
                _ => None,
            })
        else {
            return Ok(lost(handle.key().screen().unwrap_or_default()));
        };
        let epoch = self
            .adjust
            .as_mut()
            .and_then(game::AdjustDriver::prepared)
            .unwrap_or(0);
        Ok(match self.render.ready_attempt(request, epoch)? {
            crate::render_side::ReadyAnswer::Ready => game::Readiness::Ready,
            crate::render_side::ReadyAnswer::Stale(now) => lost(now),
        })
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        // G2's sample (dev-tools): `commit_ready`'s return to here.
        #[cfg(feature = "dev-tools")]
        crate::dev::print_ready_to_open(self.probe_ready.take());
        // The window gains its focus when shown: an opening before it
        // would latch the play's focus loss at once.
        if let Some(window) = &self.window {
            window.wait_to_open();
        }
        self.in_play = true;
        // A round's opening is not the full play's READY state: READY
        // and a scenario's `until` run from it.
        self.lead_in = lead_in;
        let opened = self.input.open(Request::Open {
            lead_in,
            logic: thread::current(),
            recorded: self.recorded.clone(),
            gate: None,
            focus: None,
        });
        #[cfg(feature = "dev-tools")]
        if opened.is_ok() {
            Self::print_lua("open");
        }
        opened
    }

    fn gated(&self) -> bool {
        self.gate.is_some()
    }

    fn begin_presentation(
        &mut self,
        loaded: &Loaded,
    ) -> Result<Option<game::PreparationRefusal>, InputWaitError> {
        // The loading script runs from here to the first opening.
        #[cfg(feature = "dev-tools")]
        if std::mem::take(&mut self.loading_script) {
            self.input.loading_script(self.clock.now());
        }
        if let Some(e) = self.adjustments_failure() {
            return Err(e);
        }
        // An attempt entered here (nothing attached at a loading state)
        // is a scene change for the window too.
        if self.adjust.as_ref().is_some_and(|d| !d.has_pending()) {
            self.next_window_scene();
        }
        match self.adjustments() {
            Some((driver, mut pump)) => driver.begin_presentation(loaded, &mut pump),
            None => Ok(None),
        }
    }

    fn end_presentation(&mut self) {
        #[cfg(feature = "dev-tools")]
        self.input.end_loading_script();
        if let Some(driver) = &mut self.adjust {
            driver.invalidate();
        }
    }

    fn open_candidate(
        &mut self,
        lead_in: TimeUs,
        generation: u32,
    ) -> Result<Option<Opening<SourceReader>>, InputWaitError> {
        // G2's sample (dev-tools), as at [`open`](Rig::open).
        #[cfg(feature = "dev-tools")]
        crate::dev::print_ready_to_open(self.probe_ready.take());
        if let Some(window) = &self.window {
            window.wait_to_open();
        }
        // The previous opening's marker is taken first: the tap owes one
        // at most.
        if let Some((driver, mut pump)) = self.adjustments() {
            driver.before_opening(&mut pump)?;
        }
        self.in_play = true;
        self.lead_in = lead_in;
        let opened = self.input.open_candidate(Request::Open {
            lead_in,
            logic: thread::current(),
            recorded: self.recorded.clone(),
            gate: Some(generation),
            focus: self.accepted_focus,
        })?;
        #[cfg(test)]
        if opened.is_none() {
            self.opening_gates.deferred.pass();
        }
        if opened.is_some() {
            // Recording opened: acceptance with it.
            self.accepted_focus = None;
        }
        #[cfg(feature = "dev-tools")]
        if opened.is_some() {
            Self::print_lua("open");
        }
        if let (Some(opening), Some(gate)) = (&opened, &mut self.gate) {
            gate.opened(generation, opening);
            gate.end_paused_loading(&self.render);
        }
        if opened.is_none()
            && let Some(driver) = &mut self.adjust
        {
            driver.no_opening();
        }
        Ok(opened)
    }

    /// A paused loading lifetime is drawn again, with its clock.
    fn defer(&mut self) {
        if let Some(gate) = &self.gate {
            gate.defer(&self.render);
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn holding(&self) -> bool {
        self.gate
            .as_ref()
            .is_some_and(crate::start_gate::PlayerGate::holding)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn candidate(&self) -> input::GateState {
        self.gate.as_ref().map_or(
            input::GateState {
                generation: 0,
                phase: input::GatePhase::Idle,
            },
            crate::start_gate::PlayerGate::state,
        )
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn start_tuning(&self, generation: u32) -> Option<replay::StartTuning> {
        self.adjust.as_ref().and_then(|a| a.opened(generation))
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn settle_candidate(&mut self, generation: u32) -> bool {
        self.gate.as_ref().is_some_and(|g| g.settle(generation))
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn cancel_candidate(&mut self, generation: u32) {
        self.audio.control.cancel();
        if let Some(gate) = &mut self.gate {
            crate::start_gate::wait_dropped(&self.audio.control, gate, generation);
        }
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn rearm_candidate(
        &mut self,
        readers: &mut Vec<SourceReader>,
        lead_in: TimeUs,
        generation: u32,
        history: &mut game::HistoryRecorder,
        start: &mut eval::InputStart,
    ) -> Result<game::Rearm, InputWaitError> {
        // The cancellation's and the previous opening's markers are taken
        // first: the tap owes one of each at most.
        let cancelled = self.gate.as_ref().map(|g| g.state().generation);
        if let (Some(cancelled), Some((driver, mut pump))) = (cancelled, self.adjustments()) {
            driver.after_cancel(cancelled, &mut pump)?;
            driver.before_opening(&mut pump)?;
        }
        let Some(gate) = &mut self.gate else {
            return Ok(game::Rearm::Held);
        };
        let rearmed = gate.rearm(
            &mut self.input,
            readers,
            lead_in,
            generation,
            history,
            start,
        )?;
        if let game::Rearm::Opened { origin } = rearmed {
            self.origin = Some(origin);
            self.ready_state_at = Some(TimeUs(origin.0 - lead_in.0));
        }
        if rearmed == game::Rearm::Held
            && let Some(driver) = &mut self.adjust
        {
            driver.no_opening();
        }
        Ok(rearmed)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn restart_candidate(&mut self, origin: TimeUs, generation: u32) {
        let commit_at = self.restore_at.unwrap_or(TimeUs(0));
        let round = self.restore_at.is_some();
        if let Some(gate) = &mut self.gate {
            gate.restart(
                &mut self.audio.control,
                origin,
                generation,
                commit_at,
                round,
            );
        }
    }

    fn gate_exchange(&self) -> usize {
        self.gate
            .as_ref()
            .map_or(0, crate::start_gate::PlayerGate::exchange)
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn snapshot_capacity(&self) -> usize {
        self.gate
            .as_ref()
            .map_or(0, crate::start_gate::PlayerGate::snapshot)
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        // A new scene: a settings key held into the play repeats nothing.
        self.next_window_scene();
        // The play's setup ends here, before anything is handed over; the
        // page faults count from here too.
        let started = self.clock.now();
        let page_faults = process_page_faults();
        let ready_state_at = TimeUs(origin.0 - self.lead_in.0);
        self.ready_state_at = Some(ready_state_at);
        self.origin = Some(origin);
        println!("play started");
        #[cfg(feature = "dev-tools")]
        crate::dev::print_result_work("ready");
        // A chart's first play: a retry runs from its own receipt.
        if let Some(sel) = &self.selection
            && self.retry_from.is_none()
        {
            let ms = since(sel.chosen_at, started).as_millis();
            println!("load: READY started {ms} ms after the choice");
        }
        // Before the play is sent: the audio thread may mix its first wake
        // before this thread runs again.
        let audio = AudioBase::new(self.audio.control.counters());
        // The play's gain, fixed for the whole play: its chart's
        // normalization gain, measured by its load, else the headroom.
        let gain = self
            .selection
            .as_ref()
            .map_or(self.gain, |s| s.shelf.gain());
        // Audio was readied before recording opened, so this does not
        // wait; a worker gone since shows as the output's failure.
        // A gated play's transport waits for its candidate's commit.
        let restore = self.restore.take();
        let commit_at = self.restore_at.unwrap_or(TimeUs(0));
        let _ = match &mut self.gate {
            Some(gate) => {
                gate.started(&mut self.audio.control, restore.as_deref());
                let transport = gate.transport(commit_at);
                self.audio
                    .control
                    .play_gated(origin, sounds, gain, restore, transport)
            }
            None => self.audio.control.play_at(origin, sounds, gain, restore),
        };
        self.generation = self.audio.control.generation();
        self.steps_dropped = 0;
        let measure = PlayMeasure {
            generation: self.generation,
            origin,
            ready_state_at,
            started,
            page_faults,
            load_from: self.retry_from.unwrap_or(self.load_start),
            retry: self.retry_from.is_some(),
            audio: Some(audio),
            queued_periods: self.queued_now,
        };
        #[cfg(feature = "dev-tools")]
        self.scenario_started();
        // Every play follows a begun play state; the load's start stands
        // in for none.
        let state_origin = self.state_clock.origin().unwrap_or(self.load_start);
        self.interval = Some(self.render.scripts().open_interval());
        self.render.send(RenderMsg::Play {
            side: screen,
            gauge: self.attempt_gauge,
            measure: Box::new(measure),
            state_origin,
        });
    }

    fn restore_ready(&mut self, at: TimeUs) -> Result<(), RestoreError> {
        self.restore = None;
        self.restore_at = None;
        // A round is never played as a full play.
        self.restorer
            .as_ref()
            .ok_or(RestoreError::NotPrepared)?
            .check()?;
        self.restore_at = Some(at);
        Ok(())
    }

    fn restore_audio(&mut self, origin: TimeUs, at: TimeUs) -> Result<(), RestoreError> {
        let restorer = self.restorer.as_ref().ok_or(RestoreError::NotPrepared)?;
        // A rearmed round's restore is planned again in the gate's spare
        // box, allocating nothing.
        if let Some(gate) = self.gate.as_mut().filter(|g| g.is_started()) {
            return gate.replan(restorer, &mut self.audio.control, origin, at);
        }
        self.restore = Some(plan_restore(
            restorer,
            &self.audio.control.counters(),
            origin,
            at,
        )?);
        Ok(())
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn measured(&mut self, advance: &Advance, events: &[JudgeEvent], delivered: TimeUs) {
        let generation = self.generation;
        let finalization = advance.finalization_delay.map(|d| Step::Finalization(d.0));
        let judgments = judgment_samples(events, delivered).map(Step::Judgment);
        for kind in finalization.into_iter().chain(judgments) {
            if self.steps.push(StepSample { generation, kind }).is_err() {
                self.steps_dropped += 1;
            }
        }
    }

    fn play_over(&mut self) {
        // Measurement stops at the end of the play, before the save and the
        // re-evaluation, however long they take.
        self.figures = self.render.finish(self.clock.now());
        // The interval closes; a render thread that ended meanwhile left
        // its runtimes parked in the cell, destroyed here.
        if let Some(closed) = self
            .interval
            .take()
            .map(crate::screens::PlayInterval::close)
            && self.render.scripts().orphaned()
        {
            self.render
                .scripts()
                .retire(crate::screens::Quiet::Closed(closed));
        }
        #[cfg(feature = "dev-tools")]
        Self::print_lua("play end");
        #[cfg(feature = "dev-tools")]
        self.scenario_over();
        // The tail is over: nothing reads the input thread's publications
        // or its focus checks until the next play opens.
        self.in_play = false;
        self.input.idle();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn focus_lost(&self) -> bool {
        self.input.tripped()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn connected(&self) -> bool {
        self.input.connected()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    /// A gated candidate's cancellation the audio thread never
    /// acknowledged counts as its stall.
    fn audio_failure(&self) -> Option<OutputError> {
        self.failures.output()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn screen_failed(&self) -> bool {
        // The device lost, or the rebuild's holder holds one under way or
        // a failure no one reported: either is served the same way.
        self.render.screen_outstanding()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn cancel_audio(&mut self) {
        self.audio.control.cancel();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn seal_audio(&mut self) {
        self.audio.control.seal();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        self.audio.control.ring_out(fade_at, end_at);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn audio_done(&self) -> bool {
        self.audio.control.done()
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        self.input.close()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn hand_off(&mut self) -> Result<Closing, InputWaitError> {
        self.input.hand_off()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn abort_close(&mut self) -> Result<Closing, InputWaitError> {
        self.input.abort_close()
    }

    fn end_feedback(&mut self) -> Result<(), InputWaitError> {
        self.input.close().map(|_| ())
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn input_failure(&self) -> Option<InputWaitError> {
        self.failures.input()
    }

    fn before_play(&mut self, settings: &mut Settings) {
        // The input offset keys stop working here, so the play is evaluated
        // with the input offset it starts with (design.md "Offset Changes and
        // the Watermark"); the other values are recorded as they are at the
        // start.
        self.result_input.release();
        let t = self.controls.tuning().close_end_screen();
        t.apply_to(settings);
        // The adjustments' settings as every accepted change left them.
        if let Some(driver) = &self.adjust {
            settings.player.set_options(&driver.options());
        }
        self.input_offset_us = t.input_offset_us;
    }

    /// The state clock starts with the play state's turntable, at the
    /// same instant.
    fn play_state_began(&mut self) {
        let state = self.play_states.begin(self.clock.now());
        self.state_clock.began(state.origin);
        self.render.play_state(state);
    }

    fn turntable_paused(&mut self) {
        if let Some(s) = self.play_states.current() {
            self.render.turntable(s.number, true);
        }
    }

    fn turntable_resumed(&mut self) {
        if let Some(s) = self.play_states.current() {
            self.render.turntable(s.number, false);
        }
    }

    fn ended(&mut self, report: &PlayReport) {
        #[cfg(test)]
        self.ended_plays.push((report.end, report.record.clone()));
        // The play state's carry takes the final tally.
        self.render.settled(report.tally);
        // The result is another scene: held adjustment controls act no more
        // until pressed afresh.
        self.enter_scene();
        #[cfg(feature = "dev-tools")]
        crate::dev::print_result_work("ended");
        if let Some(c) = self.figures.as_ref().and_then(|f| f.ready_state) {
            println!(
                "ready: setup {} us, first frame {} after it started",
                c.setup_us,
                c.first_frame_us
                    .map_or_else(|| "N/A".to_owned(), |us| format!("{us} us"))
            );
        }
        // Load times run to the play's first frame: from the load's start
        // for the first play, from the retry's receipt for a retry.
        if let Some(frame) = self.render.first_frame() {
            match self.retry_from.take() {
                Some(from) => self.times.retry = Some(since(from, frame)),
                None if self.times.first.is_none() => {
                    self.times.first = Some(since(self.load_start, frame));
                }
                None => {}
            }
        }
        self.print(report);
        self.print_adjustments();
        #[cfg(feature = "dev-tools")]
        self.scenario_ended(report);
        self.print_restored();
        self.judge_queue();
        self.write_measure(report);
        self.persist();
    }

    /// The result's input is taken over as its preparation begins, after
    /// the save: what was queued during the play and the save is no
    /// result's, and what comes from here on is fresh. The built-in result
    /// needs nothing more, so the preparation is ready at once.
    fn begin_result(&mut self, _report: &PlayReport, _screen: &ResultScreen) {
        // G3's start (dev-tools): the save phase's drain is over.
        #[cfg(feature = "dev-tools")]
        {
            game::probe::stamp(&game::probe::RESULT_BEGAN);
            game::probe::clear(&game::probe::STEP_ACCEPTED);
        }
        if self.result_input.enter() {
            self.viewed = None;
            self.evaluated = 0;
        }
        // A new result's identity (dev-tools).
        #[cfg(feature = "dev-tools")]
        self.result_began();
    }

    /// P: the own view's layout, asked for once its best is compared, with
    /// the view's final facts (design D11; the S10-09i design's Q1).
    fn result_compared(&mut self, _report: &PlayReport, screen: &ResultScreen) {
        if let Some(chart) = self.result_chart
            && !self.result_skin_suppressed()
        {
            // The own view's best as the request takes it (dev-tools).
            #[cfg(feature = "dev-tools")]
            {
                let best = match screen.status() {
                    game::ViewStatus::Ready(s) => match s.best {
                        None => "none",
                        Some(game::BestCompare::Unavailable) => "unavailable",
                        Some(game::BestCompare::Known(_)) => "known",
                    },
                    _ => "no view",
                };
                println!("dev: result layout requested: best {best}");
            }
            self.result_skin
                .request(crate::result_skin::view_facts(screen, chart));
            #[cfg(feature = "dev-tools")]
            if self.result_skin.has_job() {
                crate::dev::stage_began(crate::dev::SpecializingStage::ResultLayout);
            }
        }
    }

    fn poll_result(&mut self, _prep: &mut (), deadline: TimeUs) -> game::Polled {
        loop {
            // A script's step on the layout (dev-tools), at every look.
            #[cfg(feature = "dev-tools")]
            crate::dev::specialization_signal(self.controls, || self.input.fail_for_script());
            if self.land_result() {
                return game::Polled::Ready;
            }
            let left = deadline.0 - self.clock.now().0;
            if left <= 0 {
                return game::Polled::Pending;
            }
            let slice =
                Duration::from_micros(left.unsigned_abs()).min(crate::render_side::SELECT_SLICE);
            thread::park_timeout(slice);
        }
    }

    fn show_end(&mut self, report: &PlayReport, screen: &ResultScreen) {
        self.print_view(screen);
        // A switch: its layout is requested; the current generation stays
        // until it lands (design D11). The attempt's fallback requests none.
        // Once a leaving command is accepted the view is pinned, and the
        // result skin was settled: no layout is asked for (D4).
        if let Some(chart) = self.result_chart
            && self.result_outro.is_none()
            && !self.result_skin_suppressed()
        {
            self.result_skin
                .request(crate::result_skin::view_facts(screen, chart));
        }
        let tuning = self.controls.tuning();
        tuning.open_end_screen();
        let queue = self.queue_line.unwrap_or(QueueLine {
            played: self.queued_now,
            steady_underruns: 0,
            next: end_screen::next_queue(self.queue.periods(), self.queue_most),
        });
        self.end_page = Some(EndPage {
            lines: end_screen::lines(
                report,
                screen,
                self.input_offset_us,
                self.times,
                queue,
                &self.hints,
                self.autoplay,
            ),
            graphs: end_screen::view_graphs(screen),
            // The result this page is of: what draws from it names it.
            #[cfg(feature = "dev-tools")]
            id: self.scenario.as_ref().and_then(|s| s.result_id),
        });
        // The skin's presentation goes first, so the built-in page does not
        // draw before it.
        self.present_view(report, screen);
        self.send_end(tuning.get());

        self.result_shown();
        #[cfg(feature = "dev-tools")]
        for a in self.adjust_on_end.drain(..) {
            tuning.adjust(a);
        }
        #[cfg(feature = "dev-tools")]
        if self.auto_retries > 0 {
            self.auto_retries -= 1;
            self.controls.command(Command::Retry);
        } else {
            self.end_shown = Some(std::time::Instant::now());
        }
    }
}

/// The retries a result's flow took (`take_menu`), by receipt, since the
/// play was prepared: the result's preparation is withdrawn by the first
/// command taken, and the result accepts the command it has just taken,
/// so the first and the latest are kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResultRetries {
    first: Option<TimeUs>,
    last: Option<TimeUs>,
}

impl ResultRetries {
    /// A retry received at `at` taken.
    fn offer(&mut self, at: TimeUs) {
        self.first.get_or_insert(at);
        self.last = Some(at);
    }
}

/// How long the chart images' wait parks between its looks.
const IMAGE_SLICE: std::time::Duration = std::time::Duration::from_millis(2);

/// Where the wait for the chart's images ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImagesPoll {
    /// The images settled and their publication for the attempt returned.
    Ready,
    /// A stop ([`preparation_stops`]).
    Stopped,
    /// The publication's answer did not come within its bound.
    Expired,
}

/// Whether a waiting attempt's preparation stops early: the player quit or
/// left, or the input or audio thread failed, which ends the run with its
/// cause, never as the render thread's silence.
pub(crate) const fn preparation_stops(
    quitting: bool,
    leaving: bool,
    input: Option<InputWaitError>,
    output: Option<OutputError>,
) -> bool {
    quitting || leaving || input.is_some() || output.is_some()
}

/// A result skin's timing in the flow's terms.
pub(crate) const fn flow_timing(t: skin_model::ResultTiming) -> game::ResultTiming {
    game::ResultTiming {
        outro: t.outro,
        input: t.input,
        scene: t.scene,
    }
}

/// Readies the screen for a play: the end screen, if it shows, is left
/// first (a retry), its page and the render thread's slot emptied before
/// the preparation's handshake, so no page of the result is drawn after,
/// even one that waited while the render thread was behind.
impl PlayerRig<'_> {
    /// What every rebuild of the screen does first ([`game::Rig::prepare_screen`],
    /// the decision screen's recovery): within a result lifetime a screen
    /// built again is not a leaving, an outro playing included, until the
    /// flow serves the command, and the render side draws on from what it
    /// presented (the S10-09i design's D4, "Recovery"), so nothing here;
    /// otherwise the result is left, a retry asked for is taken, what a
    /// result offered is no later result's, and the end screen goes.
    pub(crate) fn before_rebuild(&mut self) {
        if self.presenter.current() && !self.result_leaving {
            return;
        }
        // A result the flow left (a retry), read before `leave_result`
        // clears it; a recovery is no leaving (dev-tools).
        #[cfg(feature = "dev-tools")]
        if self.result_leaving {
            self.finish_result(crate::dev::scenario::Leaving::Left);
        }
        self.leave_result();
        self.retry_from = self.pending_retry.take();
        self.result_retries = ResultRetries::default();
        #[cfg(feature = "dev-tools")]
        self.scenario_pending();
        self.end_page = None;
        self.render.clear_end();
    }
}

#[cfg(test)]
pub(crate) fn prepare_render(
    end_page: &mut Option<EndPage>,
    render: &RenderLink,
) -> Result<(), ScreenError> {
    *end_page = None;
    render.clear_end();
    render.prepare()
}

#[cfg(test)]
mod closing_tests {
    use bms_model::TimeUs;
    use game::{AbortCause, ClosingEnd, ClosingReport};

    use super::{closing_line, loading_gauge};
    use crate::loading_state::shown_gauge;

    /// The select screen's loading gauge resolves the settings' gauge as
    /// the attempt does: one the preset has as itself, one it lacks as its
    /// first.
    #[test]
    fn the_loading_gauge_resolves_as_the_attempt_does() {
        let preset = rules::beatoraja_style();
        assert!(shown_gauge(&preset, 3).is_some());
        assert_eq!(loading_gauge(&preset, 3), shown_gauge(&preset, 3));
        assert_eq!(shown_gauge(&preset, 99), None);
        assert!(shown_gauge(&preset, 0).is_some());
        assert_eq!(loading_gauge(&preset, 99), shown_gauge(&preset, 0));
    }

    /// The Closing line gives its start, its length and its end.
    #[test]
    fn the_closing_line_gives_start_length_and_end() {
        let c = |end| ClosingReport {
            started_at: TimeUs(2_584_000),
            ended_at: TimeUs(3_584_000),
            end,
        };
        assert_eq!(
            closing_line(&c(ClosingEnd::RanOut)),
            "closing: from 2584000 us, 1000000 us, ran out"
        );
        assert_eq!(
            closing_line(&c(ClosingEnd::Retry)),
            "closing: from 2584000 us, 1000000 us, retried"
        );
        assert_eq!(
            closing_line(&c(ClosingEnd::Cut(AbortCause::Suspend))),
            "closing: from 2584000 us, 1000000 us, cut by Suspend"
        );
    }
}

#[cfg(test)]
mod restore_tests {
    use std::sync::Arc;

    use audio::{AudioEngine, Gain, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
    use bms_model::{BgmEvent, SoundId, TimeUs};
    use game::{RESTORE_AHEAD, Restored, Restorer, audio_control, audio_link_at};

    use super::plan_restore;

    /// The player's planning, on the line a stepped audio worker publishes
    /// over a simulated device, restores a round at its lead-in's start:
    /// the round opens at chart time 7.8 s when the output is at 30 ms, and
    /// a 4 s BGM sound at 5 s enters there 3 s in, on time.
    #[test]
    fn a_round_planned_on_the_published_line_restores_at_its_lead_in() {
        let bank = Arc::new(SoundBank::from_sounds(
            48_000,
            vec![Sound::mono(vec![1_000; 4 * 48_000])],
        ));
        let schedule = vec![BgmEvent {
            time: TimeUs(5_000_000),
            sound: SoundId(0),
        }];
        let engine =
            AudioEngine::with_schedule(bank, schedule.clone(), 48_000, 480, 16, 0).unwrap();
        let (mut control, mut worker) = audio_control(engine);
        let mut out = RecordingOutput::new(
            SimDevice::new(SimConfig::steady(48_000, 480, TimeUs(0))),
            48_000,
        );
        for _ in 0..3 {
            worker.step(&mut out).unwrap();
        }
        let at = TimeUs(8_000_000);
        let origin = TimeUs(30_000 - (at.0 - RESTORE_AHEAD.0));
        let restorer = Restorer::new(control.seek_source(), schedule);
        let restore = plan_restore(&restorer, &control.counters(), origin, at).unwrap();
        assert_eq!(restore.at(), at);
        let (_sink, queue) = audio_link_at(4, origin);
        control
            .play_at(origin, queue, Gain::HEADROOM, Some(restore))
            .unwrap();
        for _ in 0..40 {
            worker.step(&mut out).unwrap();
        }
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(Restored::First)
        );
        // Frame `n` plays at `(n + 480) / 48 kHz`: the lead-in's start, 230
        // ms, is frame 10 560; silent before it.
        let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
        assert_eq!(left.iter().position(|&x| x != 0.0), Some(10_560));
    }
}

/// Whether a scripted run's end screen, shown for `shown`, is over: after
/// `after`, once the page shown last was presented and the frame sink has
/// seen it, however long the render thread takes on a loaded machine. A
/// failed screen draws no page, and one that never comes ends the wait at
/// [`STEP_TIMEOUT`](crate::dev::STEP_TIMEOUT).
#[cfg(feature = "dev-tools")]
pub(crate) fn scripted_end_over(after: Duration, shown: Duration, render: &RenderLink) -> bool {
    shown >= after
        && (render.end_page_drawn() || render.failed() || shown >= crate::dev::STEP_TIMEOUT)
}

mod loading;

#[cfg(all(test, feature = "dev-tools"))]
mod prep_tests;

#[cfg(all(test, feature = "dev-tools"))]
mod tests {
    use bms_model::TimeUs;

    use game::Command;

    use super::{IDLE_PARK, park_for, until_due, until_fired};

    /// A loading wait parks until its `until`, the idle wake at most, and
    /// the idle wake without one or for one already past (no zero park
    /// below 0: a past `until` parks 0, the look runs at once).
    #[test]
    fn a_loading_wait_parks_until_its_deadline() {
        use std::time::Duration;
        let now = TimeUs(5_000_000);
        assert_eq!(
            park_for(Some(TimeUs(5_030_000)), now),
            Duration::from_millis(30)
        );
        assert_eq!(
            park_for(Some(TimeUs(5_249_999)), now),
            Duration::from_micros(249_999)
        );
        assert_eq!(park_for(Some(TimeUs(6_000_000)), now), IDLE_PARK);
        assert_eq!(park_for(None, now), IDLE_PARK);
        assert_eq!(park_for(Some(TimeUs(4_000_000)), now), Duration::ZERO);
    }

    /// The player's rig's preparation (design.md "Play Lifecycle",
    /// preparing each attempt): it owns its jobs, and its answer stays
    /// for its final readiness.
    mod preparation {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        use game::Rig as _;
        use skin_beatoraja::JudgeClass;
        use skin_beatoraja::vm_hooks;

        use crate::imports::facts::tests::loaded;
        use crate::imports::tests as imports;
        use crate::play::PrepStage;
        use crate::render_side::ScreenError;
        use crate::render_side::tests::{Installers, bundle};
        use crate::result_skin::tests as result;
        use crate::result_skin::{ResultSkin, ResultStart};

        fn settings() -> game::Settings {
            game::Settings {
                device: game::DeviceSettings::default(),
                player: game::PlayerSettings::default(),
            }
        }

        /// A render thread with `loaded`'s chart.
        fn render(loaded: &game::Loaded) -> crate::render_side::RenderLink {
            Installers::default().spawn(&Arc::new(loaded.chart().clone()), &bundle())
        }

        /// The stage the rig's preparation stands at.
        fn stage<'r>(rig: &'r crate::play::PlayerRig<'_>) -> Option<&'r PrepStage> {
            rig.prep.as_ref().map(|p| &p.stage)
        }

        /// A pending play skin built from `body`.
        fn pending_play(body: &str) -> (tempfile::TempDir, crate::imports::PendingSkin) {
            let r = crate::render_common::offscreen(1_920, 1_080);
            let t = imports::target(&r);
            let dir = imports::lua_skin(body);
            let (p, _) = imports::pending(imports::play(
                &imports::choice(dir.path(), "play.luaskin"),
                &t,
            ));
            (dir, crate::imports::PendingSkin::new(p))
        }

        /// A rig shut down while its preparation's specialization runs
        /// settles the job through the preparation's drop: cancelled, its
        /// thread joined and its VM closed.
        #[test]
        fn a_rig_shut_down_mid_specialization_settles_its_job() {
            let _serial = imports::serial();
            let (_dir, skin) = pending_play("if ms.number(96) == 12 then while true do end end");
            let loaded = loaded("#PLAYLEVEL 12\n", "tapestry");
            let base = vm_hooks::live();
            let before = imports::jobs_counted();
            let (began, _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    rig.pending = Some(skin);
                    let _handle = rig.begin_preparation(&loaded, loaded.prep_key());
                    matches!(stage(rig), Some(PrepStage::Skin(_)))
                });
            assert!(began, "the specialization runs");
            let after = imports::jobs_counted();
            assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
            assert_eq!(vm_hooks::live(), base);
        }

        /// A preparation discarded once its specialization has ended,
        /// its outcome not taken, keeps the specialization: the next
        /// preparation with the same facts reuses it, with no job.
        #[test]
        fn a_discarded_preparation_keeps_its_finished_specialization() {
            let _serial = imports::serial();
            let r = crate::render_common::offscreen(1_920, 1_080);
            let t = imports::target(&r);
            let dir = imports::class_skin();
            let (p, _) = imports::pending(imports::play(
                &imports::choice(dir.path(), "play.luaskin"),
                &t,
            ));
            let loaded = loaded("#RANK 2\n", "tapestry");
            let before = imports::jobs_counted();
            let ((ran, reused), _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    rig.pending = Some(crate::imports::PendingSkin::new(p));
                    let _first = rig.begin_preparation(&loaded, loaded.prep_key());
                    let ran = matches!(stage(rig), Some(PrepStage::Skin(_)));
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !matches!(stage(rig), Some(PrepStage::Skin(job)) if job.finished())
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    rig.discard_attempt();
                    let _second = rig.begin_preparation(&loaded, loaded.prep_key());
                    (ran, !matches!(stage(rig), Some(PrepStage::Skin(_))))
                });
            assert!(ran, "the first preparation runs the specialization");
            assert!(reused, "the second reuses it");
            assert_eq!(imports::jobs_counted().0 - before.0, 1);
        }

        /// Polls `done` every 1 ms until it holds, at most 20 s (a hang
        /// guard).
        fn until(done: impl Fn() -> bool) {
            let end = Instant::now() + Duration::from_secs(20);
            while !done() {
                assert!(Instant::now() < end, "never came");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Lets every held job thread and settle go when dropped: made
        /// first inside a rig's closure, so a failing test lets them go
        /// before its rig drops and joins the job.
        struct LetGo;

        impl Drop for LetGo {
            fn drop(&mut self) {
                imports::hold_exit(false);
                crate::dev::hold_result_settles(false);
            }
        }

        /// A pending result skin whose images are staged, and a chart.
        fn staged_skin() -> (tempfile::TempDir, ResultSkin, game::Loaded) {
            let d = result::skin(
                "",
                "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
            );
            let p = result::pending(d.path(), result::LIMITS);
            let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
            result::staged_at(&mut rs, 12, JudgeClass::Normal);
            (d, rs, loaded("#PLAYLEVEL 12\n", "tapestry"))
        }

        /// A layout job wakes the result's idle wait once its outcome is
        /// in; the wait looks again every `LAYOUT_RELOOK` while the job's
        /// thread still ends, and lands it once the thread is released,
        /// with no other wake. The thread is released only after this
        /// rig's wait found the outcome received and not yet taken. A
        /// thread that does not end is given up at the wait's deadline,
        /// the job kept. The 60 s deadline is a liveness guard only: the
        /// wait must be woken by the job, never reach it.
        #[test]
        fn the_idle_wait_lands_a_layout_whose_thread_still_ends() {
            use std::sync::atomic::Ordering;
            let _serial = imports::serial();
            let (_d, rs, loaded) = staged_skin();
            let ((landed, kept), _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    let _let_go = LetGo;
                    rig.result_skin = rs;
                    rig.result_skin.set_wake(rig.controls.game_thread());
                    let relooked = Arc::clone(&rig.relooked);
                    imports::hold_exit(true);
                    rig.result_skin.request(result::view(5));
                    let far = bms_model::TimeUs(rig.now().0 + 60_000_000);
                    let landed = std::thread::scope(|s| {
                        s.spawn(|| {
                            let _let_go = LetGo;
                            until(|| relooked.load(Ordering::SeqCst));
                            imports::hold_exit(false);
                        });
                        // One wait at least, the outcome perhaps in already;
                        // another only while it is not in (a wake came
                        // first).
                        loop {
                            assert!(rig.now().0 < far.0, "the outcome never came in");
                            rig.idle_wait(far);
                            if !rig.result_skin.has_job() || rig.result_skin.landing() {
                                break;
                            }
                        }
                        !rig.result_skin.has_job() && rig.now().0 < far.0
                    });
                    // A thread that never ends: given up at the deadline.
                    imports::hold_exit(true);
                    rig.result_skin.request(result::view(7));
                    until(|| imports::exit_waiting() == 1);
                    rig.idle_wait(bms_model::TimeUs(rig.now().0 + 50_000));
                    let kept = rig.result_skin.has_job() && rig.result_skin.landing();
                    imports::hold_exit(false);
                    rig.result_skin.settle(|| false);
                    (landed, kept)
                });
            assert!(
                landed,
                "landed after its thread's release, woken by the job"
            );
            assert!(kept, "given up at the deadline, the job kept");
        }

        /// A close while the idle wait looks again for a received layout
        /// (its thread held after its send) leaves the looks at once: the
        /// result skin's settlement begins while the thread is still
        /// held, not once it ended or the wait's deadline passed.
        #[test]
        fn a_close_during_the_relooks_settles_at_once() {
            use std::sync::atomic::Ordering;
            let _serial = imports::serial();
            let (_d, rs, loaded) = staged_skin();
            let (held, _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    let _let_go = LetGo;
                    rig.result_skin = rs;
                    rig.result_skin.set_wake(rig.controls.game_thread());
                    let relooked = Arc::clone(&rig.relooked);
                    let controls = rig.controls;
                    imports::hold_exit(true);
                    crate::dev::hold_result_settles(true);
                    rig.result_skin.request(result::view(5));
                    let far = bms_model::TimeUs(rig.now().0 + 60_000_000);
                    std::thread::scope(|s| {
                        let closer = s.spawn(|| {
                            let _let_go = LetGo;
                            until(|| relooked.load(Ordering::SeqCst));
                            controls.close();
                            until(crate::dev::result_settle_holding);
                            let held = imports::exit_waiting() == 1;
                            crate::dev::hold_result_settles(false);
                            imports::hold_exit(false);
                            held
                        });
                        while !rig.controls.quitting() {
                            assert!(rig.now().0 < far.0, "the close never came");
                            rig.idle_wait(far);
                        }
                        closer.join().unwrap()
                    })
                });
            assert!(held, "the settlement began while the thread was still held");
        }

        /// A preparation discarded while the result skin's image stage
        /// runs hands the stage's images back to the result skin.
        #[test]
        fn a_discarded_preparation_hands_the_image_stages_images_back() {
            let _serial = imports::serial();
            let d = result::skin(
                "if ms.number(96) == 13 then while true do end end",
                "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
            );
            let p = result::pending(d.path(), result::LIMITS);
            let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
            result::staged_at(&mut rs, 12, JudgeClass::Normal);
            assert!(rs.image().is_some(), "the probe's images");
            let loaded = loaded("#PLAYLEVEL 13\n", "tapestry");
            let ((ran, back), _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    rig.result_skin = rs;
                    let _handle = rig.begin_preparation(&loaded, loaded.prep_key());
                    let ran = matches!(stage(rig), Some(PrepStage::Image(..)));
                    rig.discard_attempt();
                    (ran, rig.result_skin.image().is_some())
                });
            assert!(ran, "the image stage runs");
            assert!(back, "its images handed back");
        }

        /// A poll with another preparation's ticket leaves the rig's
        /// preparation in place, and a poll after it ended keeps its
        /// answer: its final readiness is still ready.
        #[test]
        fn a_foreign_or_late_poll_keeps_the_preparation() {
            let loaded = loaded("", "tapestry");
            let ((foreign, ended, late, ready), _) =
                super::super::super::tests::on_rig_with(settings(), render(&loaded), |rig, _| {
                    let mut handle = rig.begin_preparation(&loaded, loaded.prep_key());
                    let mut other = game::PrepHandle::new(loaded.prep_key(), handle.ticket() + 1);
                    let foreign = matches!(
                        rig.poll_preparation(&mut other, Instant::now()),
                        Err(ScreenError::Lost)
                    );
                    let ended = loop {
                        let until = Instant::now() + Duration::from_millis(10);
                        if let game::PrepPoll::Done(ended) =
                            rig.poll_preparation(&mut handle, until).unwrap()
                        {
                            break ended;
                        }
                    };
                    let late = matches!(
                        rig.poll_preparation(&mut handle, Instant::now()),
                        Err(ScreenError::Lost)
                    );
                    let ready = matches!(rig.ready_attempt(1, &handle), Ok(game::Readiness::Ready));
                    (foreign, ended, late, ready)
                });
            assert!(foreign, "another ticket's poll is lost");
            assert_eq!(ended, game::Preparation::Ready);
            assert!(late, "a poll after the end answers nothing new");
            assert!(ready, "the answered request stays");
        }
    }

    /// The awake time the marker wait reads, set by the test.
    static AWAKE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn awake() -> std::time::Duration {
        std::time::Duration::from_millis(AWAKE_MS.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// A wait for the input thread's markers is bounded by 2 s of awake
    /// time: time passed in a suspend, which the awake clock does not
    /// count, never times it out, and 2 s awake does.
    #[test]
    fn the_marker_wait_counts_awake_time_only() {
        use game::InputPump as _;
        use std::sync::atomic::Ordering;
        let controls =
            std::sync::Arc::new(crate::controls::Controls::new(input::QpcClock::start()));
        let (_hook, mut link) = crate::input_side::link(
            std::sync::Arc::clone(&controls),
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        link.set_awake(awake);
        let mut pump = super::PlayerPump {
            input: &link,
            tuning: controls.tuning(),
            look: None,
            since: None,
        };
        AWAKE_MS.store(1_000, Ordering::SeqCst);
        assert_eq!(pump.park(), Ok(()));
        // A long suspend: wall time passes, awake time does not.
        std::thread::sleep(std::time::Duration::from_millis(50));
        AWAKE_MS.store(2_999, Ordering::SeqCst);
        assert_eq!(pump.park(), Ok(()), "1.999 s awake");
        AWAKE_MS.store(3_000, Ordering::SeqCst);
        assert_eq!(pump.park(), Err(game::InputWaitError::Timeout));
    }

    /// E16c: a drain through the player's pump validates each control's
    /// observation against the ingress word and acts on it with the
    /// tuning step lock held, so the choice's close, which takes that
    /// lock, cannot fall between the two: at the meeting point between
    /// validation and action the lock is held, and it is free again after
    /// the drain (the positive control). The meeting point is reached.
    #[test]
    fn the_ingress_validation_and_action_hold_the_lock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let controls = Arc::new(crate::controls::Controls::new(input::QpcClock::start()));
        let (_hook, link) = crate::input_side::link(
            Arc::clone(&controls),
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let device = game::DeviceSettings::default();
        let keys = game::KeyConfigs {
            keyboard: device.keyboard.clone(),
            controller: device.controller.clone(),
            controller_device: None,
        };
        let plan = game::AdjustmentConfig::default()
            .plan(&keys, None, None)
            .unwrap();
        let (mut driver, mut tap, _reader) =
            game::AdjustDriver::new(&plan, game::PlayOptions::default(), 0).unwrap();
        tap.set_activity(Arc::clone(controls.tuning().activity()));
        let mut set = input::SourceSet::new(input::SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        set.set_presentation_focus(input::PresentationFocus::new());
        set.begin_batch(TimeUs(0));
        set.connect(input::DeviceId(1), input::DeviceKind::Keyboard);
        set.end_batch(TimeUs(1));
        set.set_adjustment(tap);
        // F2 pressed and released.
        set.begin_batch(TimeUs(10));
        for value in [1, 0] {
            set.packet(&input::Packet {
                device: input::DeviceId(1),
                control: eval::ControlId(0x3C),
                value,
                background: false,
            });
        }
        set.end_batch(TimeUs(11));
        let met = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(AtomicUsize::new(0));
        {
            let (met, held, tuning) = (
                Arc::clone(&met),
                Arc::clone(&held),
                Arc::clone(controls.tuning()),
            );
            super::BETWEEN_INGRESS.with_borrow_mut(|f| {
                *f = Some(Box::new(move || {
                    met.fetch_add(1, Ordering::SeqCst);
                    if !tuning.step_free() {
                        held.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            });
        }
        driver.pump(&mut super::PlayerPump {
            input: &link,
            tuning: controls.tuning(),
            look: None,
            since: None,
        });
        super::BETWEEN_INGRESS.with_borrow_mut(|f| *f = None);
        assert!(
            met.load(Ordering::SeqCst) > 0,
            "the meeting point was reached"
        );
        assert_eq!(held.load(Ordering::SeqCst), met.load(Ordering::SeqCst));
        assert!(controls.tuning().step_free());
    }

    /// The input side, synchronous, gated as the player's pump is: a fence
    /// is answered at once, and a drain validates under the tuning's lock.
    struct SyncPump<'a> {
        set: &'a mut input::SourceSet,
        tuning: &'a crate::tuning::LiveTuning,
        now: TimeUs,
    }

    impl game::InputPump for SyncPump<'_> {
        fn request_fence(&mut self, n: u64) {
            let now = self.now;
            self.set.adjustment_mut().unwrap().fence(n, now);
        }
        fn wake(&mut self) {
            self.set.tick_presentation(self.now);
        }
        fn display_offset_us(&self) -> i64 {
            0
        }
        fn with_ingress(&mut self, f: &mut game::IngressDrain<'_>) {
            self.tuning.with_ingress(|word| f(Some(word), &|| {}));
        }
        fn park(&mut self) -> Result<(), game::InputWaitError> {
            Err(game::InputWaitError::Timeout)
        }
    }

    /// F2 from the window in an autoplay session, queued for the input
    /// thread during the decision screen and taken by the tap only after
    /// the reopening, does not act: the observation carries the word the
    /// window saw it under. Queued with the word open (the positive
    /// control), it moves the hi-speed.
    #[test]
    fn a_window_speed_key_queued_in_the_epoch_does_not_act_after_it() {
        use std::sync::Arc;
        for crossing in [true, false] {
            let controls = crate::controls::Controls::new(input::QpcClock::start());
            let device = game::DeviceSettings::default();
            let keys = game::KeyConfigs {
                keyboard: device.keyboard.clone(),
                controller: device.controller.clone(),
                controller_device: None,
            };
            let plan = game::AdjustmentConfig::default()
                .plan(&keys, None, None)
                .unwrap();
            let (mut driver, mut tap, mut reader) =
                game::AdjustDriver::new(&plan, game::PlayOptions::default(), 0).unwrap();
            tap.set_activity(Arc::clone(controls.tuning().activity()));
            let number = |code| {
                plan.bindings
                    .controls
                    .iter()
                    .find(|(c, _)| *c == input::TapControl::Key(eval::ControlId(code)))
                    .map(|&(_, n)| n)
            };
            let [f1, f2] = game::SPEED_KEYS;
            let (window, receiver) = input::window_keys();
            tap.set_window_keys(receiver, [number(f1), number(f2)]);
            controls.set_window_keys(window);
            let mut set = input::SourceSet::new(input::SourceConfig {
                max_devices: 2,
                max_sources: 2,
                max_held: 8,
                queue_capacity: 64,
                max_arrivals: 2,
            })
            .unwrap();
            set.set_presentation_focus(input::PresentationFocus::new());
            set.begin_batch(TimeUs(0));
            set.connect(input::DeviceId(1), input::DeviceKind::Keyboard);
            set.end_batch(TimeUs(1));
            set.set_adjustment(tap);
            let tuning = Arc::clone(controls.tuning());
            fn pump<'a>(
                set: &'a mut input::SourceSet,
                tuning: &'a crate::tuning::LiveTuning,
                at: i64,
            ) -> SyncPump<'a> {
                SyncPump {
                    set,
                    tuning,
                    now: TimeUs(at),
                }
            }
            driver
                .enter_attempt(Some(TimeUs(-2_000_000)), &mut pump(&mut set, &tuning, 2))
                .unwrap();
            let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
            driver
                .attach(
                    game::AttemptTempo::prepare(&chart, None),
                    TimeUs(-2_000_000),
                    &mut pump(&mut set, &tuning, 3),
                )
                .unwrap();
            driver.pump(&mut pump(&mut set, &tuning, 4));
            let before = reader.read().hispeed;
            if crossing {
                controls.close_epoch();
            }
            controls.window_key(1, input::WindowKeyEvent::Press, false);
            controls.window_key(1, input::WindowKeyEvent::Release, false);
            if crossing {
                controls.open_epoch();
            }
            // The tap takes the window's queue at its next tick.
            set.tick_presentation(TimeUs(10_000));
            set.tick_presentation(TimeUs(11_000));
            driver.pump(&mut pump(&mut set, &tuning, 12_000));
            let after = reader.read().hispeed;
            if crossing {
                assert_eq!(after, before, "a key from the epoch acted after it");
            } else {
                assert_ne!(after, before, "the positive control moved nothing");
            }
        }
    }

    /// A scenario's `until` is due at its chart time exactly: chart time 0
    /// lies at 6 s on the clock, so chart time 2.25 s is clock time 8.25 s.
    #[test]
    fn until_is_due_exactly_at_its_time() {
        let origin = TimeUs(6_000_000);
        let at = TimeUs(2_250_000);
        assert!(!until_due(origin, at, TimeUs(8_249_999)));
        assert!(until_due(origin, at, TimeUs(8_250_000)));
        assert!(!until_due(origin, TimeUs(0), TimeUs(5_999_999)));
        assert!(until_due(origin, TimeUs(0), TimeUs(6_000_000)));
    }

    /// The `until` command is sent once, at its time on the play's clock:
    /// nothing before READY is known or before the time, then the
    /// command, then nothing again.
    #[test]
    fn until_fires_once_at_its_time() {
        let origin = TimeUs(6_000_000);
        let mut until = Some((TimeUs(2_250_000), Command::Retry));
        assert_eq!(until_fired(&mut until, None, TimeUs(9_000_000)), None);
        for now in [5_000_000, 7_000_000, 8_249_999] {
            assert_eq!(until_fired(&mut until, Some(origin), TimeUs(now)), None);
        }
        assert!(until.is_some());
        assert_eq!(
            until_fired(&mut until, Some(origin), TimeUs(8_250_000)),
            Some(Command::Retry)
        );
        assert_eq!(until, None);
        assert_eq!(
            until_fired(&mut until, Some(origin), TimeUs(8_250_001)),
            None
        );
    }
}
