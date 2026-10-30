//! The play lifecycle every driver shares (`game::live`), on a rig that
//! stands in for the game's threads: a virtual clock that is not at 0, an
//! input side that opens recording with the play's origin on that clock
//! (as the input thread does), and audio and screen doubles whose
//! acknowledgements and failures the test controls.

mod common;

use std::path::Path;

use bms_model::{Chart, ChartBuilder, TimeSource, TimeUs, VirtualClock};
use common::{MS, custom_settings, lane};
use eval::{ControlId, InputKind, JudgeEventKind, SourceId};
use game::{
    AbortCause, AudioControl, AudioQueue, AudioWaitError, ChartIngest, Closing, Command, DataRoot,
    DeviceSettings, IngestError, Ingested, InputWaitError, LiveError, Loaded, Opening, Parts,
    PlayEnd, PlayReport, RenderSide, Rig, SessionConfig, Settings,
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

    fn ingest(
        &self,
        _bytes: &[u8],
        _dir: &Path,
        _seed: u64,
        _speed: bms_model::Speed,
    ) -> Result<Ingested, IngestError> {
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
    let mut settings = custom_settings(&root);
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
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.clock.now().0;
        let i = self.commands.iter().position(|&(at, _)| at <= now)?;
        Some(self.commands.remove(i).1)
    }

    fn quitting(&self) -> bool {
        self.quitting || self.clock.now().0 >= self.quit_at
    }

    fn prepare_screen(&mut self) -> Result<(), SurfaceLost> {
        self.prepares += 1;
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
        self.audio.as_mut().map_or(Ok(()), AudioControl::ready)
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
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

    fn show_end(&mut self, _report: &PlayReport) {
        self.shows += 1;
    }

    fn before_play(&mut self, settings: &mut Settings) {
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
        // Released in play 2's countdown (play time -0.9 s).
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
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
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
    let settings = custom_settings(&root);
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
    let settings = custom_settings(&root);
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
/// opens: no play exists, so the run ends with that cause and nothing is
/// saved.
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
