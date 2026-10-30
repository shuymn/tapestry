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

use bms_model::{Chart, ChartBuilder, SoundId, TimeSource, TimeUs, VirtualClock};
use common::{MS, lane};
use eval::{ControlId, JudgeEventKind};
use game::{
    AbortCause, App, AppRig, AudioQueue, AudioWaitError, ChartIngest, Closing, Command, DataRoot,
    DeviceSettings, IngestError, Ingested, InputWaitError, LiveError, LoadRefusal, Loaded,
    LogicError, MenuCommand, Opening, Parts, PlayEnd, PlayReport, Refused, RenderSide, Rig,
    SessionConfig, SessionError, SessionEvent, SessionEventSink, Settings,
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

/// The presses that hit `chart`'s notes, on its play timeline.
fn presses(chart: usize) -> &'static [(i64, ControlId)] {
    match chart {
        0 => &[(1_000 * MS, Z), (1_500 * MS, Z)],
        1 => &[(1_200 * MS, Z), (1_400 * MS, X)],
        _ => &[],
    }
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
}

/// A play between its handoff and its close.
struct Play {
    finalized: bool,
    /// This thread's count at the handoff, when counting.
    allocs: Option<u64>,
}

impl Headless {
    fn new(dir: &Path, refusal: Refusal, acts: Vec<(i64, Act)>) -> Self {
        let paths: Vec<_> = ["a", "b", "refused"]
            .iter()
            .map(|n| dir.join(format!("{n}.chart")))
            .collect();
        std::fs::write(&paths[0], b"a").unwrap();
        std::fs::write(&paths[1], b"b").unwrap();
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
            preset: dir.join("presets").join("tapestry.toml"),
            refusal,
            acts,
            next: 0,
            due,
            quitting: false,
            chosen: None,
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

    fn play_script(&self, chart: usize, origin: i64) -> ScriptedInput {
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
            presses(chart)
                .iter()
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
        self.script = self.play_script(self.chosen.unwrap(), origin.0);
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
        panic!("this rig plays no practice round");
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

    fn show_end(&mut self, report: &PlayReport) {
        if self.in_selection {
            self.violations
                .push("a result shown in selection".to_owned());
        }
        self.seen.push(Seen::Shown(report.attempt));
    }
}

impl AppRig for Headless {
    type BankError = BankLost;

    fn choose(&mut self) -> Option<PathBuf> {
        let Act::Choose(i) = self.due_act()? else {
            return None;
        };
        self.consume();
        if !self.in_selection {
            self.violations
                .push("a chart chosen without entering selection".to_owned());
        }
        self.in_selection = false;
        self.chooses += 1;
        self.chosen = Some(i);
        Some(self.paths[i].clone())
    }

    fn load_chart(&mut self, _loaded: &Loaded) -> Result<(), BankLost> {
        let i = self.chosen.unwrap();
        if self.close_in_load {
            self.quitting = true;
        }
        if i == 2 && self.refusal == Refusal::Bank {
            return Err(BankLost);
        }
        self.seen.push(Seen::Load(i));
        self.loads.push((i, Vec::new()));
        Ok(())
    }

    fn idle_wait(&mut self, deadline: TimeUs) {
        if !self.in_selection {
            self.violations
                .push("an idle wait outside selection".to_owned());
        }
        // Up to the deadline, or to the next action if that is sooner.
        let next = if self.next < self.acts.len() {
            self.due.max(self.at() + MS)
        } else {
            i64::MAX
        };
        let to = deadline.0.min(next).max(self.at() + MS);
        self.clock.advance(TimeUs(to - self.at()));
        self.tick();
    }

    fn show_refusal(&mut self, refusal: &LoadRefusal<BankLost>) {
        if !self.in_selection {
            self.violations
                .push("a refusal shown outside selection".to_owned());
        }
        if refusal.chart != self.paths[2] {
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
        };
        self.refusals.push(kind.to_owned());
        // The player fixes the preset file once its refusal is shown.
        if self.refusal == Refusal::Preset {
            let text = game::preset_to_toml(&rules::tapestry());
            std::fs::write(&self.preset, text).unwrap();
        }
    }

    fn entered_selection(&mut self) {
        self.in_selection = true;
        self.entries += 1;
        self.seen.push(Seen::Select);
    }
}

fn app(dir: &Path, refusal: Refusal, events: &Events) -> App {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = Settings::load(&root).unwrap();
    if refusal == Refusal::Preset {
        let presets = dir.join("presets");
        std::fs::create_dir_all(&presets).unwrap();
        std::fs::write(presets.join("tapestry.toml"), b"not a preset").unwrap();
    }
    settings.player.preset = "tapestry.toml".to_owned();
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

    /// Random selections, retries, quits, backs, preset switches and
    /// closes at random times, over two built charts and one refused
    /// chart: the run ends cleanly; every play that finalized anything has
    /// a saved record that re-evaluates equal to live; attempts count from
    /// 1 per chart; every play is reserved at its countdown's start, before
    /// any wait and with nothing allocated after its handoff; and every
    /// entry into selection is signaled.
    #[test]
    fn any_command_sequence_ends_cleanly(
        refusal in refusal(),
        first in 0..3_usize,
        rest in proptest::collection::vec((0..3_000_i64, act()), 0..20),
    ) {
        let acts = std::iter::once((0, Act::Choose(first))).chain(rest).collect();
        counting::track(true);
        counting::thread_self_check();
        counting::track(false);
        let (rig, events, ran) = run(refusal, acts, |r| r.count_allocs = true);
        prop_assert!(ran.is_ok(), "{:?}", ran);
        prop_assert!(rig.quitting);
        prop_assert!(rig.violations.is_empty(), "{:?}", rig.violations);
        for (chart, plays) in &rig.loads {
            let expected: Vec<u32> = (1..).take(plays.len()).collect();
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
