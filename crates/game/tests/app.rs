//! The scene flow (`game::app`) on a headless rig: a virtual clock, a
//! keyboard whose presses hit each chart's notes, audio and screen doubles,
//! and the player's actions scripted at times after one another. An action
//! the current scene does not take is dropped shortly after it is due, as a
//! press on a screen that ignores it, unless the test keeps it queued.
//!
//! Allocations are counted from each play's handoff to its close on the
//! property's thread alone (see `counting`), on a per-thread count no
//! other test can move: a play reserves everything at its
//! countdown's start, so none happen there.

mod common;
mod counting;

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bms_model::{Chart, ChartBuilder, SoundId, Speed, TimeSource, TimeUs, VirtualClock};
use common::{MS, lane};
use eval::{ControlId, JudgeEventKind};
use game::{
    AbortCause, App, AppRig, AudioQueue, AudioWaitError, ChartIngest, Closing, Command, DataRoot,
    DeviceSettings, IngestError, Ingested, InputWaitError, LiveError, LoadRefusal, Loaded,
    LogicError, MenuCommand, Opening, Parts, Pick, PlayEnd, PlayReport, PracticeInput,
    PracticeScreen, Refused, RenderSide, Rig, SessionConfig, SessionError, SessionEvent,
    SessionEventSink, Settings,
};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput, SourceReader,
    SourceSet,
};
use proptest::prelude::*;
use replay::{BuildProvenance, ReceiveMethod};

/// Where the rig's clock starts.
const CLOCK_START: i64 = 5_000_000;
/// Lane 1's key in the default key config (Z), and lane 3's (X).
const Z: ControlId = ControlId(0x2c);
const X: ControlId = ControlId(0x2d);
/// An action not taken this long after it is due is dropped.
const DROP_AFTER: i64 = 50 * MS;
/// Waits after the seal before the audio tail ends.
const TAIL_WAITS: usize = 30;

/// Chart A: lane 1 at 1.0 s and 1.5 s.
fn chart_a() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_500 * MS))
        .build()
        .unwrap()
}

/// Chart B: lane 1 at 1.2 s, lane 3 at 1.4 s.
fn chart_b() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_200 * MS))
        .tap(lane(3), TimeUs(1_400 * MS))
        .build()
        .unwrap()
}

/// One note 70 minutes in: its recording window is over the reservation.
fn long() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(4_200_000 * MS))
        .build()
        .unwrap()
}

/// BGM only: nothing to judge.
fn silent() -> Chart {
    ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .bgm(TimeUs(500 * MS), SoundId(0))
        .build()
        .unwrap()
}

/// The practice chart: 120 BPM, measure 0 empty, four taps on lane 1 in
/// each of measures 1 and 2 (2.0 s to 5.5 s at 1×).
const PRACTICE: &str = "#BPM 120\n#00111:01010101\n#00211:01010101\n";

/// The presses that hit the loaded `chart`'s taps on lanes 1 and 3, on its
/// play timeline.
fn presses(chart: &Chart) -> Vec<(i64, ControlId)> {
    let mut hits: Vec<(i64, ControlId)> = [(1, Z), (3, X)]
        .into_iter()
        .flat_map(|(l, c)| chart.lanes[l].iter().map(move |n| (n.time.0, c)))
        .collect();
    hits.sort_unstable();
    hits
}

/// Reads a chart by the file's contents.
struct Charts;

impl ChartIngest for Charts {
    fn max_bytes(&self) -> usize {
        1 << 10
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(
        &self,
        bytes: &[u8],
        dir: &Path,
        seed: u64,
        speed: bms_model::Speed,
    ) -> Result<Ingested, IngestError> {
        let chart = match bytes {
            // BMS text, through the loader at the session's speed.
            text if text.starts_with(b"#BPM") => {
                return game::BmsIngest.ingest(text, dir, seed, speed);
            }
            // A 40-minute BMS chart, through the loader at the session's
            // speed.
            b"slow" => {
                let text = b"#BPM 60\n#WAV01 a.wav\n#60011:01\n";
                return game::BmsIngest.ingest(text, dir, seed, speed);
            }
            b"a" => chart_a(),
            b"b" => chart_b(),
            b"silent" => silent(),
            b"long" => long(),
            other => panic!("no chart {other:?}"),
        };
        Ok(Ingested {
            chart,
            warnings: Vec::new(),
        })
    }
}

/// Every session event, in order.
#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<SessionEvent>>>);

impl SessionEventSink for Events {
    fn on_event(&mut self, ev: &SessionEvent) {
        self.0.borrow_mut().push(*ev);
    }
}

impl Events {
    fn all(&self) -> Vec<SessionEvent> {
        self.0.borrow().clone()
    }

    fn count(&self, f: impl Fn(&SessionEvent) -> bool) -> usize {
        self.0.borrow().iter().filter(|e| f(e)).count()
    }
}

/// How the third chart is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// Its file does not exist.
    Missing,
    /// Its bank does not load.
    Bank,
    /// It has no judgeable notes, refused at the opening.
    Silent,
    /// Its recording window is over the reservation, refused at the
    /// opening.
    TooLong,
    /// The preset file is not a preset; the refusal shown fixes it.
    Preset,
    /// It is 40 minutes long and the session plays at 50 %: its real
    /// times pass ±2^32 µs, refused by the loader.
    TooLongAtSpeed,
}

/// What the player does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Choose(usize),
    /// P on chart `.0`: its practice screen.
    Practice(usize),
    /// An input on the practice screen.
    Page(PracticeInput),
    Retry,
    Quit,
    Back,
    Next,
    Prev,
    Close,
}

/// The rig's view of the flow, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Select,
    Refused(usize),
    Load(usize),
    Shown(u32),
    /// The practice screen entered, with its notice.
    Practice(Option<String>),
}

#[derive(Debug, thiserror::Error)]
#[error("the bank cannot be built")]
struct BankLost;

#[derive(Debug, thiserror::Error)]
#[error("the surface cannot be made again")]
struct SurfaceLost;

struct Headless {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    paths: Vec<PathBuf>,
    preset: PathBuf,
    refusal: Refusal,
    /// The actions, each `(ms after the previous one was taken, act)`.
    acts: Vec<(i64, Act)>,
    next: usize,
    due: i64,
    quitting: bool,
    chosen: Option<usize>,
    /// Charts whose plays the hook leaves unsaved, and each chart the hook
    /// was asked about, in order (dev-tools).
    #[cfg(feature = "dev-tools")]
    unsaved: Vec<usize>,
    #[cfg(feature = "dev-tools")]
    configured: Vec<usize>,
    /// Charts the hook sets to autoplay (dev-tools).
    #[cfg(feature = "dev-tools")]
    autoplay: Vec<usize>,
    sounds: Option<AudioQueue>,
    sealed: bool,
    waits_since_seal: usize,
    /// Recording opened and the play not yet handed over.
    opened: bool,
    in_selection: bool,
    // Failures the test injects.
    screen_fails_at: i64,
    prepare_fails_from: usize,
    prepares: usize,
    input_fails_at: i64,
    audio_gone: bool,
    close_in_load: bool,
    // What the rig saw.
    seen: Vec<Seen>,
    entries: usize,
    violations: Vec<String>,
    /// Every load with its plays' reports.
    loads: Vec<(usize, Vec<PlayReport>)>,
    refusals: Vec<String>,
    /// Openings refused before the play was handed over, by chart.
    refused_opens: Vec<usize>,
    starts: usize,
    chooses: usize,
    /// Actions stay queued until a scene takes them.
    keep_queued: bool,
    /// Allocations are counted from each handoff to its close.
    count_allocs: bool,
    /// The play handed over and not yet closed.
    play: Option<Play>,
    /// Whether each play handed over finalized a watermark, in order.
    finalized: Vec<bool>,
    /// Each idle wait: when it began and its deadline.
    idle_waits: Vec<(i64, i64)>,
    /// Edits of the files run once the clock reaches their time.
    edits: Vec<(i64, Box<dyn FnOnce()>)>,
    /// The presses that hit the loaded chart's notes.
    hits: Vec<(i64, ControlId)>,
    /// Each load's speed, in order.
    speeds: Vec<Speed>,
    /// Each entry into the practice screen, with the model it was given.
    practice_entries: Vec<PracticeScreen>,
    /// The practice screen as last shown.
    practice_shown: Option<PracticeScreen>,
    /// Rounds whose restore is refused before the opening, from the next.
    restore_fails: usize,
    /// Rounds opened, by their restore point.
    restores: Vec<TimeUs>,
    /// Whether each load was a practice's Start, as `loads`.
    practiced: Vec<bool>,
    /// Each shelf state selection was shown, with its clock time: the
    /// revision and the chosen file's error, or why it cannot open.
    presets: Vec<(i64, PresetsSeen)>,
}

/// What selection was shown of the shelf.
type PresetsSeen = Result<(u64, Option<game::PresetFileError>), game::ShelfError>;

/// A play between its handoff and its close.
struct Play {
    finalized: bool,
    /// This thread's count at the handoff, when counting.
    allocs: Option<u64>,
}

impl Headless {
    fn new(dir: &Path, refusal: Refusal, acts: Vec<(i64, Act)>) -> Self {
        let paths: Vec<_> = ["a", "b", "refused", "practice"]
            .iter()
            .map(|n| dir.join(format!("{n}.chart")))
            .collect();
        std::fs::write(&paths[0], b"a").unwrap();
        std::fs::write(&paths[1], b"b").unwrap();
        std::fs::write(&paths[3], PRACTICE).unwrap();
        match refusal {
            Refusal::Missing => {}
            Refusal::Bank | Refusal::Preset => std::fs::write(&paths[2], b"a").unwrap(),
            Refusal::Silent => std::fs::write(&paths[2], b"silent").unwrap(),
            Refusal::TooLong => std::fs::write(&paths[2], b"long").unwrap(),
            Refusal::TooLongAtSpeed => std::fs::write(&paths[2], b"slow").unwrap(),
        }
        let mut sources = SourceSet::new(common::sources()).unwrap();
        let mut script = ScriptedInput::new(vec![ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(4), DeviceKind::Keyboard),
        }]);
        script.poll(TimeUs(CLOCK_START), &mut sources);
        let due = CLOCK_START + acts.first().map_or(0, |a| a.0) * MS;
        Self {
            clock: VirtualClock::new(TimeUs(CLOCK_START)),
            sources,
            script,
            paths,
            preset: dir.join("presets").join("mine.toml"),
            refusal,
            acts,
            next: 0,
            due,
            quitting: false,
            chosen: None,
            #[cfg(feature = "dev-tools")]
            unsaved: Vec::new(),
            #[cfg(feature = "dev-tools")]
            configured: Vec::new(),
            #[cfg(feature = "dev-tools")]
            autoplay: Vec::new(),
            sounds: None,
            sealed: false,
            waits_since_seal: 0,
            opened: false,
            in_selection: false,
            screen_fails_at: i64::MAX,
            prepare_fails_from: usize::MAX,
            prepares: 0,
            input_fails_at: i64::MAX,
            audio_gone: false,
            close_in_load: false,
            seen: Vec::new(),
            entries: 0,
            violations: Vec::new(),
            loads: Vec::new(),
            refusals: Vec::new(),
            refused_opens: Vec::new(),
            starts: 0,
            chooses: 0,
            keep_queued: false,
            count_allocs: false,
            play: None,
            finalized: Vec::new(),
            idle_waits: Vec::new(),
            edits: Vec::new(),
            presets: Vec::new(),
            hits: Vec::new(),
            speeds: Vec::new(),
            practice_entries: Vec::new(),
            practice_shown: None,
            restore_fails: 0,
            restores: Vec::new(),
            practiced: Vec::new(),
        }
    }

    fn at(&self) -> i64 {
        self.clock.now().0
    }

    fn due_act(&self) -> Option<Act> {
        let &(_, act) = self.acts.get(self.next)?;
        (self.at() >= self.due).then_some(act)
    }

    fn consume(&mut self) {
        self.next += 1;
        if let Some(&(delay, _)) = self.acts.get(self.next) {
            self.due = self.at() + delay * MS;
        }
    }

    /// After the clock moved.
    fn tick(&mut self) {
        // A flow that never ends fails instead of hanging.
        assert!(
            self.at() < CLOCK_START + 600_000_000,
            "the run did not end within ten minutes of its clock"
        );
        if self.opened {
            self.violations
                .push("the countdown waited before the play was set up".to_owned());
        }
        if self.sealed {
            self.waits_since_seal += 1;
        }
        if self.input_failure().is_none() {
            self.script.poll(self.clock.now(), &mut self.sources);
        }
        if let Some(s) = &mut self.sounds {
            while s.commands().pop().is_ok() {}
        }
        let now = self.at();
        while let Some(i) = self.edits.iter().position(|(at, _)| *at <= now) {
            (self.edits.remove(i).1)();
        }
        match self.due_act() {
            Some(Act::Close) => {
                self.quitting = true;
                self.consume();
            }
            Some(_) if !self.keep_queued && self.at() >= self.due + DROP_AFTER => self.consume(),
            _ => {}
        }
        // With nothing left to do, the player closes the window.
        if self.next >= self.acts.len() {
            self.quitting = true;
        }
    }

    /// The presses of the loaded chart from `origin` on the clock, those
    /// not yet past at `now`.
    fn play_script(&self, origin: i64, now: i64) -> ScriptedInput {
        let key = |at: i64, control: ControlId, value: i32| ScriptItem {
            at: TimeUs(at),
            action: Action::Packet(Packet {
                device: DeviceId(4),
                control,
                value,
                background: false,
            }),
        };
        ScriptedInput::new(
            self.hits
                .iter()
                .filter(|&&(t, _)| origin + t > now)
                .flat_map(|&(t, c)| [key(origin + t, c, 1), key(origin + t + 50 * MS, c, 0)])
                .collect(),
        )
    }

    fn reports(&self) -> impl Iterator<Item = &PlayReport> {
        self.loads.iter().flat_map(|(_, r)| r)
    }
}

impl Rig for Headless {
    type Reader = SourceReader;
    type ScreenError = SurfaceLost;

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        self.tick();
    }

    fn take(&mut self) -> Option<Command> {
        let c = match self.due_act()? {
            Act::Retry => Command::Retry,
            Act::Quit => Command::Quit,
            _ => return None,
        };
        self.consume();
        Some(c)
    }

    fn take_menu(&mut self) -> Option<MenuCommand> {
        let c = match self.due_act()? {
            Act::Retry => MenuCommand::Retry,
            Act::Back | Act::Quit => MenuCommand::Back,
            Act::Next => MenuCommand::NextPreset,
            Act::Prev => MenuCommand::PrevPreset,
            _ => return None,
        };
        self.consume();
        Some(c)
    }

    fn quitting(&self) -> bool {
        self.quitting
    }

    fn prepare_screen(&mut self) -> Result<(), SurfaceLost> {
        self.prepares += 1;
        if self.prepares >= self.prepare_fails_from {
            return Err(SurfaceLost);
        }
        if self.at() >= self.screen_fails_at {
            self.screen_fails_at = i64::MAX;
        }
        Ok(())
    }

    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        if self.audio_gone {
            return Err(AudioWaitError::Gone);
        }
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        if let Some(e) = self.input_failure() {
            return Err(e);
        }
        if self.in_selection {
            self.violations
                .push("recording opened in selection".to_owned());
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
        self.script = self.play_script(origin.0, now.0);
        self.opened = true;
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
            axes: Vec::new(),
        })
    }

    fn start(&mut self, _origin: TimeUs, sounds: AudioQueue, _screen: RenderSide) {
        self.opened = false;
        self.starts += 1;
        self.sounds = Some(sounds);
        self.sealed = false;
        self.waits_since_seal = 0;
        self.play = Some(Play {
            finalized: false,
            allocs: self.count_allocs.then(counting::thread_calls),
        });
        // Last: anything the rig itself frees above is not the play's.
        if self.count_allocs {
            counting::track(true);
        }
    }

    fn measured(&mut self, advance: &game::Advance, _: &[eval::JudgeEvent], _: TimeUs) {
        if let Some(p) = &mut self.play {
            p.finalized |= advance.watermark.is_some();
        }
    }

    fn focus_lost(&self) -> bool {
        self.sources.gate().is_tripped()
    }

    fn connected(&self) -> bool {
        true
    }

    fn audio_failure(&self) -> Option<audio::OutputError> {
        None
    }

    fn screen_failed(&self) -> bool {
        self.at() >= self.screen_fails_at
    }

    fn cancel_audio(&mut self) {
        if let Some(s) = &mut self.sounds {
            while s.commands().pop().is_ok() {}
        }
    }

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {
        self.sealed = true;
    }

    fn restore_ready(&mut self, at: TimeUs) -> Result<(), game::RestoreError> {
        if self.restore_fails > 0 {
            self.restore_fails -= 1;
            return Err(game::RestoreError::NotPrepared);
        }
        self.restores.push(at);
        Ok(())
    }

    fn audio_done(&self) -> bool {
        self.sealed && self.waits_since_seal >= TAIL_WAITS
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        if let Some(p) = self.play.take() {
            if let Some(before) = p.allocs {
                counting::track(false);
                let allocs = counting::thread_calls() - before;
                if allocs != 0 {
                    self.violations
                        .push(format!("{allocs} allocations after the handoff"));
                }
            }
            self.finalized.push(p.finalized);
        }
        if self.opened {
            self.opened = false;
            self.refused_opens.push(self.chosen.unwrap());
        }
        if let Some(e) = self.input_failure() {
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
        (self.at() >= self.input_fails_at).then_some(InputWaitError::Gone)
    }

    fn ended(&mut self, report: &PlayReport) {
        self.loads.last_mut().unwrap().1.push(report.clone());
    }

    fn idle_wait(&mut self, deadline: TimeUs) {
        self.idle_waits.push((self.at(), deadline.0));
        // Up to the deadline, or to the next action or edit if that is
        // sooner.
        let next = if self.next < self.acts.len() {
            self.due.max(self.at() + MS)
        } else {
            i64::MAX
        };
        let edit = self
            .edits
            .iter()
            .map(|(at, _)| *at)
            .min()
            .unwrap_or(i64::MAX);
        let to = deadline.0.min(next).min(edit).max(self.at() + 1);
        self.clock.advance(TimeUs(to - self.at()));
        self.tick();
    }

    fn show_end(&mut self, report: &PlayReport, _screen: &game::ResultScreen) {
        if self.in_selection {
            self.violations
                .push("a result shown in selection".to_owned());
        }
        // A result drawn again (its view changed) is the same result.
        if self.seen.last() != Some(&Seen::Shown(report.attempt)) {
            self.seen.push(Seen::Shown(report.attempt));
        }
    }
}

impl AppRig for Headless {
    type BankError = BankLost;

    #[cfg(feature = "dev-tools")]
    fn configure(&mut self, config: &mut SessionConfig, parts: &mut Parts) {
        let i = self.chosen.unwrap();
        self.configured.push(i);
        if self.unsaved.contains(&i) {
            parts.records = None;
            config.seed = 99;
        }
        if self.autoplay.contains(&i) {
            config.autoplay = Some(game::AutoKeySounds::Scheduled);
        }
    }

    fn choose(&mut self) -> Option<Pick> {
        let (i, practice) = match self.due_act()? {
            Act::Choose(i) => (i, false),
            Act::Practice(i) => (i, true),
            _ => return None,
        };
        self.consume();
        if !self.in_selection {
            self.violations
                .push("a chart chosen without entering selection".to_owned());
        }
        self.in_selection = false;
        self.chooses += 1;
        self.chosen = Some(i);
        let path = self.paths[i].clone();
        Some(if practice {
            Pick::Practice(path)
        } else {
            Pick::Play(path)
        })
    }

    fn load_chart(&mut self, loaded: &Loaded) -> Result<(), BankLost> {
        self.hits = presses(loaded.chart());
        self.speeds.push(loaded.config().speed);
        let i = self.chosen.unwrap();
        if self.close_in_load {
            self.quitting = true;
        }
        if i == 2 && self.refusal == Refusal::Bank {
            return Err(BankLost);
        }
        self.seen.push(Seen::Load(i));
        self.loads.push((i, Vec::new()));
        self.practiced.push(loaded.practice().is_some());
        Ok(())
    }

    fn show_refusal(&mut self, refusal: &LoadRefusal<BankLost>) {
        if !self.in_selection {
            self.violations
                .push("a refusal shown outside selection".to_owned());
        }
        if refusal.chart != self.paths[self.chosen.unwrap()] {
            self.violations
                .push(format!("refused {}", refusal.chart.display()));
        }
        self.seen.push(Seen::Refused(self.chosen.unwrap()));
        let kind = match &refusal.reason {
            Refused::Chart(SessionError::Io { .. }) => "io",
            Refused::Chart(SessionError::Preset(_)) => "preset",
            Refused::Chart(SessionError::Ingest(IngestError::Load(
                bms_parser::LoadError::TooLongAtSpeed { .. },
            ))) => "too long at speed",
            Refused::Chart(e) => panic!("{e:?}"),
            Refused::Bank(BankLost) => "bank",
            Refused::Play(LogicError::Eval(eval::EvalError::NoJudgeableNotes)) => "no notes",
            Refused::Play(LogicError::RecordingTooLarge { .. }) => "too long",
            Refused::Play(e) => panic!("{e:?}"),
            Refused::Practice(game::PracticeError::Autoplay) => "autoplay",
            Refused::Practice(e) => panic!("{e:?}"),
        };
        self.refusals.push(kind.to_owned());
        // The player fixes the preset file once its refusal is shown.
        if self.refusal == Refusal::Preset {
            let text = game::preset_to_toml(&rules::tapestry());
            std::fs::write(&self.preset, text).unwrap();
        }
    }

    fn presets_changed(&mut self, shelf: Result<&game::Shelf, &game::ShelfError>) {
        let seen = shelf
            .map(|s| (s.revision(), s.chosen_error().cloned()))
            .map_err(Clone::clone);
        self.presets.push((self.at(), seen));
    }

    fn entered_selection(&mut self) {
        self.in_selection = true;
        self.entries += 1;
        self.seen.push(Seen::Select);
    }

    fn entered_practice(&mut self, screen: &PracticeScreen) {
        self.seen
            .push(Seen::Practice(screen.notice().map(str::to_owned)));
        self.practice_entries.push(screen.clone());
        self.practice_shown = Some(screen.clone());
    }

    fn show_practice(&mut self, screen: &PracticeScreen) {
        self.practice_shown = Some(screen.clone());
    }

    fn take_practice(&mut self) -> Option<PracticeInput> {
        let Act::Page(input) = self.due_act()? else {
            return None;
        };
        self.consume();
        Some(input)
    }
}

fn app(dir: &Path, refusal: Refusal, events: &Events) -> App {
    if refusal == Refusal::Preset {
        let presets = dir.join("presets");
        std::fs::create_dir_all(&presets).unwrap();
        std::fs::write(presets.join("mine.toml"), b"not a preset").unwrap();
    }
    let preset = if refusal == Refusal::Preset {
        "mine"
    } else {
        "tapestry"
    };
    app_named(dir, refusal, preset, events)
}

/// [`app`] choosing the preset named `preset`.
fn app_named(dir: &Path, refusal: Refusal, preset: &str, events: &Events) -> App {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = Settings::load(&root).unwrap();
    settings.player.preset = preset.to_owned();
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 3,
        // The built charts ignore the speed; the 40-minute one loads at it.
        speed: if refusal == Refusal::TooLongAtSpeed {
            bms_model::Speed::SLOWEST
        } else {
            bms_model::Speed::NORMAL
        },
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    let parts_root = root.clone();
    // The parts report to no sink of their own: the run's sink receives
    // every chart's events.
    App::new(
        root,
        settings,
        config,
        Box::new(move |s: &Settings| Parts {
            ingest: Box::new(Charts),
            ..Parts::standard(&parts_root, &s.player)
        }),
        Box::new(events.clone()),
    )
}

/// Runs the flow over `acts`.
fn run(
    refusal: Refusal,
    acts: Vec<(i64, Act)>,
    setup: impl FnOnce(&mut Headless),
) -> (Headless, Events, Result<(), LiveError<SurfaceLost>>) {
    let dir = tempfile::tempdir().unwrap();
    let events = Events::default();
    let mut app = app(dir.path(), refusal, &events);
    let mut rig = Headless::new(dir.path(), refusal, acts);
    setup(&mut rig);
    let ran = app.run(&mut rig);
    (rig, events, ran)
}

fn attempts(reports: &[PlayReport]) -> Vec<u32> {
    reports.iter().map(|r| r.attempt).collect()
}

/// Choose A, play it to the result, retry it, go back, choose B and play
/// it, then close the window on its result.
#[test]
fn select_play_retry_back_select_again() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Retry),
        (4_000, Act::Back),
        (500, Act::Choose(1)),
        (4_000, Act::Close),
    ];
    let (rig, events, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        rig.seen,
        [
            Seen::Select,
            Seen::Load(0),
            Seen::Shown(1),
            Seen::Shown(2),
            Seen::Select,
            Seen::Load(1),
            Seen::Shown(1),
        ]
    );
    let [(0, a), (1, b)] = &rig.loads[..] else {
        panic!("{:?}", rig.seen);
    };
    assert_eq!(attempts(a), [1, 2]);
    assert_eq!(attempts(b), [1]);
    for r in rig.reports() {
        assert_eq!(r.end, PlayEnd::Completed);
        assert!(r.result.as_ref().unwrap().matches_live);
        let judged = r
            .live
            .iter()
            .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }));
        assert_eq!(judged.count(), 2);
    }
    let status = replay::EndStatus::PlayedToEnd;
    assert_eq!(
        events.all(),
        [
            SessionEvent::Select,
            SessionEvent::Load,
            SessionEvent::Start { attempt: 1 },
            SessionEvent::End { attempt: 1, status },
            SessionEvent::Result { attempt: 1 },
            SessionEvent::Start { attempt: 2 },
            SessionEvent::End { attempt: 2, status },
            SessionEvent::Result { attempt: 2 },
            SessionEvent::Select,
            SessionEvent::Load,
            SessionEvent::Start { attempt: 1 },
            SessionEvent::End { attempt: 1, status },
            SessionEvent::Result { attempt: 1 },
        ]
    );
}

/// The hook sets each chosen chart's session before it loads: chart B's
/// plays are not saved while chart A's, before and after it, are.
#[cfg(feature = "dev-tools")]
#[test]
fn configure_sets_each_charts_session() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Back),
        (500, Act::Choose(1)),
        (4_000, Act::Back),
        (500, Act::Choose(0)),
        (4_000, Act::Close),
    ];
    let (rig, _events, ran) = run(Refusal::Missing, acts, |rig| rig.unsaved = vec![1]);
    ran.unwrap();
    assert_eq!(rig.configured, [0, 1, 0]);
    let saved: Vec<(usize, bool)> = rig
        .loads
        .iter()
        .flat_map(|(i, r)| r.iter().map(move |r| (*i, r.saved.is_some())))
        .collect();
    assert_eq!(saved, [(0, true), (1, false), (0, true)]);
}

/// Each refusal returns to selection, signaled, with the reason and the
/// refused chart named; choosing a valid chart then plays it. A chart
/// refused at the opening reports no start of its own.
#[test]
fn refused_chart_returns_to_selection_then_valid_chart_plays() {
    for (refusal, kind) in [
        (Refusal::Missing, "io"),
        (Refusal::Bank, "bank"),
        (Refusal::Silent, "no notes"),
        (Refusal::TooLong, "too long"),
        (Refusal::Preset, "preset"),
        (Refusal::TooLongAtSpeed, "too long at speed"),
    ] {
        let acts = vec![
            (0, Act::Choose(2)),
            (500, Act::Choose(0)),
            (4_000, Act::Close),
        ];
        let (rig, events, ran) = run(refusal, acts, |_| {});
        ran.unwrap();
        assert!(
            rig.violations.is_empty(),
            "{refusal:?}: {:?}",
            rig.violations
        );
        assert_eq!(rig.refusals, [kind], "{refusal:?}");
        let loaded_refused = matches!(refusal, Refusal::Silent | Refusal::TooLong);
        let mut seen = vec![Seen::Select];
        if loaded_refused {
            seen.push(Seen::Load(2));
        }
        seen.extend([
            Seen::Select,
            Seen::Refused(2),
            Seen::Load(0),
            Seen::Shown(1),
        ]);
        assert_eq!(rig.seen, seen, "{refusal:?}");
        assert_eq!(
            rig.refused_opens,
            if loaded_refused { vec![2] } else { vec![] }
        );
        let (0, plays) = rig.loads.last().unwrap() else {
            panic!("{refusal:?}");
        };
        assert_eq!(attempts(plays), [1]);
        assert_eq!(plays[0].end, PlayEnd::Completed);
        // A chart file that is read reports its load before any refusal.
        let mut expected = vec![SessionEvent::Select];
        if !matches!(
            refusal,
            Refusal::Missing | Refusal::Preset | Refusal::TooLongAtSpeed
        ) {
            expected.push(SessionEvent::Load);
        }
        let status = replay::EndStatus::PlayedToEnd;
        expected.extend([
            SessionEvent::Select,
            SessionEvent::Load,
            SessionEvent::Start { attempt: 1 },
            SessionEvent::End { attempt: 1, status },
            SessionEvent::Result { attempt: 1 },
        ]);
        assert_eq!(events.all(), expected, "{refusal:?}");
    }
}

/// Commands queued behind the retry that leaves a result belong to that
/// result: a back queued after it does not leave the next result.
#[test]
fn queued_menu_commands_do_not_outlive_their_result() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Retry),
        (0, Act::Back),
        (4_000, Act::Close),
    ];
    let (rig, _, ran) = run(Refusal::Missing, acts, |r| r.keep_queued = true);
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        rig.seen,
        [Seen::Select, Seen::Load(0), Seen::Shown(1), Seen::Shown(2)]
    );
    let (_, plays) = &rig.loads[0];
    assert_eq!(attempts(plays), [1, 2]);
    assert_eq!(plays[1].end, PlayEnd::Completed);
}

/// A quit during play leaves the chart for selection, with no result; the
/// window closing during the next chart's play ends the run.
#[test]
fn quit_leaves_the_chart_close_ends_the_run() {
    let acts = vec![
        (0, Act::Choose(0)),
        (1_500, Act::Quit),
        (100, Act::Choose(1)),
        (1_500, Act::Close),
        // Never reached: the run has ended.
        (100, Act::Choose(0)),
    ];
    let (rig, _, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        rig.seen,
        [Seen::Select, Seen::Load(0), Seen::Select, Seen::Load(1)]
    );
    for r in rig.reports() {
        assert_eq!(r.end, PlayEnd::Aborted(AbortCause::Quit));
        assert!(r.record.is_some());
    }
    assert_eq!(rig.reports().count(), 2);
    assert_eq!(rig.next, 4);
}

/// Commands of a chart or a result do nothing in selection, and none is
/// kept for the chart chosen next.
#[test]
fn commands_in_selection_are_ignored() {
    let acts = vec![
        (0, Act::Retry),
        (0, Act::Back),
        (0, Act::Quit),
        (0, Act::Next),
        (0, Act::Prev),
        (100, Act::Choose(0)),
        (4_000, Act::Close),
    ];
    let (rig, _, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(rig.seen, [Seen::Select, Seen::Load(0), Seen::Shown(1)]);
    assert_eq!(rig.reports().next().unwrap().end, PlayEnd::Completed);
}

/// A preset switch on the result changes neither the scene nor the play
/// preset: the retry after it plays under the same preset.
#[test]
fn preset_switch_keeps_the_result_and_the_play_preset() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Next),
        (100, Act::Prev),
        (100, Act::Next),
        (100, Act::Retry),
        (4_000, Act::Close),
    ];
    let (rig, _, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert_eq!(
        rig.seen,
        [Seen::Select, Seen::Load(0), Seen::Shown(1), Seen::Shown(2)]
    );
    let (_, plays) = &rig.loads[0];
    assert_eq!(attempts(plays), [1, 2]);
    assert_eq!(plays[0].preset, plays[1].preset);
}

/// An input thread that fails while selection shows ends the run with its
/// cause before anything is loaded.
#[test]
fn input_failure_in_selection_ends_the_run() {
    let acts = vec![(1_000, Act::Choose(0)), (4_000, Act::Close)];
    let (rig, _, ran) = run(Refusal::Missing, acts, |r| {
        r.input_fails_at = CLOCK_START + 100 * MS;
    });
    assert!(
        matches!(ran, Err(LiveError::Input(InputWaitError::Gone))),
        "{ran:?}"
    );
    assert!(rig.loads.is_empty());
}

/// An input thread that fails during play ends that play, which is saved
/// and shown; the run ends with its cause when the player goes back.
#[test]
fn input_failure_in_play_ends_the_run_after_the_result() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Back),
        (500, Act::Choose(1)),
    ];
    let (rig, _, ran) = run(Refusal::Missing, acts, |r| {
        r.input_fails_at = CLOCK_START + 1_200 * MS;
    });
    assert!(matches!(ran, Err(LiveError::Input(_))), "{ran:?}");
    assert_eq!(rig.seen, [Seen::Select, Seen::Load(0), Seen::Shown(1)]);
    let r = rig.reports().next().unwrap();
    assert_eq!(r.end, PlayEnd::Aborted(AbortCause::InputFailed));
}

/// The screen fails in selection: it is built again and selection entered
/// anew; one that cannot be built ends the run.
#[test]
fn screen_failure_in_selection_rebuilds_or_ends_the_run() {
    let acts = vec![(500, Act::Choose(0)), (4_000, Act::Close)];
    let (rig, events, ran) = run(Refusal::Missing, acts.clone(), |r| {
        r.screen_fails_at = CLOCK_START + 100 * MS;
    });
    ran.unwrap();
    assert_eq!(
        rig.seen,
        [Seen::Select, Seen::Select, Seen::Load(0), Seen::Shown(1)]
    );
    assert_eq!(events.count(|e| *e == SessionEvent::Select), 2);

    let (rig, _, ran) = run(Refusal::Missing, acts, |r| {
        r.screen_fails_at = CLOCK_START + 100 * MS;
        r.prepare_fails_from = 1;
    });
    assert!(
        matches!(ran, Err(LiveError::Screen(SurfaceLost))),
        "{ran:?}"
    );
    assert!(rig.loads.is_empty());
}

/// Audio that cannot play ends the run; it is not a refusal of the chart.
#[test]
fn audio_failure_ends_the_run() {
    let acts = vec![(0, Act::Choose(0)), (4_000, Act::Close)];
    let (rig, _, ran) = run(Refusal::Missing, acts, |r| r.audio_gone = true);
    assert!(
        matches!(ran, Err(LiveError::Audio(AudioWaitError::Gone))),
        "{ran:?}"
    );
    assert!(rig.refusals.is_empty());
    assert_eq!(rig.starts, 0);
}

/// The window closes while a chart loads: no play opens and the run ends.
#[test]
fn close_while_loading_ends_the_run() {
    let acts = vec![(0, Act::Choose(0)), (4_000, Act::Choose(1))];
    let (rig, _, ran) = run(Refusal::Missing, acts, |r| r.close_in_load = true);
    ran.unwrap();
    assert_eq!(rig.seen, [Seen::Select, Seen::Load(0)]);
    assert_eq!(rig.starts, 0);
    assert!(rig.refused_opens.is_empty());
}

/// The user preset `mine`'s file in `dir`.
fn mine_path(dir: &Path) -> PathBuf {
    dir.join("presets").join("mine.toml")
}

/// The tapestry preset named `mine`, as `presets/mine.toml` loads it.
fn mine(preset: rules::RulePreset) -> rules::RulePreset {
    rules::RulePreset {
        id: "mine".to_owned(),
        ..preset
    }
}

/// The tapestry preset whose normal gauge gains nothing on a PERFECT.
fn flat_normal() -> rules::RulePreset {
    let mut p = rules::tapestry();
    p.gauges[0].change.perfect = rules::GaugeDelta::Fixed(rules::GaugeValue(0));
    p
}

/// Runs the flow over `acts` with the user preset `mine`, holding the
/// tapestry preset's rules, chosen; also the app, for its shelf, and the
/// data root, for the files.
fn run_mine(
    refusal: Refusal,
    acts: Vec<(i64, Act)>,
    setup: impl FnOnce(&mut Headless, &Path),
) -> (
    Headless,
    App,
    tempfile::TempDir,
    Result<(), LiveError<SurfaceLost>>,
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("presets")).unwrap();
    // Refused as a preset: the file never loads.
    let text = if refusal == Refusal::Preset {
        "not a preset".to_owned()
    } else {
        game::preset_to_toml(&rules::tapestry())
    };
    std::fs::write(mine_path(dir.path()), text).unwrap();
    let mut app = app_named(dir.path(), refusal, "mine", &Events::default());
    let mut rig = Headless::new(dir.path(), refusal, acts);
    setup(&mut rig, dir.path());
    let ran = app.run(&mut rig);
    (rig, app, dir, ran)
}

/// An edit of `path` to `text` at clock time `at`.
fn edit(rig: &mut Headless, at: i64, path: &Path, text: String) {
    let path = path.to_owned();
    rig.edits
        .push((at, Box::new(move || common::rewrite(&path, &text))));
}

/// The chosen preset's file changes, then breaks, while selection shows:
/// each is seen at the next poll, within one poll interval, and selection
/// is shown the shelf's new state, then the file's error.
#[test]
fn select_screen_polls_the_shelf() {
    let (changed, broken) = (CLOCK_START + 500 * MS, CLOCK_START + 1_500 * MS);
    let (rig, app, dir, ran) = run_mine(Refusal::Missing, vec![(3_000, Act::Close)], |r, dir| {
        edit(
            r,
            changed,
            &mine_path(dir),
            game::preset_to_toml(&flat_normal()),
        );
        edit(r, broken, &mine_path(dir), "windows = [".to_owned());
    });
    ran.unwrap();
    let within = |t: i64, from: i64| (from..=from + game::POLL_INTERVAL.0).contains(&t);
    let [
        (t0, Ok((r0, None))),
        (t1, Ok((r1, None))),
        (t2, Ok((r2, Some(e)))),
    ] = &rig.presets[..]
    else {
        panic!("{:?}", rig.presets);
    };
    assert_eq!(*t0, CLOCK_START);
    assert!(within(*t1, changed), "{t1}");
    assert!(within(*t2, broken), "{t2}");
    assert!(r0 < r1 && r1 < r2, "{r0} {r1} {r2}");
    assert_eq!(e.path, mine_path(dir.path()));
    // The reload is chosen; the broken file leaves it in use.
    let shelf = app.shelf().unwrap();
    assert_eq!(shelf.chosen(), &mine(flat_normal()));
    assert_eq!(shelf.chosen_error(), Some(e));
}

/// Chart A plays under `mine`; back in selection, `mine.toml` breaks, and
/// chart B, chosen next, plays the last good `mine`, its first report
/// naming the error. Fixing the file clears the error selection shows.
#[test]
fn shelf_survives_returning_to_select() {
    let acts = vec![
        (0, Act::Choose(0)),
        (4_000, Act::Back),
        (1_000, Act::Choose(1)),
        (4_000, Act::Back),
        (1_000, Act::Close),
    ];
    let (broken, fixed) = (CLOCK_START + 4_500 * MS, CLOCK_START + 9_500 * MS);
    let (rig, app, dir, ran) = run_mine(Refusal::Missing, acts, |r, dir| {
        edit(r, broken, &mine_path(dir), "windows = [".to_owned());
        edit(
            r,
            fixed,
            &mine_path(dir),
            game::preset_to_toml(&rules::tapestry()),
        );
    });
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    let [(0, a), (1, b)] = &rig.loads[..] else {
        panic!("{:?}", rig.seen);
    };
    let good = replay::preset_hash(&mine(rules::tapestry()));
    assert_eq!(a[0].record.as_ref().unwrap().preset.sha256, good);
    assert_eq!(a[0].reload_error, None);
    assert_eq!(b[0].record.as_ref().unwrap().preset.sha256, good);
    let e = b[0].reload_error.as_ref().unwrap();
    assert_eq!(e.path, mine_path(dir.path()));
    // Selection was shown the error, then its recovery.
    let errors: Vec<bool> = rig
        .presets
        .iter()
        .map(|(_, s)| s.as_ref().unwrap().1.is_some())
        .collect();
    let first_error = errors.iter().position(|&e| e).unwrap();
    assert!(errors[first_error..].contains(&false), "{errors:?}");
    assert!(!errors.last().unwrap());
    assert_eq!(app.shelf().unwrap().chosen_error(), None);
}

/// However a chart ends, the run's shelf comes back as it was lent: a
/// chart-load refusal, a bank refusal, a logic refusal, a close before
/// recording opens, a quit mid-play, and a fatal screen failure. Before the
/// chart, `mine` reloaded (a revision past 0) and then broke, so the shelf
/// holds the last good `mine` with its error, which a shelf opened again
/// would not: it would refuse to open.
#[test]
fn shelf_returns_on_every_outcome() {
    type Setup = fn(&mut Headless);
    type Check = fn(&Headless, &Result<(), LiveError<SurfaceLost>>) -> bool;
    let none: Setup = |_| {};
    let refused: fn(&'static str) -> Check = |kind| match kind {
        "io" => |r, ran| ran.is_ok() && r.refusals == ["io"],
        "bank" => |r, ran| ran.is_ok() && r.refusals == ["bank"],
        _ => |r, ran| ran.is_ok() && r.refusals == ["no notes"],
    };
    let cases: [(&str, Refusal, Act, Setup, Check); 6] = [
        (
            "load refusal",
            Refusal::Missing,
            Act::Choose(2),
            none,
            refused("io"),
        ),
        (
            "bank refusal",
            Refusal::Bank,
            Act::Choose(2),
            none,
            refused("bank"),
        ),
        (
            "logic refusal",
            Refusal::Silent,
            Act::Choose(2),
            none,
            refused("no notes"),
        ),
        (
            "close before recording",
            Refusal::Missing,
            Act::Choose(0),
            |r| r.close_in_load = true,
            |r, ran| ran.is_ok() && r.loads.len() == 1 && r.starts == 0,
        ),
        (
            "quit mid-play",
            Refusal::Missing,
            Act::Choose(0),
            |r| r.acts.insert(1, (1_500, Act::Quit)),
            |r, ran| {
                ran.is_ok()
                    && r.reports().map(|p| p.end).collect::<Vec<_>>()
                        == [PlayEnd::Aborted(AbortCause::Quit)]
            },
        ),
        (
            "fatal screen failure",
            Refusal::Missing,
            Act::Choose(0),
            |r| {
                r.screen_fails_at = CLOCK_START + 2_200 * MS;
                r.prepare_fails_from = 2;
            },
            |r, ran| {
                matches!(ran, Err(LiveError::Screen(_)))
                    && r.reports().map(|p| p.end).collect::<Vec<_>>()
                        == [PlayEnd::Aborted(AbortCause::ScreenFailed)]
            },
        ),
    ];
    let (reloaded, broken) = (CLOCK_START + 100 * MS, CLOCK_START + 400 * MS);
    for (name, refusal, choose, setup, happened) in cases {
        let acts = vec![(700, choose), (10_000, Act::Close)];
        let (rig, app, dir, ran) = run_mine(refusal, acts, |r, dir| {
            edit(
                r,
                reloaded,
                &mine_path(dir),
                game::preset_to_toml(&flat_normal()),
            );
            edit(r, broken, &mine_path(dir), "windows = [".to_owned());
            setup(r);
        });
        assert!(happened(&rig, &ran), "{name}: {ran:?} {:?}", rig.seen);
        // What selection was shown last before the choice.
        let Some((_, Ok((revision, Some(error))))) = rig
            .presets
            .iter()
            .rev()
            .find(|(at, _)| *at < CLOCK_START + 700 * MS)
            .cloned()
        else {
            panic!("{name}: {:?}", rig.presets);
        };
        assert!(revision >= 2, "{name}: {revision}");
        assert_eq!(error.path, mine_path(dir.path()), "{name}");
        let shelf = app.shelf().unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(shelf.revision(), revision, "{name}");
        assert_eq!(shelf.chosen(), &mine(flat_normal()), "{name}");
        assert_eq!(shelf.chosen_error(), Some(&error), "{name}");
    }
}

/// A chosen preset that never loaded refuses the chart with its error;
/// once the file is fixed, the next poll opens the shelf, selection is
/// shown it, and the refusal is over: a selection built again after a
/// screen failure does not show it again, and the next chart plays.
#[test]
fn unloadable_chosen_preset_refuses_then_heals_on_select() {
    let acts = vec![
        (0, Act::Choose(2)),
        (500, Act::Choose(0)),
        (4_000, Act::Close),
    ];
    let (rig, _, ran) = run(Refusal::Preset, acts, |r| {
        r.screen_fails_at = CLOCK_START + 300 * MS;
    });
    ran.unwrap();
    assert_eq!(rig.refusals, ["preset"]);
    assert_eq!(
        rig.seen,
        [
            Seen::Select,
            Seen::Select,
            Seen::Refused(2),
            Seen::Select,
            Seen::Load(0),
            Seen::Shown(1),
        ]
    );
    let errs: Vec<bool> = rig.presets.iter().map(|(_, s)| s.is_err()).collect();
    assert_eq!(errs, [true, true, false, false], "{:?}", rig.presets);
    // The refusal fixed the file: the next poll, within an interval of
    // the entry, opens it.
    let entered = rig.presets[1].0;
    let healed = rig.presets[2].0;
    assert!(
        healed - entered <= game::POLL_INTERVAL.0,
        "{entered} {healed}"
    );
    let (0, plays) = rig.loads.last().unwrap() else {
        panic!("{:?}", rig.seen);
    };
    assert_eq!(plays[0].preset, mine(rules::tapestry()));
}

/// A wake 1 µs before selection's poll deadline does not check; the wait
/// after it ends at the deadline, where the check runs, not an idle wait
/// later. The same holds for a shelf that cannot open yet.
#[test]
fn select_poll_runs_at_its_deadline_after_an_early_wake() {
    // The first poll runs at the entry; the next is due an interval later.
    let deadline = CLOCK_START + game::POLL_INTERVAL.0;
    for refusal in [Refusal::Missing, Refusal::Preset] {
        let (rig, _, _, ran) = run_mine(refusal, vec![(1_000, Act::Close)], |r, dir| {
            edit(
                r,
                deadline - 2,
                &mine_path(dir),
                game::preset_to_toml(&flat_normal()),
            );
            r.edits.push((deadline - 1, Box::new(|| {})));
        });
        ran.unwrap();
        let woke = rig
            .idle_waits
            .iter()
            .find(|&&(at, _)| at == deadline - 1)
            .unwrap_or_else(|| panic!("{refusal:?}: {:?}", rig.idle_waits));
        assert_eq!(woke.1, deadline, "{refusal:?}");
        let [(_, first), (at, Ok(_))] = &rig.presets[..] else {
            panic!("{refusal:?}: {:?}", rig.presets);
        };
        assert_eq!(first.is_err(), refusal == Refusal::Preset);
        assert_eq!(*at, deadline, "{refusal:?}");
    }
}

/// The practice chart's index.
const P: usize = 3;

/// `n` presses of `input` on the practice screen, 10 ms apart.
fn page(input: PracticeInput, n: usize) -> impl Iterator<Item = (i64, Act)> {
    std::iter::repeat_n((10, Act::Page(input)), n)
}

/// From the first row: the speed set `down` steps below 100 %, then the
/// cursor on Start.
fn to_start(down: usize) -> Vec<(i64, Act)> {
    use PracticeInput::{Down, Left};
    let mut acts: Vec<(i64, Act)> = page(Down, 2).collect();
    acts.extend(page(Left, down));
    acts.extend(page(Down, 4));
    acts
}

fn notices(rig: &Headless) -> Vec<Option<String>> {
    rig.seen
        .iter()
        .filter_map(|s| match s {
            Seen::Practice(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// P on the practice chart opens its screen at 1×; 75 % with repeat on
/// and Start load it at that speed and play round 1 to the end of its
/// ring-out, then round 2 with no result between; the quit key in round
/// 2 returns to the practice screen, and Back to selection. Round 1's
/// record is a section repeat at 15/20, attempt 1.
#[test]
fn practice_screen_repeats_and_quits() {
    let mut acts = vec![(0, Act::Practice(P))];
    acts.extend(to_start(5));
    acts.push((10, Act::Page(PracticeInput::Confirm)));
    // Round 1 runs about 11 s at 75 %: the quit falls inside round 2.
    acts.push((14_000, Act::Quit));
    acts.push((500, Act::Page(PracticeInput::Back)));
    acts.push((500, Act::Close));
    let (rig, events, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        rig.seen,
        [
            Seen::Select,
            Seen::Practice(None),
            Seen::Load(P),
            Seen::Practice(None),
            Seen::Select,
        ]
    );
    let speed = Speed::from_twentieths(15).unwrap();
    assert_eq!(rig.speeds, [speed]);
    let [(P, reports)] = &rig.loads[..] else {
        panic!("{:?}", rig.seen);
    };
    assert_eq!(attempts(reports), [1, 2]);
    let first = &reports[0];
    assert_eq!((first.end, first.tail_cut), (PlayEnd::Completed, None));
    let record = first.record.as_ref().unwrap();
    assert!(
        matches!(record.play_type, replay::PlayType::SectionRepeat(_)),
        "{:?}",
        record.play_type
    );
    assert_eq!(record.speed, speed);
    assert!(matches!(first.saved, Some(Ok(_))), "{:?}", first.saved);
    assert!(first.result.as_ref().unwrap().matches_live);
    // Every tap of the chart is a target, and every one was hit.
    let judged = first
        .live
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }))
        .count();
    assert_eq!(judged, 8);
    assert_eq!(reports[1].end, PlayEnd::Aborted(game::AbortCause::Quit));
    // No result between the rounds, nor after them.
    assert_eq!(
        events.count(|e| matches!(e, SessionEvent::Result { .. })),
        0
    );
    // The model came back as it was left: 75 %, its 1× measures.
    let [entered, back] = &rig.practice_entries[..] else {
        panic!("{:?}", rig.seen);
    };
    assert_eq!(back.speed(), speed);
    assert_eq!(back.measures(), entered.measures());
}

/// Every Start loads the chart, at an unchanged speed too: a first entry
/// at 100 % loads, a second Start loads again, and practicing A then B
/// at the same speed loads B.
#[test]
fn start_always_loads_the_current_chart() {
    use PracticeInput::{Back, Confirm};
    let mut acts = vec![(0, Act::Practice(0))];
    acts.extend(to_start(0));
    acts.extend([
        (10, Act::Page(Confirm)),
        (3_000, Act::Quit),
        (500, Act::Page(Confirm)),
        (3_000, Act::Quit),
        (500, Act::Page(Back)),
        (500, Act::Practice(1)),
    ]);
    acts.extend(to_start(0));
    acts.extend([
        (10, Act::Page(Confirm)),
        (3_000, Act::Quit),
        (500, Act::Page(Back)),
        (500, Act::Close),
    ]);
    let (rig, _events, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    let loads: Vec<&Seen> = rig
        .seen
        .iter()
        .filter(|s| matches!(s, Seen::Load(_)))
        .collect();
    assert_eq!(loads, [&Seen::Load(0), &Seen::Load(0), &Seen::Load(1)]);
    assert_eq!(rig.speeds, [Speed::NORMAL; 3]);
}

/// A Start refused for its restore, for a section without targets, and
/// for the logic step's refusal at the opening each leave the player on
/// the practice screen with the reason, entered again; a practice in an
/// autoplay session is refused on the select screen.
#[test]
fn practice_refusals_stay_on_the_practice_screen() {
    use PracticeInput::{Back, Confirm, Down, Left, Up};
    // The restore refused, then measure 1 alone, which has no targets.
    let mut acts = vec![(0, Act::Practice(P))];
    acts.extend(to_start(0));
    acts.push((10, Act::Page(Confirm)));
    acts.extend(page(Up, 5));
    acts.extend(page(Left, 10));
    acts.extend(page(Down, 5));
    acts.extend([
        (10, Act::Page(Confirm)),
        (500, Act::Page(Back)),
        (500, Act::Close),
    ]);
    let (rig, events, ran) = run(Refusal::Missing, acts, |rig| rig.restore_fails = 1);
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    let n = notices(&rig);
    assert_eq!(n.len(), 3, "{:?}", rig.seen);
    assert_eq!(n[0], None);
    assert!(n[1].as_deref().unwrap().contains("restore"), "{:?}", n[1]);
    assert!(
        n[2].as_deref().unwrap().contains("no notes to play"),
        "{:?}",
        n[2]
    );
    // The restore was refused after the load and before any opening; the
    // section without targets before any load.
    assert_eq!(rig.seen.iter().filter(|s| **s == Seen::Load(P)).count(), 1);
    assert_eq!(rig.starts, 0);
    assert!(rig.refused_opens.is_empty());
    assert_eq!(events.count(|e| matches!(e, SessionEvent::Start { .. })), 0);
    assert_eq!(rig.seen.last(), Some(&Seen::Select));

    // The logic step refuses the 70-minute chart's round at its opening.
    let mut acts = vec![(0, Act::Practice(2))];
    acts.extend(to_start(0));
    acts.extend([
        (10, Act::Page(Confirm)),
        (500, Act::Page(Back)),
        (500, Act::Close),
    ]);
    let (rig, _events, ran) = run(Refusal::TooLong, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    let n = notices(&rig);
    assert_eq!(n.len(), 2, "{:?}", rig.seen);
    assert!(
        n[1].as_deref().unwrap().contains("too long to record"),
        "{:?}",
        n[1]
    );
    assert_eq!(rig.refused_opens, [2]);
    assert_eq!(rig.starts, 0);
    assert_eq!(rig.seen.last(), Some(&Seen::Select));
}

/// An autoplay session's practice is refused on the select screen, which
/// shows why; the chart's practice screen never opens.
#[cfg(feature = "dev-tools")]
#[test]
fn an_autoplay_practice_is_refused_on_selection() {
    let acts = vec![(0, Act::Practice(P)), (500, Act::Close)];
    let (rig, _events, ran) = run(Refusal::Missing, acts, |rig| rig.autoplay = vec![P]);
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(rig.seen, [Seen::Select, Seen::Select, Seen::Refused(P)]);
    assert_eq!(rig.refusals, ["autoplay"]);
}

/// Every return to the practice screen enters it again with the model it
/// opened with: after the logic step's refusal at the opening, and after
/// a round's result (repeat off) is left with Back. The model is never
/// built again from the chart at the round's speed.
#[test]
fn every_return_reenters_the_practice_screen() {
    use PracticeInput::{Confirm, Down, Right};
    let mut acts = vec![(0, Act::Practice(P))];
    acts.extend(page(Down, 2));
    acts.extend(page(PracticeInput::Left, 5));
    acts.extend(page(Down, 3));
    // Repeat off.
    acts.push((10, Act::Page(Right)));
    acts.extend(page(Down, 1));
    acts.push((10, Act::Page(Confirm)));
    // Round 1 completes and shows its result.
    acts.push((14_000, Act::Back));
    acts.extend([(500, Act::Page(PracticeInput::Back)), (500, Act::Close)]);
    let (rig, events, ran) = run(Refusal::Missing, acts, |_| {});
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        rig.seen,
        [
            Seen::Select,
            Seen::Practice(None),
            Seen::Load(P),
            Seen::Shown(1),
            Seen::Practice(None),
            Seen::Select,
        ]
    );
    assert_eq!(
        events.count(|e| matches!(e, SessionEvent::Result { .. })),
        1
    );
    let [(P, reports)] = &rig.loads[..] else {
        panic!("{:?}", rig.seen);
    };
    let record = reports[0].record.as_ref().unwrap();
    assert!(
        matches!(record.play_type, replay::PlayType::MidChartStart(_)),
        "{:?}",
        record.play_type
    );
    let [entered, back] = &rig.practice_entries[..] else {
        panic!("{:?}", rig.seen);
    };
    // The 1× measures the screen opened with, and the values left there.
    assert_eq!(back.measures(), entered.measures());
    assert_eq!(back.measures()[1], TimeUs(2_000 * MS));
    assert_eq!(back.speed(), Speed::from_twentieths(15).unwrap());
    assert!(!back.repeat());

    // After a refusal at the opening too.
    let mut acts = vec![(0, Act::Practice(2))];
    acts.extend(to_start(0));
    acts.extend([
        (10, Act::Page(Confirm)),
        (500, Act::Page(PracticeInput::Back)),
        (500, Act::Close),
    ]);
    let (rig, _events, ran) = run(Refusal::TooLong, acts, |_| {});
    ran.unwrap();
    let [entered, back] = &rig.practice_entries[..] else {
        panic!("{:?}", rig.seen);
    };
    assert_eq!(back.measures(), entered.measures());
    assert!(back.notice().is_some());
}

/// A speed change whose chart file changed since the practice screen
/// opened is refused on the screen, with no load and the screen as it was;
/// opening practice again reads the new file and plays.
#[test]
fn a_changed_chart_file_refuses_a_speed_change() {
    use PracticeInput::{Back, Confirm};
    let mut acts = vec![(0, Act::Practice(P))];
    acts.extend(to_start(5));
    acts.extend([
        (10, Act::Page(Confirm)),
        (500, Act::Page(Back)),
        (500, Act::Practice(P)),
    ]);
    acts.extend(to_start(0));
    acts.extend([
        (10, Act::Page(Confirm)),
        (3_000, Act::Quit),
        (500, Act::Page(Back)),
        (500, Act::Close),
    ]);
    let (rig, _events, ran) = run(Refusal::Missing, acts, |rig| {
        let path = rig.paths[P].clone();
        let at = CLOCK_START + 20 * MS;
        rig.edits.push((
            at,
            Box::new(move || {
                std::fs::write(path, format!("{PRACTICE}#00411:01\n")).unwrap();
            }),
        ));
    });
    ran.unwrap();
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    let n = notices(&rig);
    assert!(
        n[1].as_deref().unwrap().contains("changed"),
        "{:?}",
        rig.seen
    );
    // The refused Start loaded nothing; the screen kept its 75 %.
    assert_eq!(rig.speeds, [Speed::NORMAL]);
    assert_eq!(
        rig.practice_entries[1].speed(),
        Speed::from_twentieths(15).unwrap()
    );
    // Opened again, the new file's measures 3 and 4 are there.
    let reopened = &rig.practice_entries[2];
    assert_eq!(
        reopened.measures().len(),
        rig.practice_entries[0].measures().len() + 2
    );
    assert_eq!(rig.loads.len(), 1);
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        6 => (0..3_usize).prop_map(Act::Choose),
        3 => Just(Act::Retry),
        2 => Just(Act::Quit),
        2 => Just(Act::Back),
        1 => Just(Act::Next),
        1 => Just(Act::Prev),
        1 => Just(Act::Close),
    ]
}

/// One action, or a practice: P on a chart (the practice chart among
/// them), Start at 100 %, the quit key some time in, and Back on the page.
fn step() -> impl Strategy<Value = Vec<(i64, Act)>> {
    let practice =
        (0..3_000_i64, 0..=P, 0..4_000_i64, 0..3_000_i64).prop_map(|(after, chart, quit, back)| {
            let mut acts = vec![(after, Act::Practice(chart))];
            acts.extend(to_start(0));
            acts.extend([
                (10, Act::Page(PracticeInput::Confirm)),
                (quit, Act::Quit),
                (back, Act::Page(PracticeInput::Back)),
            ]);
            acts
        });
    prop_oneof![
        8 => (0..3_000_i64, act()).prop_map(|a| vec![a]),
        2 => practice,
    ]
}

fn refusal() -> impl Strategy<Value = Refusal> {
    prop_oneof![
        Just(Refusal::Missing),
        Just(Refusal::Bank),
        Just(Refusal::Silent),
        Just(Refusal::TooLong),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Random selections, retries, quits, backs, preset switches, closes
    /// and practices (P, Start, the quit key, Back) at random times, over
    /// two built charts, one refused chart and the practice chart: the run
    /// ends cleanly; every play that finalized anything has
    /// a saved record that re-evaluates equal to live; attempts count from
    /// 1 per chart; every play is reserved at its countdown's start, before
    /// any wait and with nothing allocated after its handoff; and every
    /// entry into selection is signaled.
    #[test]
    fn any_command_sequence_ends_cleanly(
        refusal in refusal(),
        first in 0..3_usize,
        rest in proptest::collection::vec(step(), 0..20),
    ) {
        let acts = std::iter::once((0, Act::Choose(first)))
            .chain(rest.into_iter().flatten())
            .collect();
        counting::track(true);
        counting::thread_self_check();
        counting::track(false);
        let (rig, events, ran) = run(refusal, acts, |r| r.count_allocs = true);
        prop_assert!(ran.is_ok(), "{:?}", ran);
        prop_assert!(rig.quitting);
        prop_assert!(rig.violations.is_empty(), "{:?}", rig.violations);
        // A chart's plays count from 1; a practice's Starts go on counting
        // the plays of its one load.
        for ((chart, plays), &practiced) in rig.loads.iter().zip(&rig.practiced) {
            let from = if practiced {
                plays.first().map_or(1, |p| p.attempt)
            } else {
                1
            };
            let expected: Vec<u32> = (from..).take(plays.len()).collect();
            prop_assert_eq!(attempts(plays), expected, "chart {}", chart);
        }
        // Every play handed over was closed and reported, and each that
        // finalized a watermark has a record, saved, that re-evaluates to
        // its live stream.
        prop_assert_eq!(rig.starts, rig.reports().count());
        prop_assert_eq!(rig.finalized.len(), rig.starts);
        for (r, &finalized) in rig.reports().zip(&rig.finalized) {
            prop_assert_eq!(r.record.is_some(), finalized, "{:?}", r.end);
            let Some(record) = &r.record else { continue };
            prop_assert!(matches!(r.saved, Some(Ok(_))), "{:?}", r.saved);
            let result = r.result.as_ref().unwrap();
            prop_assert!(result.matches_live, "{:?}", r.end);
            let evaluation = result.evaluation.as_ref().unwrap();
            prop_assert_eq!(&evaluation.events, &r.live);
            prop_assert_eq!(&evaluation.tally, &r.tally);
            prop_assert_eq!(record.status.end == replay::EndStatus::PlayedToEnd,
                r.end == PlayEnd::Completed);
        }
        // Only the chart refused at the opening opens recording unplayed,
        // and only admitted plays report a start.
        let at_opening = matches!(refusal, Refusal::Silent | Refusal::TooLong);
        prop_assert!(rig.refused_opens.iter().all(|&c| c == 2 && at_opening));
        prop_assert_eq!(events.count(|e| matches!(e, SessionEvent::Start { .. })), rig.starts);
        prop_assert_eq!(events.count(|e| *e == SessionEvent::Select), rig.entries);
        let shown = rig.seen.iter().filter(|s| matches!(s, Seen::Shown(_))).count();
        prop_assert_eq!(events.count(|e| matches!(e, SessionEvent::Result { .. })), shown);
        // One entry at the start, and one for each chart chosen that
        // was refused or left; the run ends in selection or in a chart.
        let returned = rig.chooses - usize::from(!rig.in_selection);
        prop_assert_eq!(rig.entries, 1 + returned);
    }
}
