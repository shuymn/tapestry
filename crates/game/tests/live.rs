//! The play lifecycle every driver shares (`game::live`), on a rig that
//! stands in for the game's threads: a virtual clock that is not at 0, an
//! input side that opens recording with the play's origin on that clock
//! (as the input thread does), and audio and screen doubles whose
//! acknowledgements and failures the test controls.

mod common;

use std::path::{Path, PathBuf};

use bms_model::{Chart, ChartBuilder, TimeSource, TimeUs, VirtualClock};
use common::{Built, MS, lane, tapestry_settings};
use eval::{ControlId, InputKind, JudgeEventKind, SourceId};
use game::{
    AbortCause, AudioControl, AudioQueue, AudioWaitError, Closing, Command, DataRoot,
    DeviceSettings, EntryId, InputWaitError, LiveError, Loaded, Notice, Opening, Parts, PlayEnd,
    PlayReport, Preparation, PreparationRefusal, RenderSide, ResultScreen, Rig, SessionConfig,
    Settings, ViewStatus,
};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput, SourceReader,
    SourceSet,
};
use replay::{BuildProvenance, ReceiveMethod};
use rules::Judgment;

/// Where the rig's clock starts: no play's origin is near 0.
const CLOCK_START: i64 = 7_300_000;
/// The input offset every play is evaluated with.
const OFFSET: i64 = 3_000;
/// Lane 1's key in the default key config (Z).
const Z: ControlId = ControlId(0x2c);
/// Lane 3's key (X).
const X: ControlId = ControlId(0x2d);

/// Lane 1 at 1.0 s and 1.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_500 * MS))
        .build()
        .unwrap()
}

fn loaded(dir: &Path) -> Loaded {
    loaded_with(dir, "tapestry")
}

/// The user preset `presets/mine.toml` in `dir`, holding `preset`.
fn write_user_preset(dir: &Path, preset: &rules::RulePreset) -> PathBuf {
    let presets = dir.join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let path = presets.join("mine.toml");
    std::fs::write(&path, game::preset_to_toml(preset)).unwrap();
    path
}

/// The file at `path` rewritten to hold `preset` ([`common::rewrite`]).
fn rewrite_preset(path: &Path, preset: &rules::RulePreset) {
    common::rewrite(path, &game::preset_to_toml(preset));
}

/// [`loaded`] with the preset named `preset` in `player.toml`.
fn loaded_with(dir: &Path, preset: &str) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.player.preset = preset.to_owned();
    settings.device.input_offset_us = OFFSET;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart())),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

fn key(at: i64, control: ControlId, value: i32) -> ScriptItem {
    ScriptItem {
        at: TimeUs(at),
        action: Action::Packet(Packet {
            device: DeviceId(4),
            control,
            value,
            background: false,
        }),
    }
}

/// The screen double's failure to build again.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the surface cannot be made again")]
struct SurfaceLost;

/// A rig on a virtual clock: each wait moves it 1 ms and feeds the
/// script's batches at their clock times, as the input thread receives
/// them.
struct Fake {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    commands: Vec<(i64, Command)>,
    quit_at: i64,
    /// The current play's origin, once it started.
    origin: Option<TimeUs>,
    /// The close request takes effect after the input thread's batches up
    /// to this much later.
    close_lag: i64,
    quit_in_prepare: bool,
    quitting: bool,
    screen_fails_at: i64,
    /// Preparations from this one on fail.
    prepare_fails_from: usize,
    prepares: usize,
    origins: Vec<TimeUs>,
    sounds: Option<AudioQueue>,
    sealed: bool,
    /// Waits after the seal before the audio is done.
    tail_waits: usize,
    waits_since_seal: usize,
    /// A command that arrives while the close request is handled.
    in_close: Option<Command>,
    cancels: usize,
    shows: usize,
    /// An audio control whose readiness the play waits for.
    audio: Option<game::AudioControl>,
    reports: Vec<PlayReport>,
    /// Input offsets the player sets at these clock times, as on the end
    /// screen: each waits for the next play.
    offset_changes: Vec<(i64, i64)>,
    pending_offset: Option<i64>,
    /// Every step's measurement, by play.
    measured: Vec<Vec<game::Advance>>,
    /// Each step's judgments with an origin and its delivery time, as
    /// (seq, origin, delivered), by play.
    judged_at: Vec<Vec<(u64, i64, i64)>>,
    /// Each reading of the clock moves it this much further, as real
    /// processing would.
    read_cost: i64,
    reads: std::cell::Cell<i64>,
    /// The lifecycle order: `play_over`, `ended`, and what the record sink
    /// adds.
    order: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    /// Every open fails with this.
    open_fails: Option<InputWaitError>,
    /// Every close fails with this; with `Timeout`, the input thread still
    /// pushes the batches of `close_lag`.
    close_fails: Option<InputWaitError>,
    /// The input thread is gone from this clock time on: nothing more is
    /// received or published.
    input_fails_at: i64,
    /// The input thread is gone once the screen has been prepared.
    input_fails_in_prepare: bool,
    /// The window closes while recording opens or closes.
    quit_in_open: bool,
    quit_in_close: bool,
    /// The failure a handshake found: it sticks, as on a poisoned link.
    failed: Option<InputWaitError>,
    /// Opens attempted, failed ones included.
    opens: usize,
    /// Actions run once the clock reaches their time, as edits the player
    /// makes meanwhile.
    at: Vec<(i64, Box<dyn FnOnce()>)>,
    /// The clock time of each end screen shown.
    shown_at: Vec<i64>,
    /// Each result screen shown, with its clock time.
    screens: Vec<(i64, ResultScreen)>,
    /// Idle waits move the clock to their deadline, or to the next action
    /// in `at` if that is sooner, instead of one step.
    idle_to_deadline: bool,
    /// The deadline of each idle wait, with the clock time it began.
    idle_waits: Vec<(i64, i64)>,
    /// Menu commands, taken on the result at these clock times.
    menu: Vec<(i64, game::MenuCommand)>,
    /// Commands pressed by a record sink while the play is saved, taken
    /// first.
    pressed: std::sync::Arc<std::sync::Mutex<Vec<Command>>>,
    /// The handshakes before each opening, in order.
    steps: Vec<&'static str>,
    /// Every attempt the screen was asked to prepare.
    attempts: Vec<game::Attempt>,
    /// How each preparation ends, in order; then ready.
    preparations: Vec<Preparation>,
    /// The audio output failed.
    output_failed: bool,
    /// Waits each preparation takes, a quit or a close during which
    /// cancels it.
    prepare_waits: usize,
    /// The play's screen end, read at each step.
    side: Option<RenderSide>,
    /// Each step's play time and the phase its view published, by play.
    phases: Vec<Vec<(i64, bool)>>,
    /// The phase published once each play's recording closes.
    closed_phases: Vec<bool>,
    /// Whether plays run the start gate; with `lifetime`, a deferred opening
    /// resumes a loading lifetime.
    gated: bool,
    lifetime: bool,
    generation: u32,
    /// The bytes a gated play's exchange with the input side keeps.
    exchange: usize,
    /// The play skin's declared Closing duration.
    declared_close: Option<TimeUs>,
    /// Each abort close asked, with the clock time, and the view's
    /// Closing start and phase it found published.
    abort_closes: Vec<(i64, Option<TimeUs>, bool)>,
    /// Every abort close fails with this; an ordinary close still
    /// succeeds.
    abort_close_fails: Option<InputWaitError>,
    /// Final closes asked (`end_feedback`).
    end_feedbacks: usize,
    /// Each command taken as Closing's, with its clock time.
    closing_taken: Vec<(i64, Command)>,
    /// The audio output fails from this clock time on.
    output_fails_at: i64,
    /// The last device is gone from this clock time on.
    disconnected_at: i64,
    /// While set and not released, each result preparation stays pending,
    /// each poll one wait.
    prep_gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// What happens during the next preparation's first poll.
    in_first_poll: Option<FirstPoll>,
    /// What happens while the screen is built again during a result's
    /// preparation, once.
    in_rebuild: Option<FirstPoll>,
    /// Each preparation begun, with its clock time.
    preps: Vec<i64>,
    /// Each poll of a preparation: the clock time and its deadline.
    poll_deadlines: Vec<(i64, i64)>,
    /// The clock time of each screen preparation.
    screen_at: Vec<i64>,
    /// Preparations dropped unfinished.
    withdrawn: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Result commands pressed from another thread, as the window's keys:
    /// those pressed before a preparation begins are dropped there, as the
    /// player's result input drops what was queued before the result.
    menu_pressed: std::sync::Arc<std::sync::Mutex<Vec<game::MenuCommand>>>,
}

/// What happens during a preparation's first poll ([`Fake::in_first_poll`])
/// or a rebuild of the screen during it ([`Fake::in_rebuild`]).
#[derive(Clone, Copy, Debug)]
enum FirstPoll {
    /// This in-play command is pressed.
    Command(Command),
    /// This result command is pressed.
    Menu(game::MenuCommand),
    /// The window closes.
    Close,
    /// The screen fails.
    ScreenFails,
}

impl Fake {
    /// `what` happens now.
    fn happen(&mut self, what: FirstPoll) {
        match what {
            FirstPoll::Command(c) => self.commands.push((i64::MIN, c)),
            FirstPoll::Menu(c) => self.menu.push((i64::MIN, c)),
            FirstPoll::Close => self.quitting = true,
            FirstPoll::ScreenFails => self.screen_fails_at = self.clock.now().0,
        }
    }
}

/// [`Fake`]'s result preparation: dropped before it was found ready, it
/// counts as withdrawn.
#[derive(Default)]
struct FakePrep {
    withdrawn: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    polls: usize,
    ready: bool,
}

impl Drop for FakePrep {
    fn drop(&mut self) {
        if let Some(w) = &self.withdrawn
            && !self.ready
        {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl Fake {
    fn new(items: Vec<ScriptItem>, commands: &[(i64, Command)], quit_at: i64) -> Self {
        let mut items = items;
        items.push(ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(4), DeviceKind::Keyboard),
        });
        let mut sources = SourceSet::new(common::sources()).unwrap();
        let mut script = ScriptedInput::new(items);
        script.poll(TimeUs(CLOCK_START), &mut sources);
        Self {
            clock: VirtualClock::new(TimeUs(CLOCK_START)),
            sources,
            script,
            commands: commands.to_vec(),
            quit_at,
            origin: None,
            close_lag: 0,
            quit_in_prepare: false,
            quitting: false,
            screen_fails_at: i64::MAX,
            prepare_fails_from: usize::MAX,
            prepares: 0,
            origins: Vec::new(),
            sounds: None,
            sealed: false,
            tail_waits: 0,
            waits_since_seal: 0,
            in_close: None,
            cancels: 0,
            shows: 0,
            audio: None,
            reports: Vec::new(),
            offset_changes: Vec::new(),
            pending_offset: None,
            measured: Vec::new(),
            judged_at: Vec::new(),
            read_cost: 0,
            reads: std::cell::Cell::new(0),
            order: std::sync::Arc::default(),
            open_fails: None,
            close_fails: None,
            input_fails_at: i64::MAX,
            input_fails_in_prepare: false,
            quit_in_open: false,
            quit_in_close: false,
            failed: None,
            opens: 0,
            at: Vec::new(),
            shown_at: Vec::new(),
            screens: Vec::new(),
            idle_to_deadline: false,
            idle_waits: Vec::new(),
            menu: Vec::new(),
            pressed: std::sync::Arc::default(),
            steps: Vec::new(),
            attempts: Vec::new(),
            preparations: Vec::new(),
            output_failed: false,
            prepare_waits: 0,
            side: None,
            phases: Vec::new(),
            closed_phases: Vec::new(),
            gated: false,
            lifetime: false,
            generation: 0,
            exchange: 0,
            declared_close: None,
            abort_closes: Vec::new(),
            abort_close_fails: None,
            end_feedbacks: 0,
            closing_taken: Vec::new(),
            output_fails_at: i64::MAX,
            disconnected_at: i64::MAX,
            prep_gate: None,
            in_first_poll: None,
            in_rebuild: None,
            poll_deadlines: Vec::new(),
            screen_at: Vec::new(),
            preps: Vec::new(),
            withdrawn: std::sync::Arc::default(),
            menu_pressed: std::sync::Arc::default(),
        }
    }

    fn drain_sounds(&mut self) {
        if let Some(s) = &mut self.sounds {
            while s.commands().pop().is_ok() {}
        }
    }
}

impl Rig for Fake {
    type Reader = SourceReader;
    type ScreenError = SurfaceLost;
    type ResultPrep = FakePrep;

    fn now(&self) -> TimeUs {
        self.reads.set(self.reads.get() + self.read_cost);
        TimeUs(self.clock.now().0 + self.reads.get())
    }

    fn wait(&mut self) {
        self.reads.set(0);
        self.clock.advance(TimeUs(MS));
        // A run that never ends fails instead of hanging.
        assert!(
            self.clock.now().0 < CLOCK_START + 60_000_000,
            "the run did not end within a minute of its clock"
        );
        if self.sealed {
            self.waits_since_seal += 1;
        }
        if self.input_failure().is_none() {
            self.script.poll(self.clock.now(), &mut self.sources);
        }
        self.drain_sounds();
        let now = self.clock.now().0;
        while let Some(i) = self.offset_changes.iter().position(|&(at, _)| at <= now) {
            self.pending_offset = Some(self.offset_changes.remove(i).1);
        }
        while let Some(i) = self.at.iter().position(|(at, _)| *at <= now) {
            (self.at.remove(i).1)();
        }
    }

    fn idle_wait(&mut self, deadline: TimeUs) {
        let now = self.clock.now().0;
        self.idle_waits.push((now, deadline.0));
        if !self.idle_to_deadline {
            self.wait();
            return;
        }
        let next = self.at.iter().map(|(at, _)| *at).min().unwrap_or(i64::MAX);
        let to = deadline.0.min(next).max(now + 1);
        self.clock.advance(TimeUs(to - now - 1));
        // The last microsecond through `wait`, which runs what is due.
        self.reads.set(0);
        self.clock.advance(TimeUs(1));
        let now = self.clock.now().0;
        while let Some(i) = self.at.iter().position(|(at, _)| *at <= now) {
            (self.at.remove(i).1)();
        }
    }

    fn take(&mut self) -> Option<Command> {
        if let Some(c) = self.pressed.lock().unwrap().pop() {
            return Some(c);
        }
        let now = self.clock.now().0;
        let i = self.commands.iter().position(|&(at, _)| at <= now)?;
        Some(self.commands.remove(i).1)
    }

    fn take_menu(&mut self) -> Option<game::MenuCommand> {
        let pressed = self.menu_pressed.lock().unwrap().pop();
        if pressed.is_some() {
            return pressed;
        }
        let now = self.clock.now().0;
        if let Some(i) = self.menu.iter().position(|&(at, _)| at <= now) {
            return Some(self.menu.remove(i).1);
        }
        match self.take()? {
            Command::Retry => Some(game::MenuCommand::Retry),
            Command::Quit => Some(game::MenuCommand::Back),
            Command::Suspend => None,
        }
    }

    fn quitting(&self) -> bool {
        self.quitting || self.clock.now().0 >= self.quit_at
    }

    fn prepare_screen(&mut self) -> Result<(), SurfaceLost> {
        self.steps.push("screen");
        self.screen_at.push(self.clock.now().0);
        self.prepares += 1;
        if !self.preps.is_empty()
            && let Some(what) = self.in_rebuild.take()
        {
            self.happen(what);
        }
        if self.quit_in_prepare {
            // The window closes while the screen gets ready.
            self.quitting = true;
        }
        if self.input_fails_in_prepare {
            self.failed = Some(InputWaitError::Gone);
        }
        if self.prepares >= self.prepare_fails_from {
            return Err(SurfaceLost);
        }
        // Built again: no longer failed.
        if self.clock.now().0 >= self.screen_fails_at {
            self.screen_fails_at = i64::MAX;
        }
        Ok(())
    }

    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        self.steps.push("audio");
        self.audio.as_mut().map_or(Ok(()), AudioControl::ready)
    }

    /// Takes [`Self::prepare_waits`] waits, servicing the quit key and a
    /// close at each, as the player's rig does while the screen prepares.
    fn prepare_attempt(&mut self, loaded: &Loaded) -> Result<Preparation, SurfaceLost> {
        self.steps.push("attempt");
        self.attempts.push(loaded.attempt());
        for _ in 0..self.prepare_waits {
            self.wait();
            let now = self.clock.now().0;
            let quit = self
                .commands
                .iter()
                .any(|&(at, c)| at <= now && c == Command::Quit);
            if self.quitting() || quit {
                return Ok(Preparation::Cancelled);
            }
        }
        if self.preparations.is_empty() {
            Ok(Preparation::Ready)
        } else {
            Ok(self.preparations.remove(0))
        }
    }

    fn commit_attempt(&mut self) -> Result<(), SurfaceLost> {
        self.steps.push("commit");
        Ok(())
    }

    fn discard_attempt(&mut self) {
        self.steps.push("discard");
    }

    fn end_loading(&mut self) -> Result<(), SurfaceLost> {
        self.steps.push("end loading");
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        self.steps.push("open");
        self.opens += 1;
        if let Some(e) = self.input_failure().or(self.open_fails) {
            self.failed = Some(e);
            return Err(e);
        }
        if self.quit_in_open {
            self.quitting = true;
        }
        let now = self.clock.now();
        let origin = TimeUs(now.0 + lead_in.0);
        let recorded = DeviceSettings::default().keyboard.recorded();
        let devices: Vec<_> = self.sources.devices().map(|(d, _)| d).collect();
        for d in devices {
            self.sources.set_recorded(d, recorded.clone());
        }
        self.sources.gate_mut().reset();
        let start = self.sources.open_play(now, origin);
        self.origins.push(origin);
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
            axes: Vec::new(),
        })
    }

    fn gated(&self) -> bool {
        self.gated
    }

    fn gate_exchange(&self) -> usize {
        self.exchange
    }

    fn open_candidate(
        &mut self,
        lead_in: TimeUs,
        generation: u32,
    ) -> Result<Option<Opening<SourceReader>>, InputWaitError> {
        if self.sources.holding() {
            self.steps.push("deferred");
            return Ok(None);
        }
        self.generation = generation;
        self.open(lead_in).map(Some)
    }

    fn defer(&mut self) {
        self.steps.push(if self.lifetime {
            "resume loading"
        } else {
            "wait in preparing"
        });
    }

    fn holding(&self) -> bool {
        self.sources.holding()
    }

    /// The transport commits as soon as it is asked.
    fn candidate(&self) -> input::GateState {
        input::GateState {
            generation: self.generation,
            phase: input::GatePhase::Committed,
        }
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        self.origin = Some(origin);
        self.side = Some(screen);
        self.phases.push(Vec::new());
        self.sounds = Some(sounds);
        self.sealed = false;
        self.waits_since_seal = 0;
    }

    fn focus_lost(&self) -> bool {
        self.sources.gate().is_tripped()
    }

    fn connected(&self) -> bool {
        self.clock.now().0 < self.disconnected_at
    }

    fn audio_failure(&self) -> Option<audio::OutputError> {
        (self.output_failed || self.clock.now().0 >= self.output_fails_at)
            .then_some(audio::OutputError::DeviceLost)
    }

    fn declared_close(&self) -> Option<TimeUs> {
        self.declared_close
    }

    fn take_closing(&mut self) -> Option<Command> {
        let c = self.take()?;
        self.closing_taken.push((self.clock.now().0, c));
        Some(c)
    }

    fn abort_close(&mut self) -> Result<Closing, InputWaitError> {
        let (closing, playing) = self
            .side
            .as_mut()
            .map_or((None, false), |s| (s.view().closing, s.view().playing));
        self.abort_closes
            .push((self.clock.now().0, closing, playing));
        if let Some(e) = self.abort_close_fails {
            // A command that arrived while the request waited, then the
            // failure, which sticks as on a poisoned link.
            if let Some(c) = self.in_close.take() {
                self.commands.push((i64::MIN, c));
            }
            self.failed = Some(e);
            return Err(e);
        }
        self.close()
    }

    fn end_feedback(&mut self) -> Result<(), InputWaitError> {
        self.end_feedbacks += 1;
        Ok(())
    }

    fn screen_failed(&self) -> bool {
        self.clock.now().0 >= self.screen_fails_at
    }

    fn cancel_audio(&mut self) {
        self.cancels += 1;
        self.drain_sounds();
    }

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {
        panic!("this rig plays no practice round");
    }

    fn audio_done(&self) -> bool {
        self.sealed && self.waits_since_seal >= self.tail_waits
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        if let Some(side) = &mut self.side {
            self.closed_phases.push(side.view().playing);
        }
        // Batches the input thread received before it took the request.
        if let Some(c) = self.in_close.take() {
            self.commands.push((i64::MIN, c));
        }
        if self.quit_in_close {
            self.quitting = true;
        }
        // A thread that is gone pushes nothing more.
        let gone = self.close_fails.filter(|&e| e == InputWaitError::Gone);
        if let Some(e) = self.input_failure().or(gone) {
            self.failed = Some(e);
            return Err(e);
        }
        let lag = TimeUs(self.clock.now().0 + self.close_lag);
        self.script.poll(lag, &mut self.sources);
        // One that does not answer may still push while the close waits.
        if let Some(e) = self.close_fails {
            self.failed = Some(e);
            return Err(e);
        }
        let closing = Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: self.sources.axis_out_of_range().collect(),
        };
        self.sources.close_recording();
        Ok(closing)
    }

    fn input_failure(&self) -> Option<InputWaitError> {
        self.failed
            .or((self.clock.now().0 >= self.input_fails_at).then_some(InputWaitError::Gone))
    }

    fn ended(&mut self, report: &PlayReport) {
        self.order.lock().unwrap().push("ended");
        self.reports.push(report.clone());
    }

    fn play_over(&mut self) {
        self.order.lock().unwrap().push("play over");
    }

    fn stepped(&mut self, _aborted: bool) {
        if let (Some(side), Some(origin), Some(phases)) =
            (&mut self.side, self.origin, self.phases.last_mut())
        {
            phases.push((self.clock.now().0 - origin.0, side.view().playing));
        }
    }

    fn measured(
        &mut self,
        advance: &game::Advance,
        events: &[eval::JudgeEvent],
        delivered: TimeUs,
    ) {
        if self.measured.len() < self.origins.len() {
            self.measured.push(Vec::new());
            self.judged_at.push(Vec::new());
        }
        self.measured.last_mut().unwrap().push(*advance);
        for e in events {
            if let (JudgeEventKind::Judged { .. }, Some(o)) = (e.kind, e.origin) {
                self.judged_at
                    .last_mut()
                    .unwrap()
                    .push((e.seq, o.0, delivered.0));
            }
        }
    }

    fn begin_result(&mut self, _report: &PlayReport, _screen: &ResultScreen) -> FakePrep {
        self.preps.push(self.clock.now().0);
        self.menu_pressed.lock().unwrap().clear();
        FakePrep {
            withdrawn: Some(std::sync::Arc::clone(&self.withdrawn)),
            polls: 0,
            ready: false,
        }
    }

    fn poll_result(&mut self, prep: &mut FakePrep, deadline: TimeUs) -> game::Polled {
        prep.polls += 1;
        self.poll_deadlines.push((self.clock.now().0, deadline.0));
        if prep.polls == 1
            && let Some(what) = self.in_first_poll.take()
        {
            self.happen(what);
        }
        let held = self
            .prep_gate
            .as_ref()
            .is_some_and(|g| !g.load(std::sync::atomic::Ordering::SeqCst));
        if held {
            self.wait();
            return game::Polled::Pending;
        }
        prep.ready = true;
        game::Polled::Ready
    }

    fn show_end(&mut self, _report: &PlayReport, screen: &ResultScreen) {
        self.shows += 1;
        self.shown_at.push(self.clock.now().0);
        self.screens.push((self.clock.now().0, screen.clone()));
    }

    fn before_play(&mut self, settings: &mut Settings) {
        self.steps.push("before play");
        if let Some(o) = self.pending_offset.take() {
            settings.device.input_offset_us = o;
        }
    }
}

/// The judgments in `report`'s live stream: (note, judgment, deviation).
fn judged(report: &PlayReport) -> Vec<(usize, Judgment, Option<i64>)> {
    report
        .live
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                note,
                judgment,
                deviation,
                ..
            } => Some((note, judgment, deviation.map(|d| d.0))),
            _ => None,
        })
        .collect()
}

fn received(report: &PlayReport) -> Vec<(i64, u32, i32)> {
    report
        .record
        .as_ref()
        .unwrap()
        .events
        .iter()
        .map(|e| match e.kind {
            InputKind::Control { control, value } => (e.received.0, control.0, value),
            InputKind::Disconnected => (e.received.0, 0, -1),
        })
        .collect()
}

/// Play 1 opens at 7.3 s (origin 8.3 s) and is retried at 9.5 s; play 2
/// opens there (origin 10.5 s). Presses land 3 ms after each note on the
/// clock, which the 3 ms input offset turns into exact hits.
#[test]
fn two_plays_at_nonzero_origins_with_an_input_offset() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        key(11_503_000, Z, 1),
        key(11_553_000, Z, 0),
        key(12_003_000, Z, 1),
        key(12_053_000, Z, 0),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    assert_eq!(rig.origins, [TimeUs(8_300_000), TimeUs(10_500_000)]);
    let [one, two] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    assert_eq!(two.end, PlayEnd::Completed);
    for r in [one, two] {
        assert_eq!(
            r.record.as_ref().unwrap().start.opened_at,
            TimeUs(-1_000_000)
        );
    }
    assert_eq!(received(one), [(1_003_000, Z.0, 1), (1_053_000, Z.0, 0)]);
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
    assert_eq!(
        received(two),
        [
            (1_003_000, Z.0, 1),
            (1_053_000, Z.0, 0),
            (1_503_000, Z.0, 1),
            (1_553_000, Z.0, 0)
        ]
    );
    assert_eq!(
        judged(two),
        [
            (0, Judgment::Perfect, Some(0)),
            (1, Judgment::Perfect, Some(0))
        ]
    );
}

/// Z is pressed for play 1's first note and still held when retry
/// arrives: play 2 starts with it held, its release judges nothing, and the
/// next press is judged.
#[test]
fn retry_with_a_key_held_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        // Released in play 2's READY state (play time -0.9 s).
        key(9_600_000, Z, 0),
        key(11_503_000, Z, 1),
        key(11_553_000, Z, 0),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let two = &rig.reports[1];
    let rec = two.record.as_ref().unwrap();
    assert_eq!(rec.start.held, [(SourceId(0), Z, 1)]);
    assert_eq!(received(two)[0], (-900_000, Z.0, 0));
    // Nothing from the release; the press hits note 0, note 1 is missed.
    assert_eq!(
        judged(two),
        [(0, Judgment::Perfect, Some(0)), (1, Judgment::Poor, None)]
    );
    assert!(
        two.live
            .iter()
            .all(|e| !matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
    );
}

/// Input that reaches the queues after the abort, before recording closes, is
/// kept in the record and not evaluated (design.md "Play Lifecycle").
#[test]
fn abort_and_close_drain_late_input_into_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        // 2 ms after the retry, inside the input side's last batches.
        key(9_502_000, X, 1),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 9_500_000);
    rig.close_lag = 5 * MS;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_eq!(
        received(one),
        [
            (1_003_000, Z.0, 1),
            (1_053_000, Z.0, 0),
            (1_202_000, X.0, 1)
        ]
    );
    // Finalized before the late press, which nothing judged.
    let rec = one.record.as_ref().unwrap();
    assert!(rec.status.finalized < TimeUs(1_202_000));
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
}

/// The window closes while the screen is readied for the first play: no
/// recording opens and nothing is saved.
#[test]
fn a_close_during_startup_quits_before_recording() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(vec![key(9_303_000, Z, 1)], &[], i64::MAX);
    rig.quit_in_prepare = true;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.prepares, 1);
    assert!(rig.origins.is_empty());
    assert!(rig.reports.is_empty());
    assert!(!dir.path().join("records").exists());
}

/// The screen fails during play 1: the play ends there and is saved; the
/// retry's preparation cannot build the screen again, which ends the run
/// with that error.
#[test]
fn a_screen_failure_ends_the_play_and_a_failed_recovery_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_600_000, Command::Retry)], i64::MAX);
    rig.screen_fails_at = 9_400_000;
    rig.prepare_fails_from = 2;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(&ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::ScreenFailed));
    assert!(matches!(one.saved, Some(Ok(_))));
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
}

fn incomplete(report: &PlayReport) -> replay::Incomplete {
    report.record.as_ref().unwrap().status.incomplete
}

/// A suspend and a quit before the same step, after the first note was
/// finalized: the quit ends the play and the run, and the record is still
/// marked suspended.
#[test]
fn a_suspend_with_a_quit_in_one_step_marks_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Suspend)], 9_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
    assert!(incomplete(one).suspend);
}

/// A suspend that arrives while recording closes after a quit.
#[test]
fn a_suspend_during_the_close_marks_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 9_500_000);
    rig.in_close = Some(Command::Suspend);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert!(incomplete(one).suspend);
}

/// The screen fails during a completed play's audio tail: the tail is cut
/// and the play stays complete; the end screen's wait builds the screen
/// again, which fails, so the run ends with that error.
#[test]
fn a_screen_failure_in_the_tail_cuts_it_and_a_failed_rebuild_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], i64::MAX);
    // Complete at play time 1.683 s (9.983 s on the clock), with a 200 ms
    // tail; the screen fails inside it.
    rig.tail_waits = 200;
    rig.screen_fails_at = 10_100_000;
    rig.prepare_fails_from = 2;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(matches!(ran, Err(LiveError::Screen(_))), "{ran:?}");
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::ScreenFailed));
    assert_eq!(
        one.record.as_ref().unwrap().status.end,
        replay::EndStatus::PlayedToEnd
    );
    assert_eq!(rig.prepares, 2);
}

/// The screen fails while the end screen waits: it is built again, with
/// an acknowledgement, and the end screen shows again.
#[test]
fn a_screen_failure_on_the_end_screen_is_rebuilt_and_shown_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 10_600_000);
    rig.screen_fails_at = 10_400_000;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.reports[0].end, PlayEnd::Completed);
    assert_eq!(rig.reports[0].tail_cut, None);
    assert_eq!((rig.prepares, rig.shows), (2, 2));
}

/// The gauges change in the preset file during the first play, before a
/// retry: each play's report carries the preset it was evaluated under.
#[test]
fn each_report_carries_the_preset_its_play_used() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let mut renamed = rules::tapestry();
    for g in &mut renamed.gauges {
        g.id = format!("{}-renamed", g.id);
    }
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 12_500_000);
    let edited = renamed.clone();
    rig.at
        .push((8_000_000, Box::new(move || rewrite_preset(&path, &edited))));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let ids = |r: &PlayReport| {
        r.preset
            .gauges
            .iter()
            .map(|g| g.id.clone())
            .collect::<Vec<_>>()
    };
    let old: Vec<_> = rules::tapestry()
        .gauges
        .iter()
        .map(|g| g.id.clone())
        .collect();
    let new: Vec<_> = renamed.gauges.iter().map(|g| g.id.clone()).collect();
    assert_ne!(old, new);
    assert_eq!(ids(&rig.reports[0]), old);
    assert_eq!(ids(&rig.reports[1]), new);
}

fn status(report: &PlayReport) -> replay::Status {
    report.record.as_ref().unwrap().status
}

/// A retry that arrives as a completed play's recording closes, its audio
/// already done: the run retries, and the record stays complete.
#[test]
fn a_retry_during_close_with_the_audio_done_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_500_000);
    rig.in_close = Some(Command::Retry);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    // Retried without an end screen; the quit comes during play 2.
    assert_eq!(rig.reports.len(), 2);
    assert_eq!(rig.shows, 0);
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::Retry));
    // Complete at the step whose watermark passed the last miss deadline,
    // 1.68 s after the 3 ms offset.
    let s = status(one);
    assert_eq!(s.end, replay::EndStatus::PlayedToEnd);
    assert!(s.finalized >= TimeUs(1_680_000));
}

/// A retry that arrives as an aborted play's recording closes is the
/// run's next step: it retries without the end screen.
#[test]
fn a_retry_during_close_after_an_abort_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Suspend)], 12_500_000);
    rig.in_close = Some(Command::Retry);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.reports[0].end, PlayEnd::Aborted(AbortCause::Suspend));
    assert_eq!(rig.reports.len(), 2);
    assert_eq!(rig.origins.len(), 2);
}

/// A suspend that arrives as a completed play's recording closes, with a
/// long tail still to play: the tail is cut and the record marked.
#[test]
fn a_suspend_during_close_cuts_a_long_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_500_000);
    rig.tail_waits = 2_000;
    rig.in_close = Some(Command::Suspend);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::Suspend));
    assert_eq!(rig.cancels, 1);
    let s = status(one);
    assert_eq!(s.end, replay::EndStatus::PlayedToEnd);
    assert!(s.incomplete.suspend);
}

/// An audio control whose worker is gone: dropped before it took any play,
/// or after it took one and returned its retired queue.
fn gone_audio(took_a_play: bool) -> game::AudioControl {
    let chart = chart();
    let engine = audio::AudioEngine::new(
        common::bank(&chart),
        &chart.bgm,
        common::RATE,
        common::PERIOD,
        audio::MAX_VOICES,
        0,
    )
    .unwrap();
    let (mut control, mut worker) = game::audio_control(engine);
    if took_a_play {
        let mut out = audio::SimDevice::new(audio::SimConfig::steady(
            common::RATE,
            common::PERIOD,
            TimeUs(0),
        ));
        let (_sink, queue) = game::audio_link_at(4, TimeUs(0));
        control.play(TimeUs(0), queue).unwrap();
        worker.step(&mut out).unwrap();
        // The switch was taken and acknowledged.
        control.ready().unwrap();
    }
    drop(worker);
    control
}

#[test]
fn a_gone_audio_thread_fails_the_preparation() {
    for took_a_play in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Fake::new(Vec::new(), &[], i64::MAX);
        rig.audio = Some(gone_audio(took_a_play));
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        assert!(
            matches!(&ran, Err(LiveError::Audio(AudioWaitError::Gone))),
            "{took_a_play}: {ran:?}"
        );
        assert!(rig.origins.is_empty(), "{took_a_play}");
    }
}

/// The input offset is changed from 3 ms to 8 ms during play 1, which is
/// then retried: play 1 is evaluated and recorded with 3 ms, play 2 with
/// 8 ms. A change during play 2 waits for a play after it.
#[test]
fn input_offset_change_applies_next_play_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    // Presses 3 ms after each note on the clock, as in
    // `two_plays_at_nonzero_origins_with_an_input_offset`.
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        key(11_503_000, Z, 1),
        key(11_553_000, Z, 0),
        key(12_003_000, Z, 1),
        key(12_053_000, Z, 0),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    rig.offset_changes = vec![(9_400_000, 8_000), (11_600_000, 21_000)];
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let [one, two] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    let offset = |r: &PlayReport| {
        let sources = &r.record.as_ref().unwrap().devices.sources;
        assert_eq!(sources.len(), 1);
        sources[0].profile.input_offset
    };
    assert_eq!(offset(one), TimeUs(OFFSET));
    assert_eq!(offset(two), TimeUs(8_000));
    // Play 1 hits exactly with 3 ms. Play 2's presses, 3 ms late on the
    // clock, are 8 − 3 = 5 ms early with 8 ms.
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
    assert_eq!(
        judged(two),
        [
            (0, Judgment::Perfect, Some(-5_000)),
            (1, Judgment::Perfect, Some(-5_000))
        ]
    );
    // The change made during play 2 is still waiting.
    assert_eq!(rig.pending_offset, Some(21_000));
}

/// Every logic step of a play reaches the rig's measurement (design.md "Latency
/// Figures"): each step that advanced has a finalization delay, and input to
/// judgment appears exactly in the steps that judged a press, never in the
/// others. The aborted first play's steps end at its abort.
#[test]
fn every_step_is_measured() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        key(11_503_000, Z, 1),
        key(11_553_000, Z, 0),
        key(12_003_000, Z, 1),
        key(12_053_000, Z, 0),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.measured.len(), 2);
    for (play, steps) in rig.measured.iter().enumerate() {
        assert!(!steps.is_empty());
        let advanced = steps.iter().filter(|a| a.watermark.is_some()).count();
        let delays = steps
            .iter()
            .filter_map(|a| a.finalization_delay)
            .collect::<Vec<_>>();
        assert_eq!(delays.len(), advanced);
        assert!(delays.iter().all(|d| d.0 >= 0));
        // Play 1 judged one press before the retry, play 2 two presses.
        let judging = steps
            .iter()
            .filter(|a| a.judgment_latency.is_some())
            .count();
        assert_eq!(judging, [1, 2][play], "play {}", play + 1);
        // Received at a 1 ms boundary and judged within the next steps.
        for a in steps.iter().filter_map(|a| a.judgment_latency) {
            assert!((0..=3 * MS).contains(&a.0), "{a:?}");
        }
    }
    // The aborted play's last step is its abort.
    assert!(rig.measured[0].last().unwrap().aborted.is_some());
}

/// The chart of `loaded` with a second note on lane 3 at the time of the
/// first, so one step can judge two presses.
fn chord_loaded(dir: &Path) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let settings = tapestry_settings(&root);
    let chart_path = dir.join("chord.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// Input to judgment is one sample per judgment, timed when the step has
/// delivered it: two presses 300 µs apart judged in one step give two
/// samples 300 µs apart, and each reading of the clock costing 70 µs puts
/// the delivery 140 µs past the step's whole-millisecond clock (two
/// readings), not 70 µs (the reading the step started with).
#[test]
fn input_to_judgment_is_timed_per_judgment_at_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = chord_loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_303_300, X, 1),
        key(9_353_000, Z, 0),
        key(9_353_000, X, 0),
    ];
    let mut rig = Fake::new(items, &[], 10_500_000);
    rig.read_cost = 70;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let origin = rig.origins[0].0;
    let judged = &rig.judged_at[0];
    assert_eq!(judged.len(), 2, "{judged:?}");
    let (a, b) = (judged[0], judged[1]);
    assert_eq!(a.2, b.2, "one step");
    let mut origins = [a.1, b.1];
    origins.sort_unstable();
    assert_eq!(origins, [1_003_000, 1_003_300]);
    assert_eq!((a.2 + origin - 140) % MS, 0, "delivered at {}", a.2);
}

/// A record sink that notes when it saved, in the lifecycle order.
struct NotingSink(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

impl game::RecordSink for NotingSink {
    fn save(&mut self, _rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        self.0.lock().unwrap().push("saved");
        Ok(game::RecordRef {
            path: "noted.tprl".into(),
        })
    }
}

/// The play is over, and its measurement stops, before it is saved, however
/// long saving takes: each play goes play over, saved, ended.
#[test]
fn the_play_is_over_before_it_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let settings = tapestry_settings(&root);
    let chart_path = dir.path().join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let order = std::sync::Arc::default();
    let parts = Parts {
        ingest: Box::new(Built(chart())),
        records: Some(Box::new(NotingSink(std::sync::Arc::clone(&order)))),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    let mut loaded = Loaded::load(root, settings, &chart_path, config, parts).unwrap();
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    rig.order = std::sync::Arc::clone(&order);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let order = order.lock().unwrap().clone();
    assert_eq!(
        order,
        ["play over", "saved", "ended", "play over", "saved", "ended"]
    );
}

/// T1, T2: the input thread is gone, or does not answer, when recording
/// opens: no play exists, so the run ends with that cause, nothing is
/// saved, and the play's number is given back.
#[test]
fn an_input_thread_gone_at_the_open_ends_the_run_with_its_cause() {
    for cause in [InputWaitError::Gone, InputWaitError::Timeout] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Fake::new(vec![key(9_303_000, Z, 1)], &[], i64::MAX);
        rig.open_fails = Some(cause);
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        assert!(
            matches!(&ran, Err(LiveError::Input(e)) if *e == cause),
            "{cause:?}: {ran:?}"
        );
        assert_eq!(rig.opens, 1, "{cause:?}");
        assert!(rig.reports.is_empty(), "{cause:?}");
        assert!(!dir.path().join("records").exists(), "{cause:?}");
        assert_eq!(loaded.attempt().number, 0, "{cause:?}");
    }
}

/// The window closes while recording opens: the quit is latched and the
/// first step aborts the play before it advances, so nothing is recorded.
#[test]
fn a_quit_in_the_open_saves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(vec![key(9_303_000, Z, 1)], &[], i64::MAX);
    rig.quit_in_open = true;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert!(one.record.is_none());
    assert!(!dir.path().join("records").exists());
}

/// T3: the close fails after a quit: the record is still saved through the
/// normal path, marked incomplete (input failed), with what the play took
/// and none of the closing (the arrival during play is unknown); the run
/// ends with the input cause.
#[test]
fn a_failed_close_saves_the_record_marked_input_failed() {
    for cause in [InputWaitError::Gone, InputWaitError::Timeout] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let items = vec![
            key(9_303_000, Z, 1),
            key(9_353_000, Z, 0),
            ScriptItem {
                at: TimeUs(9_400_000),
                action: Action::Connect(DeviceId(5), DeviceKind::Keyboard),
            },
        ];
        let mut rig = Fake::new(items, &[], 9_500_000);
        rig.close_fails = Some(cause);
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        assert!(
            matches!(&ran, Err(LiveError::Input(e)) if *e == cause),
            "{cause:?}: {ran:?}"
        );
        let [one] = &rig.reports[..] else {
            panic!("{} plays", rig.reports.len());
        };
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
        assert_eq!(one.input_error, Some(cause));
        assert!(matches!(one.saved, Some(Ok(_))));
        let rec = one.record.as_ref().unwrap();
        assert_eq!(rec.status.end, replay::EndStatus::EndedMidway);
        assert!(rec.status.incomplete.input_failed);
        assert!(rec.arrivals.is_empty());
        assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
        assert_eq!(rig.shows, 0);
    }
}

/// A3: a failed close skips the drain after it: input that reached the
/// queues after the play's last step (pushed by a thread that does not
/// answer) is not in the record, which holds exactly what the play took.
#[test]
fn a_failed_close_keeps_only_what_the_play_took() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        // Inside the batches pushed while the close waits.
        key(9_502_000, X, 1),
    ];
    let mut rig = Fake::new(items, &[], 9_500_000);
    rig.close_lag = 5 * MS;
    rig.close_fails = Some(InputWaitError::Timeout);
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(matches!(ran, Err(LiveError::Input(_))), "{ran:?}");
    let one = &rig.reports[0];
    assert_eq!(received(one), [(1_003_000, Z.0, 1), (1_053_000, Z.0, 0)]);
    assert!(incomplete(one).input_failed);
}

/// T4: a retry after a failed close ends the run with the input cause,
/// before another preparation or open.
#[test]
fn a_retry_after_a_failed_close_ends_the_run_with_the_input_cause() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], i64::MAX);
    rig.close_fails = Some(InputWaitError::Gone);
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    assert_eq!(status(one).end, replay::EndStatus::EndedMidway);
    assert!(incomplete(one).input_failed);
    assert_eq!((rig.opens, rig.prepares), (1, 1));
}

/// A4: the input thread fails while the screen is prepared and the window
/// closes there too: the run ends with the input cause, not as a quit.
#[test]
fn a_quit_during_preparation_with_a_failed_input_thread_ends_the_run_with_its_cause() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(vec![key(9_303_000, Z, 1)], &[], i64::MAX);
    rig.quit_in_prepare = true;
    rig.input_fails_in_prepare = true;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    assert_eq!(rig.opens, 0);
    assert!(rig.reports.is_empty());
}

/// T5: the input thread is gone during play, after the first note was
/// judged: the play aborts at the last watermark finalized before it, and
/// its saved record, marked incomplete, re-evaluates to the live stream.
/// The end screen shows and the run ends with the input cause at the quit.
#[test]
fn an_input_failure_in_play_aborts_at_the_last_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        key(9_803_000, Z, 1),
    ];
    let mut rig = Fake::new(items, &[], 10_500_000);
    rig.input_fails_at = 9_600_000;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::InputFailed));
    assert_eq!(one.input_error, Some(InputWaitError::Gone));
    let last = rig.measured[0]
        .iter()
        .filter_map(|a| a.watermark)
        .next_back()
        .unwrap();
    let s = status(one);
    assert_eq!(s.finalized, last);
    assert!(last < TimeUs(9_600_000 - rig.origins[0].0));
    assert!(s.incomplete.input_failed);
    assert!(one.result.as_ref().unwrap().matches_live);
    assert_eq!(judged(one), [(0, Judgment::Perfect, Some(0))]);
    assert_eq!(rig.shows, 1);
}

/// T6: the close of a completed play fails: the play stays complete, its
/// tail is cut with the input cause, and its record is marked.
#[test]
fn a_failed_close_after_completion_keeps_the_play_complete() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    rig.tail_waits = 200;
    rig.close_fails = Some(InputWaitError::Timeout);
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Timeout))),
        "{ran:?}"
    );
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::InputFailed));
    assert_eq!(rig.cancels, 1);
    let s = status(one);
    assert_eq!(s.end, replay::EndStatus::PlayedToEnd);
    assert!(s.incomplete.input_failed);
    assert_eq!(rig.shows, 1);
}

/// T7: the window closes while the close of a completed play fails: the
/// record is saved, the end screen never shows, and the run ends with the
/// input cause.
#[test]
fn a_quit_during_a_failed_close_ends_the_run_with_the_input_cause() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], i64::MAX);
    rig.tail_waits = 200;
    rig.close_fails = Some(InputWaitError::Timeout);
    rig.quit_in_close = true;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Timeout))),
        "{ran:?}"
    );
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::InputFailed));
    assert!(matches!(one.saved, Some(Ok(_))));
    assert!(incomplete(one).input_failed);
    assert_eq!(rig.shows, 0);
}

/// A5, A7: the input thread is gone during the audio tail, after a close
/// that succeeded: the tail is cut with the input cause, the closed record
/// stays complete and unmarked, and the run ends with the cause at the
/// quit on the end screen.
#[test]
fn an_input_failure_in_the_tail_cuts_it_and_keeps_the_record_complete() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    rig.tail_waits = 2_000;
    rig.input_fails_at = 10_100_000;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::InputFailed));
    assert_eq!(one.input_error, Some(InputWaitError::Gone));
    let s = status(one);
    assert_eq!(s.end, replay::EndStatus::PlayedToEnd);
    assert!(!s.incomplete.any());
    assert_eq!(rig.shows, 1);
}

/// T8, A7: the input thread is gone while the end screen shows: nothing
/// changes there (the tail was over, the record closed and unmarked), and
/// the retry ends the run with the input cause, before another open.
#[test]
fn an_input_failure_on_the_end_screen_ends_the_run_at_the_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(10_600_000, Command::Retry)], i64::MAX);
    rig.input_fails_at = 10_400_000;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, None);
    assert_eq!(one.input_error, None);
    assert!(!incomplete(one).input_failed);
    assert_eq!((rig.opens, rig.shows), (1, 1));
}

/// The input thread is gone on the end screen, and the screen fails as the
/// retry arrives, with no rebuild possible: the retry is taken first, so the
/// run ends with the input cause without another preparation of the screen.
#[test]
fn a_retry_with_a_failed_input_thread_and_screen_ends_with_the_input_cause() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(10_600_000, Command::Retry)], i64::MAX);
    rig.input_fails_at = 10_400_000;
    rig.screen_fails_at = 10_600_000;
    rig.prepare_fails_from = 2;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    assert_eq!((rig.prepares, rig.opens, rig.shows), (1, 1, 1));
}

/// The tapestry preset named `mine`, as `presets/mine.toml` loads it.
fn mine(preset: rules::RulePreset) -> rules::RulePreset {
    rules::RulePreset {
        id: "mine".to_owned(),
        ..preset
    }
}

/// The tapestry preset whose normal gauge gains nothing on a PERFECT: a
/// play re-evaluated under it ends with another gauge value.
fn flat_normal() -> rules::RulePreset {
    let mut p = rules::tapestry();
    p.gauges[0].change.perfect = rules::GaugeDelta::Fixed(rules::GaugeValue(0));
    p
}

/// The result screen last shown at or before clock time `at`.
fn shown(rig: &Fake, at: i64) -> &ResultScreen {
    &rig.screens
        .iter()
        .rev()
        .find(|(t, _)| *t <= at)
        .unwrap_or_else(|| panic!("no result by {at}"))
        .1
}

/// The summary a result screen shows, which must be ready.
fn ready(screen: &ResultScreen) -> game::ViewSummary {
    match screen.status() {
        ViewStatus::Ready(s) => s.clone(),
        other => panic!("{other:?}"),
    }
}

/// The viewed entry's id.
fn viewed(screen: &ResultScreen) -> EntryId {
    screen.viewed().unwrap().id.clone()
}

fn user(name: &str) -> EntryId {
    EntryId::UserFile(name.to_owned())
}

/// The chosen user preset's file changes while the result shows: the next
/// check, within one poll interval, reloads it, and the view, which
/// follows the chosen file, shows the play under it as a what-if; the
/// play's own preset is pinned as played, and the play's report keeps the
/// preset it played.
#[test]
fn changed_user_preset_reloads_within_a_poll() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    let changed = 11_000_000;
    let edit = path.clone();
    rig.at.push((
        changed,
        Box::new(move || rewrite_preset(&edit, &flat_normal())),
    ));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    assert!(rig.shown_at[0] < changed, "{:?}", rig.shown_at);
    let (at, after) = rig
        .screens
        .iter()
        .find(|(_, s)| s.notices().next().is_some())
        .unwrap();
    assert!(
        (changed..=changed + game::POLL_INTERVAL.0).contains(at),
        "reloaded at {at}"
    );
    assert_eq!(viewed(after), user("mine"));
    assert_eq!(
        after.viewed().unwrap().hash,
        Some(replay::preset_hash(&mine(flat_normal())))
    );
    let what_if = ready(after);
    assert!(!what_if.as_played);
    let played = &rig.reports[0];
    assert_eq!(played.preset, mine(rules::tapestry()));
    let live = played.result.as_ref().unwrap().evaluation.as_ref().unwrap();
    assert_ne!(what_if.gauges[0].unwrap().value, live.gauges[0].value);
    assert_eq!(what_if.tally, live.tally);
    assert_eq!(
        after.notices().cloned().collect::<Vec<_>>(),
        [
            Notice::Reloaded("mine".to_owned()),
            Notice::NextPlayUsesIt("mine".to_owned())
        ]
    );
    let pinned = EntryId::AsPlayed(replay::preset_hash(&mine(rules::tapestry())));
    let ids: Vec<EntryId> = after.entries().iter().map(|e| e.id.clone()).collect();
    let at_pin = ids.iter().position(|id| *id == pinned).unwrap();
    assert_eq!(ids[at_pin + 1], user("mine"), "{ids:?}");
    // Chosen for the next play; this chart's plays so far keep theirs.
    assert_eq!(loaded.shelf().chosen(), &mine(flat_normal()));
    assert_eq!(loaded.preset(), &mine(rules::tapestry()));
}

/// The chosen user preset's file changes to text that does not load: the
/// result keeps showing the play under the last good version with the
/// file's error, a notice names it, and the retry plays the previous
/// preset.
#[test]
fn failed_reload_keeps_the_previous_preset() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(11_500_000, Command::Retry)], 15_000_000);
    let edit = path.clone();
    rig.at.push((
        11_000_000,
        Box::new(move || common::rewrite(&edit, "windows = [")),
    ));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let failed = shown(&rig, 11_400_000);
    let e = failed.file_error().unwrap().clone();
    assert_eq!(e.path, path);
    assert!(
        matches!(
            &e.cause,
            game::PresetFileCause::Preset(game::PresetError::Parse(_))
        ),
        "{e:?}"
    );
    assert_eq!(
        failed.notices().cloned().collect::<Vec<_>>(),
        [Notice::Failed(e.clone())]
    );
    assert_eq!(viewed(failed), user("mine"));
    assert!(ready(failed).as_played);
    assert_eq!(rig.reports.len(), 2);
    for r in &rig.reports {
        assert_eq!(r.preset, mine(rules::tapestry()));
        assert_eq!(
            r.record.as_ref().unwrap().preset.sha256,
            replay::preset_hash(&mine(rules::tapestry()))
        );
    }
    // Already shown on the result: the retry does not report it again.
    assert_eq!(rig.reports[1].reload_error, None);
    let entry = loaded
        .shelf()
        .entries()
        .into_iter()
        .find(|e| e.name == "mine")
        .unwrap();
    assert_eq!(entry.error, Some(&e));
    assert_eq!(entry.preset, Some(&mine(rules::tapestry())));
}

/// A chosen reload, then a failure, then a recovery to the same rules,
/// each within a poll: the reload moves the view to the new version with
/// its notices, the failure keeps it with the file's error, and the
/// recovery clears the error and its notice.
#[test]
fn result_screen_shows_reload_and_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 13_000_000);
    for (at, text) in [
        (11_000_000, Some(game::preset_to_toml(&flat_normal()))),
        (11_500_000, None),
        (12_000_000, Some(game::preset_to_toml(&flat_normal()))),
    ] {
        let edit = path.clone();
        rig.at.push((
            at,
            Box::new(move || common::rewrite(&edit, text.as_deref().unwrap_or("windows = ["))),
        ));
    }
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let new_hash = Some(replay::preset_hash(&mine(flat_normal())));
    let reloaded = shown(&rig, 11_400_000);
    assert_eq!(viewed(reloaded), user("mine"));
    assert_eq!(reloaded.viewed().unwrap().hash, new_hash);
    assert!(!ready(reloaded).as_played);
    assert_eq!(reloaded.file_error(), None);
    assert_eq!(
        reloaded.notices().cloned().collect::<Vec<_>>(),
        [
            Notice::Reloaded("mine".to_owned()),
            Notice::NextPlayUsesIt("mine".to_owned())
        ]
    );
    let failed = shown(&rig, 11_900_000);
    let e = failed.file_error().unwrap().clone();
    assert_eq!(e.path, path);
    assert_eq!(viewed(failed), user("mine"));
    assert_eq!(ready(failed), ready(reloaded));
    assert_eq!(failed.notices().last(), Some(&Notice::Failed(e)));
    let recovered = shown(&rig, 12_900_000);
    assert_eq!(recovered.file_error(), None);
    assert!(
        !recovered.notices().any(|n| matches!(n, Notice::Failed(_))),
        "{:?}",
        recovered.notices().collect::<Vec<_>>()
    );
    assert_eq!(ready(recovered), ready(reloaded));
    // Each state was drawn once, by the poll that found it.
    assert_eq!(rig.screens.len(), 4, "{:?}", rig.shown_at);
}

/// A user preset `name` holding `preset`, loaded under that name.
fn named(name: &str, preset: rules::RulePreset) -> rules::RulePreset {
    rules::RulePreset {
        id: name.to_owned(),
        ..preset
    }
}

/// Writes `presets/<name>.toml` in `dir`, holding `preset`.
fn write_named(dir: &Path, name: &str, preset: &rules::RulePreset) -> PathBuf {
    let presets = dir.join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let path = presets.join(format!("{name}.toml"));
    std::fs::write(&path, game::preset_to_toml(preset)).unwrap();
    path
}

/// The tapestry preset with BAD's late edge at 220 ms, whose final
/// deadline comes after the tapestry preset's, and a normal gauge that
/// clears at its floor.
fn late() -> rules::RulePreset {
    let mut p = rules::tapestry();
    for t in [&mut p.tables.keys, &mut p.tables.scratch] {
        t.windows.bad.late = TimeUs(220 * MS);
    }
    p.gauges[0].kind = rules::GaugeKind::EndThreshold {
        clear: p.gauges[0].min,
    };
    p
}

/// The tapestry preset with BAD's late edge at 100 ms, whose final deadline
/// comes before the tapestry preset's, and one gauge that starts at its
/// clear border and never changes: it clears whenever evaluation completes.
fn narrow() -> rules::RulePreset {
    let mut p = rules::tapestry();
    for t in [&mut p.tables.keys, &mut p.tables.scratch] {
        t.windows.bad.late = TimeUs(110 * MS);
    }
    let zero = rules::GaugeDelta::Fixed(rules::GaugeValue(0));
    let mut g = p.gauges[0].clone();
    g.kind = rules::GaugeKind::EndThreshold { clear: g.start };
    g.change = rules::PerJudgment {
        perfect: zero,
        great: zero,
        good: zero,
        bad: zero,
        poor: zero,
    };
    g.empty_poor = zero;
    g.low_damage = Vec::new();
    p.gauges = vec![g];
    p
}

/// Played under the built-in tapestry preset with the user presets `late`
/// and `narrow` beside the built-ins: the view opens as played, equal to the
/// live tally, steps through the other built-ins to `late`, a completed
/// what-if with no input after the recorded watermark that clears, and wraps
/// both ways. An aborted play's what-if under `narrow`, whose final deadline
/// the abort came after, ends midway with nothing cleared, though the same
/// input evaluated in full clears. The play preset never changes.
#[test]
fn result_switches_presets_and_labels_what_if() {
    use game::MenuCommand::{NextPreset, PrevPreset};
    let dir = tempfile::tempdir().unwrap();
    write_named(dir.path(), "late", &late());
    write_named(dir.path(), "narrow", &narrow());
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(13_000_000, Command::Retry)], 16_000_000);
    rig.menu = vec![
        (11_000_000, NextPreset),
        (11_100_000, NextPreset),
        (11_200_000, NextPreset),
        (11_300_000, PrevPreset),
        (11_400_000, NextPreset),
        (11_500_000, NextPreset),
        (11_600_000, NextPreset),
    ];
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let names = |s: &ResultScreen| s.entries().iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    let first = shown(&rig, 10_999_000);
    let built_in = |n: &str| EntryId::BuiltIn(n.to_owned());
    assert_eq!(
        names(first),
        [
            built_in("tapestry"),
            built_in("beatoraja-style"),
            built_in("lr2-style"),
            user("late"),
            user("narrow"),
        ]
    );
    assert_eq!(viewed(first), built_in("tapestry"));
    let own = ready(first);
    let played = &rig.reports[0];
    assert!(own.as_played);
    assert_eq!(own.tally, played.tally);
    assert_eq!(own.coverage, replay::verify::Coverage::Recorded);
    let path: Vec<EntryId> = [
        11_050_000, 11_150_000, 11_250_000, 11_350_000, 11_450_000, 11_550_000, 11_650_000,
    ]
    .iter()
    .map(|&t| viewed(shown(&rig, t)))
    .collect();
    assert_eq!(
        path,
        [
            built_in("beatoraja-style"),
            built_in("lr2-style"),
            user("late"),
            built_in("lr2-style"),
            user("late"),
            user("narrow"),
            // Past the last entry, back to the first.
            built_in("tapestry"),
        ]
    );
    let at_late = ready(shown(&rig, 11_250_000));
    let finalized = played.record.as_ref().unwrap().status.finalized;
    assert!(!at_late.as_played);
    assert_eq!(
        at_late.coverage,
        replay::verify::Coverage::NoInputAfter(finalized)
    );
    assert!(at_late.gauges[0].unwrap().cleared);
    // `late` was evaluated once, the second visit came from the cache.
    let last = shown(&rig, 12_000_000);
    assert_eq!(last.evaluations(), 4);
    assert_eq!(loaded.preset(), &rules::tapestry());
    assert_eq!(rig.reports[1].preset, rules::tapestry());

    // From the first entry, Prev wraps to the last.
    let mut loaded = loaded_with(dir.path(), "tapestry");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    rig.menu = vec![(11_000_000, PrevPreset)];
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(viewed(shown(&rig, 11_500_000)), user("narrow"));

    // Aborted by focus loss at play time 1.65 s: after `narrow`'s final
    // deadline (1.61 s), before the tapestry preset's (1.68 s).
    let mut loaded = loaded_with(dir.path(), "tapestry");
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        ScriptItem {
            at: TimeUs(9_950_000),
            action: Action::FocusLoss,
        },
    ];
    let mut rig = Fake::new(items, &[], 13_000_000);
    rig.menu = vec![(11_000_000, PrevPreset)];
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let report = &rig.reports[0];
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::FocusLoss));
    let record = report.record.as_ref().unwrap();
    let narrow_preset = named("narrow", narrow());
    // The same input evaluated in full completes, and the gauge clears.
    let full = eval::evaluate(
        &chart(),
        &eval::EvalInput {
            events: record.events.clone(),
            device: record.devices.profile(),
            start: record.start.clone(),
            finalized: None,
            section: None,
        },
        &narrow_preset,
    )
    .unwrap();
    assert!(full.completed);
    assert!(full.gauges.iter().all(|g| g.cleared));
    let aborted = shown(&rig, 11_500_000);
    assert_eq!(viewed(aborted), user("narrow"));
    let s = ready(aborted);
    assert!(!s.as_played);
    assert_eq!(
        s.coverage,
        replay::verify::Coverage::EndedMidway(record.status.finalized)
    );
    assert!(s.gauges.iter().flatten().all(|g| !g.cleared));
    assert!(s.gauges[0].is_some());
}

/// A user preset that is viewed but not chosen changes while the result
/// shows: within one poll the view is evaluated again under it, with
/// another gauge value and the same tally, and the chosen preset stays.
#[test]
fn edited_preset_reevaluates_on_the_result_screen() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    // tapestry, beatoraja-style, lr2-style, mine.
    rig.menu = vec![(10_900_000, game::MenuCommand::PrevPreset)];
    let changed = 11_000_000;
    let edit = path.clone();
    rig.at.push((
        changed,
        Box::new(move || rewrite_preset(&edit, &flat_normal())),
    ));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    let before = shown(&rig, changed - 1);
    assert_eq!(viewed(before), user("mine"));
    let (at, after) = rig
        .screens
        .iter()
        .find(|(_, s)| s.viewed().unwrap().hash == Some(replay::preset_hash(&mine(flat_normal()))))
        .unwrap();
    assert!(
        (changed..=changed + game::POLL_INTERVAL.0).contains(at),
        "re-evaluated at {at}"
    );
    let (was, now) = (ready(before), ready(after));
    let expected = replay::verify::evaluate_under(
        rig.reports[0].record.as_ref().unwrap(),
        &chart(),
        &mine(flat_normal()),
    )
    .unwrap();
    assert_eq!(
        now.gauges[0].unwrap().value,
        expected.evaluation.gauges[0].value
    );
    assert_ne!(now.gauges[0], was.gauges[0]);
    assert_eq!(now.tally, was.tally);
    assert_eq!(
        after.notices().cloned().collect::<Vec<_>>(),
        [Notice::Reloaded("mine".to_owned())]
    );
    assert_eq!(loaded.shelf().chosen(), &rules::tapestry());
}

/// A wake 1 µs before the result's poll deadline does not check; the wait
/// after it ends at the deadline, where the check runs, not an idle wait
/// later.
#[test]
fn result_poll_runs_at_its_deadline_after_an_early_wake() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    rig.idle_to_deadline = true;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    // The first poll checks at the result's first wake; each wait after
    // ends at the next deadline.
    let waits = rig.idle_waits.clone();
    let deadline = waits[1].1;
    assert_eq!(
        deadline,
        waits[0].0 + 1 + game::POLL_INTERVAL.0,
        "{waits:?}"
    );

    // Again, with an edit landing at the deadline and a wake 1 µs before it.
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_000_000);
    rig.idle_to_deadline = true;
    let edit = path.clone();
    rig.at.push((
        deadline - 2,
        Box::new(move || rewrite_preset(&edit, &flat_normal())),
    ));
    rig.at.push((deadline - 1, Box::new(|| {})));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let woke = rig
        .idle_waits
        .iter()
        .position(|&(at, _)| at == deadline - 1);
    let woke = woke.unwrap_or_else(|| panic!("{:?}", rig.idle_waits));
    // The early wake waits to the deadline, not an idle wait later.
    assert_eq!(rig.idle_waits[woke].1, deadline);
    let (at, _) = rig
        .screens
        .iter()
        .find(|(_, s)| s.notices().next().is_some())
        .unwrap();
    assert_eq!(*at, deadline);
}

/// The chosen user preset changes during a play: nothing is polled then, so
/// that play keeps the preset it started with, and the retry plays the
/// reloaded one.
#[test]
fn plays_use_reloads_from_the_next_play() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(11_500_000, Command::Retry)], 15_000_000);
    // Between the press and the second note, mid-play.
    let edit = path.clone();
    rig.at.push((
        9_400_000,
        Box::new(move || rewrite_preset(&edit, &flat_normal())),
    ));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();

    assert_eq!(rig.reports.len(), 2);
    assert!(
        rig.origins[0].0 + 1_500_000 > 9_400_000,
        "the edit is mid-play"
    );
    let [one, two] = &rig.reports[..] else {
        unreachable!()
    };
    assert_eq!(one.preset, mine(rules::tapestry()));
    assert_eq!(
        one.record.as_ref().unwrap().preset.sha256,
        replay::preset_hash(&mine(rules::tapestry()))
    );
    assert_eq!(two.preset, mine(flat_normal()));
    assert_eq!(
        two.record.as_ref().unwrap().preset.sha256,
        replay::preset_hash(&mine(flat_normal()))
    );
    assert_eq!(two.reload_error, None);
}

/// One play of [`chart`] under the preset named `preset` in `dir`, left on
/// its result: the loaded chart and the report.
fn played(dir: &Path, preset: &str) -> (Loaded, PlayReport) {
    let mut loaded = loaded_with(dir, preset);
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 11_000_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let report = rig.reports.pop().unwrap();
    (loaded, report)
}

/// A shelf of `dir`'s presets choosing `name`, apart from the chart's.
fn shelf_of(dir: &Path, name: &str) -> game::Shelf {
    game::Shelf::open(dir.join("presets"), name).unwrap()
}

/// The screen's view moved to the entry `id`, stepping forward.
fn view(screen: &mut ResultScreen, id: &EntryId, report: &PlayReport) {
    for _ in 0..screen.entries().len() {
        if viewed(screen) == *id {
            return;
        }
        screen.step(1, report, &chart());
    }
    panic!("{id:?} not listed");
}

/// Every view status moves on a switch: Ready, CannotLoad, OverBudget and
/// Failed each land on the next entry, a Failed summary is cached and not
/// evaluated again, and with nothing recorded every view stays so, a file
/// that cannot load included.
#[test]
fn every_status_moves_on_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    write_named(dir.path(), "broken", &rules::tapestry());
    common::rewrite(
        &dir.path().join("presets").join("broken.toml"),
        "windows = [",
    );
    let (loaded, report) = played(dir.path(), "tapestry");
    let shelf = loaded.shelf();
    let budget = game::RECORDING_MEMORY;
    let built_in = |n: &str| EntryId::BuiltIn(n.to_owned());

    // C → the next entry, then back to C.
    let mut s = ResultScreen::new(&report, shelf, &chart(), budget);
    view(&mut s, &user("broken"), &report);
    assert!(matches!(s.status(), ViewStatus::CannotLoad(e) if e.path.ends_with("broken.toml")));
    s.step(1, &report, &chart());
    assert_eq!(viewed(&s), built_in("tapestry"));
    assert!(ready(&s).as_played);
    s.step(-1, &report, &chart());
    assert!(matches!(s.status(), ViewStatus::CannotLoad(_)));

    // R (what-if) → R, and O → R.
    s.step(-1, &report, &chart());
    assert!(!ready(&s).as_played);
    let mut tiny = ResultScreen::new(&report, shelf, &chart(), 1_024);
    assert!(ready(&tiny).as_played);
    tiny.step(1, &report, &chart());
    assert!(matches!(
        tiny.status(),
        ViewStatus::OverBudget { needed, budget: 1_024 } if needed > 1_024
    ));
    tiny.step(1, &report, &chart());
    assert!(matches!(tiny.status(), ViewStatus::OverBudget { .. }));
    tiny.step(-2, &report, &chart());
    assert!(ready(&tiny).as_played);
    assert_eq!(tiny.evaluations(), 0);

    // F → F, cached: a record of another evaluator version.
    let mut other = report.clone();
    other.record.as_mut().unwrap().versions.evaluator += 1;
    let mut f = ResultScreen::new(&other, shelf, &chart(), budget);
    f.step(1, &other, &chart());
    assert!(matches!(
        f.status(),
        ViewStatus::Failed(game::ReEvaluationError::UnsupportedEvaluator(_))
    ));
    f.step(1, &other, &chart());
    assert!(matches!(f.status(), ViewStatus::Failed(_)));
    assert_eq!(f.evaluations(), 2);
    f.step(-1, &other, &chart());
    assert!(matches!(f.status(), ViewStatus::Failed(_)));
    assert_eq!(
        f.evaluations(),
        2,
        "a cached failure is not evaluated again"
    );

    // N everywhere, CannotLoad included.
    let mut nothing = report.clone();
    nothing.record = None;
    nothing.result = None;
    let mut n = ResultScreen::new(&nothing, shelf, &chart(), budget);
    assert_eq!(n.status(), ViewStatus::NothingRecorded);
    for _ in 0..n.entries().len() {
        n.step(1, &nothing, &chart());
        assert_eq!(n.status(), ViewStatus::NothingRecorded, "{:?}", viewed(&n));
    }
    view(&mut n, &user("broken"), &nothing);
    assert_eq!(n.status(), ViewStatus::NothingRecorded);
    assert_eq!(n.evaluations(), 0);
}

/// A 1 KiB budget refuses every what-if without evaluating it, with the
/// bytes it needs.
#[test]
fn over_budget_view_is_refused_without_evaluating() {
    let dir = tempfile::tempdir().unwrap();
    let (loaded, report) = played(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, loaded.shelf(), &chart(), 1_024);
    for _ in 1..s.entries().len() {
        s.step(1, &report, &chart());
        let ViewStatus::OverBudget { needed, budget } = s.status() else {
            panic!("{:?}", s.status());
        };
        let preset = s.viewed().unwrap().preset.clone().unwrap();
        let expected = game::what_if_bytes(&chart(), &preset, report.record.as_ref().unwrap());
        assert_eq!((Some(needed), budget), (expected, 1_024));
    }
    assert_eq!(s.evaluations(), 0);
}

/// The viewed entry's file goes away: the view returns to the play's own
/// preset with a notice. With nothing recorded it moves there and stays
/// so. The pinned as-played entry going away, once a file has the play's
/// hash again, is no removal.
#[test]
fn viewed_entry_removed_returns_to_as_played() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_named(dir.path(), "other", &rules::tapestry());
    let (loaded, report) = played(dir.path(), "tapestry");
    let mut shelf = shelf_of(dir.path(), "tapestry");
    let mut nothing = report.clone();
    nothing.record = None;
    nothing.result = None;
    let mut s = ResultScreen::new(&report, &shelf, &chart(), game::RECORDING_MEMORY);
    let mut n = ResultScreen::new(&nothing, loaded.shelf(), &chart(), game::RECORDING_MEMORY);
    view(&mut s, &user("other"), &report);
    view(&mut n, &user("other"), &nothing);
    std::fs::remove_file(&path).unwrap();
    shelf.check();
    assert!(s.sync(&report, &shelf, &chart()));
    assert!(n.sync(&nothing, &shelf, &chart()));
    assert_eq!(viewed(&s), EntryId::BuiltIn("tapestry".to_owned()));
    assert!(ready(&s).as_played);
    assert_eq!(
        s.notices().cloned().collect::<Vec<_>>(),
        [Notice::Removed("other".to_owned())]
    );
    assert_eq!(viewed(&n), EntryId::BuiltIn("tapestry".to_owned()));
    assert_eq!(n.status(), ViewStatus::NothingRecorded);

    // Played under `mine`, edited away: the play is pinned as played.
    // Restoring the file drops the pin, with no notice.
    let mine_path = write_user_preset(dir.path(), &rules::tapestry());
    let (_, report) = played(dir.path(), "mine");
    let mut shelf = shelf_of(dir.path(), "mine");
    rewrite_preset(&mine_path, &flat_normal());
    shelf.check();
    let mut s = ResultScreen::new(&report, &shelf, &chart(), game::RECORDING_MEMORY);
    let pinned = EntryId::AsPlayed(replay::preset_hash(&mine(rules::tapestry())));
    assert_eq!(viewed(&s), pinned);
    rewrite_preset(&mine_path, &rules::tapestry());
    shelf.check();
    assert!(s.sync(&report, &shelf, &chart()));
    assert_eq!(viewed(&s), user("mine"));
    assert!(ready(&s).as_played);
    assert!(!s.entries().iter().any(|e| e.id == pinned));
    assert!(
        !s.notices().any(|n| matches!(n, Notice::Removed(_))),
        "{:?}",
        s.notices().collect::<Vec<_>>()
    );
}

/// A viewed file that breaks keeps its last good version on view, with its
/// error beside it; a recovery clears both the error and its notice. Only
/// the latest two notices are kept.
#[test]
fn viewed_failure_keeps_the_view_and_recovery_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_named(dir.path(), "a", &flat_normal());
    let b = write_named(dir.path(), "b", &rules::tapestry());
    let c = write_named(dir.path(), "c", &rules::tapestry());
    let (_, report) = played(dir.path(), "tapestry");
    let mut shelf = shelf_of(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, &shelf, &chart(), game::RECORDING_MEMORY);
    view(&mut s, &user("a"), &report);
    let was = ready(&s);
    common::rewrite(&a, "windows = [");
    shelf.check();
    s.sync(&report, &shelf, &chart());
    assert_eq!(viewed(&s), user("a"));
    assert_eq!(ready(&s), was);
    let e = s.file_error().unwrap().clone();
    assert_eq!(e.path, a);
    rewrite_preset(&a, &flat_normal());
    shelf.check();
    s.sync(&report, &shelf, &chart());
    assert_eq!(s.file_error(), None);
    assert_eq!(s.notices().count(), 0);
    assert_eq!(ready(&s), was);
    // Three failures: the latest two are kept.
    for p in [&a, &b, &c] {
        common::rewrite(p, "windows = [");
        shelf.check();
        s.sync(&report, &shelf, &chart());
    }
    let failed: Vec<_> = s
        .notices()
        .map(|n| match n {
            Notice::Failed(e) => e.path.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(failed, [b, c]);
    assert_eq!(game::NOTICES, 2);
}

/// Past [`game::RESULT_CACHE`] presets viewed, the least recently viewed
/// summary goes, never the viewed one: viewing it again evaluates it again.
#[test]
fn result_cache_evicts_the_least_recently_viewed() {
    let dir = tempfile::tempdir().unwrap();
    let n = game::RESULT_CACHE + 1;
    for i in 0..n {
        let mut p = rules::tapestry();
        p.gauges[0].start = rules::GaugeValue(p.gauges[0].start.0 + i64::try_from(i).unwrap());
        write_named(dir.path(), &format!("p{i:02}"), &p);
    }
    let (loaded, report) = played(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, loaded.shelf(), &chart(), game::RECORDING_MEMORY);
    // On the way, beatoraja-style and lr2-style are evaluated first.
    view(&mut s, &user("p00"), &report);
    let base = s.evaluations();
    for _ in 1..n {
        s.step(1, &report, &chart());
        assert!(!ready(&s).as_played);
    }
    let last = user(&format!("p{:02}", n - 1));
    assert_eq!(viewed(&s), last);
    let evaluated = base + u64::try_from(n - 1).unwrap();
    assert_eq!(s.evaluations(), evaluated);
    assert_eq!(s.cached(), game::RESULT_CACHE);
    // The viewed one stays cached.
    s.step(0, &report, &chart());
    assert_eq!(s.evaluations(), evaluated);
    // p00 went with the two viewed before it: viewing it evaluates again.
    s.step(-isize::try_from(n - 1).unwrap(), &report, &chart());
    assert_eq!(viewed(&s), user("p00"));
    assert_eq!(s.evaluations(), evaluated + 1);
    assert_eq!(s.cached(), game::RESULT_CACHE);
    // And the last one, viewed most recently but one, is still cached.
    s.step(isize::try_from(n - 1).unwrap(), &report, &chart());
    assert_eq!(viewed(&s), last);
    assert_eq!(s.evaluations(), evaluated + 1);
}

/// A refused `tapestry.toml` sits beside the built-in `tapestry`: the view on
/// the file stays on it across rebuilds, and switching never takes one
/// for the other.
#[test]
fn duplicate_stem_beside_a_built_in_keeps_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    std::fs::write(presets.join("tapestry.toml"), game::preset_to_toml(&late())).unwrap();
    let other = write_named(dir.path(), "other", &rules::tapestry());
    let (_, report) = played(dir.path(), "tapestry");
    let mut shelf = shelf_of(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, &shelf, &chart(), game::RECORDING_MEMORY);
    let file = user("tapestry");
    let built_in = EntryId::BuiltIn("tapestry".to_owned());
    assert_eq!(viewed(&s), built_in);
    view(&mut s, &file, &report);
    assert!(matches!(s.status(), ViewStatus::CannotLoad(_)));
    for preset in [flat_normal(), late()] {
        rewrite_preset(&other, &preset);
        shelf.check();
        assert!(s.sync(&report, &shelf, &chart()));
        assert_eq!(viewed(&s), file);
    }
    let ids: Vec<_> = s.entries().iter().map(|e| e.id.clone()).collect();
    assert_eq!(ids.iter().filter(|id| **id == file).count(), 1);
    assert_eq!(ids.iter().filter(|id| **id == built_in).count(), 1);
    s.step(-(isize::try_from(ids.len()).unwrap()), &report, &chart());
    assert_eq!(viewed(&s), file);
}

/// With a built-in chosen, the presets folder cannot be listed, then can
/// again with the same files: a notice says so, then goes; the view stays.
#[test]
fn a_listing_failure_is_noticed_then_cleared() {
    let dir = tempfile::tempdir().unwrap();
    write_named(dir.path(), "other", &rules::tapestry());
    let (_, report) = played(dir.path(), "tapestry");
    let presets = dir.path().join("presets");
    let mut shelf = shelf_of(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, &shelf, &chart(), game::RECORDING_MEMORY);
    let kept = presets.with_extension("kept");
    std::fs::rename(&presets, &kept).unwrap();
    // A file where the folder was: listing it fails.
    std::fs::write(&presets, b"").unwrap();
    shelf.check();
    assert!(shelf.listing_error().is_some());
    assert!(s.sync(&report, &shelf, &chart()));
    assert!(matches!(
        s.notices().cloned().collect::<Vec<_>>()[..],
        [Notice::ListingFailed(_)]
    ));
    assert!(s.entries().iter().any(|e| e.id == user("other")));
    std::fs::remove_file(&presets).unwrap();
    std::fs::rename(&kept, &presets).unwrap();
    shelf.check();
    assert!(s.sync(&report, &shelf, &chart()));
    assert_eq!(s.notices().count(), 0);
    assert!(ready(&s).as_played);
}

/// The presets folder already cannot be listed when the chart loads, with a
/// built-in chosen: the result says so from its entry, though no check
/// during the result changes anything.
#[test]
fn a_standing_listing_failure_shows_on_result_entry() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the folder is: listing it fails.
    std::fs::write(dir.path().join("presets"), b"").unwrap();
    // Opened so, the shelf already holds the failure, before any check: the
    // select screen shows it from the start.
    let opened = shelf_of(dir.path(), "tapestry");
    assert_eq!(
        opened.listing_error().map(|e| &e.path),
        Some(&dir.path().join("presets"))
    );
    assert_eq!(opened.revision(), 0);
    let (loaded, report) = played(dir.path(), "tapestry");
    assert!(loaded.shelf().listing_error().is_some());
    let s = ResultScreen::new(&report, loaded.shelf(), &chart(), game::RECORDING_MEMORY);
    let notices: Vec<Notice> = s.notices().cloned().collect();
    let [Notice::ListingFailed(e)] = &notices[..] else {
        panic!("{notices:?}");
    };
    assert_eq!(e.path, dir.path().join("presets"));
    assert!(ready(&s).as_played);
}

/// G7 and the shared axis: a user preset of the played rules under another
/// name is a what-if whose graphs and deviations equal the play's own,
/// which are not empty; under `late`, whose final deadline comes later,
/// each gauge is condensed on the as-played axis, not on its own.
#[test]
fn what_ifs_are_condensed_on_the_as_played_axis() {
    let dir = tempfile::tempdir().unwrap();
    write_named(dir.path(), "same", &rules::tapestry());
    write_named(dir.path(), "late", &late());
    let (loaded, report) = played(dir.path(), "tapestry");
    let mut s = ResultScreen::new(&report, loaded.shelf(), &chart(), game::RECORDING_MEMORY);
    let own = ready(&s);
    assert!(own.as_played);
    let played = report.result.as_ref().unwrap().evaluation.as_ref().unwrap();
    let axis = s.axis().unwrap();
    assert_eq!(
        axis,
        game::Axis {
            origin: TimeUs(0),
            end: played.final_deadline,
        }
    );
    // The press and the miss moved the normal gauge.
    assert_eq!(own.graphs.timing.samples(), 1);
    let normal = own.graphs.traces[0].unwrap();
    let start = rules::tapestry().gauges[0].start;
    assert!(normal.columns.iter().any(|c| c.high > start));
    assert!(normal.columns.iter().any(|c| c.low < c.high));

    view(&mut s, &user("same"), &report);
    let same = ready(&s);
    assert!(!same.as_played);
    assert_eq!(same.graphs, own.graphs);
    assert_eq!(same.gauges, own.gauges);

    view(&mut s, &user("late"), &report);
    let at_late = ready(&s);
    let preset = s.viewed().unwrap().preset.clone().unwrap();
    let ctx = game::ResultContext {
        record: report.record.as_ref().unwrap(),
        chart: &chart(),
        preset: &report.preset,
        live: &report.live,
        tally: &report.tally,
    };
    let r = game::ReEvaluate::under(&ctx, &preset).unwrap();
    assert!(r.evaluation.final_deadline > axis.end);
    let own_axis = game::Axis {
        end: r.evaluation.final_deadline,
        ..axis
    };
    for (i, g) in r.evaluation.gauges.iter().enumerate() {
        let mut on_play = [normal.columns[0]; game::GRAPH_COLUMNS];
        let mut on_own = on_play;
        game::condense(g, axis, &mut on_play);
        game::condense(g, own_axis, &mut on_own);
        assert_eq!(at_late.graphs.traces[i].unwrap().columns, on_play);
        // The normal gauge rose at the press, which the two axes put in
        // different columns.
        if i == 0 {
            assert_ne!(on_play, on_own);
        }
    }
}

/// An event the player or the system brings, at a clock time.
#[derive(Clone, Copy, Debug)]
enum Event {
    QuitKey,
    Retry,
    Close,
    FocusLoss,
}

/// How a chart's first play went: its end, the tail's cut and skip, how
/// many plays ran, how many results were shown, and how the chart was
/// left.
type Went = (PlayEnd, Option<AbortCause>, bool, usize, usize, game::Leave);

/// `event` at clock time `at` in a play that opens at 7.3 s (origin 8.3
/// s), completes at 9.983 s and has a 2 s audio tail; the quit key at 15 s
/// leaves a result shown by then, and the window closes at 16 s.
fn went(event: Event, at: i64) -> Went {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut commands = vec![(15_000_000, Command::Quit)];
    let mut quit_at = 16_000_000;
    match event {
        Event::QuitKey => commands.push((at, Command::Quit)),
        Event::Retry => commands.push((at, Command::Retry)),
        Event::Close => quit_at = at,
        Event::FocusLoss => items.push(ScriptItem {
            at: TimeUs(at),
            action: Action::FocusLoss,
        }),
    }
    let mut rig = Fake::new(items, &commands, quit_at);
    rig.tail_waits = 2_000;
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    (
        one.end,
        one.tail_cut,
        one.tail_skipped,
        rig.reports.len(),
        rig.shows,
        left,
    )
}

/// The state × event table of design.md "Play Lifecycle": in READY
/// the quit key aborts the play and leaves the chart without a result;
/// while playing it aborts the play and shows its result, which the next
/// quit leaves; once the play is complete, in its audio tail, it skips to
/// the result, which the next quit leaves. Retry retries from every state,
/// closing the window ends the run from every state, and focus loss ends
/// the play, or cuts its tail, and shows the result.
#[test]
fn every_state_and_event_of_a_play() {
    use game::Leave::{Back, Quit};
    let aborted = PlayEnd::Aborted;
    let done = PlayEnd::Completed;
    // READY (play time -0.5 s) and playing (1.2 s): only the quit
    // key's result differs.
    for (state, at, quit_shows) in [("ready", 7_800_000, 0), ("playing", 9_500_000, 1)] {
        let cases = [
            (
                Event::QuitKey,
                (aborted(AbortCause::Quit), None, false, 1, quit_shows, Back),
            ),
            (
                Event::Retry,
                (aborted(AbortCause::Retry), None, false, 2, 1, Back),
            ),
            (
                Event::Close,
                (aborted(AbortCause::Quit), None, false, 1, 0, Quit),
            ),
            (
                Event::FocusLoss,
                (aborted(AbortCause::FocusLoss), None, false, 1, 1, Back),
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(went(event, at), expected, "{state}, {event:?}");
        }
    }
    // The tail (play time 1.8 s).
    let tail = [
        (Event::QuitKey, (done, None, true, 1, 1, Back)),
        (
            Event::Retry,
            (done, Some(AbortCause::Retry), false, 2, 1, Back),
        ),
        (
            Event::Close,
            (done, Some(AbortCause::Quit), false, 1, 0, Quit),
        ),
        (
            Event::FocusLoss,
            (done, Some(AbortCause::FocusLoss), false, 1, 1, Back),
        ),
    ];
    for (event, expected) in tail {
        assert_eq!(went(event, 10_100_000), expected, "tail, {event:?}");
    }
}

/// The chart starts at chart time 0 (clock 8.3 s): the quit key in the
/// step before it leaves the chart with no result, and from the step at it
/// on aborts the play into Closing, whose end 1 s later shows its result,
/// ended midway and never cleared, until the back at 15 s. Both plays are
/// saved, ended midway.
#[test]
fn the_quit_key_shows_the_result_from_the_charts_start() {
    for (at, shown) in [(8_299_000, None), (8_300_000, Some(9_300_000))] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
        let commands = [(at, Command::Quit), (15_000_000, Command::Quit)];
        let mut rig = Fake::new(items, &commands, i64::MAX);
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, game::Leave::Back, "{at}");
        let [one] = &rig.reports[..] else {
            panic!("{at}: {} plays", rig.reports.len());
        };
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit), "{at}");
        assert_eq!(rig.shown_at.first().copied(), shown, "{at}");
        assert_eq!(status(one).end, replay::EndStatus::EndedMidway, "{at}");
        if shown.is_some() {
            // Left by the back at 15 s, not at once.
            assert!(rig.clock.now().0 >= 15_000_000, "{}", rig.clock.now().0);
            let result = one.result.as_ref().unwrap();
            let evaluation = result.evaluation.as_ref().unwrap();
            assert!(
                evaluation.gauges.iter().all(|g| !g.cleared),
                "{evaluation:?}"
            );
        }
    }
}

/// A quit that arrives as recording closes, on a play aborted after its
/// chart started, is taken with that play: its result shows and stays
/// until the back at 15 s.
#[test]
fn a_quit_during_close_after_an_abort_shows_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![
        key(9_303_000, Z, 1),
        key(9_353_000, Z, 0),
        ScriptItem {
            at: TimeUs(9_500_000),
            action: Action::FocusLoss,
        },
    ];
    let mut rig = Fake::new(items, &[(15_000_000, Command::Quit)], i64::MAX);
    rig.in_close = Some(Command::Quit);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::FocusLoss));
    assert_eq!(rig.shows, 1);
    assert!(rig.clock.now().0 >= 15_000_000, "{}", rig.clock.now().0);
}

/// The quit key in the tail stops the audio in that step, shows the
/// result there, and saves the record complete.
#[test]
fn a_quit_in_the_tail_stops_the_audio_and_shows_the_result_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let commands = [(10_100_000, Command::Quit), (10_500_000, Command::Quit)];
    let mut rig = Fake::new(items, &commands, i64::MAX);
    rig.tail_waits = 2_000;
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    let [one] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!((one.end, one.tail_cut), (PlayEnd::Completed, None));
    assert!(one.tail_skipped);
    assert_eq!(rig.cancels, 1);
    assert_eq!(rig.shown_at.first(), Some(&10_100_000));
    assert!(matches!(one.saved, Some(Ok(_))), "{:?}", one.saved);
    assert_eq!(status(one).end, replay::EndStatus::PlayedToEnd);
}

/// The quit key that arrives as a completed play's recording closes skips
/// the tail too.
#[test]
fn a_quit_during_close_after_completion_skips_to_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(10_500_000, Command::Quit)], i64::MAX);
    rig.tail_waits = 2_000;
    rig.in_close = Some(Command::Quit);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert!(one.tail_skipped);
    assert_eq!((rig.cancels, rig.shows), (1, 1));
}

/// A record sink that saves nothing and presses the quit key while it
/// saves: a slow save the player does not wait for.
struct QuitWhileSaving(std::sync::Arc<std::sync::Mutex<Vec<Command>>>);

impl game::RecordSink for QuitWhileSaving {
    fn save(&mut self, _rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        self.0.lock().unwrap().push(Command::Quit);
        Ok(game::RecordRef {
            path: "pressed.tprl".into(),
        })
    }
}

/// The quit key pressed while a play is saved belongs to that play, never
/// to its result: after a completed play's tail ran out it is the skip to
/// its result, and after a play the quit key ended at 9.5 s, while the
/// chart played, it is taken with that play; either way the result shows
/// and stays until the next quit at 11 s, instead of reading that press
/// as its back.
#[test]
fn a_quit_during_the_save_shows_the_result() {
    for (abort, end) in [
        (None, PlayEnd::Completed),
        (Some(9_500_000), PlayEnd::Aborted(AbortCause::Quit)),
    ] {
        quit_during_the_save(abort, end);
    }
}

/// [`a_quit_during_the_save_shows_the_result`] with the quit key at
/// `abort`, if given, and the play ending as `end`.
fn quit_during_the_save(abort: Option<i64>, end: PlayEnd) {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let settings = tapestry_settings(&root);
    let chart_path = dir.path().join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let pressed = std::sync::Arc::default();
    let parts = Parts {
        ingest: Box::new(Built(chart())),
        records: Some(Box::new(QuitWhileSaving(std::sync::Arc::clone(&pressed)))),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    let mut loaded = Loaded::load(root, settings, &chart_path, config, parts).unwrap();
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut commands = vec![(11_000_000, Command::Quit)];
    commands.extend(abort.map(|at| (at, Command::Quit)));
    let mut rig = Fake::new(items, &commands, i64::MAX);
    rig.pressed = pressed;
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back, "{end:?}");
    let [one] = &rig.reports[..] else {
        panic!("{end:?}: {} plays", rig.reports.len());
    };
    assert_eq!((one.end, one.tail_cut), (end, None));
    assert!(matches!(one.saved, Some(Ok(_))), "{end:?}: {:?}", one.saved);
    assert_eq!(rig.shows, 1, "{end:?}");
    assert!(rig.pressed.lock().unwrap().is_empty(), "{end:?}");
    // Left by the quit at 11 s, not at once.
    assert!(
        rig.clock.now().0 >= 11_000_000,
        "{end:?}: {}",
        rig.clock.now().0
    );
}

/// A record sink whose save tells its test it began, then blocks until the
/// test releases it: a slow save, with the test acting while it waits.
struct BlockedSave {
    began: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl game::RecordSink for BlockedSave {
    fn save(&mut self, _rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        self.began.send(()).unwrap();
        self.release.recv().unwrap();
        Ok(game::RecordRef {
            path: "blocked.tprl".into(),
        })
    }
}

/// Where the commands of the ResultPreparing tests arrive, after the
/// first play's save (its chart ends near 10 s on the clock), and where the
/// result they show is left.
const FRESH_AT: i64 = 12_000_000;
const LEAVE_AT: i64 = 20_000_000;

/// [`chart`] played on `rig` through [`run_chart`](game::live::run_chart)
/// with every save blocked: while the first one waits, `during_save` runs
/// on another thread, then each save is released. How the chart was left,
/// and the session's events.
fn with_blocked_save(
    rig: &mut Fake,
    during_save: impl FnOnce() + Send,
) -> (game::Leave, Vec<SessionEvent>) {
    let (left, events) = try_with_blocked_save(rig, during_save);
    (left.unwrap(), events)
}

/// [`with_blocked_save`], the run's error kept.
fn try_with_blocked_save(
    rig: &mut Fake,
    during_save: impl FnOnce() + Send,
) -> (
    Result<game::Leave, LiveError<SurfaceLost>>,
    Vec<SessionEvent>,
) {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let (began, saving) = std::sync::mpsc::sync_channel(0);
    let (release, released) = std::sync::mpsc::sync_channel(0);
    std::thread::scope(|s| {
        s.spawn(move || {
            saving.recv().unwrap();
            during_save();
            release.send(()).unwrap();
            // Every later save, released at once.
            while saving.recv().is_ok() {
                release.send(()).unwrap();
            }
        });
        let mut loaded = closing_loaded_with(
            dir.path(),
            chart(),
            0,
            FailureAction::PlayToEnd,
            &events,
            None,
            TimeUs(1_000 * MS),
            |p| Parts {
                records: Some(Box::new(BlockedSave {
                    began,
                    release: released,
                })),
                ..p
            },
        );
        let left = game::live::run_chart(&mut loaded, rig);
        (left, events.all())
    })
}

/// Whether `events` hold a result shown for `attempt`.
fn result_of(events: &[SessionEvent], attempt: u32) -> bool {
    events.contains(&SessionEvent::Result { attempt })
}

fn saved(report: &PlayReport) -> bool {
    matches!(report.saved, Some(Ok(_)))
}

fn withdrawn(rig: &Fake) -> usize {
    rig.withdrawn.load(std::sync::atomic::Ordering::SeqCst)
}

/// The quit key, or the result's back, pressed while the save blocks
/// belongs to the play (design.md "State Model", ResultPreparing): the
/// save completes, the result's preparation follows, and the result shows
/// once, its event emitted, until the quit at 20 s leaves it.
#[test]
fn a_quit_during_a_blocked_save_completes_it_and_shows_the_result() {
    for case in ["quit", "back"] {
        let mut rig = Fake::new(Vec::new(), &[(LEAVE_AT, Command::Quit)], i64::MAX);
        let pressed = std::sync::Arc::clone(&rig.pressed);
        let menu = std::sync::Arc::clone(&rig.menu_pressed);
        let (left, events) = with_blocked_save(&mut rig, move || {
            if case == "quit" {
                pressed.lock().unwrap().push(Command::Quit);
            } else {
                menu.lock().unwrap().push(game::MenuCommand::Back);
            }
        });
        assert_eq!(left, game::Leave::Back, "{case}");
        let [one] = &rig.reports[..] else {
            panic!("{case}: {} plays", rig.reports.len());
        };
        assert!(saved(one), "{case}: {:?}", one.saved);
        assert_eq!(rig.preps.len(), 1, "{case}");
        assert_eq!(rig.shows, 1, "{case}");
        assert_eq!(withdrawn(&rig), 0, "{case}");
        assert_eq!(
            events.last(),
            Some(&SessionEvent::Result { attempt: 1 }),
            "{case}"
        );
        assert!(rig.clock.now().0 >= LEAVE_AT, "{case}");
    }
}

/// Retry pressed while the save blocks belongs to the play: the save
/// completes and the next attempt is prepared, with no preparation, no
/// end screen and no result event for the first play; the second play's
/// result is the only one.
#[test]
fn a_retry_during_a_blocked_save_completes_it_and_retries() {
    let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
    let pressed = std::sync::Arc::clone(&rig.pressed);
    let (left, events) = with_blocked_save(&mut rig, move || {
        pressed.lock().unwrap().push(Command::Retry);
    });
    assert_eq!(left, game::Leave::Back);
    let [one, two] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert!(saved(one) && saved(two));
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(rig.attempts.len(), 2);
    assert_eq!((rig.preps.len(), rig.shows), (1, 1));
    assert!(!result_of(&events, 1));
    assert!(result_of(&events, 2));
}

/// After the save, while the result's preparation is pending, a fresh
/// quit, back or retry withdraws it (design.md "State Model",
/// ResultPreparing): no result event and no end screen for that play;
/// quit and back leave the chart, retry plays the next attempt, whose
/// result shows. A quit pressed during the save comes before and is the
/// play's, never the fresh one.
#[test]
fn a_fresh_command_during_the_preparation_withdraws_it() {
    use game::MenuCommand::Back;
    for case in ["quit after a quit in the save", "back", "retry"] {
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut commands = vec![(LEAVE_AT, Command::Quit)];
        match case {
            "back" => {}
            "retry" => commands.push((FRESH_AT, Command::Retry)),
            _ => commands.push((FRESH_AT, Command::Quit)),
        }
        let mut rig = Fake::new(Vec::new(), &commands, i64::MAX);
        rig.prep_gate = Some(std::sync::Arc::clone(&gate));
        if case == "back" {
            rig.menu = vec![(FRESH_AT, Back)];
        }
        if case == "retry" {
            // The next attempt's preparation finishes.
            let open = std::sync::Arc::clone(&gate);
            rig.at.push((
                FRESH_AT + 1,
                Box::new(move || open.store(true, std::sync::atomic::Ordering::SeqCst)),
            ));
        }
        let pressed = std::sync::Arc::clone(&rig.pressed);
        let quit_in_save = case.starts_with("quit");
        let (left, events) = with_blocked_save(&mut rig, move || {
            if quit_in_save {
                pressed.lock().unwrap().push(Command::Quit);
            }
        });
        assert_eq!(left, game::Leave::Back, "{case}");
        assert!(saved(&rig.reports[0]), "{case}");
        assert!(!result_of(&events, 1), "{case}");
        assert_eq!(withdrawn(&rig), 1, "{case}");
        // The preparation began after the save and before the fresh
        // command.
        assert!(rig.preps[0] < FRESH_AT, "{case}");
        if case == "retry" {
            assert_eq!((rig.reports.len(), rig.attempts.len()), (2, 2));
            assert!(saved(&rig.reports[1]));
            assert_eq!((rig.preps.len(), rig.shows), (2, 1));
            assert!(result_of(&events, 2));
            assert!(rig.screens[0].0 > FRESH_AT);
        } else {
            assert_eq!(rig.reports.len(), 1, "{case}");
            assert_eq!(rig.shows, 0, "{case}");
            // Left by the fresh command, before the result's quit.
            let now = rig.clock.now().0;
            assert!((FRESH_AT..LEAVE_AT).contains(&now), "{case}: {now}");
        }
    }
}

/// A fresh command taken during the poll that finds the preparation
/// ready is still served before the result shows: a quit leaves with no
/// result, a retry plays the next attempt with the first play's result
/// never shown.
#[test]
fn a_command_during_the_ready_poll_is_served_before_the_result() {
    for (pressed, plays, leave) in [
        (FirstPoll::Command(Command::Quit), 1, game::Leave::Back),
        (FirstPoll::Command(Command::Retry), 2, game::Leave::Back),
        (
            FirstPoll::Menu(game::MenuCommand::Back),
            1,
            game::Leave::Back,
        ),
        (FirstPoll::Close, 1, game::Leave::Quit),
    ] {
        let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
        rig.in_first_poll = Some(pressed);
        let (left, events) = with_blocked_save(&mut rig, || {});
        assert_eq!(left, leave, "{pressed:?}");
        assert_eq!(rig.reports.len(), plays, "{pressed:?}");
        assert!(saved(&rig.reports[0]), "{pressed:?}");
        assert!(!result_of(&events, 1), "{pressed:?}");
        assert_eq!(rig.shows, plays - 1, "{pressed:?}");
        assert_eq!(result_of(&events, 2), plays == 2, "{pressed:?}");
    }
}

/// A record sink whose save succeeds at once and whose best lookup tells
/// its test it began, then blocks until the test releases it: a slow read
/// of the own view's best, with the test acting while it waits.
struct BlockedLookup {
    began: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl game::RecordSink for BlockedLookup {
    fn save(&mut self, _rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        Ok(game::RecordRef {
            path: "saved.tprl".into(),
        })
    }

    fn best_before(&mut self, _: &game::BestKey, _: &rules::RulePreset) -> game::BestLookup {
        self.began.send(()).unwrap();
        self.release.recv().unwrap();
        game::BestLookup::KnownEmpty
    }
}

/// C3 for the best's read: the uncounted play's own best is read after
/// the preparation began (design.md "State Model", ResultPreparing), so a
/// result back, a quit or a retry pressed while that read blocks is fresh
/// and withdraws the result, as during a pending poll: no result event
/// and no end screen for the play; back and quit leave the chart, retry
/// plays the next attempt.
#[test]
fn a_fresh_command_during_the_best_read_withdraws_the_result() {
    for case in ["back", "quit", "retry"] {
        let mut rig = Fake::new(Vec::new(), &[(LEAVE_AT, Command::Quit)], i64::MAX);
        let pressed = std::sync::Arc::clone(&rig.pressed);
        let menu = std::sync::Arc::clone(&rig.menu_pressed);
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let (began, reading) = std::sync::mpsc::sync_channel(0);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let left = std::thread::scope(|s| {
            s.spawn(move || {
                reading.recv().unwrap();
                match case {
                    "back" => menu.lock().unwrap().push(game::MenuCommand::Back),
                    "quit" => pressed.lock().unwrap().push(Command::Quit),
                    _ => pressed.lock().unwrap().push(Command::Retry),
                }
                release.send(()).unwrap();
                // Every later read, released at once.
                while reading.recv().is_ok() {
                    release.send(()).unwrap();
                }
            });
            let mut loaded = closing_loaded_with(
                dir.path(),
                chart(),
                0,
                FailureAction::PlayToEnd,
                &events,
                None,
                TimeUs(1_000 * MS),
                |p| Parts {
                    records: Some(Box::new(BlockedLookup {
                        began,
                        release: released,
                    })),
                    ..p
                },
            );
            game::live::run_chart(&mut loaded, &mut rig)
        });
        let events = events.all();
        assert_eq!(left.unwrap(), game::Leave::Back, "{case}");
        assert!(saved(&rig.reports[0]), "{case}");
        assert!(!result_of(&events, 1), "{case}");
        assert_eq!(withdrawn(&rig), 1, "{case}");
        if case == "retry" {
            assert_eq!(rig.reports.len(), 2);
            assert_eq!(rig.shows, 1);
            assert!(result_of(&events, 2));
        } else {
            assert_eq!(rig.reports.len(), 1, "{case}");
            assert_eq!(rig.shows, 0, "{case}");
            assert!(rig.clock.now().0 < LEAVE_AT, "{case}");
        }
    }
}

// The screen failing as the preparation is found ready is built again
/// first, and a command or a close that arrives while it is rebuilt is
/// served before the result shows: quit and back leave, retry plays the
/// next attempt, a close ends the run; the first play's result never
/// shows.
#[test]
fn a_command_during_a_screen_rebuild_is_served_before_the_result() {
    for (pressed, plays, leave) in [
        (FirstPoll::Command(Command::Quit), 1, game::Leave::Back),
        (
            FirstPoll::Menu(game::MenuCommand::Back),
            1,
            game::Leave::Back,
        ),
        (FirstPoll::Command(Command::Retry), 2, game::Leave::Back),
        (FirstPoll::Close, 1, game::Leave::Quit),
    ] {
        let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
        rig.in_first_poll = Some(FirstPoll::ScreenFails);
        rig.in_rebuild = Some(pressed);
        let (left, events) = with_blocked_save(&mut rig, || {});
        assert_eq!(left, leave, "{pressed:?}");
        // The play's own screen preparation, then the rebuild.
        assert!(rig.screen_at.len() >= 2, "{pressed:?}");
        assert!(rig.screen_at[1] >= rig.preps[0], "{pressed:?}");
        assert_eq!(rig.reports.len(), plays, "{pressed:?}");
        assert!(saved(&rig.reports[0]), "{pressed:?}");
        assert!(!result_of(&events, 1), "{pressed:?}");
        assert_eq!(rig.shows, plays - 1, "{pressed:?}");
        assert_eq!(result_of(&events, 2), plays == 2, "{pressed:?}");
    }
}

/// Closing the window while the preparation is pending withdraws it and
/// ends the run, the play saved and no result shown.
#[test]
fn a_window_close_during_the_preparation_ends_the_run() {
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rig = Fake::new(Vec::new(), &[], FRESH_AT);
    rig.prep_gate = Some(gate);
    let (left, events) = with_blocked_save(&mut rig, || {});
    assert_eq!(left, game::Leave::Quit);
    assert!(saved(&rig.reports[0]));
    assert!(rig.preps[0] < FRESH_AT);
    assert_eq!(withdrawn(&rig), 1);
    assert_eq!(rig.shows, 0);
    assert!(!result_of(&events, 1));
    assert_eq!(rig.clock.now().0, FRESH_AT);
}

/// The screen failing while the preparation is pending is built again
/// between polls, before the result shows on it, as on the result; a
/// screen that cannot be built again ends the run with its error, no
/// result shown. Each pending poll waits at most `IDLE_WAIT`.
#[test]
fn a_screen_failure_during_the_preparation_rebuilds_it_first() {
    for rebuilt in [true, false] {
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut rig = Fake::new(Vec::new(), &[(LEAVE_AT, Command::Quit)], i64::MAX);
        rig.prep_gate = Some(std::sync::Arc::clone(&gate));
        rig.screen_fails_at = FRESH_AT;
        if !rebuilt {
            // The play's own preparation was the first; the next fails.
            rig.prepare_fails_from = 2;
        }
        rig.at.push((
            FRESH_AT + 10_000,
            Box::new(move || gate.store(true, std::sync::atomic::Ordering::SeqCst)),
        ));
        let (left, events) = try_with_blocked_save(&mut rig, || {});
        assert_eq!(rig.screen_at.len(), 2, "{rebuilt}");
        let at = rig.screen_at[1];
        assert!(
            (FRESH_AT..FRESH_AT + 10_000).contains(&at),
            "{rebuilt}: {at}"
        );
        assert!(rig.poll_deadlines.len() > 1, "{rebuilt}");
        for &(now, deadline) in &rig.poll_deadlines {
            assert_eq!(deadline, now + game::IDLE_WAIT.0, "{rebuilt}");
        }
        if rebuilt {
            assert_eq!(left.unwrap(), game::Leave::Back);
            assert_eq!(rig.shows, 1);
            assert!(rig.shown_at[0] > at);
            assert!(result_of(&events, 1));
        } else {
            assert!(matches!(left, Err(LiveError::Screen(SurfaceLost))));
            assert_eq!(rig.shows, 0);
            assert!(!result_of(&events, 1));
        }
        assert!(saved(&rig.reports[0]), "{rebuilt}");
    }
}

/// A preset step during the preparation is dropped: the result opens on
/// the played preset, as it does with no step.
#[test]
fn a_preset_step_during_the_preparation_is_dropped() {
    use game::MenuCommand::{NextPreset, PrevPreset};
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut rig = Fake::new(Vec::new(), &[(LEAVE_AT, Command::Quit)], i64::MAX);
    rig.prep_gate = Some(std::sync::Arc::clone(&gate));
    rig.menu = vec![
        (FRESH_AT, NextPreset),
        (FRESH_AT, PrevPreset),
        (FRESH_AT, NextPreset),
    ];
    rig.at.push((
        FRESH_AT + 1,
        Box::new(move || gate.store(true, std::sync::atomic::Ordering::SeqCst)),
    ));
    let (left, events) = with_blocked_save(&mut rig, || {});
    assert_eq!(left, game::Leave::Back);
    assert!(result_of(&events, 1));
    assert_eq!(rig.shows, 1);
    let (at, first) = &rig.screens[0];
    assert!(*at > FRESH_AT);
    assert_eq!(viewed(first), EntryId::BuiltIn("tapestry".to_owned()));
    assert_eq!(withdrawn(&rig), 0);
}

/// Before each opening: the device, then the preset refreshed, then audio
/// and the attempt on the screen with that preset, then its commit and the
/// end of the loading state; a retry goes through the same steps.
#[test]
fn the_attempt_is_prepared_after_the_preset_refresh_and_before_loading_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 12_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = [
        "screen",
        "before play",
        "audio",
        "attempt",
        "commit",
        "end loading",
        "open",
    ];
    assert_eq!(rig.steps, [one, one].concat());
    assert_eq!(
        rig.attempts.iter().map(|a| a.number).collect::<Vec<_>>(),
        [1, 2]
    );
}

/// The play preset is edited while the result views another preset: the
/// retry's attempt is prepared, fresh, with the refreshed play preset, its
/// gauges in their order, never with the viewed one.
#[test]
fn a_retry_prepares_with_the_refreshed_play_preset_not_the_viewed_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_user_preset(dir.path(), &rules::tapestry());
    let mut loaded = loaded_with(dir.path(), "mine");
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(11_500_000, Command::Retry)], 15_000_000);
    rig.menu = vec![(10_500_000, game::MenuCommand::NextPreset)];
    let mut edited = rules::tapestry();
    edited.gauges.reverse();
    for g in &mut edited.gauges {
        g.id = format!("{}-edited", g.id);
    }
    let to = edited.clone();
    rig.at
        .push((11_000_000, Box::new(move || rewrite_preset(&path, &to))));
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    // The result viewed another preset when the retry was taken.
    assert_ne!(viewed(shown(&rig, 11_400_000)), user("mine"));
    let [one, two, ..] = &rig.attempts[..] else {
        panic!("{:?}", rig.attempts.len());
    };
    assert_eq!(one.gauges, rules::tapestry().gauges);
    assert_eq!(two.gauges, edited.gauges);
    assert_eq!(two.preset.0, "mine");
    assert_eq!(rig.reports[1].preset, mine(edited));
    // Fresh per attempt: its own number, from the same chart.
    assert_eq!((one.number, two.number), (1, 2));
    assert_eq!(one.kind, two.kind);
}

/// A screen lost while a held START defers the opening ends the run as
/// before any opening, without waiting for the release: recording never
/// opens. Exercises `game::live::play_and_next` through `open_gated`.
#[test]
fn a_screen_lost_while_the_opening_is_deferred_opens_nothing() {
    const Q: ControlId = ControlId(0x10);
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(CLOCK_START, Q, 1), key(9_000_000, Q, 0)];
    let mut rig = Fake::new(items, &[], 11_000_000);
    let (mut tap, _rx) = input::adjustment_queue(input::AdjustmentBindings {
        controls: vec![(input::TapControl::Key(Q), 0)],
        latched: 1,
        wrap: None,
        ..input::AdjustmentBindings::default()
    })
    .unwrap();
    tap.set_active(true);
    rig.sources.set_adjustment(tap);
    rig.gated = true;
    rig.screen_fails_at = 8_000_000;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(matches!(ran, Err(LiveError::ScreenLost)), "{ran:?}");
    assert_eq!(rig.opens, 0);
    assert!(
        rig.clock.now().0 < 9_000_000,
        "it did not wait for the release"
    );
}

/// With the start gate (design.md "Play Lifecycle", the start gate), a
/// START held at the opening defers it: an attempt with a loading lifetime
/// resumes it (Preparing → Loading), one without waits in Preparing;
/// either way the loading state is quiesced again and the opening asked
/// again once the hold is released, and READY opens at that release.
/// Exercises `game::live::play_and_next` through `open_gated`.
#[test]
fn a_hold_at_the_opening_defers_it_with_or_without_a_loading_lifetime() {
    const Q: ControlId = ControlId(0x10);
    for lifetime in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let items = vec![key(CLOCK_START, Q, 1), key(8_000_000, Q, 0)];
        let mut rig = Fake::new(items, &[], 11_000_000);
        let (mut tap, _rx) = input::adjustment_queue(input::AdjustmentBindings {
            controls: vec![(input::TapControl::Key(Q), 0)],
            latched: 1,
            wrap: None,
            ..input::AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_active(true);
        rig.sources.set_adjustment(tap);
        rig.gated = true;
        rig.lifetime = lifetime;
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
        let waited = if lifetime {
            "resume loading"
        } else {
            "wait in preparing"
        };
        assert_eq!(
            rig.steps,
            [
                "screen",
                "before play",
                "audio",
                "attempt",
                "commit",
                "end loading",
                "deferred",
                waited,
                "end loading",
                "open",
            ]
        );
        // READY opened at the release, a full lead-in before chart time 0.
        assert_eq!(rig.origins.len(), 1);
        assert!(
            rig.origins[0].0 >= 8_000_000 + 1_000_000,
            "{:?}",
            rig.origins
        );
    }
}

/// The screen refuses the attempt: the run stops with the refusal before
/// anything opens or is reported, and the play's number goes back.
#[test]
fn a_refused_preparation_opens_nothing_and_gives_the_number_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(Vec::new(), &[], 12_500_000);
    let why = PreparationRefusal {
        location: Some("skin.json:7".to_owned()),
        reason: "a font cannot be drawn".to_owned(),
    };
    rig.preparations = vec![Preparation::Refused(why.clone())];
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    match ran {
        Err(LiveError::Preparation(r)) => assert_eq!(r, why),
        other => panic!("{other:?}"),
    }
    assert_eq!(why.to_string(), "skin.json:7: a font cannot be drawn");
    assert_eq!((rig.opens, rig.reports.len()), (0, 0));
    assert!(!rig.steps.contains(&"end loading"));
    // What the refused preparation readied is dropped, never committed.
    assert!(
        rig.steps.ends_with(&["attempt", "discard"]),
        "{:?}",
        rig.steps
    );
    assert!(!rig.steps.contains(&"commit"));
    assert_eq!(loaded.begin_play().attempt, 1);
}

/// A refusal of the attempt that comes with a failed audio output ends the
/// run with the output's failure, not with the refusal the player could go
/// back from; nothing opens.
#[test]
fn a_refused_preparation_never_hides_a_failed_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(Vec::new(), &[], 12_500_000);
    rig.preparations = vec![Preparation::Refused(PreparationRefusal {
        location: None,
        reason: "a font cannot be drawn".to_owned(),
    })];
    rig.output_failed = true;
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    assert!(matches!(ran, Err(LiveError::Output(_))), "{ran:?}");
    assert_eq!((rig.opens, rig.reports.len()), (0, 0));
}

/// A preparation slower than the player: the quit key during it cancels it
/// at the next wait, recording never opens and the chart is left; the
/// quit is taken with it. A close during it ends the run the same way.
#[test]
fn a_slow_preparation_stays_cancellable_and_never_opens_recording() {
    for (quit, close, leave) in [
        (7_400_000, i64::MAX, game::Leave::Back),
        (i64::MAX, 7_400_000, game::Leave::Quit),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let commands = if quit == i64::MAX {
            Vec::new()
        } else {
            vec![(quit, Command::Quit)]
        };
        let mut rig = Fake::new(Vec::new(), &commands, close);
        // Ten seconds of preparation.
        rig.prepare_waits = 10_000;
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, leave);
        assert_eq!((rig.opens, rig.reports.len()), (0, 0), "{leave:?}");
        assert!(!rig.steps.contains(&"end loading"), "{leave:?}");
        // A late answer of the cancelled preparation can never install:
        // it is discarded, and nothing is committed.
        assert!(
            rig.steps.ends_with(&["attempt", "discard"]),
            "{leave:?}: {:?}",
            rig.steps
        );
        assert!(!rig.steps.contains(&"commit"), "{leave:?}");
        // Cancelled at the wait that saw it, not at the preparation's end.
        assert_eq!(rig.clock.now().0, 7_400_000, "{leave:?}");
        assert!(rig.commands.is_empty(), "{leave:?}");
        assert_eq!(loaded.begin_play().attempt, 1, "{leave:?}");
    }
}

/// Whether each step's view published playing: false through
/// READY, true from the chart's start through the step that completes
/// the play, and false through the tail and once recording closed.
#[test]
fn the_play_phase_is_published_as_each_step_enters_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[], 12_500_000);
    rig.tail_waits = 50;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.reports[0].end, PlayEnd::Completed);
    let phases = &rig.phases[0];
    let completed = rig.measured[0].iter().position(|a| a.complete).unwrap();
    for (i, &(t, playing)) in phases.iter().enumerate() {
        let expected = t >= 0 && i <= completed;
        assert_eq!(playing, expected, "step {i} at {t}");
    }
    // The completing step entered playing; the tail did not.
    assert!(phases[completed].1);
    assert!(phases.len() > completed + 40);
    assert_eq!(rig.closed_phases, [false]);
}

/// The quit key during play stops it: no frame from the stop on is
/// playing.
#[test]
fn a_stopped_play_publishes_that_it_is_not_playing() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(Vec::new(), &[(9_000_000, Command::Quit)], 12_500_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.reports[0].end, PlayEnd::Aborted(AbortCause::Quit));
    assert!(rig.phases[0].iter().any(|&(_, p)| p));
    assert_eq!(rig.closed_phases, [false]);
}

/// A policy that ends the play when a survival gauge fails.
struct Strict;

impl game::SessionPolicy for Strict {
    fn id(&self) -> &str {
        "strict"
    }
    fn displayed_gauge(&self, _gauges: &[eval::GaugeState]) -> game::GaugeIndex {
        game::GaugeIndex(1)
    }
    fn on_survival_failure(&self) -> game::FailureAction {
        game::FailureAction::Close
    }
}

/// The step that processes a survival gauge's failure entered playing and
/// publishes so; the stop that follows does not.
#[test]
fn the_step_processing_a_gauge_failure_enters_playing() {
    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    let chart_path = dir.path().join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    // Sixteen unpressed notes 100 ms apart: the survival gauge fails at
    // the fifteenth miss.
    let notes = (0..16).fold(ChartBuilder::new(150_000), |b, i| {
        b.tap(lane(1), TimeUs((1_000 + i * 100) * MS))
    });
    let parts = Parts {
        ingest: Box::new(Built(notes.build().unwrap())),
        policy: Box::new(Strict),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    let mut loaded = Loaded::load(root, settings, &chart_path, config, parts).unwrap();
    let mut rig = Fake::new(Vec::new(), &[], 15_000_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(
        rig.reports[0].end,
        PlayEnd::Aborted(AbortCause::GaugeFailed)
    );
    assert_eq!(rig.phases[0].last().map(|&(_, p)| p), Some(true));
    assert_eq!(rig.closed_phases, [false]);
}

/// A quit key already pending when the preparation answers at once: the
/// attempt is never committed, but discarded, and recording never opens.
#[test]
fn a_quit_pending_at_an_immediate_preparation_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Fake::new(Vec::new(), &[(i64::MIN, Command::Quit)], i64::MAX);
    assert_eq!(rig.prepare_waits, 0);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    assert_eq!((rig.opens, rig.reports.len()), (0, 0));
    assert!(!rig.steps.contains(&"commit"), "{:?}", rig.steps);
    assert!(
        rig.steps.ends_with(&["attempt", "discard"]),
        "{:?}",
        rig.steps
    );
    assert!(rig.commands.is_empty());
}

/// A gated play admits the rig's exchange with its input side with its
/// recording (`Rig::gate_exchange`): one byte under the most that fits, the
/// play is admitted; at the most plus one, it is refused right after
/// recording opens, before it starts.
#[test]
fn a_gated_play_admits_the_rigs_exchange_to_the_byte() {
    let dir = tempfile::tempdir().unwrap();
    let probe = loaded(dir.path());
    let refused = |exchange| {
        probe
            .plan_gated(
                &[input::DeviceKind::Keyboard],
                &[Vec::new()],
                Some(game::GateStorage { held: 0, exchange }),
            )
            .is_err()
    };
    // The least exchange refused for the double's one keyboard, by
    // bisection.
    let (mut lo, mut hi) = (0, game::RECORDING_MEMORY);
    assert!(!refused(lo) && refused(hi));
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if refused(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    for (exchange, admitted) in [(lo, true), (hi, false)] {
        let mut loaded = loaded(dir.path());
        let mut rig = Fake::new(Vec::new(), &[], 12_500_000);
        rig.gated = true;
        rig.exchange = exchange;
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        if admitted {
            assert!(ran.is_ok(), "{exchange}: {ran:?}");
            assert_eq!(rig.reports.len(), 1, "{exchange}");
        } else {
            assert!(
                matches!(
                    ran,
                    Err(LiveError::Logic(game::LogicError::RecordingTooLarge { .. }))
                ),
                "{exchange}: {ran:?}"
            );
            assert!(rig.reports.is_empty());
        }
    }
}

/// Session events, collected.
#[derive(Clone, Default)]
struct Events(std::rc::Rc<std::cell::RefCell<Vec<game::SessionEvent>>>);

impl game::SessionEventSink for Events {
    fn on_event(&mut self, ev: &game::SessionEvent) {
        self.0.borrow_mut().push(*ev);
    }
}

impl Events {
    fn all(&self) -> Vec<game::SessionEvent> {
        self.0.borrow().clone()
    }
}

/// Sixteen notes on lane 1, 100 ms apart from 1 s: left unpressed, the
/// tapestry preset's survival gauge (its second) fails at the fifteenth
/// miss, at 2.58 s, and the last note's miss completes the play at 2.68 s.
fn sixteen() -> Chart {
    (0..16)
        .fold(ChartBuilder::new(150_000), |b, i| {
            b.tap(lane(1), TimeUs((1_000 + i * 100) * MS))
        })
        .build()
        .unwrap()
}

/// A play of `chart` through `player.toml`'s policy: the gauge
/// `displayed` shown, `on_failure` for its failure; the session's events
/// into `events`, autoplayed with `autoplay`, its READY `lead_in` long.
fn closing_loaded(
    dir: &Path,
    chart: Chart,
    displayed: usize,
    on_failure: game::FailureAction,
    events: &Events,
    autoplay: Option<game::AutoKeySounds>,
    lead_in: TimeUs,
) -> Loaded {
    closing_loaded_with(
        dir,
        chart,
        displayed,
        on_failure,
        events,
        autoplay,
        lead_in,
        |p| p,
    )
}

/// [`closing_loaded`] with its parts changed by `with`.
#[expect(
    clippy::too_many_arguments,
    reason = "closing_loaded's, and the change"
)]
fn closing_loaded_with(
    dir: &Path,
    chart: Chart,
    displayed: usize,
    on_failure: game::FailureAction,
    events: &Events,
    autoplay: Option<game::AutoKeySounds>,
    lead_in: TimeUs,
    with: impl FnOnce(Parts) -> Parts,
) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    settings.player.displayed_gauge = displayed;
    settings.player.on_failure = on_failure;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let parts = with(Parts {
        ingest: Box::new(Built(chart)),
        events: Box::new(events.clone()),
        ..Parts::standard(&root, &settings.player)
    });
    let config = SessionConfig {
        lead_in,
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay,
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// [`closing_loaded`] of [`sixteen`] with the survival gauge shown.
fn failing(dir: &Path, on_failure: game::FailureAction, events: &Events) -> Loaded {
    closing_loaded(
        dir,
        sixteen(),
        1,
        on_failure,
        events,
        None,
        TimeUs(1_000 * MS),
    )
}

/// The play's origin on the rig's clock: READY opens at the clock's
/// start, 1 s before chart time 0.
const ORIGIN: i64 = CLOCK_START + 1_000_000;

use game::{ClosingEnd, ClosingReport, FailureAction, SessionEvent};

/// Where the sixteen-note play's survival failure is processed: the step
/// at 2.584 s of play time, its advance finalizing 2.580 999 s.
const FAILS_AT: i64 = 2_584_000;
const FAILING_WATERMARK: i64 = 2_580_999;

fn closing(report: &PlayReport) -> ClosingReport {
    report.closing.expect("the play entered Closing")
}

fn start(attempt: u32) -> SessionEvent {
    SessionEvent::Start { attempt }
}

fn end(attempt: u32, status: replay::EndStatus) -> SessionEvent {
    SessionEvent::End { attempt, status }
}

/// The displayed survival gauge's failure under `close` enters Closing in
/// the step of the failing advance: the view published at its entry has
/// Closing's start and is not playing; Closing runs out at the first step
/// 1 s on, and only then is the play saved, ended midway at the failing
/// watermark, under `close-on-failure`, and its result shown. The saved
/// record re-evaluates to the live result, the gauge's real failure time
/// kept.
#[test]
fn a_displayed_survival_failure_closes_the_play() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
    // The failing advance's step, the last that published playing.
    assert_eq!(rig.phases[0].last(), Some(&(FAILS_AT, true)));
    assert_eq!(
        rig.measured[0].last().unwrap().watermark,
        Some(TimeUs(FAILING_WATERMARK))
    );
    assert_eq!(
        closing(one),
        ClosingReport {
            started_at: TimeUs(FAILS_AT),
            ended_at: TimeUs(FAILS_AT + 1_000_000),
            end: ClosingEnd::RanOut,
        }
    );
    assert_eq!(
        rig.abort_closes,
        [(ORIGIN + FAILS_AT, Some(TimeUs(FAILS_AT)), false)]
    );
    assert_eq!(rig.end_feedbacks, 1);
    assert_eq!(rig.shown_at, [ORIGIN + FAILS_AT + 1_000_000]);
    let record = one.record.as_ref().unwrap();
    assert_eq!(record.status.end, replay::EndStatus::EndedMidway);
    assert_eq!(record.status.finalized, TimeUs(FAILING_WATERMARK));
    assert_eq!(record.session_policy, "close-on-failure");
    let result = one.result.as_ref().unwrap();
    assert!(result.matches_live);
    let gauges = &result.evaluation.as_ref().unwrap().gauges;
    assert_eq!(gauges[1].failed_at, Some(TimeUs(2_580_000)));
    assert_eq!(
        events.all(),
        [
            SessionEvent::Load,
            start(1),
            end(1, replay::EndStatus::EndedMidway),
            SessionEvent::Result { attempt: 1 },
        ]
    );
}

/// A hidden survival gauge's failure stops nothing: with the first gauge
/// shown, the play completes with its tail, no Closing, the hidden
/// gauge's failure kept.
#[test]
fn a_hidden_survival_failure_plays_to_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = closing_loaded(
        dir.path(),
        sixteen(),
        0,
        FailureAction::Close,
        &events,
        None,
        TimeUs(1_000 * MS),
    );
    let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.closing, None);
    assert!(rig.abort_closes.is_empty());
    let gauges = &one
        .result
        .as_ref()
        .unwrap()
        .evaluation
        .as_ref()
        .unwrap()
        .gauges;
    assert_eq!(gauges[1].failed_at, Some(TimeUs(2_580_000)));
}

/// Under play-to-end the shown survival gauge fails and the play goes on
/// to its end, with no Closing, under `play-to-end`.
#[test]
fn play_to_end_keeps_playing_after_a_displayed_failure() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::PlayToEnd, &events);
    let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.closing, None);
    let record = one.record.as_ref().unwrap();
    assert_eq!(record.status.end, replay::EndStatus::PlayedToEnd);
    assert_eq!(record.session_policy, "play-to-end");
    let gauges = &one
        .result
        .as_ref()
        .unwrap()
        .evaluation
        .as_ref()
        .unwrap()
        .gauges;
    assert_eq!(gauges[1].failed_at, Some(TimeUs(2_580_000)));
}

/// The quit key while the chart plays, its gauges healthy, enters Closing
/// with cause `Quit` under either `on_failure`: nothing is evaluated during
/// it, the record ends at the step before the quit, and the result shows
/// once it ran out: Start, End, Result.
#[test]
fn the_quit_key_during_play_enters_closing() {
    for on_failure in [FailureAction::Close, FailureAction::PlayToEnd] {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let mut loaded = closing_loaded(
            dir.path(),
            chart(),
            1,
            on_failure,
            &events,
            None,
            TimeUs(1_000 * MS),
        );
        let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
        let commands = [(9_500_000, Command::Quit), (15_000_000, Command::Quit)];
        let mut rig = Fake::new(items, &commands, i64::MAX);
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, game::Leave::Back, "{on_failure:?}");
        let one = &rig.reports[0];
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
        assert_eq!(
            closing(one),
            ClosingReport {
                started_at: TimeUs(1_200_000),
                ended_at: TimeUs(2_200_000),
                end: ClosingEnd::RanOut,
            }
        );
        // Healthy: nothing failed.
        let gauges = &one
            .result
            .as_ref()
            .unwrap()
            .evaluation
            .as_ref()
            .unwrap()
            .gauges;
        assert!(gauges.iter().all(|g| g.failed_at.is_none()));
        // No step after the quit's: the watermark and the events stop.
        let last = rig.measured[0].iter().rev().find_map(|a| a.watermark);
        assert_eq!(rig.phases[0].last().unwrap().0, 1_200_000);
        let record = one.record.as_ref().unwrap();
        assert_eq!(Some(record.status.finalized), last);
        assert_eq!(record.status.end, replay::EndStatus::EndedMidway);
        assert_eq!(rig.shown_at, [ORIGIN + 2_200_000]);
        assert_eq!(
            events.all(),
            [
                SessionEvent::Load,
                start(1),
                end(1, replay::EndStatus::EndedMidway),
                SessionEvent::Result { attempt: 1 },
            ]
        );
    }
}

/// The quit key in a full play's READY (its lead-in) leaves with no
/// result and no Closing.
#[test]
fn a_quit_in_ready_leaves_without_closing() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let mut rig = Fake::new(Vec::new(), &[(ORIGIN - 1_000, Command::Quit)], i64::MAX);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    assert_eq!(rig.reports[0].closing, None);
    assert!(rig.abort_closes.is_empty());
    assert_eq!(rig.shows, 0);
}

/// In autoplay the quit key keeps its meaning: during play it aborts the
/// play and its result shows at once, with no Closing; in READY it
/// leaves. A gauge failure still closes an autoplayed play.
#[test]
fn autoplay_quits_at_once_and_closes_on_failure() {
    for (at, shown) in [(ORIGIN - 1_000, false), (ORIGIN + 1_200_000, true)] {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let mut loaded = closing_loaded(
            dir.path(),
            chart(),
            1,
            FailureAction::Close,
            &events,
            Some(game::AutoKeySounds::Scheduled),
            TimeUs(1_000 * MS),
        );
        let commands = [(at, Command::Quit), (15_000_000, Command::Quit)];
        let mut rig = Fake::new(Vec::new(), &commands, i64::MAX);
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, game::Leave::Back);
        let one = &rig.reports[0];
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit), "{at}");
        assert_eq!(one.closing, None, "{at}");
        assert_eq!(rig.shown_at, if shown { vec![at] } else { vec![] });
    }
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = closing_loaded(
        dir.path(),
        sixteen(),
        1,
        FailureAction::Close,
        &events,
        Some(game::AutoKeySounds::Scheduled),
        TimeUs(1_000 * MS),
    );
    let mut rig = Fake::new(Vec::new(), &[(30_000_000, Command::Quit)], i64::MAX);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
    assert_eq!(closing(one).end, ClosingEnd::RanOut);
}

/// Where the sixteen-note play's Closing starts and runs out, on the
/// rig's clock.
const CLOSING_AT: i64 = ORIGIN + FAILS_AT;
const RUNS_OUT_AT: i64 = CLOSING_AT + 1_000_000;

/// A rig for the sixteen-note play: `commands`, then the quit key at 30 s
/// that leaves the result.
fn failing_rig(commands: &[(i64, Command)]) -> Fake {
    let mut all = commands.to_vec();
    all.push((30_000_000, Command::Quit));
    Fake::new(Vec::new(), &all, i64::MAX)
}

/// The quit key during Closing does nothing, fresh, repeated, or carried
/// from the close handshake: each is taken as Closing's, Closing runs out
/// and its result shows, which only the quit at 30 s leaves.
#[test]
fn a_quit_in_closing_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let mut rig = failing_rig(&[
        (11_000_000, Command::Quit),
        (11_200_000, Command::Quit),
        (11_200_000, Command::Quit),
    ]);
    rig.in_close = Some(Command::Quit);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    assert!(rig.clock.now().0 >= 30_000_000);
    assert_eq!(closing(&rig.reports[0]).end, ClosingEnd::RanOut);
    assert_eq!(
        closing(&rig.reports[0]).ended_at,
        TimeUs(FAILS_AT + 1_000_000)
    );
    assert_eq!(
        rig.closing_taken,
        [
            (CLOSING_AT, Command::Quit),
            (11_000_000, Command::Quit),
            (11_200_000, Command::Quit),
            (11_200_000, Command::Quit),
        ]
    );
    assert_eq!(rig.shown_at, [RUNS_OUT_AT]);
}

/// A record sink that presses `command` once, while the play is saved.
struct PressOnce(
    std::sync::Arc<std::sync::Mutex<Vec<Command>>>,
    Option<Command>,
);

impl game::RecordSink for PressOnce {
    fn save(&mut self, _rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        if let Some(c) = self.1.take() {
            self.0.lock().unwrap().push(c);
        }
        Ok(game::RecordRef {
            path: "pressed.tprl".into(),
        })
    }
}

/// A retry during Closing ends it at that wake, fresh, carried from the
/// close handshake, or behind a quit in the same drain: the play is saved
/// ended midway, its cause kept, and retried with no result: Retry, End,
/// then the next Start. Retries in the same wake and while it is saved
/// add no attempt.
#[test]
fn a_retry_in_closing_saves_and_retries_once() {
    let fresh = [(11_000_000, Command::Retry), (11_000_000, Command::Retry)];
    let behind = [(11_000_000, Command::Quit), (11_000_000, Command::Retry)];
    for (case, commands, carried, at) in [
        ("fresh", &fresh[..], None, 11_000_000),
        ("carried", &[][..], Some(Command::Retry), CLOSING_AT),
        ("behind a quit", &behind[..], None, 11_000_000),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let pressed = std::sync::Arc::default();
        let mut loaded = closing_loaded_with(
            dir.path(),
            sixteen(),
            1,
            FailureAction::Close,
            &events,
            None,
            TimeUs(1_000 * MS),
            |p| Parts {
                records: Some(Box::new(PressOnce(
                    std::sync::Arc::clone(&pressed),
                    Some(Command::Retry),
                ))),
                ..p
            },
        );
        let mut rig = failing_rig(commands);
        rig.pressed = pressed;
        rig.in_close = carried;
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, game::Leave::Back, "{case}");
        let one = &rig.reports[0];
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed), "{case}");
        assert_eq!(
            closing(one),
            ClosingReport {
                started_at: TimeUs(FAILS_AT),
                ended_at: TimeUs(at - ORIGIN),
                end: ClosingEnd::Retry,
            },
            "{case}"
        );
        assert_eq!(status(one).end, replay::EndStatus::EndedMidway, "{case}");
        // One next attempt, which fails and closes in turn; its result is
        // the only one.
        assert_eq!(rig.reports.len(), 2, "{case}");
        assert_eq!(rig.shows, 1, "{case}");
        assert_eq!(
            events.all()[..6],
            [
                SessionEvent::Load,
                start(1),
                SessionEvent::Retry { attempt: 1 },
                end(1, replay::EndStatus::EndedMidway),
                start(2),
                end(2, replay::EndStatus::EndedMidway),
            ],
            "{case}"
        );
        assert_eq!(events.all()[6..], [SessionEvent::Result { attempt: 2 }]);
    }
}

/// Closing the window during Closing cuts it at once and ends the run:
/// the play is saved, and no result shows. With the longest close a skin
/// can declare, the deadline does not overflow.
#[test]
fn a_window_close_in_closing_saves_and_ends_the_run() {
    for declared in [None, Some(TimeUs(i64::from(i32::MAX) * 1_000))] {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let mut loaded = failing(dir.path(), FailureAction::Close, &events);
        let mut rig = Fake::new(Vec::new(), &[], CLOSING_AT + 1_000);
        rig.declared_close = declared;
        let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
        assert_eq!(left, game::Leave::Quit);
        let one = &rig.reports[0];
        assert_eq!(
            closing(one),
            ClosingReport {
                started_at: TimeUs(FAILS_AT),
                ended_at: TimeUs(FAILS_AT + 1_000),
                end: ClosingEnd::Cut(AbortCause::Quit),
            }
        );
        assert!(matches!(one.saved, Some(Ok(_))), "{:?}", one.saved);
        assert_eq!(rig.shows, 0);
        assert_eq!(
            events.all(),
            [
                SessionEvent::Load,
                start(1),
                end(1, replay::EndStatus::EndedMidway)
            ]
        );
    }
}

/// Closing the window while the chart plays aborts the play with no
/// Closing.
#[test]
fn a_window_close_during_play_enters_no_closing() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let mut rig = Fake::new(Vec::new(), &[], ORIGIN + 1_200_000);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Quit);
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_eq!(one.closing, None);
    assert!(rig.abort_closes.is_empty());
}

/// Focus loss and the removal of the last device during Closing change
/// nothing: it runs out.
#[test]
fn focus_loss_and_device_removal_in_closing_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let focus = ScriptItem {
        at: TimeUs(11_000_000),
        action: Action::FocusLoss,
    };
    let mut rig = Fake::new(vec![focus], &[(30_000_000, Command::Quit)], i64::MAX);
    rig.disconnected_at = 11_100_000;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(closing(one).end, ClosingEnd::RanOut);
    assert_eq!(closing(one).ended_at, TimeUs(FAILS_AT + 1_000_000));
    assert_eq!(rig.shown_at, [RUNS_OUT_AT]);
}

/// A suspend in the drain that enters Closing (behind the quit key) or at
/// the close handshake cuts Closing at once, its cause kept, and marks
/// the record; one during Closing cuts it and leaves the record complete;
/// a retry beside a suspend in Closing retries.
#[test]
fn a_suspend_cuts_closing_and_marks_only_what_recording_saw() {
    // With the quit that enters it, in the play's last drain.
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = closing_loaded(
        dir.path(),
        chart(),
        1,
        FailureAction::Close,
        &events,
        None,
        TimeUs(1_000 * MS),
    );
    let commands = [
        (9_500_000, Command::Quit),
        (9_500_000, Command::Suspend),
        (15_000_000, Command::Quit),
    ];
    let mut rig = Fake::new(Vec::new(), &commands, i64::MAX);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_eq!(
        closing(one),
        ClosingReport {
            started_at: TimeUs(1_200_000),
            ended_at: TimeUs(1_200_000),
            end: ClosingEnd::Cut(AbortCause::Suspend),
        }
    );
    assert!(status(one).incomplete.suspend);
    assert_eq!(rig.shown_at, [9_500_000]);

    // At the handshake, during Closing, and beside a retry.
    for (case, carried, commands, ended, marked) in [
        (
            "handshake",
            Some(Command::Suspend),
            vec![],
            CLOSING_AT,
            true,
        ),
        (
            "closing",
            None,
            vec![(11_000_000, Command::Suspend)],
            11_000_000,
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
        let mut rig = failing_rig(&commands);
        rig.in_close = carried;
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
        let one = &rig.reports[0];
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed), "{case}");
        assert_eq!(
            closing(one).end,
            ClosingEnd::Cut(AbortCause::Suspend),
            "{case}"
        );
        assert_eq!(closing(one).ended_at, TimeUs(ended - ORIGIN), "{case}");
        assert_eq!(status(one).incomplete.suspend, marked, "{case}");
        assert_eq!(rig.shown_at, [ended], "{case}");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
    let mut rig = failing_rig(&[(11_000_000, Command::Suspend), (11_000_000, Command::Retry)]);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(closing(&rig.reports[0]).end, ClosingEnd::Retry);
    assert!(!status(&rig.reports[0]).incomplete.suspend);
    assert_eq!(rig.reports.len(), 2);
}

/// The input thread's, the audio output's and the screen's failures cut
/// Closing to the result, the play's cause kept and its record complete,
/// with their diagnostics; one at Closing's deadline is still latched.
#[test]
fn failures_cut_closing_with_their_diagnostics() {
    for at in [11_000_000, RUNS_OUT_AT] {
        // The input thread.
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
        let mut rig = failing_rig(&[]);
        rig.input_fails_at = at;
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        assert!(
            matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
            "{ran:?}"
        );
        let one = &rig.reports[0];
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
        assert_eq!(
            closing(one).end,
            ClosingEnd::Cut(AbortCause::InputFailed),
            "{at}"
        );
        assert_eq!(closing(one).ended_at, TimeUs(at - ORIGIN));
        assert_eq!(one.input_error, Some(InputWaitError::Gone));
        assert!(!status(one).incomplete.any());
        assert_eq!(rig.shown_at, [at]);

        // The audio output.
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
        let mut rig = failing_rig(&[]);
        rig.output_fails_at = at;
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
        let one = &rig.reports[0];
        assert_eq!(
            closing(one).end,
            ClosingEnd::Cut(AbortCause::OutputFailed),
            "{at}"
        );
        assert_eq!(closing(one).ended_at, TimeUs(at - ORIGIN));
        assert_eq!(one.output_error, Some(audio::OutputError::DeviceLost));
        assert_eq!(rig.shown_at, [at]);
    }
    // The screen: built again before the result shows.
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
    let mut rig = failing_rig(&[]);
    rig.screen_fails_at = 11_000_000;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(closing(one).end, ClosingEnd::Cut(AbortCause::ScreenFailed));
    assert_eq!(closing(one).ended_at, TimeUs(11_000_000 - ORIGIN));
}

/// Closing lasts the skin's declared close, at least 1 s: none, zero,
/// negative and 500 ms give the floor, 3 s gives 3 s.
#[test]
fn closing_lasts_the_declared_close_at_least_the_floor() {
    for (declared, lasts) in [
        (None, 1_000_000),
        (Some(0), 1_000_000),
        (Some(-1), 1_000_000),
        (Some(500_000), 1_000_000),
        (Some(3_000_000), 3_000_000),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = failing(dir.path(), FailureAction::Close, &Events::default());
        let mut rig = failing_rig(&[]);
        rig.declared_close = declared.map(TimeUs);
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
        let c = closing(&rig.reports[0]);
        assert_eq!(c.end, ClosingEnd::RanOut, "{declared:?}");
        assert_eq!(c.ended_at.0 - c.started_at.0, lasts, "{declared:?}");
    }
}

/// The duration and the deadline, exactly: the floor, no cap, and a
/// deadline that saturates at the end of the clock.
#[test]
fn closing_time_and_deadline_saturate() {
    let max_ms = TimeUs(i64::from(i32::MAX) * 1_000);
    assert_eq!(game::CLOSE_FLOOR, TimeUs(1_000_000));
    assert_eq!(game::closing_time(None), game::CLOSE_FLOOR);
    assert_eq!(
        game::closing_time(Some(TimeUs(i64::MIN))),
        game::CLOSE_FLOOR
    );
    assert_eq!(
        game::closing_time(Some(TimeUs(1_000_001))),
        TimeUs(1_000_001)
    );
    assert_eq!(game::closing_time(Some(max_ms)), max_ms);
    assert_eq!(
        game::closing_deadline(TimeUs(i64::MAX - 5), Some(max_ms)),
        TimeUs(i64::MAX)
    );
    assert_eq!(
        game::closing_deadline(TimeUs(i64::MAX - 5), None),
        TimeUs(i64::MAX)
    );
    assert_eq!(
        game::closing_deadline(TimeUs(2_000), Some(max_ms)),
        TimeUs(2_000 + max_ms.0)
    );
}

/// A quit at the chart's start before any advance, READY having no
/// lead-in: Closing still runs and its result shows, and with nothing
/// finalized no record is made or saved.
#[test]
fn a_closing_with_no_watermark_saves_nothing_and_shows_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = closing_loaded(
        dir.path(),
        chart(),
        1,
        FailureAction::Close,
        &events,
        None,
        TimeUs(0),
    );
    let commands = [
        (CLOCK_START + 1_000, Command::Quit),
        (15_000_000, Command::Quit),
    ];
    let mut rig = Fake::new(Vec::new(), &commands, i64::MAX);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let one = &rig.reports[0];
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_eq!(closing(one).end, ClosingEnd::RanOut);
    assert_eq!((one.record.as_ref(), one.saved.as_ref()), (None, None));
    assert_eq!(rig.shown_at, [CLOCK_START + 1_001_000]);
}

/// An abort close that fails, while the ordinary close would succeed,
/// marks the record incomplete (input failed) with no arrivals, and the
/// failure cuts Closing; a retry carried from that handshake still ends
/// Closing first, and the run then ends with the input cause after the
/// Retry and End events, with no next attempt.
#[test]
fn a_failed_abort_close_marks_the_record_and_ends_the_run() {
    for carried in [None, Some(Command::Retry)] {
        let dir = tempfile::tempdir().unwrap();
        let events = Events::default();
        let mut loaded = failing(dir.path(), FailureAction::Close, &events);
        let mut rig = failing_rig(&[]);
        rig.abort_close_fails = Some(InputWaitError::Timeout);
        rig.in_close = carried;
        let ran = game::live::run_chart(&mut loaded, &mut rig);
        assert!(
            matches!(ran, Err(LiveError::Input(InputWaitError::Timeout))),
            "{ran:?}"
        );
        assert_eq!(rig.abort_closes.len(), 1);
        assert_eq!(rig.end_feedbacks, 0);
        let [one] = &rig.reports[..] else {
            panic!("{} plays", rig.reports.len());
        };
        let record = one.record.as_ref().unwrap();
        assert!(record.status.incomplete.input_failed);
        assert!(record.arrivals.is_empty());
        let retried = carried.is_some();
        assert_eq!(
            closing(one).end,
            if retried {
                ClosingEnd::Retry
            } else {
                ClosingEnd::Cut(AbortCause::InputFailed)
            }
        );
        let mut expected = vec![SessionEvent::Load, start(1)];
        if retried {
            expected.push(SessionEvent::Retry { attempt: 1 });
        }
        expected.push(end(1, replay::EndStatus::EndedMidway));
        if !retried {
            expected.push(SessionEvent::Result { attempt: 1 });
        }
        assert_eq!(events.all(), expected);
    }
}

/// Closing the window with a retry in the same Closing wake ends the run:
/// no Retry event, no result.
#[test]
fn a_window_close_beside_a_retry_in_closing_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut loaded = failing(dir.path(), FailureAction::Close, &events);
    let mut rig = Fake::new(Vec::new(), &[(11_000_000, Command::Retry)], 11_000_000);
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Quit);
    assert_eq!(
        closing(&rig.reports[0]).end,
        ClosingEnd::Cut(AbortCause::Quit)
    );
    assert_eq!(
        events.all(),
        [
            SessionEvent::Load,
            start(1),
            end(1, replay::EndStatus::EndedMidway)
        ]
    );
}

/// A shown gauge the preset does not have resolves to the first, once for
/// the attempt: the attempt the screen prepares, what closes the play and
/// the report all use it, so the sixteen misses, failing only the hidden
/// survival gauge, play to the end. Under the survival gauge's own index
/// every one of them is the survival gauge.
#[test]
fn an_unknown_shown_gauge_resolves_to_the_first_everywhere() {
    for (displayed, resolved, closes) in [(5, 0, false), (1, 1, true)] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = closing_loaded(
            dir.path(),
            sixteen(),
            displayed,
            FailureAction::Close,
            &Events::default(),
            None,
            TimeUs(1_000 * MS),
        );
        let mut rig = failing_rig(&[]);
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
        let index = game::GaugeIndex(resolved);
        assert_eq!(loaded.displayed(), index, "{displayed}");
        assert_eq!(rig.attempts[0].displayed, index, "{displayed}");
        let one = &rig.reports[0];
        assert_eq!(one.displayed_gauge, index, "{displayed}");
        assert_eq!(one.closing.is_some(), closes, "{displayed}");
    }
}

/// A record sink that saves locally, answers the play's count as `count`
/// says, and answers each view's best before the play by its preset's
/// hash, noting each key it is asked.
struct BestsSink {
    local: game::LocalRecords,
    count: Option<game::BestView>,
    answers: Vec<([u8; 32], game::BestLookup)>,
    asked: std::sync::Arc<std::sync::Mutex<Vec<game::BestKey>>>,
}

impl game::RecordSink for BestsSink {
    fn save(&mut self, rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        self.local.save(rec)
    }

    fn after_result(&mut self, _: &game::SavedPlay<'_>) -> Option<game::BestView> {
        self.count.clone()
    }

    fn best_before(&mut self, key: &game::BestKey, _: &rules::RulePreset) -> game::BestLookup {
        self.asked.lock().unwrap().push(*key);
        self.answers
            .iter()
            .find(|(h, _)| *h == key.preset_sha256)
            .map_or(game::BestLookup::Unavailable, |(_, a)| a.clone())
    }
}

/// [`loaded`] of `chart` with a [`BestsSink`].
fn loaded_with_bests(
    dir: &Path,
    chart: Chart,
    count: Option<game::BestView>,
    answers: Vec<([u8; 32], game::BestLookup)>,
) -> (Loaded, std::sync::Arc<std::sync::Mutex<Vec<game::BestKey>>>) {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let asked = std::sync::Arc::default();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        records: Some(Box::new(BestsSink {
            local: game::LocalRecords::new(root.records()),
            count,
            answers,
            asked: std::sync::Arc::clone(&asked),
        })),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    (
        Loaded::load(root, settings, &chart_path, config, parts).unwrap(),
        asked,
    )
}

/// A best of `ex`, `bp` and `clear`.
fn best(ex: u32, bp: u32, clear: game::BestLamp) -> game::BestScore {
    game::BestScore {
        plays: 3,
        ex: Some(ex),
        bp: Some(bp),
        clear: Some(clear),
    }
}

/// The best comparison a result screen shows, which must be read.
fn compared(screen: &ResultScreen) -> game::BestCompare {
    ready(screen)
        .best
        .expect("the best was read before the view showed")
}

/// C7: the own view of a counted play compares with its count's previous
/// best, read from no store; another view with the store's best under its
/// preset before the play, read once per view; a known-empty best is the
/// source's empty score.
#[test]
fn each_view_compares_with_its_presets_best_before_the_play() {
    use game::BestCompare::Known;
    use game::MenuCommand::{NextPreset, PrevPreset};
    use game::{BestFigures, BestLamp, BestLookup, BestRate, BestView, Rate};
    let dir = tempfile::tempdir().unwrap();
    let hash = |p: &rules::RulePreset| replay::preset_hash(p);
    let count = BestView::Counted {
        play: (0, 0),
        previous: Some(best(1, 5, BestLamp::Cleared { gauge: 0 })),
        current: best(2, 1, BestLamp::Cleared { gauge: 0 }),
        ineligible: None,
    };
    let answers = vec![
        (
            hash(&rules::beatoraja_style()),
            BestLookup::Known(best(3, 0, BestLamp::FullCombo)),
        ),
        (hash(&rules::lr2_style()), BestLookup::KnownEmpty),
        // Never asked: the own view takes the count's previous best.
        (hash(&rules::tapestry()), BestLookup::KnownEmpty),
    ];
    let (mut loaded, asked) = loaded_with_bests(dir.path(), chart(), Some(count), answers);
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(13_000_000, Command::Retry)], 16_000_000);
    rig.menu = vec![
        (11_000_000, NextPreset),
        (11_100_000, NextPreset),
        (11_200_000, PrevPreset),
        (11_300_000, NextPreset),
        (11_400_000, NextPreset),
    ];
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let own = ready(shown(&rig, 10_999_000));
    let (ex, bp) = (own.tally.ex_score, own.tally.bp);
    // Two notes at the tapestry preset's largest weight.
    assert_eq!((own.figures.notes, own.figures.max_ex), (2, 4));
    assert!(ex > 1 && bp == 1, "{:?}", own.tally);
    // The view's figures come from its evaluation: one press, one sample.
    let events = &rig.reports[0]
        .result
        .as_ref()
        .unwrap()
        .evaluation
        .as_ref()
        .unwrap()
        .events;
    assert_eq!(own.figures.timing, game::TimingStats::of(events));
    assert_eq!(own.figures.timing.map(|t| t.samples), Some(1));
    // One press and one miss: its deviation and a second, over two notes.
    assert_eq!(own.figures.duration_us, game::mean_duration(events, 2));
    assert!(own.figures.duration_us.is_some_and(|d| d >= 500_000));
    assert_eq!(own.figures.rate, game::Rate::of(ex, 4));
    let rate = |best_ex| {
        Some(BestRate {
            rate: Rate::of(best_ex, 4).unwrap(),
            updated: ex > best_ex,
        })
    };
    assert_eq!(
        compared(shown(&rig, 10_999_000)),
        Known(BestFigures {
            ex: 1,
            ex_diff: i64::from(ex) - 1,
            bp: Some(5),
            bp_diff: Some(i64::from(bp) - 5),
            clear: Some(BestLamp::Cleared { gauge: 0 }),
            ex_updated: true,
            bp_updated: true,
            rate: rate(1),
        })
    );
    let beatoraja = ready(shown(&rig, 11_050_000));
    let max = beatoraja.figures.max_ex;
    assert_eq!(
        compared(shown(&rig, 11_050_000)),
        Known(BestFigures {
            ex: 3,
            ex_diff: i64::from(beatoraja.tally.ex_score) - 3,
            bp: Some(0),
            bp_diff: Some(i64::from(beatoraja.tally.bp)),
            clear: Some(BestLamp::FullCombo),
            ex_updated: beatoraja.tally.ex_score > 3,
            bp_updated: false,
            rate: Some(BestRate {
                rate: Rate::of(3, max).unwrap(),
                updated: beatoraja.tally.ex_score > 3,
            }),
        })
    );
    let lr2 = ready(shown(&rig, 11_150_000));
    let empty = compared(shown(&rig, 11_150_000));
    assert_eq!(
        empty,
        Known(BestFigures {
            ex: 0,
            ex_diff: i64::from(lr2.tally.ex_score),
            bp: None,
            bp_diff: None,
            clear: None,
            ex_updated: lr2.tally.ex_score > 0,
            bp_updated: true,
            rate: Some(BestRate {
                rate: Rate::of(0, lr2.figures.max_ex).unwrap(),
                updated: lr2.tally.ex_score > 0,
            }),
        })
    );
    // Back on each view, its best is the one read before: each key asked
    // once, the own never.
    assert_eq!(
        compared(shown(&rig, 11_250_000)),
        compared(shown(&rig, 11_050_000))
    );
    assert_eq!(compared(shown(&rig, 11_350_000)), empty);
    assert_eq!(
        compared(shown(&rig, 11_450_000)),
        compared(shown(&rig, 10_999_000))
    );
    let chart_sha = rig.reports[0]
        .record
        .as_ref()
        .unwrap()
        .chart
        .identity
        .sha256;
    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [hash(&rules::beatoraja_style()), hash(&rules::lr2_style())].map(|preset_sha256| {
            game::BestKey {
                chart_sha256: chart_sha,
                preset_sha256,
            }
        })
    );
}

/// C7: a play that was not counted reads its own view's best from the
/// store too, and an unavailable answer leaves every best value
/// unavailable; a chart that declares `#RANDOM` has no best rate, while
/// its EX and BP differences stay.
#[test]
fn an_uncounted_play_reads_its_own_best_and_a_random_chart_has_no_best_rate() {
    use game::{BestCompare, BestLookup, BestView};
    let tapestry = replay::preset_hash(&rules::tapestry());
    let items = || vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let not_counted = || Some(BestView::NotCounted("not saved".to_owned()));
    // Unavailable: nothing answers for the key.
    let dir = tempfile::tempdir().unwrap();
    let (mut loaded, asked) = loaded_with_bests(dir.path(), chart(), not_counted(), Vec::new());
    let mut rig = Fake::new(items(), &[(11_000_000, Command::Retry)], 12_000_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(compared(shown(&rig, 10_999_000)), BestCompare::Unavailable);
    assert_eq!(asked.lock().unwrap()[0].preset_sha256, tapestry);
    // Known on a `#RANDOM` chart: the differences, no rate.
    let dir = tempfile::tempdir().unwrap();
    let mut random = chart();
    random.identity.random_results = vec![2];
    let answers = vec![(
        tapestry,
        BestLookup::Known(best(1, 5, game::BestLamp::Failed)),
    )];
    let (mut loaded, _) = loaded_with_bests(dir.path(), random, not_counted(), answers);
    let mut rig = Fake::new(items(), &[(11_000_000, Command::Retry)], 12_000_000);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let own = ready(shown(&rig, 10_999_000));
    let BestCompare::Known(b) = compared(shown(&rig, 10_999_000)) else {
        panic!("unavailable");
    };
    assert_eq!(b.ex_diff, i64::from(own.tally.ex_score) - 1);
    assert_eq!(b.bp_diff, Some(i64::from(own.tally.bp) - 5));
    assert_eq!(b.clear, Some(game::BestLamp::Failed));
    assert_eq!(b.rate, None);
    // The view's own rate still shows.
    assert!(own.figures.rate.is_some());
}
