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
use crate::render_side::{AttemptSkin, EndPage, RenderEnd, RenderLink, RenderMsg, ScreenError};

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
            since: None,
        });
    }
}

/// Each saved play's record and figures, in order (dev-tools).
#[cfg(feature = "dev-tools")]
pub type MeasuredPlays = std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, Figures)>>>;

/// The game's threads as a rig.
pub struct PlayerRig<'a> {
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
    /// Load times shown on the end screen.
    pub times: LoadTimes,
    /// When the current play's READY state started, on the clock.
    pub ready_state_at: Option<TimeUs>,
    /// Where the current play's chart time 0 lies on the clock.
    pub origin: Option<TimeUs>,
    /// The play skin's declared Closing duration, from the current
    /// attempt's commit receipt ([`commit_ready`]); `None` before it.
    pub declared_close: Option<TimeUs>,
    /// Where each logic step's measurement goes (the render thread).
    pub steps: rtrb::Producer<StepSample>,
    /// Step samples of the current play the full queue refused.
    pub steps_dropped: u64,
    /// The figures of the play just over, until they are written.
    pub figures: Option<Figures>,
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
    /// A leaving command was accepted on the result: the next screen's
    /// preparation leaves its lifetime.
    pub result_leaving: bool,
}

impl PlayerRig<'_> {
    /// The result skin's image stage for the attempt at `loaded` (design
    /// D11), after the play skin's specialization and before READY: the
    /// quit key cancels its job and waits for its thread; the window's
    /// close and a failure of the input or audio thread end it within the
    /// stop bound.
    fn result_stage(&mut self, loaded: &Loaded) -> crate::result_skin::Staged {
        let chart = crate::result_skin::attempt_chart(loaded);
        self.result_chart = Some(chart);
        self.result_labels = Some(crate::result_present::labels_of(&loaded.attempt().labels));
        let mut result = std::mem::take(&mut self.result_skin);
        #[cfg(feature = "dev-tools")]
        crate::dev::stage_began(crate::dev::SpecializingStage::ResultImage);
        let rig = &*self;
        let class = crate::result_skin::attempt_class(loaded);
        let staged = result.image_stage(chart, class, || rig.controls.leaving(), || rig.run_ends());
        self.result_skin = result;
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: result image stage: {}",
            self.result_skin.staged_line()
        );
        staged
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
    /// show. Nothing without a result skin, or once the run disabled it.
    fn present_view(&mut self, report: &PlayReport, screen: &ResultScreen) {
        let (Some(chart), Some(page)) = (self.result_chart, self.end_page.clone()) else {
            return;
        };
        if !self.result_skin.has_skin() || self.presenter.disabled().is_some() {
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

    /// The result's lifetime ends: nothing of it is drawn any more.
    pub(crate) fn leave_result(&mut self) {
        self.presenter.end(&mut self.render);
        self.result_facts = None;
        self.result_offset = None;
        self.result_leaving = false;
    }

    /// The attempt's result is uploaded after its commit and before
    /// recording opens (the S10-09i design's D2): a fixed skin reinstalled
    /// when play's admission released it, a chart-only specialization, or
    /// a prepared atlas, unless its atlas is installed. The wait looks at
    /// the quit key, the close and the input and audio threads between
    /// slices: on any of them the upload is withdrawn, so a late staging
    /// installs nothing, and the end of loading does not wait on the
    /// render thread. A refusal shows the built-in result for the attempt.
    fn upload_result(&mut self) {
        self.result_withdrawn = false;
        let status = self.render.result_status();
        if status.disabled.is_some() {
            return;
        }
        let Some(upload) = self.result_skin.upload() else {
            return;
        };
        if status.installed == Some(upload.atlas()) {
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
            log::info!("skin: result: built-in for this attempt: {why}");
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
            skin_beatoraja::lua::vm_hooks::live(),
            crate::imports::jobs_joined(),
            crate::imports::jobs_started()
        );
    }

    /// The play skin the attempt at `loaded` is prepared with: the skin in
    /// use, or the pending play skin's specialization for the attempt's
    /// facts ([`crate::imports::PendingSkin::attempt`]), reused while the
    /// facts it read hold. A job's wait shows the stage line on the
    /// loading state, or the preparation page without one (a retry, a
    /// round). The quit key cancels the job and waits for its thread's
    /// end; the window's close and a failure of the input or audio thread
    /// end it within the stop bound. The adjustments and the dev presses
    /// are served meanwhile.
    ///
    /// # Errors
    ///
    /// The attempt's preparation instead: cancelled once a signal came,
    /// or refused with the reason the skin cannot be built.
    fn attempt_skin(&mut self, loaded: &Loaded) -> Result<AttemptSkin, Preparation> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(AttemptSkin::InUse);
        };
        let facts = crate::imports::facts::attempt_facts(loaded, self.autoplay);
        let mut adjust = self.adjust.take();
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
        let quit = || {
            #[cfg(feature = "dev-tools")]
            crate::dev::press_due(rig.controls, crate::dev::LoadPhase::Preparing);
            serve_adjustments(adjust.as_mut(), &rig.input, rig.controls.tuning());
            rig.controls.leaving()
        };
        // A script's step on the specialization (dev-tools) is asked at
        // every look of the wait, an attempt's quit included.
        let end_run = || rig.run_ends();
        let (skin, how) = pending.attempt(facts, wait, quit, end_run);
        #[cfg(feature = "dev-tools")]
        println!("dev: specialization wait left");
        self.adjust = adjust;
        self.pending = Some(pending);
        skin.report(&how);
        // The job's thread has ended, and its VMs with it, before the
        // render thread prepares the attempt (dev-tools witness).
        #[cfg(feature = "dev-tools")]
        println!(
            "dev: specialization ended: live {} joined {} of {}",
            skin_beatoraja::lua::vm_hooks::live(),
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
    /// jobs started, the passes and the instruction steps run, which stay
    /// still from recording's open to the play's end.
    #[cfg(feature = "dev-tools")]
    fn print_lua(when: &str) {
        use skin_beatoraja::lua::vm_hooks;
        println!(
            "dev: lua at {when}: live {} jobs {} passes {} steps {}",
            vm_hooks::live(),
            crate::imports::jobs_started(),
            vm_hooks::passes(),
            vm_hooks::steps()
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
    fn next_window_scene(&self) {
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
            since: None,
        };
        self.adjust.as_mut().map(|d| (d, pump))
    }

    /// Writes changed settings, then ends the render and audio threads;
    /// how the render thread ended: when [`RenderEnd::Stuck`], the window
    /// must stay open until the process exits.
    #[must_use]
    pub fn shut_down(mut self) -> RenderEnd {
        self.persist();
        let ended = self.render.exit();
        self.audio.exit();
        ended
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
            self.present_overlay();
        }
    }

    /// The hi-speed the end screen shows: the adjustments' newest state,
    /// its green number at the attempt's reference tempo.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn speed_line(&self) -> end_screen::SpeedLine {
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
    declared: &mut Option<TimeUs>,
) -> Result<(), ScreenError> {
    *declared = None;
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
    preset: &rules::RulePreset,
    gauge: usize,
) {
    if loading {
        let gauge = shown_gauge(preset, gauge);
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

impl Rig for PlayerRig<'_> {
    type Reader = SourceReader;
    type ScreenError = ScreenError;
    type ResultPrep = ();

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait(&mut self) {
        thread::park_timeout(if self.in_play { PARK } else { IDLE_PARK });
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

    fn idle_wait(&mut self, deadline: TimeUs) {
        let left = deadline.0 - self.clock.now().0;
        if left > 0 {
            thread::park_timeout(Duration::from_micros(left.unsigned_abs()));
        }
        // A switch's layout lands when its job is done; a close leaves its
        // wait within the stop bound.
        self.land_result();
        if self.controls.quitting() && self.result_skin.has_job() {
            self.result_skin.settle(|| true);
        }
        self.serve_adjustments();
        self.sync();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take(&mut self) -> Option<Command> {
        let c = self.controls.take();
        if c == Some(Command::Retry) {
            self.pending_retry = self.controls.retry_at();
        }
        c
    }

    /// The current attempt's commit receipt.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn declared_close(&self) -> Option<TimeUs> {
        self.declared_close
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
    /// then, while a result shows, its own keys and controller events.
    fn take_menu(&mut self) -> Option<MenuCommand> {
        loop {
            match self.take() {
                Some(Command::Retry) => return Some(MenuCommand::Retry),
                Some(Command::Quit) => return Some(MenuCommand::Back),
                Some(Command::Suspend) => {}
                None => break,
            }
        }
        self.result_input.take()
    }

    /// A withdrawn preparation cancels the result skin's layout job and
    /// waits for its thread (design D11, "Cancellation"); the run's end
    /// bounds the wait.
    fn withdraw_result(&mut self) {
        self.settle_result();
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settled: withdrawn");
    }

    /// A leaving command the Result state acts on cancels a switch's
    /// layout job and waits for its thread before it is served (D11); the
    /// run's end bounds the wait.
    fn accept_leaving(&mut self, _cmd: MenuCommand) {
        self.result_leaving = true;
        self.settle_result();
        #[cfg(feature = "dev-tools")]
        println!("dev: result skin settled: accepted");
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
        // recording opens and READY starts.
        self.pause_scan();
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
        commit_ready(&self.render, loading, &mut self.declared_close)?;
        self.upload_result();
        Ok(())
    }

    fn discard_attempt(&mut self) {
        self.render.discard_attempt();
    }

    /// A gated attempt pauses its loading lifetime instead, so a hold at
    /// the opening returns to the same loading state ([`defer`](Rig::defer));
    /// the opening ends it.
    fn end_loading(&mut self) -> Result<(), ScreenError> {
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
        // Within a result lifetime a screen built again is not a leaving:
        // the render side draws on from what it presented (the S10-09i
        // design's D4, "Recovery").
        if self.presenter.current() && !self.result_leaving {
            return self.render.prepare();
        }
        self.leave_result();
        self.retry_from = self.pending_retry.take();
        #[cfg(feature = "dev-tools")]
        self.scenario_pending();
        prepare_render(&mut self.end_page, &self.render)
    }

    /// The render thread prepares the attempt from its owned context; a
    /// quit key, a close, or a failure of the input or audio thread while
    /// it does stops the wait ([`preparation_stops`]), and the game looks
    /// at the threads for a failure's cause.
    ///
    /// A pending play skin is built for the attempt first
    /// ([`attempt_skin`](Self::attempt_skin)), and the attempt prepared
    /// with its specialization, staged with it. The loading lifetime's
    /// gauge descriptor is replaced from the preset the play refreshed,
    /// with the gauge the attempt shows.
    fn prepare_attempt(&mut self, loaded: &Loaded) -> Result<Preparation, ScreenError> {
        self.result_skin.next_attempt();
        self.presenter.next_attempt();
        let skin = match self.attempt_skin(loaded) {
            Ok(skin) => skin,
            Err(stopped) => return Ok(stopped),
        };
        if self.result_stage(loaded) == crate::result_skin::Staged::Cancelled {
            return Ok(Preparation::Cancelled);
        }
        let specialized = matches!(skin, AttemptSkin::Specialized(_));
        self.attempt_gauge = loaded.displayed().0;
        refresh_loading_gauge(
            &self.render,
            self.loading_active(),
            loaded.preset(),
            self.attempt_gauge,
        );
        // The attempt attached last is the one prepared: its epoch is
        // the frames'.
        let epoch = self.adjust.as_mut().and_then(game::AdjustDriver::prepared);
        let attempt = AttemptContext::new(
            loaded.attempt(),
            SIMPLE_SCRATCH,
            epoch.unwrap_or(0),
            self.autoplay,
        );
        // The adjustments are served while the screen prepares, so an
        // adjustment then is published before the preparation ends.
        let mut adjust = self.adjust.take();
        let rig = &*self;
        let staging = Instant::now();
        let prepared = rig.render.prepare_attempt_with(attempt, skin, || {
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
        // The render thread's staging and upload of the specialization,
        // apart from the job's time.
        if specialized {
            crate::imports::log_staged(staging.elapsed(), &prepared);
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::print_adjustments("preparing", self.adjust.as_ref());
        prepared
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
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
        })?;
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
        self.render.send(RenderMsg::Play {
            side: screen,
            gauge: self.attempt_gauge,
            measure: Box::new(measure),
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
        crate::start_gate::output_failure(&self.audio.control, self.gate.as_ref())
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn screen_failed(&self) -> bool {
        self.render.failed()
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
        self.input.failure().or(self.adjustments_failure())
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

    fn ended(&mut self, report: &PlayReport) {
        // The result is another scene: held adjustment controls act no more
        // until pressed afresh.
        self.enter_scene();
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
        if self.result_input.enter() {
            self.viewed = None;
            self.evaluated = 0;
        }
    }

    /// P: the own view's layout, asked for once its best is compared, with
    /// the view's final facts (design D11; the S10-09i design's Q1).
    fn result_compared(&mut self, _report: &PlayReport, screen: &ResultScreen) {
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
        if let Some(chart) = self.result_chart {
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
        if let Some(chart) = self.result_chart
            && self.presenter.attempt_fallen().is_none()
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

/// Readies the screen for a play: the end screen, if it shows, is left
/// first (a retry), its page and the render thread's slot emptied before
/// the preparation's handshake, so no page of the result is drawn after,
/// even one that waited while the render thread was behind.
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

#[cfg(all(test, feature = "dev-tools"))]
mod tests {
    use bms_model::TimeUs;

    use game::Command;

    use super::{until_due, until_fired};

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
