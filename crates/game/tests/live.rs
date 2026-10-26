//! The play lifecycle every driver shares (`game::live`), on a rig that
//! stands in for the game's threads: a virtual clock that is not at 0, an
//! input side that opens recording with the play's origin on that clock
//! (as the input thread does), and audio and screen doubles whose
//! acknowledgements and failures the test controls.

mod common;

use std::path::Path;

use bms_model::{Chart, ChartBuilder, TimeSource, TimeUs, VirtualClock};
use common::{MS, lane};
use eval::{ControlId, InputKind, JudgeEventKind, SourceId};
use game::{
    AbortCause, AudioQueue, ChartIngest, Closing, Command, DataRoot, DeviceSettings, IngestError,
    Ingested, LiveError, Loaded, Opening, Parts, PlayEnd, PlayReport, RenderSide, Rig,
    SessionConfig, Settings,
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

struct Built(Chart);

impl ChartIngest for Built {
    fn max_bytes(&self) -> usize {
        1 << 20
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(&self, _bytes: &[u8], _dir: &Path, _seed: u64) -> Result<Ingested, IngestError> {
        Ok(Ingested {
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

/// Lane 1 at 1.0 s and 1.5 s.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_500 * MS))
        .build()
        .unwrap()
}

fn loaded(dir: &Path) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = Settings::load(&root).unwrap();
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
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
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

/// A rig on a virtual clock: each wait moves it 1 ms and feeds the
/// script's batches at their clock times, as the input thread receives
/// them.
struct Fake {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    commands: Vec<(i64, Command)>,
    quit_at: i64,
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
    /// A suspend arrives while the close request is handled.
    suspend_in_close: bool,
    shows: usize,
    reports: Vec<PlayReport>,
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
            suspend_in_close: false,
            shows: 0,
            reports: Vec::new(),
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

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        // A run that never ends fails instead of hanging.
        assert!(
            self.clock.now().0 < CLOCK_START + 60_000_000,
            "the run did not end within a minute of its clock"
        );
        if self.sealed {
            self.waits_since_seal += 1;
        }
        self.script.poll(self.clock.now(), &mut self.sources);
        self.drain_sounds();
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.clock.now().0;
        let i = self.commands.iter().position(|&(at, _)| at <= now)?;
        Some(self.commands.remove(i).1)
    }

    fn quitting(&self) -> bool {
        self.quitting || self.clock.now().0 >= self.quit_at
    }

    fn prepare_screen(&mut self) -> Result<(), String> {
        self.prepares += 1;
        if self.quit_in_prepare {
            // The window closes while the screen gets ready.
            self.quitting = true;
        }
        if self.prepares >= self.prepare_fails_from {
            return Err("the surface cannot be made again".to_owned());
        }
        // Built again: no longer failed.
        if self.clock.now().0 >= self.screen_fails_at {
            self.screen_fails_at = i64::MAX;
        }
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Opening<SourceReader> {
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
        Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
        }
    }

    fn start(&mut self, _origin: TimeUs, sounds: AudioQueue, _screen: RenderSide) {
        self.sounds = Some(sounds);
        self.sealed = false;
        self.waits_since_seal = 0;
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
        self.clock.now().0 >= self.screen_fails_at
    }

    fn cancel_audio(&mut self) {
        self.drain_sounds();
    }

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn audio_done(&self) -> bool {
        self.sealed && self.waits_since_seal >= self.tail_waits
    }

    fn close(&mut self) -> Closing {
        // Batches the input thread received before it took the request.
        if self.suspend_in_close {
            self.commands.push((i64::MIN, Command::Suspend));
        }
        let lag = TimeUs(self.clock.now().0 + self.close_lag);
        self.script.poll(lag, &mut self.sources);
        let closing = Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
        };
        self.sources.close_recording();
        closing
    }

    fn ended(&mut self, report: &PlayReport) {
        self.reports.push(report.clone());
    }

    fn show_end(&mut self, _report: &PlayReport) {
        self.shows += 1;
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
    game::live::run(&mut loaded, &mut rig).unwrap();

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
        // Released in play 2's countdown (play time -0.9 s).
        key(9_600_000, Z, 0),
        key(11_503_000, Z, 1),
        key(11_553_000, Z, 0),
    ];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 13_500_000);
    game::live::run(&mut loaded, &mut rig).unwrap();

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

/// Input that reaches the queues after the abort, before recording closes,
/// is kept in the record and not evaluated (D13).
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
    game::live::run(&mut loaded, &mut rig).unwrap();

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
    game::live::run(&mut loaded, &mut rig).unwrap();
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
    let ran = game::live::run(&mut loaded, &mut rig);
    assert!(
        matches!(&ran, Err(LiveError::Screen(e)) if e == "the surface cannot be made again"),
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
    game::live::run(&mut loaded, &mut rig).unwrap();
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
    rig.suspend_in_close = true;
    game::live::run(&mut loaded, &mut rig).unwrap();
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
    let ran = game::live::run(&mut loaded, &mut rig);
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
    game::live::run(&mut loaded, &mut rig).unwrap();
    assert_eq!(rig.reports[0].end, PlayEnd::Completed);
    assert_eq!(rig.reports[0].tail_cut, None);
    assert_eq!((rig.prepares, rig.shows), (2, 2));
}

/// The gauges change in the preset file before a retry: each play's report
/// carries the preset it was evaluated under.
#[test]
fn each_report_carries_the_preset_its_play_used() {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut renamed = rules::custom();
    for g in &mut renamed.gauges {
        g.id = format!("{}-renamed", g.id);
    }
    let path = dir.path().join("presets").join("custom.toml");
    std::fs::write(&path, game::preset_to_toml(&renamed)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    let items = vec![key(9_303_000, Z, 1), key(9_353_000, Z, 0)];
    let mut rig = Fake::new(items, &[(9_500_000, Command::Retry)], 12_500_000);
    game::live::run(&mut loaded, &mut rig).unwrap();
    let ids = |r: &PlayReport| {
        r.preset
            .gauges
            .iter()
            .map(|g| g.id.clone())
            .collect::<Vec<_>>()
    };
    let old: Vec<_> = rules::custom()
        .gauges
        .iter()
        .map(|g| g.id.clone())
        .collect();
    let new: Vec<_> = renamed.gauges.iter().map(|g| g.id.clone()).collect();
    assert_ne!(old, new);
    assert_eq!(ids(&rig.reports[0]), old);
    assert_eq!(ids(&rig.reports[1]), new);
}
