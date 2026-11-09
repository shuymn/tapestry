//! The loading state's opener (`game::opening`, plan S10 9.4a §2.1-§2.5),
//! on a rig that stands in for the player: a virtual clock, the render
//! side's first-frame acknowledgements, a tuning model with the window
//! side's revision, held set, focus and acceptance, the input side's
//! START and SELECT, and a preparation whose answers the test schedules.
//! The window, input and render threads' own parts are the player's.

mod common;

use std::path::Path;

use bms_model::{Chart, ChartBuilder, TimeSource, TimeUs, VirtualClock};
use common::{Built, MS, lane, tapestry_settings};
use game::{
    Adjust, AudioQueue, AudioWaitError, Closing, Command, DataRoot, DeviceSettings, InputWaitError,
    Leave, LiveError, Loaded, LoadingRig, Opening, Parts, PlayReport, Preparation,
    PreparationRefusal, RenderSide, RestoreError, Revalidated, Rig, ScreenKey, SessionConfig,
    Settings, Tuning, TuningSnapshot,
};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter as _, ScriptItem, ScriptedInput, SourceReader,
    SourceSet,
};
use replay::{BuildProvenance, ReceiveMethod};

/// Where the rig's clock starts: the chart's load is ready here.
const T0: i64 = 7_300_000;
/// The input offset every play is evaluated with.
const OFFSET: i64 = 3_000;
/// The display offset the settings start with.
const DISPLAY: i64 = 2_000;
/// The display offset's up key, a window-side tuning control.
const DISPLAY_UP: u32 = 0x40;
/// Another tuning control the window side holds (a lane cover key).
const COVER: u32 = 0x70;
/// One second, the floor of the minimum and the quiet wait.
const SECOND: i64 = 1_000_000;

/// Lane 1 at 1.0 s and 1.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_500 * MS))
        .build()
        .unwrap()
}

fn config() -> SessionConfig {
    SessionConfig {
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
    }
}

/// The chart loaded under the tapestry preset, or under the user preset
/// `mine` when `preset` is given, with `policy` when given.
fn loaded_with(
    dir: &Path,
    preset: Option<&rules::RulePreset>,
    policy: Option<Box<dyn game::SessionPolicy>>,
) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    settings.device.display_offset_us = DISPLAY;
    if let Some(preset) = preset {
        let presets = dir.join("presets");
        std::fs::create_dir_all(&presets).unwrap();
        std::fs::write(presets.join("mine.toml"), game::preset_to_toml(preset)).unwrap();
        settings.player.preset = "mine".to_owned();
    }
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let standard = Parts::standard(&root, &settings.player);
    let parts = match policy {
        Some(policy) => Parts {
            ingest: Box::new(Built(chart())),
            policy,
            ..standard
        },
        None => Parts {
            ingest: Box::new(Built(chart())),
            ..standard
        },
    };
    Loaded::load(root, settings, &chart_path, config(), parts).unwrap()
}

fn loaded(dir: &Path) -> Loaded {
    loaded_with(dir, None, None)
}

/// [`loaded`], practising the whole chart in repeated rounds.
fn practising(dir: &Path) -> Loaded {
    let mut loaded = loaded(dir);
    let initial_gauges = loaded.preset().gauges.iter().map(|g| g.start).collect();
    loaded
        .set_practice(Some(game::Practice {
            start: TimeUs(500 * MS),
            end: TimeUs(2_000 * MS),
            lead_in: TimeUs(1_000 * MS),
            initial_gauges,
            repeat: true,
        }))
        .unwrap();
    loaded
}

/// The screen double's failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the surface is lost")]
struct SurfaceLost;

/// Something the tuning model takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    /// A window-side tuning control pressed.
    Press(u32),
    /// Released.
    Release(u32),
    /// Pressed and released at once.
    Tap(u32),
    /// The window loses focus.
    FocusLost,
    /// Focus returns.
    FocusBack,
    /// A press does not fit the held set.
    Overflow,
    /// A START play adjustment (the input side and the reducer).
    StartAdjust,
}

/// The tuning as the window side, the input side and the reducer keep it.
#[derive(Clone, Debug)]
struct Model {
    values: Tuning,
    window: u64,
    input: u64,
    reducer: u64,
    held: Vec<u32>,
    overflow: bool,
    focused: bool,
    /// Focus was lost while acceptance was closed, since it last closed:
    /// the input side defers an opening for focus.
    gap_lost: bool,
    /// Focus was lost since acceptance last reopened.
    lost_since_reopen: bool,
    focus_losses: u64,
    last: Option<i64>,
    accepting: bool,
    /// Presses that acted, and presses discarded while closed.
    acted: usize,
    discarded: usize,
}

impl Model {
    fn new(settings: &Settings) -> Self {
        Self {
            values: Tuning::of(settings),
            window: 0,
            input: 0,
            reducer: 0,
            held: Vec::new(),
            overflow: false,
            focused: true,
            gap_lost: false,
            lost_since_reopen: false,
            focus_losses: 0,
            last: None,
            accepting: true,
            acted: 0,
            discarded: 0,
        }
    }

    fn step(&mut self, control: u32) {
        if control == DISPLAY_UP {
            self.values = self.values.adjust(Adjust::DisplayOffsetUp);
        }
    }

    fn take(&mut self, ev: Ev, now: i64) {
        match ev {
            Ev::Press(c) | Ev::Tap(c) => {
                if !self.focused {
                    return;
                }
                if !self.accepting {
                    self.discarded += 1;
                    return;
                }
                self.acted += 1;
                self.step(c);
                self.window += 1;
                self.last = Some(now);
                if ev == Ev::Press(c) {
                    self.held.push(c);
                } else {
                    self.window += 1;
                }
            }
            Ev::Release(c) => {
                if let Some(i) = self.held.iter().position(|&h| h == c) {
                    self.held.remove(i);
                    self.window += 1;
                    self.last = Some(now);
                }
            }
            Ev::FocusLost => {
                self.held.clear();
                self.overflow = false;
                self.focused = false;
                self.lost_since_reopen = true;
                self.focus_losses += 1;
                self.gap_lost |= !self.accepting;
                self.window += 1;
            }
            Ev::FocusBack => {
                self.focused = true;
                self.window += 1;
                self.last = Some(now);
            }
            Ev::Overflow => {
                self.overflow = true;
                self.window += 1;
                self.last = Some(now);
            }
            Ev::StartAdjust => {
                if !self.accepting {
                    self.discarded += 1;
                    return;
                }
                self.acted += 1;
                self.input += 1;
                self.reducer += 1;
                self.last = Some(now);
            }
        }
    }

    fn snapshot(&self) -> TuningSnapshot {
        TuningSnapshot {
            stable: true,
            values: Some(self.values),
            revision: game::AcceptanceRevision {
                window: self.window,
                input: self.input,
                reducer: self.reducer,
            },
            held: !self.held.is_empty(),
            overflow: self.overflow,
            focused: self.focused,
            focus_losses: self.focus_losses,
            last_activity: self.last.map(TimeUs),
        }
    }
}

/// What happens at a time or at a step.
#[derive(Clone, Copy, Debug)]
enum Happen {
    Ev(Ev),
    Quit,
    Close,
    InputFails,
    AudioFails,
    ScreenFails,
    /// START is held from now for this long.
    Hold(i64),
    /// The session policy shows this gauge from now on.
    Gauge(usize),
}

/// A rig on a virtual clock whose every wait (and every poll of the
/// preparation) moves it 1 ms.
struct Loader {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    commands: Vec<(i64, Command)>,
    closing: bool,
    /// Every handshake, with its clock time.
    log: Vec<(i64, &'static str)>,
    failed: Option<InputWaitError>,
    output_failed: bool,
    screen_failed: bool,
    /// Screen preparations from this one on fail (1-based).
    screen_fails_from: usize,
    prepares: usize,
    screen: ScreenKey,
    /// Final readinesses answering `Stale` with these screens, in order.
    stale: Vec<ScreenKey>,
    lifetime: u64,
    /// The acknowledgements (visible from, lifetime, presented at).
    acks: Vec<(i64, u64, i64)>,
    /// The declared loadend from a clock time on.
    loadends: Vec<(i64, Option<TimeUs>)>,
    begun: Vec<game::PrepKey>,
    /// Each preparation's answer and the clock time it comes; then ready
    /// at once.
    answers: Vec<(i64, Preparation)>,
    polls: Vec<(i64, u64)>,
    poll_fails: bool,
    readied: Vec<(u32, u64)>,
    model: Model,
    /// Things that happen once the clock reaches their time.
    events: Vec<(i64, Happen)>,
    /// Things that happen at a step's first handshake from now on.
    at_step: Vec<(&'static str, Happen)>,
    /// START held over these clock intervals.
    holds: Vec<(i64, i64)>,
    gated: bool,
    generation: u32,
    exchange: usize,
    deferred_for_focus: bool,
    refresh_refuses: bool,
    restore_refuses: bool,
    audio_fails: bool,
    commit_waits: usize,
    commit_fails: bool,
    end_loading_waits: usize,
    end_loading_fails: bool,
    /// The generations asked to close acceptance, by step.
    closes: Vec<(&'static str, u32)>,
    applied: Vec<TuningSnapshot>,
    origins: Vec<i64>,
    sounds: Option<AudioQueue>,
    sealed: bool,
    reports: Vec<PlayReport>,
    /// A quit is pressed as the n-th recording opens (from 1): that play
    /// is left in READY.
    quit_on_open: usize,
    /// The final readiness finds the staging lost with nothing
    /// structural changed.
    lost_staging: bool,
    opens: usize,
    /// The gauge [`Shown`] shows.
    shown: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Taken right after the first snapshot at or after its time that is
    /// quiet: an event between a quiet look and the copy that follows it.
    after_quiet: Option<(i64, Ev)>,
}

impl Loader {
    fn new(loaded: &Loaded) -> Self {
        let mut sources = SourceSet::new(common::sources()).unwrap();
        let mut script = ScriptedInput::new(vec![ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(4), DeviceKind::Keyboard),
        }]);
        script.poll(TimeUs(T0), &mut sources);
        Self {
            clock: VirtualClock::new(TimeUs(T0)),
            sources,
            script,
            commands: Vec::new(),
            closing: false,
            log: Vec::new(),
            failed: None,
            output_failed: false,
            screen_failed: false,
            screen_fails_from: usize::MAX,
            prepares: 0,
            screen: ScreenKey::default(),
            stale: Vec::new(),
            lifetime: 7,
            acks: vec![(T0, 7, T0)],
            loadends: Vec::new(),
            begun: Vec::new(),
            answers: Vec::new(),
            polls: Vec::new(),
            poll_fails: false,
            readied: Vec::new(),
            model: Model::new(loaded.settings()),
            events: Vec::new(),
            at_step: Vec::new(),
            holds: Vec::new(),
            gated: true,
            generation: 0,
            exchange: 0,
            deferred_for_focus: false,
            refresh_refuses: false,
            restore_refuses: false,
            audio_fails: false,
            commit_waits: 0,
            commit_fails: false,
            end_loading_waits: 0,
            end_loading_fails: false,
            closes: Vec::new(),
            applied: Vec::new(),
            origins: Vec::new(),
            sounds: None,
            sealed: false,
            reports: Vec::new(),
            quit_on_open: 1,
            lost_staging: false,
            opens: 0,
            shown: std::sync::Arc::default(),
            after_quiet: None,
        }
    }

    fn at(&mut self, at: i64, what: Happen) {
        self.events.push((at, what));
    }

    fn happen(&mut self, what: Happen) {
        let now = self.clock.now().0;
        match what {
            Happen::Ev(ev) => self.model.take(ev, now),
            Happen::Quit => self.commands.push((i64::MIN, Command::Quit)),
            Happen::Close => self.closing = true,
            Happen::InputFails => self.failed = Some(InputWaitError::Gone),
            Happen::AudioFails => self.output_failed = true,
            Happen::ScreenFails => self.screen_failed = true,
            Happen::Hold(d) => self.holds.push((now, now + d)),
            Happen::Gauge(g) => self.shown.store(g, std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Logs `name` and runs what waits on it.
    fn step(&mut self, name: &'static str) {
        self.log.push((self.clock.now().0, name));
        while let Some(i) = self.at_step.iter().position(|(n, _)| *n == name) {
            let what = self.at_step.remove(i).1;
            self.happen(what);
        }
    }

    /// The clock times `name` was logged at.
    fn times(&self, name: &str) -> Vec<i64> {
        self.log
            .iter()
            .filter(|(_, n)| *n == name)
            .map(|&(t, _)| t)
            .collect()
    }

    /// The steps logged, in order.
    fn steps(&self) -> Vec<&'static str> {
        self.log.iter().map(|&(_, n)| n).collect()
    }

    /// When recording opened: the one opening.
    fn opened_at(&self) -> i64 {
        let [at] = self.times("open")[..] else {
            panic!("{:?}", self.log);
        };
        at
    }

    fn count(&self, name: &str) -> usize {
        self.times(name).len()
    }
}

impl Rig for Loader {
    type Reader = SourceReader;
    type ScreenError = SurfaceLost;
    type ResultPrep = ();

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        let now = self.clock.now().0;
        assert!(
            now < T0 + 60 * SECOND,
            "the run did not end: {:?}",
            self.log
        );
        if self.failed.is_none() {
            self.script.poll(TimeUs(now), &mut self.sources);
        }
        if let Some(s) = &mut self.sounds {
            while s.commands().pop().is_ok() {}
        }
        while let Some(i) = self.events.iter().position(|&(at, _)| at <= now) {
            let what = self.events.remove(i).1;
            self.happen(what);
        }
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.clock.now().0;
        let i = self.commands.iter().position(|&(at, _)| at <= now)?;
        Some(self.commands.remove(i).1)
    }

    fn quitting(&self) -> bool {
        self.closing
    }

    fn prepare_screen(&mut self) -> Result<(), SurfaceLost> {
        self.step("screen");
        self.prepares += 1;
        if self.prepares >= self.screen_fails_from {
            return Err(SurfaceLost);
        }
        if self.screen_failed {
            // Built again: another device.
            self.screen_failed = false;
            self.screen.device += 1;
        }
        Ok(())
    }

    fn begin_preparation(&mut self, _loaded: &Loaded, key: game::PrepKey) -> game::PrepHandle {
        self.step("begin");
        self.begun.push(key.clone());
        game::PrepHandle::new(key, self.begun.len() as u64)
    }

    fn poll_preparation(
        &mut self,
        handle: &mut game::PrepHandle,
        _deadline: std::time::Instant,
    ) -> Result<game::PrepPoll<Preparation>, SurfaceLost> {
        self.step("poll");
        self.polls.push((self.clock.now().0, handle.ticket()));
        self.wait();
        if self.poll_fails {
            return Err(SurfaceLost);
        }
        let i = usize::try_from(handle.ticket() - 1).unwrap();
        let (at, answer) = self
            .answers
            .get(i)
            .cloned()
            .unwrap_or((i64::MIN, Preparation::Ready));
        if self.clock.now().0 < at {
            return Ok(game::PrepPoll::Pending);
        }
        if answer == Preparation::Ready {
            handle.staged_with(self.screen);
        }
        Ok(game::PrepPoll::Done(answer))
    }

    /// `Stale` while the screen is not the one staged on, or with the next
    /// of [`Self::stale`]; else ready.
    fn ready_attempt(
        &mut self,
        number: u32,
        handle: &game::PrepHandle,
    ) -> Result<game::Readiness<SurfaceLost>, SurfaceLost> {
        self.step("ready");
        self.readied.push((number, handle.ticket()));
        if !self.stale.is_empty() {
            self.screen = self.stale.remove(0);
        }
        if handle.key().screen() == Some(self.screen) && !self.lost_staging {
            return Ok(game::Readiness::Ready);
        }
        Ok(game::Readiness::Stale(game::Stale {
            screen: self.screen,
            lost: SurfaceLost,
        }))
    }

    fn commit_attempt(&mut self) -> Result<(), SurfaceLost> {
        self.step("commit");
        for _ in 0..self.commit_waits {
            self.wait();
        }
        if self.commit_fails {
            return Err(SurfaceLost);
        }
        Ok(())
    }

    fn discard_attempt(&mut self) {
        self.step("discard");
    }

    fn end_loading(&mut self) -> Result<(), SurfaceLost> {
        self.step("end loading");
        for _ in 0..self.end_loading_waits {
            self.wait();
        }
        if self.end_loading_fails {
            return Err(SurfaceLost);
        }
        Ok(())
    }

    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        self.step("audio");
        if self.audio_fails {
            return Err(AudioWaitError::Gone);
        }
        Ok(())
    }

    fn before_play(&mut self, settings: &mut Settings) {
        self.step("before play");
        let _ = settings;
    }

    fn begin_presentation(
        &mut self,
        _loaded: &Loaded,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        self.step("presentation");
        Ok(None)
    }

    fn end_presentation(&mut self) {
        self.step("end presentation");
    }

    fn restore_ready(&mut self, _at: TimeUs) -> Result<(), RestoreError> {
        self.step("restore");
        if self.restore_refuses {
            return Err(RestoreError::NotPrepared);
        }
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        self.step("open");
        if let Some(e) = self.failed {
            return Err(e);
        }
        // Recording opened: acceptance with it.
        self.model.accepting = true;
        let now = self.clock.now();
        self.opens += 1;
        if self.opens == self.quit_on_open {
            self.commands.push((now.0, Command::Quit));
        }
        let origin = TimeUs(now.0 + lead_in.0);
        let recorded = DeviceSettings::default().keyboard.recorded();
        let devices: Vec<_> = self.sources.devices().map(|(d, _)| d).collect();
        for d in devices {
            self.sources.set_recorded(d, recorded.clone());
        }
        self.sources.gate_mut().reset();
        let start = self.sources.open_play(now, origin);
        self.origins.push(origin.0);
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
        if !self.model.focused || self.model.gap_lost {
            self.step("deferred");
            self.deferred_for_focus = true;
            return Ok(None);
        }
        if self.holding() {
            self.step("deferred");
            self.deferred_for_focus = false;
            return Ok(None);
        }
        self.generation = generation;
        self.open(lead_in).map(Some)
    }

    fn defer(&mut self) {
        self.step("defer");
    }

    fn holding(&self) -> bool {
        let now = self.clock.now().0;
        self.holds.iter().any(|&(from, to)| from <= now && now < to)
    }

    /// The transport commits as soon as it is asked.
    fn candidate(&self) -> input::GateState {
        input::GateState {
            generation: self.generation,
            phase: input::GatePhase::Committed,
        }
    }

    fn start(&mut self, _origin: TimeUs, sounds: AudioQueue, _screen: RenderSide) {
        self.sounds = Some(sounds);
        self.sealed = false;
    }

    fn focus_lost(&self) -> bool {
        false
    }

    fn connected(&self) -> bool {
        true
    }

    fn audio_failure(&self) -> Option<audio::OutputError> {
        self.output_failed.then_some(audio::OutputError::DeviceLost)
    }

    fn screen_failed(&self) -> bool {
        self.screen_failed
    }

    fn cancel_audio(&mut self) {}

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {
        self.sealed = true;
    }

    fn audio_done(&self) -> bool {
        self.sealed
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        if let Some(e) = self.failed {
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
    }

    fn ended(&mut self, report: &PlayReport) {
        self.reports.push(report.clone());
    }

    fn show_end(&mut self, _report: &PlayReport, _screen: &game::ResultScreen) {
        self.step("result");
    }
}

impl LoadingRig for Loader {
    fn loading_lifetime(&self) -> u64 {
        self.lifetime
    }

    fn loading_ack(&self) -> Option<(u64, TimeUs)> {
        let now = self.clock.now().0;
        self.acks
            .iter()
            .rev()
            .find(|&&(visible, _, _)| visible <= now)
            .map(|&(_, l, at)| (l, TimeUs(at)))
    }

    fn loadend(&self) -> Option<TimeUs> {
        let now = self.clock.now().0;
        self.loadends
            .iter()
            .rev()
            .find(|&&(from, _)| from <= now)
            .and_then(|&(_, d)| d)
    }

    fn tuning_snapshot(&mut self) -> Result<TuningSnapshot, InputWaitError> {
        if let Some(e) = self.failed {
            return Err(e);
        }
        let snapshot = self.model.snapshot();
        let now = self.clock.now();
        if let Some((from, ev)) = self.after_quiet
            && now.0 >= from
            && snapshot.quiet_at(now)
        {
            self.after_quiet = None;
            self.model.take(ev, now.0);
        }
        Ok(snapshot)
    }

    fn apply_snapshot(&mut self, snapshot: &TuningSnapshot, settings: &mut Settings) {
        self.step("apply");
        self.applied.push(*snapshot);
        if let Some(values) = snapshot.values {
            values.apply_to(settings);
        }
    }

    fn refresh_presentation(
        &mut self,
        _loaded: &Loaded,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        self.step("refresh");
        if let Some(e) = self.failed {
            return Err(e);
        }
        Ok(self.refresh_refuses.then(|| PreparationRefusal {
            location: None,
            reason: "floating cannot be solved".to_owned(),
        }))
    }

    fn revalidate_and_close(
        &mut self,
        at_o1: &TuningSnapshot,
        generation: u32,
    ) -> Result<Revalidated, InputWaitError> {
        self.step("revalidate");
        if let Some(e) = self.failed {
            return Err(e);
        }
        if !self
            .model
            .snapshot()
            .unchanged_since(at_o1, self.clock.now())
        {
            return Ok(Revalidated::Changed);
        }
        self.model.accepting = false;
        self.model.gap_lost = false;
        self.closes.push(("revalidate", generation));
        Ok(Revalidated::Closed)
    }

    fn close_acceptance(&mut self, generation: u32) -> Result<TuningSnapshot, InputWaitError> {
        self.step("close");
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.model.accepting = false;
        self.model.gap_lost = false;
        self.closes.push(("close", generation));
        Ok(self.model.snapshot())
    }

    fn reopen_acceptance(&mut self) -> Result<(), InputWaitError> {
        self.step("reopen");
        if let Some(e) = self.failed {
            return Err(e);
        }
        self.model.accepting = true;
        self.model.lost_since_reopen = false;
        Ok(())
    }

    fn focus_deferred(&self) -> bool {
        self.deferred_for_focus || self.model.lost_since_reopen
    }
}

/// The steps of one opening out of the loading state once its wait ends,
/// with the preparation ready: from the starting settings to the opening.
const OPENING: [&str; 9] = [
    "apply",
    "screen",
    "refresh",
    "audio",
    "ready",
    "revalidate",
    "commit",
    "end loading",
    "open",
];

/// The steps of the loading state's opening from its wait on.
fn opening_steps(rig: &Loader) -> Vec<&'static str> {
    let steps = rig.steps();
    let from = steps
        .iter()
        .position(|&s| s == "apply")
        .expect("no opening");
    steps[from..].to_vec()
}

/// A session policy that shows the gauge a test sets: the resolved gauge
/// moves at the play's number with no preset or settings change.
struct Shown(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl game::SessionPolicy for Shown {
    fn id(&self) -> &str {
        "shown"
    }

    fn displayed_gauge(&self, _gauges: &[eval::GaugeState]) -> game::GaugeIndex {
        game::GaugeIndex(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn on_survival_failure(&self) -> game::FailureAction {
        game::FailureAction::PlayToEnd
    }
}

/// [`loaded`] with [`Shown`], and its rig sharing the gauge shown.
fn showing(dir: &Path) -> (Loaded, Loader) {
    let shown = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let loaded = loaded_with(
        dir,
        None,
        Some(Box::new(Shown(std::sync::Arc::clone(&shown)))),
    );
    let mut rig = Loader::new(&loaded);
    rig.shown = shown;
    (loaded, rig)
}

type Ran = Result<Leave, LiveError<SurfaceLost>>;

fn run(loaded: &mut Loaded, rig: &mut Loader) -> Ran {
    game::load_and_open(loaded, rig)
}

/// The clock time of each `apply` (the opening's first step).
fn applied_at(rig: &Loader) -> Vec<i64> {
    rig.times("apply")
}

/// The display offset the opening's snapshot `i` gave the attempt.
fn display_of(rig: &Loader, i: usize) -> i64 {
    rig.applied[i].values.unwrap().display_offset_us
}

/// The last `n` steps logged.
fn last_steps(rig: &Loader, n: usize) -> Vec<&'static str> {
    let steps = rig.steps();
    steps[steps.len().saturating_sub(n)..].to_vec()
}

/// An exit that does not end the run left tuning acceptance open on both
/// sides: a tap of the display offset key acts.
fn assert_tuning_open(rig: &mut Loader) {
    assert!(rig.model.accepting, "{:?}", rig.log);
    if !rig.model.focused {
        rig.happen(Happen::Ev(Ev::FocusBack));
    }
    let acted = rig.model.acted;
    rig.happen(Happen::Ev(Ev::Tap(DISPLAY_UP)));
    assert_eq!(rig.model.acted, acted + 1);
}

/// A run that left before any opening step took no number, discarded the
/// preparation begun at load-ready, and kept acceptance open.
fn assert_abandoned_in_the_wait(loaded: &Loaded, rig: &mut Loader) {
    assert!(rig.times("apply").is_empty(), "{:?}", rig.log);
    assert_eq!(rig.count("discard"), 1, "{:?}", rig.log);
    assert_eq!(rig.count("open"), 0);
    assert_eq!(rig.begun.len(), 1);
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(rig);
}

fn refusal() -> PreparationRefusal {
    PreparationRefusal {
        location: Some("skin.json".to_owned()),
        reason: "the skin is refused".to_owned(),
    }
}

fn fails(input: bool) -> Happen {
    if input {
        Happen::InputFails
    } else {
        Happen::AudioFails
    }
}

/// The run ended with the input thread's failure (`input`) or the audio
/// output's.
fn assert_failed(ran: &Ran, input: bool, name: &str) {
    if input {
        assert!(
            matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
            "{name}: {ran:?}"
        );
    } else {
        assert!(
            matches!(ran, Err(LiveError::Output(audio::OutputError::DeviceLost))),
            "{name}: {ran:?}"
        );
    }
}

type Set = Box<dyn Fn(&mut Loader)>;

// ---------------------------------------------------------------- §2.1 W

/// L1: the minimum ends one second after this lifetime's first presented
/// frame; a later acknowledgement of the same lifetime changes nothing.
#[test]
fn the_minimum_counts_from_this_lifetimes_first_presented_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![
        (T0 + 200_000, 7, T0 + 150_000),
        (T0 + 600_000, 7, T0 + 580_000),
    ];
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(applied_at(&rig), [T0 + 1_150_000]);
    assert_eq!(opening_steps(&rig), OPENING);
    assert_eq!(rig.opened_at(), T0 + 1_150_000);
}

/// L2: an acknowledgement of an earlier lifetime, presented after this one
/// began, does not start the minimum.
#[test]
fn an_older_lifetimes_frame_does_not_start_the_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![
        (T0 + 10_000, 6, T0 + 10_000),
        (T0 + 400_000, 7, T0 + 400_000),
    ];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + 1_400_000]);
}

/// L3: a first frame presented late moves the minimum with it.
#[test]
fn a_delayed_first_frame_moves_the_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0 + 2_000_000, 7, T0 + 2_000_000)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + 3_000_000]);
}

/// L4: the declared loadend against the one-second floor, and the wait
/// with a skin declaring 1.5 s.
#[test]
fn loadend_converts_to_the_minimum() {
    // The import gives a negative declaration as 0.
    for (loadend, minimum) in [
        (None, SECOND),
        (Some(0), SECOND),
        (Some(500_000), SECOND),
        (Some(1_500_000), 1_500_000),
        (Some(2_147_483_647_000), 2_147_483_647_000),
    ] {
        assert_eq!(
            game::loading_minimum(loadend.map(TimeUs)),
            TimeUs(minimum),
            "{loadend:?}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.loadends = vec![(T0, Some(TimeUs(1_500_000)))];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + 1_500_000]);
}

/// L4: an acknowledgement near the clock's end saturates the deadline.
#[test]
fn a_minimum_near_the_clocks_end_saturates() {
    let ack = TimeUs(i64::MAX - 10);
    assert_eq!(game::minimum_ends(ack, None), TimeUs(i64::MAX));
    assert_eq!(
        game::minimum_ends(ack, Some(TimeUs(2_147_483_647_000))),
        TimeUs(i64::MAX)
    );
    assert_eq!(
        game::minimum_ends(TimeUs(5), Some(TimeUs(2_000_000))),
        TimeUs(2_000_005)
    );
}

/// L5: the rig declares a pending skin's base loadend until its
/// specialization commits; the wait reads the declaration at each look.
#[test]
fn a_pending_skin_uses_its_base_loadend_until_committed() {
    for (loadends, end) in [
        // Never committed within the lifetime: the base's 2 s.
        (vec![(T0, Some(TimeUs(2_000_000)))], T0 + 2_000_000),
        // Committed at 1.5 s, declaring 3 s: from then on, 3 s.
        (
            vec![
                (T0, Some(TimeUs(2_000_000))),
                (T0 + 1_500_000, Some(TimeUs(3_000_000))),
            ],
            T0 + 3_000_000,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.loadends = loadends;
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(applied_at(&rig), [end]);
    }
}

/// The waits of L10 to L12, each holding the wait from its first slice to
/// past 500 ms: no
/// acknowledgement (the preparation pending); the minimum only, the
/// preparation ready; a held control; activity; no focus.
fn each_wait() -> Vec<(&'static str, Set)> {
    vec![
        (
            "no ack",
            Box::new(|r: &mut Loader| {
                r.acks.clear();
                r.answers = vec![(T0 + 5 * SECOND, Preparation::Ready)];
            }),
        ),
        ("minimum only", Box::new(|_: &mut Loader| {})),
        (
            "held",
            Box::new(|r: &mut Loader| {
                r.at(T0, Happen::Ev(Ev::Press(COVER)));
                r.acks = vec![(T0, 7, T0 - 2 * SECOND)];
            }),
        ),
        (
            "quiet",
            Box::new(|r: &mut Loader| {
                r.at(T0, Happen::Ev(Ev::Tap(DISPLAY_UP)));
                r.acks = vec![(T0, 7, T0 - 2 * SECOND)];
            }),
        ),
        (
            "unfocused",
            Box::new(|r: &mut Loader| {
                r.at(T0, Happen::Ev(Ev::FocusLost));
                r.acks = vec![(T0, 7, T0 - 2 * SECOND)];
            }),
        ),
    ]
}

/// L10, O-1: a quit in each wait returns at once, to where the chart was
/// chosen, the preparation discarded.
#[test]
fn a_quit_returns_at_once_from_each_wait() {
    for (name, set) in each_wait() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        set(&mut rig);
        rig.at(T0 + 500_000, Happen::Quit);
        assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back, "{name}");
        assert_eq!(rig.clock.now().0, T0 + 500_000, "{name}: {:?}", rig.log);
        assert_abandoned_in_the_wait(&loaded, &mut rig);
    }
}

/// L11, O-1: closing the window in each wait ends the run.
#[test]
fn a_close_ends_the_run_from_each_wait() {
    for (name, set) in each_wait() {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        set(&mut rig);
        rig.at(T0 + 500_000, Happen::Close);
        assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Quit, "{name}");
        assert_eq!(rig.clock.now().0, T0 + 500_000, "{name}");
        assert_eq!(rig.count("discard"), 1, "{name}");
        assert_eq!(rig.count("apply"), 0, "{name}");
    }
}

/// L12, O-1: the input or the audio thread failing in each wait ends the
/// run with that cause.
#[test]
fn a_thread_failure_ends_the_run_from_each_wait() {
    for (name, set) in each_wait() {
        for input in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut loaded = loaded(dir.path());
            let mut rig = Loader::new(&loaded);
            set(&mut rig);
            rig.at(T0 + 500_000, fails(input));
            let ran = run(&mut loaded, &mut rig);
            assert_failed(&ran, input, name);
            assert_eq!(rig.clock.now().0, T0 + 500_000, "{name}");
            assert_eq!(rig.count("discard"), 1, "{name}");
            assert_eq!(rig.count("apply"), 0, "{name}");
        }
    }
}

/// L12b: a refusal latched in the slice a thread failed in gives way to
/// the failure.
#[test]
fn a_failure_beats_a_refusal_in_the_same_slice() {
    for input in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.answers = vec![(T0 + 300_000, Preparation::Refused(refusal()))];
        rig.at(T0 + 300_000, fails(input));
        let ran = run(&mut loaded, &mut rig);
        assert_failed(&ran, input, "refused");
        assert_eq!(rig.clock.now().0, T0 + 300_000);
    }
}

/// L13, Q4: a refusal latched while the minimum still runs acts in that
/// slice, with its reason.
#[test]
fn a_preparation_refusal_acts_before_the_minimum_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.answers = vec![(T0 + 300_000, Preparation::Refused(refusal()))];
    let ran = run(&mut loaded, &mut rig);
    let Err(LiveError::Preparation(why)) = ran else {
        panic!("{ran:?}");
    };
    assert_eq!(why, refusal());
    assert_eq!(rig.clock.now().0, T0 + 300_000);
    assert_eq!(rig.polls.last(), Some(&(T0 + 299_000, 1)));
    assert_abandoned_in_the_wait(&loaded, &mut rig);
}

/// L14: a screen built again in the wait keeps the loading lifetime: an
/// acknowledgement taken stays; without one, the lifetime's first frame
/// after the rebuild starts the minimum. The new device moves the
/// preparation's key at the final readiness.
#[test]
fn a_screen_rebuilt_while_waiting_keeps_its_lifetime_and_minimum() {
    for acked in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        if !acked {
            rig.acks = vec![(T0 + 500_000, 7, T0 + 500_000)];
        }
        rig.at(T0 + 200_000, Happen::ScreenFails);
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(rig.times("screen")[0], T0 + 200_000, "{acked}");
        let end = if acked { T0 + SECOND } else { T0 + 1_500_000 };
        assert_eq!(applied_at(&rig), [end], "{acked}");
        // Staged on device 0, readied on device 1: begun again once.
        assert_eq!(rig.begun.len(), 2);
        assert_eq!(rig.readied, [(1, 1), (1, 2)]);
        assert_eq!(rig.count("revalidate"), 1);
        // The new preparation's one poll.
        assert_eq!(rig.opened_at(), end + MS);
    }
}

/// L15: each condition alone holds the wait until it holds. The load is
/// done before the loading opener begins, so the tuning's two conditions
/// (nothing held, quiet) are separate cases.
#[test]
fn each_condition_alone_holds_the_wait() {
    let cases: [(&str, Set, i64); 5] = [
        (
            "minimum",
            Box::new(|r| r.acks = vec![(T0 + 1_500_000, 7, T0 + 1_500_000)]),
            T0 + 2_500_000,
        ),
        (
            "held",
            Box::new(|r| {
                r.at(T0 + 100_000, Happen::Ev(Ev::Press(COVER)));
                r.at(T0 + 1_200_000, Happen::Ev(Ev::Release(COVER)));
            }),
            T0 + 2_200_000,
        ),
        (
            "quiet",
            Box::new(|r| r.at(T0 + 800_000, Happen::Ev(Ev::Tap(DISPLAY_UP)))),
            T0 + 1_800_000,
        ),
        (
            "focus",
            Box::new(|r| {
                r.at(T0 + 100_000, Happen::Ev(Ev::FocusLost));
                r.at(T0 + 1_300_000, Happen::Ev(Ev::FocusBack));
            }),
            T0 + 2_300_000,
        ),
        // An overflow alone, nothing held: it holds the wait until a focus
        // loss clears it, past the quiet second its own press began.
        (
            "overflow",
            Box::new(|r| {
                r.at(T0 + 100_000, Happen::Ev(Ev::Overflow));
                r.at(T0 + 1_200_000, Happen::Ev(Ev::FocusLost));
                r.at(T0 + 1_300_000, Happen::Ev(Ev::FocusBack));
            }),
            T0 + 2_300_000,
        ),
    ];
    for (name, set, end) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        set(&mut rig);
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(applied_at(&rig), [end], "{name}");
        assert_eq!(opening_steps(&rig), OPENING, "{name}");
    }
}

/// L15: with every condition turning true at another time, the wait ends
/// when the last does, and the opening reads the tuning again.
#[test]
fn the_wait_ends_when_all_four_hold() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0 + 500_000, 7, T0 + 500_000)];
    rig.answers = vec![(T0 + 2_000_000, Preparation::Ready)];
    rig.at(T0 + 700_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    rig.at(T0 + 800_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_000_000, Happen::Ev(Ev::FocusBack));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::Press(COVER)));
    rig.at(T0 + 1_900_000, Happen::Ev(Ev::Release(COVER)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + 2_900_000]);
    assert_eq!(opening_steps(&rig), OPENING);
    assert_eq!(display_of(&rig, 0), DISPLAY + 1_000);
    assert_eq!(rig.applied[0].last_activity, Some(TimeUs(T0 + 1_900_000)));
}

// ------------------------------------------------------------ §2.2 latch

/// Q1: ready before the first acknowledgement: polled once, never again.
#[test]
fn a_preparation_ready_before_the_first_ack_is_not_polled_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0 + 500_000, 7, T0 + 500_000)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.polls, [(T0, 1)]);
    assert_eq!(applied_at(&rig), [T0 + 1_500_000]);
    assert_eq!(rig.readied, [(1, 1)]);
}

/// Q2: ready with 900 ms of the minimum left: the rest of the wait and
/// the key check poll nothing.
#[test]
fn a_preparation_ready_before_the_minimum_ends_is_not_polled_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.answers = vec![(T0 + 100_000, Preparation::Ready)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.polls.len(), 100);
    assert_eq!(rig.polls.last(), Some(&(T0 + 99_000, 1)));
    assert_eq!(applied_at(&rig), [T0 + SECOND]);
    assert_eq!(opening_steps(&rig), OPENING);
}

/// Q3, O-14: a local retry with the key unchanged keeps the ready
/// preparation: the next key check polls nothing and the final readiness
/// runs on it.
#[test]
fn a_ready_preparation_survives_a_same_key_local_retry() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("audio", Happen::Ev(Ev::Tap(DISPLAY_UP))));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.begun.len(), 1);
    assert_eq!(rig.polls, [(T0, 1)]);
    assert_eq!(rig.readied, [(1, 1), (1, 1)]);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 2 * SECOND]);
    assert_eq!(rig.count("discard"), 0);
    assert_eq!(rig.count("end presentation"), 1);
    assert_eq!(rig.closes, [("revalidate", 1)]);
}

/// Q5: a cancelled preparation looks at the threads first, then the quit.
#[test]
fn a_cancelled_preparation_defers_to_the_thread_look() {
    for failed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.answers = vec![(T0 + 200_000, Preparation::Cancelled)];
        rig.at(
            T0 + 200_000,
            if failed {
                Happen::InputFails
            } else {
                Happen::Quit
            },
        );
        let ran = run(&mut loaded, &mut rig);
        if failed {
            assert_failed(&ran, true, "cancelled");
        } else {
            assert_eq!(ran.unwrap(), Leave::Back);
        }
        assert_eq!(rig.clock.now().0, T0 + 200_000);
        assert_eq!(rig.count("discard"), 1);
        assert_eq!(rig.count("apply"), 0);
    }
}

/// Q8: a screen error from a poll ends the run.
#[test]
fn a_screen_error_from_the_poll_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.poll_fails = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert_eq!(rig.polls.len(), 1);
    assert_eq!(rig.count("discard"), 1);
}

// ---------------------------------------------------------- §2.4 exits

/// O-2: the screen failing at the opening's screen step disposes of the
/// preparation begun before the wait; no number was given.
#[test]
fn a_screen_error_at_o2_disposes_of_the_early_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.screen_fails_from = 1;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert_eq!(last_steps(&rig, 3), ["apply", "screen", "discard"]);
    assert_eq!(loaded.attempt().number, 0);
}

/// [`practising`] under the user preset `mine`, with gauge 0's initial
/// value set away from its start; the preset file then drops that gauge,
/// so the first round's refit refuses it.
fn unfit(dir: &Path) -> Loaded {
    let mut preset = rules::tapestry();
    preset.id = "mine".to_owned();
    let mut loaded = loaded_with(dir, Some(&preset), None);
    let gauges = &loaded.preset().gauges;
    let mut initial_gauges: Vec<_> = gauges.iter().map(|g| g.start).collect();
    let set = rules::GaugeValue(gauges[0].start.0 + 10 * rules::GaugeValue::PERCENT);
    assert!(eval::accepts_initial(&gauges[0], set));
    initial_gauges[0] = set;
    loaded
        .set_practice(Some(game::Practice {
            start: TimeUs(500 * MS),
            end: TimeUs(2_000 * MS),
            lead_in: TimeUs(1_000 * MS),
            initial_gauges,
            repeat: true,
        }))
        .unwrap();
    preset.gauges[0].id = "renamed".to_owned();
    common::rewrite(
        &dir.join("presets").join("mine.toml"),
        &game::preset_to_toml(&preset),
    );
    loaded
}

/// O-3: a round refused by its refit disposes of the preparation, gives
/// its number back and ends the presentation; the page shows the reason.
#[test]
fn opening_o4_refit_refusal_disposes_and_returns_to_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = unfit(dir.path());
    let mut rig = Loader::new(&loaded);
    let ran = run(&mut loaded, &mut rig);
    let Err(LiveError::Preparation(why)) = ran else {
        panic!("{ran:?}");
    };
    assert_eq!(why.location, None);
    assert!(!why.reason.is_empty());
    assert_eq!(
        last_steps(&rig, 4),
        ["apply", "screen", "discard", "end presentation"]
    );
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(&mut rig);
}

/// O-4: a presentation refused at its refresh, or a fence the input
/// thread does not answer there, disposes of the preparation.
#[test]
fn opening_o5_refusal_disposes_of_the_handle() {
    for input in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        if input {
            rig.at_step.push(("refresh", Happen::InputFails));
        } else {
            rig.refresh_refuses = true;
        }
        let ran = run(&mut loaded, &mut rig);
        if input {
            assert_failed(&ran, true, "refresh");
        } else {
            assert!(matches!(ran, Err(LiveError::Preparation(_))), "{ran:?}");
        }
        assert_eq!(
            last_steps(&rig, 3),
            ["refresh", "discard", "end presentation"]
        );
        assert_eq!(loaded.attempt().number, 0);
    }
}

/// O-5: audio that cannot play disposes of the preparation and ends the
/// run.
#[test]
fn opening_o6_audio_error_disposes_of_the_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.audio_fails = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Audio(AudioWaitError::Gone))),
        "{ran:?}"
    );
    assert_eq!(
        last_steps(&rig, 3),
        ["audio", "discard", "end presentation"]
    );
    assert_eq!(loaded.attempt().number, 0);
}

/// O-6 to O-8 (the flow's part; each stage's settle is the player's): a
/// key moved while the preparation is still pending begins it again at
/// the key check, the new one polled to its answer.
#[test]
fn a_moved_key_replaces_a_pending_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut loaded, mut rig) = showing(dir.path());
    rig.answers = vec![(T0 + 5 * SECOND, Preparation::Ready)];
    rig.at_step.push(("apply", Happen::Gauge(1)));
    run(&mut loaded, &mut rig).unwrap();
    let [first, second] = &rig.begun[..] else {
        panic!("{:?}", rig.begun);
    };
    assert_eq!(
        first.changes(second),
        game::Changes {
            displayed: true,
            ..game::Changes::default()
        }
    );
    // The first was polled through the wait, the second once.
    assert!(
        rig.polls
            .iter()
            .all(|&(t, k)| (k == 1) == (t < T0 + SECOND))
    );
    assert_eq!(rig.polls.iter().filter(|&&(_, k)| k == 2).count(), 1);
    assert_eq!(rig.readied, [(1, 2)]);
    assert_eq!(rig.count("revalidate"), 1);
    assert_eq!(rig.opened_at(), T0 + SECOND + MS);
}

/// O-8b: the displayed gauge resolved anew at the play's number, with no
/// tuning activity, begins the preparation again once, with no local
/// retry.
#[test]
fn an_o3_gauge_change_replaces_the_preparation_without_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (mut loaded, mut rig) = showing(dir.path());
    rig.at_step.push(("apply", Happen::Gauge(1)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.begun.len(), 2);
    assert_eq!(
        rig.polls.iter().map(|&(_, k)| k).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(rig.readied, [(1, 2)]);
    assert_eq!(applied_at(&rig).len(), 1);
    assert_eq!(rig.closes, [("revalidate", 1)]);
    assert_eq!(loaded.displayed(), game::GaugeIndex(1));
}

/// O-9: a structural `Stale` begins the preparation again with the new
/// key and goes on.
#[test]
fn a_structural_stale_begins_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.stale = vec![ScreenKey {
        target: [640, 480],
        ..ScreenKey::default()
    }];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.begun.len(), 2);
    assert_eq!(rig.readied, [(1, 1), (1, 2)]);
    assert_eq!(applied_at(&rig).len(), 1);
    assert_eq!(rig.count("revalidate"), 1);
    assert_eq!(rig.count("open"), 1);
}

/// O-9: the screen's inputs found at the first staging are discovery: the
/// key check does not begin the preparation again for them.
#[test]
fn a_first_staging_is_not_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.screen = ScreenKey {
        device: 3,
        target: [800, 600],
        skin: 9,
    };
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.begun.len(), 1);
    assert_eq!(rig.begun[0].screen(), None);
    assert_eq!(rig.readied, [(1, 1)]);
}

/// O-9b: a `Stale` with a quit abandons without beginning again.
#[test]
fn a_stale_with_a_quit_abandons_without_replacing() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.stale = vec![ScreenKey {
        device: 1,
        ..ScreenKey::default()
    }];
    rig.at_step.push(("ready", Happen::Quit));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(rig.begun.len(), 1);
    assert_eq!(
        last_steps(&rig, 3),
        ["ready", "discard", "end presentation"]
    );
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(&mut rig);
}

/// O-9b: a `Stale` with an input or audio failure ends the run with it,
/// beginning nothing again.
#[test]
fn a_stale_with_a_failure_ends_the_run() {
    for input in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.stale = vec![ScreenKey {
            device: 1,
            ..ScreenKey::default()
        }];
        rig.at_step.push(("ready", fails(input)));
        let ran = run(&mut loaded, &mut rig);
        assert_failed(&ran, input, "stale");
        assert_eq!(rig.begun.len(), 1);
        assert_eq!(rig.count("discard"), 1);
    }
}

/// O-10: staging lost with nothing structural changed ends the run with
/// the screen's error, untried.
#[test]
fn a_stale_readiness_with_an_unchanged_key_ends_the_run_untried() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.lost_staging = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert_eq!(rig.begun.len(), 1);
    assert_eq!(rig.readied, [(1, 1)]);
    assert_eq!(rig.count("discard"), 1);
}

/// O-11: tuning activity together with a structural `Stale` goes back to
/// the wait first, then begins the preparation again with the new key.
#[test]
fn a_mixed_tuning_and_structural_stale_retries_through_the_wait() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.stale = vec![ScreenKey {
        device: 1,
        ..ScreenKey::default()
    }];
    rig.at_step.push(("ready", Happen::Ev(Ev::Tap(DISPLAY_UP))));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 2 * SECOND]);
    assert_eq!(rig.begun.len(), 2);
    assert_eq!(rig.times("begin")[1], T0 + 2 * SECOND);
    assert_eq!(rig.readied, [(1, 1), (1, 2)]);
    assert_eq!(rig.count("revalidate"), 1);
    assert_eq!(display_of(&rig, 1), DISPLAY + 1_000);
}

/// O-12: a round whose audio cannot be restored disposes of the
/// preparation and returns to the page.
#[test]
fn opening_o9_restore_refusal_disposes_and_returns_to_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = practising(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.restore_refuses = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Restore(RestoreError::NotPrepared))),
        "{ran:?}"
    );
    assert_eq!(
        last_steps(&rig, 3),
        ["restore", "discard", "end presentation"]
    );
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(&mut rig);
}

/// O-13: a quit or a close at the look after the readiness disposes of the
/// preparation.
#[test]
fn a_quit_at_o10_disposes() {
    for (what, left) in [(Happen::Quit, Leave::Back), (Happen::Close, Leave::Quit)] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.at_step.push(("ready", what));
        assert_eq!(run(&mut loaded, &mut rig).unwrap(), left);
        assert_eq!(
            last_steps(&rig, 3),
            ["ready", "discard", "end presentation"]
        );
        assert_eq!(rig.count("revalidate"), 0);
        assert_eq!(loaded.attempt().number, 0);
    }
}

/// O-14: activity, a hold, an overflow or a focus loss after the opening's
/// snapshot goes back to the wait, which counts the quiet from it.
#[test]
fn opening_o11_activity_goes_back_to_the_wait() {
    let cases: [(&str, Set, i64); 4] = [
        (
            "activity",
            Box::new(|r| r.at_step.push(("audio", Happen::Ev(Ev::Tap(DISPLAY_UP))))),
            T0 + 2 * SECOND,
        ),
        (
            "hold",
            Box::new(|r| {
                r.at_step.push(("audio", Happen::Ev(Ev::Press(COVER))));
                r.at(T0 + 1_500_000, Happen::Ev(Ev::Release(COVER)));
            }),
            T0 + 2_500_000,
        ),
        (
            "overflow",
            Box::new(|r| {
                r.at_step.push(("audio", Happen::Ev(Ev::Overflow)));
                r.at(T0 + 1_200_000, Happen::Ev(Ev::FocusLost));
                r.at(T0 + 1_400_000, Happen::Ev(Ev::FocusBack));
            }),
            T0 + 2_400_000,
        ),
        (
            "focus",
            Box::new(|r| {
                r.at_step.push(("audio", Happen::Ev(Ev::FocusLost)));
                r.at(T0 + 1_300_000, Happen::Ev(Ev::FocusBack));
            }),
            T0 + 2_300_000,
        ),
    ];
    for (name, set, again) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        set(&mut rig);
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(applied_at(&rig), [T0 + SECOND, again], "{name}");
        assert_eq!(rig.count("revalidate"), 2, "{name}");
        assert_eq!(rig.closes, [("revalidate", 1)], "{name}");
        assert_eq!(rig.count("discard"), 0, "{name}");
        assert_eq!(rig.opened_at(), again, "{name}");
    }
}

/// A15, O-15: a commit whose reply is held and then fails ends the run;
/// nothing is discarded and acceptance is not reopened.
#[test]
fn a_delayed_then_failed_commit_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_waits = 100;
    rig.commit_fails = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert_eq!(rig.clock.now().0, T0 + SECOND + 100_000);
    assert_eq!(last_steps(&rig, 2), ["commit", "end presentation"]);
    assert_eq!(rig.count("reopen"), 0);
    assert_eq!(rig.count("discard"), 0);
}

/// A15, O-16: an end of loading whose acknowledgement is held and then
/// fails ends the run, the commit standing.
#[test]
fn a_failed_end_loading_acknowledgement_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.end_loading_waits = 100;
    rig.end_loading_fails = true;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert_eq!(rig.clock.now().0, T0 + SECOND + 100_000);
    assert_eq!(
        last_steps(&rig, 3),
        ["commit", "end loading", "end presentation"]
    );
    assert_eq!(rig.count("reopen"), 0);
    assert_eq!(rig.count("discard"), 0);
    assert_eq!(loaded.attempt().number, 0);
}

// ------------------------------------------------------- §2.3 acceptance

/// A11: a local retry reverses the close at once: acceptance stays open,
/// so during the next wait a START adjustment and a settings key both
/// act, and both reach the starting settings.
#[test]
fn a_local_retry_keeps_holds_selection_and_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("audio", Happen::Ev(Ev::Tap(DISPLAY_UP))));
    rig.at(T0 + 1_200_000, Happen::Ev(Ev::StartAdjust));
    rig.at(T0 + 1_400_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.model.discarded, 0);
    assert_eq!(rig.model.acted, 3);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 2_400_000]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 2_000);
    assert_eq!(rig.applied[1].revision.reducer, 1);
    assert_eq!(rig.count("end presentation"), 1);
    assert_eq!(rig.closes, [("revalidate", 1)]);
}

/// A12: the revalidation with nothing changed closes acceptance on both
/// sides until recording opens: presses at the commit and the end of
/// loading are discarded.
#[test]
fn the_revalidation_closes_both_sides() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step
        .push(("commit", Happen::Ev(Ev::Tap(DISPLAY_UP))));
    rig.at_step
        .push(("end loading", Happen::Ev(Ev::StartAdjust)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.closes, [("revalidate", 1)]);
    assert_eq!(rig.model.discarded, 2);
    assert_eq!(rig.model.acted, 0);
    assert_eq!(rig.generation, 1);
    assert_eq!(loaded.settings().device.display_offset_us, DISPLAY);
}

/// A14, O-17: a quit at the last look leaves with the installation
/// committed and reopens acceptance on both sides.
#[test]
fn a_quit_at_the_last_look_reopens_tuning_on_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Quit));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(
        last_steps(&rig, 4),
        ["commit", "end loading", "end presentation", "reopen"]
    );
    assert_eq!(rig.count("discard"), 0);
    assert_eq!(rig.count("open"), 0);
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(&mut rig);
}

/// The steps from the first end of loading on.
fn after_end_loading(rig: &Loader) -> Vec<&'static str> {
    let steps = rig.steps();
    let from = steps.iter().position(|&s| s == "end loading").unwrap();
    steps[from..].to_vec()
}

/// A18, O-18: an opening deferred by a held START reopens tuning, resumes
/// the loading presentation and is asked again at the release alone,
/// with the same generation.
#[test]
fn a_deferred_opening_reopens_tuning() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::Tap(COVER)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(
        after_end_loading(&rig),
        [
            "end loading",
            "deferred",
            "defer",
            "reopen",
            "close",
            "apply",
            "end loading",
            "open"
        ]
    );
    // Released at 1.3 s: no quiet second from the tap at 1.1 s.
    assert_eq!(rig.times("close"), [T0 + 1_300_000]);
    assert_eq!(rig.opened_at(), T0 + 1_300_000);
    assert_eq!(rig.model.acted, 1);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
    assert_eq!(rig.generation, 1);
}

/// A18f: focus lost during the held commit reply: the opening is deferred
/// for focus and asked again a quiet second after focus returns.
#[test]
fn focus_lost_during_the_commit_reply_waits_a_quiet_second_after_return() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_waits = 100;
    rig.at(T0 + 1_050_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_200_000, Happen::Ev(Ev::FocusBack));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("deferred"), [T0 + 1_100_000]);
    assert_eq!(rig.times("close"), [T0 + 2_200_000]);
    assert_eq!(rig.opened_at(), T0 + 2_200_000);
    assert_eq!(rig.count("commit"), 1);
    assert_eq!(rig.count("discard"), 0);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
    assert_eq!(rig.reports[0].attempt, 1);
}

/// A18f: focus lost and back during the held end-of-loading
/// acknowledgement: deferred for focus, asked again a quiet second after
/// the return.
#[test]
fn focus_lost_during_the_end_loading_acknowledgement_waits_a_quiet_second_after_return() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.end_loading_waits = 100;
    rig.at(T0 + 1_020_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_080_000, Happen::Ev(Ev::FocusBack));
    run(&mut loaded, &mut rig).unwrap();
    // Focused again at the request, but lost since: deferred for focus.
    assert_eq!(rig.times("deferred"), [T0 + 1_100_000]);
    assert_eq!(rig.times("close"), [T0 + 2_080_000]);
    // The second end of loading takes its 100 ms too.
    assert_eq!(rig.opened_at(), T0 + 2_180_000);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
}

/// A18f: focus lost while an opening is deferred by a hold: the release
/// is not enough, the quiet second from the return is waited.
#[test]
fn focus_lost_while_deferred_waits_a_quiet_second_after_return() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_200_000, Happen::Ev(Ev::FocusBack));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("deferred"), [T0 + SECOND]);
    assert_eq!(rig.times("close"), [T0 + 2_200_000]);
    assert_eq!(rig.opened_at(), T0 + 2_200_000);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
}

/// §2.10 R26: deferred for focus, a tap right after the wait's quiet look
/// lands in the copy the close takes: that copy is not quiet, so
/// acceptance reopens and the wait goes on, a quiet second from the tap;
/// the copy then closed has the tap.
#[test]
fn a_tap_between_the_deferrals_quiet_look_and_its_close_waits_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_waits = 100;
    rig.at(T0 + 1_050_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_200_000, Happen::Ev(Ev::FocusBack));
    rig.after_quiet = Some((T0 + 2_000_000, Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("deferred"), [T0 + 1_100_000]);
    assert_eq!(rig.times("close"), [T0 + 2_200_000, T0 + 3_200_000]);
    assert_eq!(rig.times("reopen"), [T0 + 1_100_000, T0 + 2_200_000]);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 3_200_000]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 1_000);
    assert_eq!(rig.opened_at(), T0 + 3_200_000);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1), ("close", 1)]);
}

/// §2.10 R24 (finding 6): the wait's last look is quiet, and a tap comes
/// right after it, before the opening's own copy. That copy is not quiet
/// at its time, so it is not applied: the wait goes on and the opening
/// takes its copy a quiet second after the tap, with the tap in it.
#[test]
fn an_event_between_the_wait_and_o1_goes_back_to_the_wait() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.after_quiet = Some((T0 + SECOND, Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    // The minimum ends at T0 + 1 s, where the wait's look is quiet; the
    // tap at that instant holds the opening another second.
    assert_eq!(applied_at(&rig), [T0 + 2 * SECOND]);
    assert_eq!(display_of(&rig, 0), DISPLAY + 1_000);
    assert_eq!(rig.applied[0].last_activity, Some(TimeUs(T0 + SECOND)));
    assert_eq!(opening_steps(&rig), OPENING);
    assert_eq!(rig.opened_at(), T0 + 2 * SECOND);
}

/// A20: the release takes a fresh snapshot as acceptance closes again and
/// applies it to the starting settings, with no number taken and no quiet
/// wait.
#[test]
fn the_release_snapshots_and_closes_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 1_300_000]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 1_000);
    assert_eq!(loaded.settings().device.display_offset_us, DISPLAY + 1_000);
    assert_eq!(rig.count("screen"), 1);
    assert_eq!(rig.reports[0].attempt, 1);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
}

/// A22: an opening deferred again after its release reopens and closes
/// again each time, with no added delay and the same generation.
#[test]
fn repeated_deferrals_each_reopen_and_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(200_000)));
    rig.at_step.push(("close", Happen::Hold(200_000)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("deferred"), [T0 + SECOND, T0 + 1_200_000]);
    assert_eq!(rig.times("reopen"), [T0 + SECOND, T0 + 1_200_000]);
    assert_eq!(rig.times("close"), [T0 + 1_200_000, T0 + 1_400_000]);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1), ("close", 1)]);
    assert_eq!(rig.opened_at(), T0 + 1_400_000);
}

/// A23: a played attempt after a deferral starts with the deferral's
/// tuning (its record's display offset), commits under the generation
/// asked first, and a retry then opens the standard way.
#[test]
fn a_deferred_opening_plays_with_its_starting_tuning_then_retries_standard() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.quit_on_open = 2;
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    // After the chart's start at 2.3 s: a played attempt.
    rig.commands.push((T0 + 2_500_000, Command::Retry));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    let [one, two] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    let display = |r: &PlayReport| r.record.as_ref().unwrap().devices.display_offset;
    assert_eq!(display(one), TimeUs(DISPLAY + 1_000));
    assert_eq!(one.end, game::PlayEnd::Aborted(game::AbortCause::Retry));
    // The retry's settings are the starting settings the deferral left.
    assert_eq!(loaded.settings().device.display_offset_us, DISPLAY + 1_000);
    assert_eq!((one.attempt, two.attempt), (1, 2));
    assert_eq!(rig.times("open")[0], T0 + 1_300_000);
    // The retry: the standard opening, with no loading state.
    let steps = rig.steps();
    let second = steps.iter().rposition(|&s| s == "before play").unwrap();
    assert_eq!(
        steps[second..],
        [
            "before play",
            "presentation",
            "audio",
            "begin",
            "poll",
            "ready",
            "commit",
            "end loading",
            "open"
        ]
    );
    assert_eq!(rig.steps()[second - 1], "screen");
    assert_eq!(rig.count("apply"), 2);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
    assert_eq!(rig.generation, 2);
}

/// A24: a quit while deferred (S4), or after the release before recording
/// opens (S5), abandons with tuning reopened.
#[test]
fn a_quit_during_a_deferral_reopens_tuning() {
    for released in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.at_step.push(("end loading", Happen::Hold(300_000)));
        if released {
            rig.at_step.push(("close", Happen::Quit));
        } else {
            rig.at(T0 + 1_100_000, Happen::Quit);
        }
        assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
        assert_eq!(rig.count("open"), 0, "{released}");
        assert_eq!(rig.count("discard"), 0, "{released}");
        let last = if released {
            "reopen"
        } else {
            "end presentation"
        };
        assert_eq!(last_steps(&rig, 1), [last], "{released}");
        assert_eq!(loaded.attempt().number, 0);
        assert_tuning_open(&mut rig);
    }
}

/// A25: a close or a thread's failure while deferred ends the run.
#[test]
fn a_failure_during_a_deferral_ends_the_run() {
    for what in [Happen::Close, Happen::InputFails, Happen::AudioFails] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.at_step.push(("end loading", Happen::Hold(300_000)));
        rig.at(T0 + 1_100_000, what);
        let ran = run(&mut loaded, &mut rig);
        match what {
            Happen::Close => assert_eq!(ran.unwrap(), Leave::Quit),
            Happen::InputFails => assert_failed(&ran, true, "deferred"),
            _ => assert_failed(&ran, false, "deferred"),
        }
        assert_eq!(rig.clock.now().0, T0 + 1_100_000, "{what:?}");
        assert_eq!(rig.count("open"), 0);
        assert_eq!(rig.count("reopen"), 1);
    }
}

/// A26, O-22: a logic refusal after a loading opening closes recording,
/// saves nothing and returns with the reason, tuning acceptance open.
#[test]
fn a_logic_refusal_after_a_loading_opening_returns_with_tuning() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.exchange = game::RECORDING_MEMORY;
    let ran = run(&mut loaded, &mut rig);
    assert!(
        matches!(
            ran,
            Err(LiveError::Logic(game::LogicError::RecordingTooLarge { .. }))
        ),
        "{ran:?}"
    );
    assert_eq!(rig.count("open"), 1);
    assert!(rig.reports.is_empty());
    assert_tuning_open(&mut rig);
}

/// A27: every exit that does not end the run leaves tuning acceptance
/// open, whichever side closed it.
#[test]
fn every_nonterminal_return_reopens_tuning() {
    let cases: Vec<(&str, Set)> = vec![
        (
            "quit in the wait",
            Box::new(|r| r.at(T0 + 500_000, Happen::Quit)),
        ),
        (
            "quit at O10",
            Box::new(|r| r.at_step.push(("ready", Happen::Quit))),
        ),
        ("refused at O5", Box::new(|r| r.refresh_refuses = true)),
        (
            "quit at the last look",
            Box::new(|r| r.at_step.push(("end loading", Happen::Quit))),
        ),
        (
            "quit while deferred",
            Box::new(|r| {
                r.at_step.push(("end loading", Happen::Hold(300_000)));
                r.at(T0 + 1_100_000, Happen::Quit);
            }),
        ),
        (
            "quit after the release",
            Box::new(|r| {
                r.at_step.push(("end loading", Happen::Hold(300_000)));
                r.at_step.push(("close", Happen::Quit));
            }),
        ),
        (
            "logic refusal",
            Box::new(|r| r.exchange = game::RECORDING_MEMORY),
        ),
    ];
    for (name, set) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        set(&mut rig);
        let ran = run(&mut loaded, &mut rig);
        match &ran {
            Ok(left) => assert_eq!(*left, Leave::Back, "{name}"),
            Err(e) => assert!(
                matches!(e, LiveError::Preparation(_) | LiveError::Logic(_)),
                "{name}: {e:?}"
            ),
        }
        assert_tuning_open(&mut rig);
    }
    // The page's refusals: a refit and a restore.
    for restore in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = if restore {
            practising(dir.path())
        } else {
            unfit(dir.path())
        };
        let mut rig = Loader::new(&loaded);
        rig.restore_refuses = restore;
        let ran = run(&mut loaded, &mut rig);
        assert!(
            matches!(ran, Err(LiveError::Preparation(_) | LiveError::Restore(_))),
            "{ran:?}"
        );
        assert_tuning_open(&mut rig);
    }
}

/// A28: a chart run without the loading opener keeps today's rules: no
/// wait, no snapshot, no closed gap, and a deferral that neither reopens
/// nor closes tuning.
#[test]
fn standard_openings_keep_todays_tuning_rules() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 100_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    assert_eq!(
        game::live::run_chart(&mut loaded, &mut rig).unwrap(),
        Leave::Back
    );
    assert_eq!(
        rig.steps(),
        [
            "screen",
            "before play",
            "presentation",
            "audio",
            "begin",
            "poll",
            "ready",
            "commit",
            "end loading",
            "deferred",
            "defer",
            "end loading",
            "open"
        ]
    );
    assert_eq!(rig.times("open"), [T0 + 300_000 + MS]);
    assert!(rig.closes.is_empty());
    assert_eq!(rig.model.discarded, 0);
}

/// §2.4.1: a practice Start's first round opens out of the loading state;
/// the loop's next round opens the standard way.
#[test]
fn a_two_round_practice_loop_uses_loading_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = practising(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.quit_on_open = 2;
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(rig.count("open"), 2);
    assert_eq!(rig.count("apply"), 1);
    assert_eq!(rig.count("refresh"), 1);
    assert_eq!(rig.count("revalidate"), 1);
    assert_eq!(rig.count("before play"), 1);
    assert_eq!(rig.count("presentation"), 1);
    assert_eq!(rig.count("restore"), 2);
    assert_eq!(rig.begun.len(), 2);
    let [one, two] = &rig.reports[..] else {
        panic!("{} rounds", rig.reports.len());
    };
    assert_eq!((one.attempt, two.attempt), (1, 2));
    assert_eq!(one.end, game::PlayEnd::Completed);
    assert!(rig.times("before play")[0] > rig.times("open")[0]);
    assert_eq!(rig.closes, [("revalidate", 1)]);
}

/// O-20 (the flow's part): adjustments at the opening's steps before the
/// revalidation go back to the wait once and reach the starting settings
/// and the record.
#[test]
fn an_adjustment_at_each_opening_step_reaches_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.quit_on_open = 0;
    for step in ["apply", "screen", "refresh", "audio", "ready"] {
        rig.at_step.push((step, Happen::Ev(Ev::Tap(DISPLAY_UP))));
    }
    rig.commands.push((T0 + 10 * SECOND, Command::Quit));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 2 * SECOND]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 5_000);
    let [report] = &rig.reports[..] else {
        panic!("{} plays", rig.reports.len());
    };
    assert_eq!(report.end, game::PlayEnd::Completed);
    let record = report.record.as_ref().unwrap();
    assert_eq!(record.devices.display_offset, TimeUs(DISPLAY + 5_000));
    assert_eq!(loaded.settings().device.display_offset_us, DISPLAY + 5_000);
}
