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
/// The pending preparation's poll slice, µs (production's 10 ms).
const PREP_SLICE_US: i64 = 10_000;

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

/// A record sink that saves nothing and answers every best read with
/// `answer`, counting the reads.
struct Lookups {
    answer: game::BestLookup,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl game::RecordSink for Lookups {
    fn save(&mut self, _: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        Ok(game::RecordRef {
            path: "saved.tprl".into(),
        })
    }

    fn best_before(&mut self, _: &game::BestKey, _: &rules::RulePreset) -> game::BestLookup {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answer.clone()
    }
}

/// `chart` loaded under the tapestry preset with the player's `target`,
/// its best reads answered with `answer` (no record sink for `None`); and
/// the reads.
fn scored(
    dir: &Path,
    chart: Chart,
    answer: Option<game::BestLookup>,
    target: game::ScoreTarget,
) -> (Loaded, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    settings.device.display_offset_us = DISPLAY;
    settings.player.target = target;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let reads = std::sync::Arc::default();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        records: answer.map(|answer| {
            Box::new(Lookups {
                answer,
                reads: std::sync::Arc::clone(&reads),
            }) as Box<dyn game::RecordSink>
        }),
        ..Parts::standard(&root, &settings.player)
    };
    let loaded = Loaded::load(root, settings, &chart_path, config(), parts).unwrap();
    (loaded, reads)
}

/// A known best of `ex`.
fn known(ex: u32) -> game::BestLookup {
    game::BestLookup::Known(game::BestScore {
        plays: 1,
        ex: Some(ex),
        bp: Some(0),
        max_combo: None,
        clear: None,
    })
}

/// The reads a [`Lookups`] counted.
fn reads(n: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
    n.load(std::sync::atomic::Ordering::SeqCst)
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
    /// A read of the record finds it mid-write (`true`) until the write
    /// ends (`false`), which wakes nothing.
    Unstable(bool),
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
    unstable: bool,
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
            unstable: false,
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
            Ev::Unstable(unstable) => self.unstable = unstable,
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
            stable: !self.unstable,
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
/// preparation) moves it 1 ms, but for the loading state's: those move it
/// as production's do ([`LoadingRig::loading_wait`],
/// [`LoadingRig::loading_poll`]).
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
    /// From this time on, the next look at the commands (`take`) also
    /// loses the screen: a loss between two looks at it.
    screen_fails_at_take: Option<i64>,
    prepares: usize,
    screen: ScreenKey,
    /// Final readinesses answering `Stale` with these screens, in order.
    stale: Vec<ScreenKey>,
    lifetime: u64,
    /// The acknowledgements (visible from, lifetime, presented at).
    acks: Vec<(i64, u64, i64)>,
    /// The declared loadend from a clock time on.
    loadends: Vec<(i64, Option<TimeUs>)>,
    /// The loadend a specialization committed in the lifetime declares,
    /// from the commit's answer on.
    commit_loadend: Option<Option<TimeUs>>,
    begun: Vec<game::PrepKey>,
    /// The best and target of the attempt each preparation was begun
    /// with: the last is the committed one's.
    scores: Vec<game::ScoreBasis>,
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
    /// The loading state's waits and polls: the clock time each began
    /// at and the `until` it was given.
    requests: Vec<(i64, Option<i64>)>,
    /// The pending preparation's polls go through production's
    /// [`game::poll_until`], each moving the clock 1 ms; their slices on the
    /// monotonic clock, with the clock time each began at.
    /// Each production poll's rig time and its slice's bounds: the
    /// deadline measured from just after the poll began (at most the slice
    /// `poll_until` asked for) and from the clock read before it (at least
    /// that slice), so no assertion depends on how long the thread ran.
    production_poll: Option<Vec<(i64, std::time::Duration, std::time::Duration)>>,
    /// When the rig's clock was last read (the production poll's bound).
    read_at: std::cell::Cell<Option<std::time::Instant>>,
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
            screen_fails_at_take: None,
            prepares: 0,
            screen: ScreenKey::default(),
            stale: Vec::new(),
            lifetime: 7,
            acks: vec![(T0, 7, T0)],
            loadends: Vec::new(),
            commit_loadend: None,
            begun: Vec::new(),
            scores: Vec::new(),
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
            requests: Vec::new(),
            production_poll: None,
            read_at: std::cell::Cell::new(None),
            shown: std::sync::Arc::default(),
            after_quiet: None,
        }
    }

    fn at(&mut self, at: i64, what: Happen) {
        self.events.push((at, what));
    }

    fn happen(&mut self, what: Happen) {
        self.happen_at(what, self.clock.now().0);
    }

    /// What happens, at its own time `at` (an event keeps the time it was
    /// received at, whenever the game thread looks).
    fn happen_at(&mut self, what: Happen, now: i64) {
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

    /// What a wait serves once the clock moved: the input script, the
    /// audio's commands and the happenings due.
    fn serve(&mut self) {
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
            let (at, what) = self.events.remove(i);
            self.happen_at(what, at);
        }
    }

    /// A poll's answer for the clock now.
    fn answer(
        &mut self,
        handle: &mut game::PrepHandle,
    ) -> Result<game::PrepPoll<Preparation>, SurfaceLost> {
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

    /// Takes a loading request from now, never for an instant already
    /// reached (the wait would spin on it), and moves the clock to the
    /// first of `ends` (now at the earliest) and `until`, where the look
    /// runs.
    fn request(&mut self, until: Option<TimeUs>, ends: [Option<i64>; 3]) {
        let now = self.clock.now().0;
        assert!(
            until.is_none_or(|u| u.0 > now),
            "a wait for {until:?} at {now}: {:?}",
            self.log
        );
        self.requests.push((now, until.map(|u| u.0)));
        assert!(
            self.requests.len() < 100_000,
            "the wait spins: {:?}",
            self.log
        );
        let end = ends
            .into_iter()
            .chain([until.map(|u| u.0)])
            .flatten()
            .map(|at| at.max(now))
            .min()
            .unwrap();
        self.clock.advance(TimeUs(end - now));
    }

    /// The next scripted happening that wakes production's parked wait:
    /// an input event (the window unparks it), a command or a close; not
    /// a record's write ending, a failure or the rest.
    fn next_wake(&self, now: i64) -> Option<i64> {
        self.events
            .iter()
            .filter(|(_, what)| {
                matches!(what, Happen::Ev(ev) if !matches!(ev, Ev::Unstable(_)))
                    || matches!(what, Happen::Quit | Happen::Close)
            })
            .map(|&(at, _)| at)
            // A held START's release changes the panels' level, which
            // wakes it too.
            .chain(self.holds.iter().map(|&(_, to)| to))
            .filter(|&at| at > now)
            .min()
    }

    /// The `until` the loading request beginning at `at` was given.
    fn until_at(&self, at: i64) -> Option<i64> {
        let [(_, until)] = self
            .requests
            .iter()
            .filter(|&&(t, _)| t == at)
            .copied()
            .collect::<Vec<_>>()[..]
        else {
            panic!("no one request at {at}: {:?}", self.requests);
        };
        until
    }
}

impl Rig for Loader {
    type Reader = SourceReader;
    type ScreenError = SurfaceLost;
    type ResultPrep = ();

    fn now(&self) -> TimeUs {
        self.read_at.set(Some(std::time::Instant::now()));
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        self.serve();
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.clock.now().0;
        if self.screen_fails_at_take.is_some_and(|at| at <= now) {
            self.screen_fails_at_take = None;
            self.screen_failed = true;
        }
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

    fn begin_preparation(&mut self, loaded: &Loaded, key: game::PrepKey) -> game::PrepHandle {
        self.step("begin");
        self.begun.push(key.clone());
        self.scores.push(loaded.attempt().score);
        game::PrepHandle::new(key, self.begun.len() as u64)
    }

    fn poll_preparation(
        &mut self,
        handle: &mut game::PrepHandle,
        deadline: std::time::Instant,
    ) -> Result<game::PrepPoll<Preparation>, SurfaceLost> {
        let least = deadline.saturating_duration_since(std::time::Instant::now());
        let most = self
            .read_at
            .get()
            .map_or(least, |read| deadline.saturating_duration_since(read));
        let now = self.clock.now().0;
        if let Some(slices) = &mut self.production_poll {
            slices.push((now, least, most));
        }
        self.step("poll");
        self.polls.push((self.clock.now().0, handle.ticket()));
        self.wait();
        self.answer(handle)
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
        if let Some(d) = self.commit_loadend.take() {
            self.loadends.push((self.clock.now().0, d));
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
    /// Parks the idle wake (250 ms) at most, as production does, ending
    /// at `until` and at the next happening that wakes production.
    fn loading_wait(&mut self, until: Option<TimeUs>) {
        let now = self.clock.now().0;
        let wake = self.next_wake(now);
        self.request(until, [Some(now + game::IDLE_WAIT.0), wake, None]);
        self.serve();
    }

    /// Polls the pending preparation for 10 ms at most, as production
    /// does, ending at `until` and when its answer comes; nothing else
    /// ends a poll.
    fn loading_poll(
        &mut self,
        handle: &mut game::PrepHandle,
        until: Option<TimeUs>,
    ) -> Result<game::PrepPoll<Preparation>, SurfaceLost> {
        if self.production_poll.is_some() {
            return game::poll_until(self, handle, until);
        }
        let now = self.clock.now().0;
        let answer = usize::try_from(handle.ticket() - 1)
            .ok()
            .and_then(|i| self.answers.get(i))
            .map_or(i64::MIN, |&(at, _)| at);
        self.step("poll");
        self.polls.push((now, handle.ticket()));
        // A poll until now, an entry's, waits for nothing and is not a
        // wait's request.
        if until == Some(TimeUs(now)) {
            self.serve();
            return self.answer(handle);
        }
        // An answer already in ends the poll at once.
        self.request(until, [Some(now + PREP_SLICE_US), Some(answer), None]);
        self.serve();
        self.answer(handle)
    }

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
    // What happens at the start has happened before the first look.
    rig.serve();
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

/// How long a loading wait may last (design.md "Play Lifecycle", the
/// loading state's wait): its bound at most, never past its `until`, and
/// nothing for an `until` reached.
#[test]
fn a_wait_slice_ends_at_its_until() {
    use std::time::Duration;
    let now = TimeUs(5_000_000);
    let idle = Duration::from_millis(250);
    let slice = |until: Option<i64>, most| game::wait_slice(until.map(TimeUs), now, most);
    assert_eq!(slice(Some(5_030_000), idle), Duration::from_millis(30));
    assert_eq!(slice(Some(5_249_999), idle), Duration::from_micros(249_999));
    assert_eq!(slice(Some(6_000_000), idle), idle);
    assert_eq!(slice(None, idle), idle);
    assert_eq!(
        slice(Some(5_003_000), Duration::from_millis(10)),
        Duration::from_millis(3)
    );
    assert_eq!(slice(Some(5_000_000), idle), Duration::ZERO);
    assert_eq!(slice(Some(4_000_000), idle), Duration::ZERO);
}

/// The loading state's wait (design.md "Play Lifecycle"): parking the
/// idle wake (250 ms) at most as production does, it asks to wake at the
/// minimum's end and looks there, not at the next idle wake past it. The
/// first frame's acknowledgement, seen at 250 ms (it wakes nothing), puts
/// the minimum's end at 1 150 ms, between idle wakes (1 000, 1 250).
#[test]
fn the_loading_wait_wakes_at_the_minimum_s_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0 + 150_000, 7, T0 + 150_000)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.until_at(T0 + 250_000), Some(T0 + 1_150_000));
    assert_eq!(applied_at(&rig), [T0 + 1_150_000]);
    assert_eq!(rig.opened_at(), T0 + 1_150_000);
}

/// The wait wakes at the tuning's quiet deadline: a tap at 620 ms makes
/// the tuning quiet at 1 620 ms, past the minimum (1 000 ms), whose look
/// asks for it; the idle wakes from there are 1 250 and 1 500, 1 750.
#[test]
fn the_loading_wait_wakes_at_the_quiet_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at(T0 + 620_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.model.acted, 1);
    assert_eq!(rig.until_at(T0 + 620_000), Some(T0 + SECOND));
    assert_eq!(rig.until_at(T0 + SECOND), Some(T0 + 1_620_000));
    assert_eq!(applied_at(&rig), [T0 + 1_620_000]);
}

/// The wait reopened after the commit wakes at the minimum the commit
/// lengthened: a specialization declaring 2 900 ms ends it at 2 900 ms,
/// between the idle wakes after the commit (2 750, 3 000).
#[test]
fn the_reopened_wait_wakes_at_the_lengthened_minimum() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(2_900_000)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("commit"), [T0 + SECOND]);
    assert_eq!(rig.until_at(T0 + 2_750_000), Some(T0 + 2_900_000));
    assert_eq!(rig.times("close"), [T0 + 2_900_000]);
    assert_eq!(rig.opened_at(), T0 + 2_900_000);
}

/// While the preparation is pending the wait polls it in 10 ms slices,
/// the last ending at the next wake: the minimum's end at 1 003 ms, off
/// the slices' grid, is looked at there, and the opening's steps begin
/// there while the preparation stays pending.
#[test]
fn a_pending_poll_ends_at_the_next_wake() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0, 7, T0 + 3_000)];
    rig.answers = vec![(T0 + 3 * SECOND, Preparation::Ready)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.until_at(T0 + SECOND), Some(T0 + 1_003_000));
    assert_eq!(applied_at(&rig)[0], T0 + 1_003_000);
    // Every slice before it was a whole 10 ms.
    let early: Vec<_> = rig
        .requests
        .iter()
        .filter(|&&(at, _)| at < T0 + SECOND)
        .collect();
    assert!(
        early.windows(2).all(|w| w[1].0 - w[0].0 == PREP_SLICE_US),
        "{early:?}"
    );
}

/// A condition that changes only on an event adds no timed wake: while
/// it alone holds the wait (focus lost, a held control, an overflow, a
/// record read mid-write), each look asks for none and parks the idle
/// wake, never for an instant already reached (no spin); the look after
/// the event that ends it (at once for one that wakes production, else
/// at the next idle wake) asks for the quiet deadline.
#[test]
fn an_event_condition_parks_until_its_event() {
    // The events, the look whose `until` is none, the look after the
    // last event, the opening's time.
    type Case = (&'static str, Vec<(i64, Ev)>, i64, i64, i64);
    let cases: [Case; 4] = [
        (
            "focus",
            vec![
                (100_000, Ev::Tap(DISPLAY_UP)),
                (200_000, Ev::FocusLost),
                (3_000_000, Ev::FocusBack),
            ],
            SECOND,
            3_000_000,
            4_000_000,
        ),
        (
            "held",
            vec![(100_000, Ev::Press(COVER)), (1_234_000, Ev::Release(COVER))],
            SECOND,
            1_234_000,
            2_234_000,
        ),
        (
            "overflow",
            vec![
                (100_000, Ev::Overflow),
                (1_200_000, Ev::FocusLost),
                (1_300_000, Ev::FocusBack),
            ],
            SECOND,
            1_300_000,
            2_300_000,
        ),
        (
            "unstable",
            vec![
                (300_000, Ev::Tap(DISPLAY_UP)),
                (900_000, Ev::Unstable(true)),
                (1_100_000, Ev::Unstable(false)),
            ],
            SECOND,
            1_250_000,
            1_300_000,
        ),
    ];
    for (name, events, none_at, ask_at, opens) in cases {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        let times: Vec<i64> = events.iter().map(|&(at, _)| T0 + at).collect();
        for (at, ev) in events {
            rig.at(T0 + at, Happen::Ev(ev));
        }
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(rig.until_at(T0 + none_at), None, "{name}");
        assert_eq!(rig.until_at(T0 + ask_at), Some(T0 + opens), "{name}");
        assert_eq!(applied_at(&rig), [T0 + opens], "{name}");
        // Between the look asking for none and the one asking again, the
        // idle wake apart or at an event: no zero-length parks.
        let parks: Vec<_> = rig
            .requests
            .iter()
            .filter(|&&(at, _)| at >= T0 + none_at && at < T0 + ask_at)
            .collect();
        assert!(
            parks.iter().all(|&&(_, until)| until.is_none()),
            "{name}: {parks:?}"
        );
        assert!(
            parks
                .windows(2)
                .all(|w| w[1].0 - w[0].0 == game::IDLE_WAIT.0 || times.contains(&w[1].0)),
            "{name}: {parks:?}"
        );
    }
}

/// The wait reopened after the commit looks before it waits: a
/// specialization declaring 1 100 ms commits at 1 000 ms, so the minimum
/// ends 100 ms on, sooner than an idle wake; it is looked at there.
#[test]
fn a_committed_minimum_sooner_than_the_idle_wake_is_not_waited_past() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(1_100_000)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("commit"), [T0 + SECOND]);
    assert_eq!(rig.until_at(T0 + SECOND), Some(T0 + 1_100_000));
    assert_eq!(rig.times("close"), [T0 + 1_100_000]);
}

/// A wait entered with its conditions already met (the first frame
/// presented 2 s ago, the tuning quiet) opens at once, waiting nothing:
/// its preparation, ready or still pending (until 3 s), is polled once
/// without waiting.
#[test]
fn a_wait_entered_with_its_conditions_met_waits_nothing() {
    for pending in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.acks = vec![(T0, 7, T0 - 2 * SECOND)];
        if pending {
            rig.answers = vec![(T0 + 3 * SECOND, Preparation::Ready)];
        }
        run(&mut loaded, &mut rig).unwrap();
        assert_eq!(applied_at(&rig), [T0], "{pending}");
        assert!(rig.requests.is_empty(), "{pending}: {:?}", rig.requests);
    }
}

/// A tap during a pending poll keeps the time it was received at, off
/// the polls' 10 ms grid: tapped at 625 ms, the tuning is quiet at
/// 1 625 ms, and the wait looks there while the preparation is pending.
#[test]
fn a_tap_during_a_pending_poll_keeps_its_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.answers = vec![(T0 + 3 * SECOND, Preparation::Ready)];
    rig.at(T0 + 625_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.applied[0].last_activity, Some(TimeUs(T0 + 625_000)));
    assert_eq!(rig.until_at(T0 + 1_620_000), Some(T0 + 1_625_000));
    assert_eq!(applied_at(&rig)[0], T0 + 1_625_000);
}

/// Deferred by a held START and then for focus, the deferral's waits are
/// the loading state's: the hold's release at 1 300 ms wakes it, and its
/// look asks for the quiet deadline a second after focus came back
/// (1 234 ms), where the opening closes.
#[test]
fn a_focus_deferral_waits_for_its_quiet_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("end loading", Happen::Hold(300_000)));
    rig.at(T0 + 1_100_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 1_234_000, Happen::Ev(Ev::FocusBack));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("deferred"), [T0 + SECOND]);
    assert_eq!(rig.until_at(T0 + 1_234_000), None);
    assert_eq!(rig.until_at(T0 + 1_300_000), Some(T0 + 2_234_000));
    assert_eq!(rig.times("close"), [T0 + 2_234_000]);
}

/// Production's poll ([`game::poll_until`], the default of
/// [`LoadingRig::loading_poll`]) asks the preparation for at most 10 ms
/// on the monotonic clock, and no further than the next wake is on the
/// rig's: at 500 ms (the minimum's end 503 ms away) a whole slice, at
/// 1 000 ms (3 ms away) 3 ms at most.
#[test]
fn production_s_poll_ends_at_the_next_wake() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0, 7, T0 + 3_000)];
    rig.answers = vec![(T0 + 3 * SECOND, Preparation::Ready)];
    rig.production_poll = Some(Vec::new());
    run(&mut loaded, &mut rig).unwrap();
    let slices = rig.production_poll.take().unwrap();
    // Each poll's (least, most): the slice asked for lies between them.
    let at = |t: i64| {
        let [(_, least, most)] = slices
            .iter()
            .filter(|&&(n, _, _)| n == T0 + t)
            .copied()
            .collect::<Vec<_>>()[..]
        else {
            panic!("no one poll at {t}");
        };
        (least, most)
    };
    let (least, most) = at(500_000);
    assert!(most > Duration::from_millis(3), "{most:?}");
    assert!(least <= Duration::from_millis(10), "{least:?}");
    let (least, _) = at(SECOND);
    assert!(least <= Duration::from_millis(3), "{least:?}");
    assert_eq!(applied_at(&rig)[0], T0 + 1_003_000);
}

/// A deadline already passed is never asked for: the quiet deadline
/// (1 100 ms) passes while the minimum still runs (to 2 500 ms), and the
/// minimum's end passes while focus is lost; every look after either asks
/// for the one still ahead, or for none.
#[test]
fn a_passed_deadline_is_not_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.acks = vec![(T0 + 1_500_000, 7, T0 + 1_500_000)];
    rig.at(T0 + 100_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    rig.at(T0 + 2_400_000, Happen::Ev(Ev::FocusLost));
    rig.at(T0 + 2_600_000, Happen::Ev(Ev::FocusBack));
    run(&mut loaded, &mut rig).unwrap();
    // The fixture refuses a request for an instant reached; these are the
    // looks between.
    assert_eq!(rig.until_at(T0 + 1_350_000), None);
    assert_eq!(rig.until_at(T0 + 1_600_000), Some(T0 + 2_500_000));
    assert_eq!(rig.until_at(T0 + 2_500_000), None);
    assert_eq!(rig.until_at(T0 + 2_600_000), Some(T0 + 3_600_000));
    assert_eq!(applied_at(&rig), [T0 + 3_600_000]);
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

/// L5b, O12a: a pending skin's specialization committed at the opening
/// declares 3 s, while the wait passed on the floor's second. The end of
/// loading waits for the 3 s from the same first frame with tuning
/// acceptance reopened, then closes it again on a fresh copy, applied as
/// the starting settings; the commit is taken once.
#[test]
fn a_committed_specializations_minimum_holds_the_end_of_loading() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(
        opening_steps(&rig),
        [
            "apply",
            "screen",
            "refresh",
            "audio",
            "ready",
            "revalidate",
            "commit",
            "reopen",
            "close",
            "apply",
            "end loading",
            "open"
        ]
    );
    assert_eq!(rig.times("commit"), [T0 + SECOND]);
    assert_eq!(rig.times("reopen"), [T0 + SECOND]);
    assert_eq!(rig.times("close"), [T0 + 3 * SECOND]);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 3 * SECOND]);
    assert_eq!(rig.times("end loading"), [T0 + 3 * SECOND]);
    assert_eq!(rig.opened_at(), T0 + 3 * SECOND);
    assert_eq!(rig.closes, [("revalidate", 1), ("close", 1)]);
    assert_eq!(rig.reports[0].attempt, 1);
}

/// L5b, O12a: a tap while the committed minimum runs acts (acceptance is
/// open), and the end of loading waits a quiet second from it; the copy
/// closed then has the tap.
#[test]
fn a_tap_while_a_committed_minimum_runs_acts_and_restarts_the_quiet_wait() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
    rig.at(T0 + 2_500_000, Happen::Ev(Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!((rig.model.acted, rig.model.discarded), (1, 0));
    assert_eq!(rig.times("close"), [T0 + 3_500_000]);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 3_500_000]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 1_000);
    assert_eq!(rig.opened_at(), T0 + 3_500_000);
    assert_eq!(rig.count("commit"), 1);
}

/// L5b, O12a: a tap right after the committed minimum's quiet look lands
/// in the copy the close takes: that copy is not quiet, so acceptance
/// reopens and the wait goes on a quiet second from the tap.
#[test]
fn a_tap_between_the_committed_minimums_quiet_look_and_its_close_waits_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
    rig.after_quiet = Some((T0 + 3 * SECOND, Ev::Tap(DISPLAY_UP)));
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.times("reopen"), [T0 + SECOND, T0 + 3 * SECOND]);
    assert_eq!(rig.times("close"), [T0 + 3 * SECOND, T0 + 4 * SECOND]);
    assert_eq!(applied_at(&rig), [T0 + SECOND, T0 + 4 * SECOND]);
    assert_eq!(display_of(&rig, 1), DISPLAY + 1_000);
    assert_eq!(rig.times("end loading"), [T0 + 4 * SECOND]);
    assert_eq!(rig.opened_at(), T0 + 4 * SECOND);
    assert_eq!(rig.count("commit"), 1);
}

/// L5b, O-17b: a quit while the committed minimum runs leaves with the
/// installation committed (nothing discarded), the number given back, and
/// acceptance open; loading is not ended and nothing opens.
#[test]
fn a_quit_while_a_committed_minimum_runs_keeps_the_installation() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
    rig.at(T0 + 2 * SECOND, Happen::Quit);
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Back);
    assert_eq!(rig.clock.now().0, T0 + 2 * SECOND);
    assert_eq!(
        last_steps(&rig, 3),
        ["commit", "reopen", "end presentation"]
    );
    assert_eq!(rig.count("discard"), 0);
    assert_eq!(rig.count("end loading"), 0);
    assert_eq!(rig.count("open"), 0);
    assert_eq!(loaded.attempt().number, 0);
    assert_tuning_open(&mut rig);
}

/// L5b, O12a, I8: the screen lost while the committed minimum runs ends
/// the run as a lost screen, alone or with a quit in the same slice (the
/// run's end goes before the quit): it is not built again (the
/// replacement would hold no prepared attempt), the commit is not taken
/// again, loading is not ended and recording never opens.
#[test]
fn a_screen_lost_while_a_committed_minimum_runs_ends_the_run() {
    for with_quit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
        rig.at(T0 + 2 * SECOND, Happen::ScreenFails);
        if with_quit {
            rig.at(T0 + 2 * SECOND, Happen::Quit);
        }
        let ran = run(&mut loaded, &mut rig);
        assert!(
            matches!(ran, Err(LiveError::ScreenLost)),
            "quit {with_quit}: {ran:?}"
        );
        assert_eq!(rig.clock.now().0, T0 + 2 * SECOND, "quit {with_quit}");
        assert_eq!(
            last_steps(&rig, 3),
            ["commit", "reopen", "end presentation"],
            "quit {with_quit}"
        );
        // O2's preparation only: no recovery after the commit.
        assert_eq!(rig.count("screen"), 1, "quit {with_quit}");
        assert_eq!(rig.count("commit"), 1, "quit {with_quit}");
        assert_eq!(rig.count("discard"), 0, "quit {with_quit}");
        assert_eq!(rig.count("end loading"), 0, "quit {with_quit}");
        assert_eq!(rig.count("open"), 0, "quit {with_quit}");
        assert!(rig.reports.is_empty(), "quit {with_quit}");
        assert_eq!(loaded.attempt().number, 0, "quit {with_quit}");
    }
}

/// I8: the screen lost after the commit's wait looked at it, while it
/// looks for a quit, is never built again: the next look ends the run.
#[test]
fn a_screen_lost_between_two_looks_after_the_commit_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.commit_loadend = Some(Some(TimeUs(3 * SECOND)));
    rig.screen_fails_at_take = Some(T0 + 2 * SECOND);
    let ran = run(&mut loaded, &mut rig);
    assert!(matches!(ran, Err(LiveError::ScreenLost)), "{ran:?}");
    assert!(rig.screen_fails_at_take.is_none(), "the loss came");
    // O2's preparation only: no recovery after the commit.
    assert_eq!(rig.count("screen"), 1);
    assert_eq!(rig.count("commit"), 1);
    assert_eq!(rig.count("end loading"), 0);
    assert_eq!(rig.count("open"), 0);
    assert!(rig.reports.is_empty());
    assert_eq!(loaded.attempt().number, 0);
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

/// O-1 and H4 (finding 8): a quit in the wait, then a failure or a close
/// while the discard settles the preparation (a blocking cleanup): the
/// run ends with that cause, not on the page.
#[test]
fn a_failure_during_the_abandonments_cleanup_ends_the_run() {
    for input in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = loaded(dir.path());
        let mut rig = Loader::new(&loaded);
        rig.at(T0 + 500_000, Happen::Quit);
        rig.at_step.push(("discard", fails(input)));
        let ran = run(&mut loaded, &mut rig);
        assert_failed(&ran, input, "discard");
        assert_eq!(rig.count("discard"), 1);
        assert_eq!(rig.count("apply"), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.at(T0 + 500_000, Happen::Quit);
    rig.at_step.push(("discard", Happen::Close));
    assert_eq!(run(&mut loaded, &mut rig).unwrap(), Leave::Quit);
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
    // The 10 ms poll the answer ends.
    assert_eq!(rig.polls.last(), Some(&(T0 + 290_000, 1)));
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
        // The loss wakes nothing: seen at the next idle wake.
        assert_eq!(rig.times("screen")[0], T0 + 250_000, "{acked}");
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

/// Q2: ready with 900 ms of the minimum left: polled once without
/// waiting as the wait is entered, then in 10 ms slices until then; the
/// rest of the wait and the key check poll nothing.
#[test]
fn a_preparation_ready_before_the_minimum_ends_is_not_polled_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Loader::new(&loaded);
    rig.answers = vec![(T0 + 100_000, Preparation::Ready)];
    run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.polls.len(), 11);
    assert_eq!(rig.polls.last(), Some(&(T0 + 90_000, 1)));
    assert_eq!(applied_at(&rig), [T0 + SECOND]);
    assert_eq!(opening_steps(&rig), OPENING);
}

/// Q3, O-14: a local retry with the key unchanged keeps the ready
/// preparation: the next key check polls nothing and the final readiness
/// runs on it.
#[test]
fn a_ready_preparation_survives_a_same_key_local_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (mut loaded, lookups) =
        scored(dir.path(), chart(), Some(known(1)), game::ScoreTarget::Next);
    let mut rig = Loader::new(&loaded);
    rig.at_step.push(("audio", Happen::Ev(Ev::Tap(DISPLAY_UP))));
    run(&mut loaded, &mut rig).unwrap();
    // The play's best is read once, before the preparation at load-ready,
    // and the retried begin of the same play reads no more (stage 3,
    // S10-10): two notes, `"next"` above 1 is rank 15, 3.
    let basis = game::ScoreBasis {
        best: 1,
        best_known: true,
        target: 3,
    };
    assert_eq!(reads(&lookups), 1);
    assert_eq!(rig.scores, [basis]);
    assert_eq!(loaded.attempt().score, basis);
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
    let (mut loaded, lookups) =
        scored(dir.path(), chart(), Some(known(1)), game::ScoreTarget::Next);
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
    // One read for the play, the begin again after the wait and the
    // preparation begun again included; both preparations carry it.
    let basis = game::ScoreBasis {
        best: 1,
        best_known: true,
        target: 3,
    };
    assert_eq!(reads(&lookups), 1);
    assert_eq!(rig.scores, [basis, basis]);
}

/// The chart's first attempt is prepared at load-ready, before its play
/// begins: the preparation the skin commits carries the play's best and
/// target, read once, the preset and the screen unchanged so it is never
/// begun again (stage 3, S10-10). 27 notes: a known best of 40 under
/// `"next"` gives rank 21, 42; no library under `"max"` gives 54.
#[test]
fn the_preparation_at_load_ready_carries_the_plays_best_and_target() {
    let notes = || {
        let mut b = ChartBuilder::new(150_000);
        for i in 0..27 {
            b = b.tap(lane(1), TimeUs((1_000 + 100 * i) * MS));
        }
        b.build().unwrap()
    };
    let cases = [
        (Some(known(40)), game::ScoreTarget::Next, (40, true, 42), 1),
        (None, game::ScoreTarget::Max, (0, false, 54), 0),
    ];
    for (answer, target, (best, best_known, t), read) in cases {
        let dir = tempfile::tempdir().unwrap();
        let (mut loaded, lookups) = scored(dir.path(), notes(), answer, target);
        let mut rig = Loader::new(&loaded);
        run(&mut loaded, &mut rig).unwrap();
        let basis = game::ScoreBasis {
            best,
            best_known,
            target: t,
        };
        assert_eq!(rig.begun.len(), 1, "{target:?}");
        assert_eq!(rig.scores, [basis], "{target:?}");
        assert_eq!(loaded.attempt().score, basis, "{target:?}");
        assert_eq!(reads(&lookups), read, "{target:?}");
    }
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
        // A close wakes the deferred wait; a thread's failure wakes
        // nothing and is seen at its next look, the idle wake after the
        // look at 1 001 ms (held START).
        let seen = if matches!(what, Happen::Close) {
            1_100_000
        } else {
            1_251_000
        };
        assert_eq!(rig.clock.now().0, T0 + seen, "{what:?}");
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
