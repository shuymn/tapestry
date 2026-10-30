//! Autoplay sessions (design.md "Autoplay"), headless: the generated input
//! fed by the input side's own feeder on a virtual clock, plays planned
//! with the built-in key config and an input offset of 0, nothing saved,
//! and key sounds scheduled ahead with the BGM.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, ScheduledKind, SimConfig, SimDevice};
use bms_model::{Chart, ChartBuilder, SoundId, TimeSource, TimeUs, VirtualClock};
use common::{MS, PERIOD, RATE, lane};
use eval::{ControlId, EvalInput, InputStart, JudgeEventKind};
use game::{
    Adjust, AudioQueue, ChartIngest, Closing, Command, ControllerDevice, DataRoot, DeviceSettings,
    IngestError, Ingested, KeyConfig, LaneKey, Loaded, Opening, Parts, PlayEnd, PlayReport,
    PlayScript, RenderSide, Rig, RunnerAudio, SessionConfig, Settings, Tuning,
};
use input::{AutoplayInput, DeviceKind, InputAdapter, ScriptedInput, SourceConfig, SourceSet};
use replay::{BuildProvenance, ReceiveMethod};
use rules::Judgment;

/// Reads any `.chart` file as the chart it was built with.
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

/// A data root in the build's temporary directory, never the real one.
fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("autoplay")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    (dir, root)
}

fn chart_file(root: &DataRoot) -> PathBuf {
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("play.chart");
    fs::write(&path, b"built in code").unwrap();
    path
}

fn config() -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 3,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: Some(game::AutoKeySounds::Scheduled),
    }
}

/// Every lane: taps, a chord on lanes 1, 2 and 5 at 1.6 s, long notes on
/// the scratch and lane 3, a mine on lane 4 between its taps, and a
/// soundless tap on lane 6; three sounds, each note sounded by its lane,
/// and BGM at 1.0 s and 2.0 s.
fn chart() -> Chart {
    let mut b = ChartBuilder::new(150_000)
        .sound("a.wav")
        .sound("b.wav")
        .sound("c.wav");
    for l in 1..=7 {
        b = b.tap(lane(l), TimeUs((1_000 + 30 * i64::from(l)) * MS));
    }
    for l in [1, 2, 5] {
        b = b.tap(lane(l), TimeUs(1_600 * MS));
    }
    let mut chart = b
        .long(lane(0), TimeUs(1_000 * MS), TimeUs(1_400 * MS))
        .tap(lane(0), TimeUs(1_600 * MS))
        .long(lane(3), TimeUs(1_700 * MS), TimeUs(1_900 * MS))
        .tap(lane(4), TimeUs(1_800 * MS))
        .mine(lane(4), TimeUs(1_900 * MS), 5)
        .tap(lane(4), TimeUs(2_000 * MS))
        .bgm(TimeUs(1_000 * MS), SoundId(2))
        .bgm(TimeUs(2_000 * MS), SoundId(2))
        .build()
        .unwrap();
    for (l, notes) in chart.lanes.iter_mut().enumerate() {
        for n in notes {
            n.sound = (l != 6).then_some(SoundId(u16::try_from(l % 3).unwrap()));
        }
    }
    chart
}

/// The notes autoplay judges: every tap and long note head.
fn judgeable(chart: &Chart) -> usize {
    chart
        .lanes
        .iter()
        .flatten()
        .filter(|n| !matches!(n.kind, bms_model::NoteKind::Mine { .. }))
        .count()
}

/// A loaded autoplay session of `chart` with `device` in `device.toml`.
fn load(root: &DataRoot, chart: &Chart, device: &DeviceSettings, saved: bool) -> Loaded {
    game::save_settings(&root.device_settings(), device).unwrap();
    let settings = Settings::load(root).unwrap();
    let base = if saved {
        Parts::standard(root, &settings.player)
    } else {
        Parts::unsaved(&settings.player)
    };
    let parts = Parts {
        ingest: Box::new(Built(chart.clone())),
        ..base
    };
    Loaded::load(root.clone(), settings, &chart_file(root), config(), parts).unwrap()
}

/// Changes the settings before the play with the given index.
type BeforePlay = Box<dyn FnMut(&mut Settings, usize)>;

/// The input side of an autoplay session on a virtual clock: the feeder
/// polled at every 1 ms wait on the play's timeline, as the clocked input
/// thread's hook does, on one source set for the whole session. Audio is
/// left out: the key sounds the logic sends are counted, and the tail ends
/// at the seal.
struct AutoRig {
    clock: VirtualClock,
    sources: SourceSet,
    feeder: AutoplayInput,
    recorded: Vec<ControlId>,
    origin: Option<TimeUs>,
    sounds: Option<AudioQueue>,
    screen: Option<RenderSide>,
    key_commands: usize,
    sealed: bool,
    /// The play time at which the next play is retried, once.
    retry_at: Option<TimeUs>,
    /// Called before each play with the play's index.
    before: BeforePlay,
    plays: usize,
}

impl AutoRig {
    fn new(loaded: &Loaded) -> Self {
        let made = loaded.autoplay().unwrap();
        let sources = SourceSet::new(SourceConfig {
            queue_capacity: made.queue_capacity(),
            ..common::sources()
        })
        .unwrap();
        Self {
            clock: VirtualClock::new(TimeUs(5_000_000)),
            sources,
            feeder: AutoplayInput::new(made.events),
            recorded: made.recorded,
            origin: None,
            sounds: None,
            screen: None,
            key_commands: 0,
            sealed: false,
            retry_at: None,
            before: Box::new(|_, _| {}),
            plays: 0,
        }
    }
}

impl Rig for AutoRig {
    type Reader = input::SourceReader;
    type ScreenError = std::convert::Infallible;

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        let now = self.clock.now();
        match self.origin {
            Some(origin) => self
                .feeder
                .poll(TimeUs(now.0 - origin.0), &mut self.sources),
            // Recording closed: the focus check alone.
            None => self.sources.end_batch(now),
        }
        if let Some(sounds) = &mut self.sounds {
            while sounds.commands().pop().is_ok() {
                self.key_commands += 1;
            }
        }
        if let Some(screen) = &mut self.screen {
            screen.drain_effects(|_| {});
        }
    }

    fn take(&mut self) -> Option<Command> {
        let at = self.retry_at?;
        let now = TimeUs(self.clock.now().0 - self.origin?.0);
        (now >= at).then(|| {
            self.retry_at = None;
            Command::Retry
        })
    }

    fn quitting(&self) -> bool {
        false
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<Self::Reader>, game::InputWaitError> {
        let origin = TimeUs(self.clock.now().0 + lead_in.0);
        self.feeder
            .restart(&mut self.sources, self.recorded.clone());
        self.sources.gate_mut().reset();
        let start = self.sources.open_recording(TimeUs(-lead_in.0));
        self.origin = Some(origin);
        self.sealed = false;
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
            axes: self.sources.participants().map(|_| Vec::new()).collect(),
        })
    }

    fn start(&mut self, _origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        self.sounds = Some(sounds);
        self.screen = Some(screen);
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
        false
    }

    fn cancel_audio(&mut self) {}

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {
        panic!("this rig plays no practice round");
    }

    fn audio_done(&self) -> bool {
        self.sealed
    }

    fn close(&mut self) -> Result<Closing, game::InputWaitError> {
        let closing = Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: self.sources.axis_out_of_range().collect(),
        };
        self.sources.close_recording();
        self.origin = None;
        Ok(closing)
    }

    fn ended(&mut self, _report: &PlayReport) {}

    fn show_end(&mut self, _report: &PlayReport) {}

    fn before_play(&mut self, settings: &mut Settings) {
        (self.before)(settings, self.plays);
        self.plays += 1;
    }
}

fn play(loaded: &mut Loaded, rig: &mut AutoRig) -> PlayReport {
    game::live::play(loaded, rig).unwrap().unwrap()
}

/// Each judgment's (judgment, deviation) of a report's live stream.
fn live_judgments(report: &PlayReport) -> Vec<(Judgment, Option<i64>)> {
    report
        .live
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                judgment,
                deviation,
                ..
            } => Some((judgment, deviation.map(|d| d.0))),
            _ => None,
        })
        .collect()
}

/// A completed play that judged every note PERFECT on time, re-evaluated
/// to the same.
fn assert_all_perfect(report: &PlayReport, notes: usize) {
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(
        live_judgments(report),
        vec![(Judgment::Perfect, Some(0)); notes]
    );
    let result = report.result.as_ref().unwrap();
    assert!(result.matches_live);
    let evaluation = result.evaluation.as_ref().unwrap();
    assert_eq!(
        evaluation.tally.counts.perfect,
        u32::try_from(notes).unwrap()
    );
    assert_eq!(
        (evaluation.tally.bp, evaluation.tally.max_combo),
        (0, u32::try_from(notes).unwrap())
    );
}

/// G1: an unsaved session keeps its record in memory, re-evaluates it to
/// the live play (every note PERFECT, both long notes held and judged by
/// their release at their end), and writes no records folder; the same
/// session with the standard parts saves (the positive control).
#[test]
fn unsaved_session_keeps_no_records_and_judges_every_note_perfect() {
    let chart = chart();
    let notes = judgeable(&chart);
    assert_eq!(notes, 15);
    for saved in [false, true] {
        let (_dir, root) = temp_root();
        let mut loaded = load(&root, &chart, &DeviceSettings::default(), saved);
        let mut rig = AutoRig::new(&loaded);
        let report = play(&mut loaded, &mut rig);
        assert_all_perfect(&report, notes);
        let record = report.record.as_ref().unwrap();
        assert_eq!(record.events.len(), 2 * notes);
        let evaluation = report.result.as_ref().unwrap().evaluation.as_ref().unwrap();
        // Each long note is held, then judged by its release exactly at
        // its end.
        let count = |f: fn(&JudgeEventKind) -> bool| {
            evaluation.events.iter().filter(|e| f(&e.kind)).count()
        };
        assert_eq!(
            count(|k| matches!(
                k,
                JudgeEventKind::LnHeld {
                    judgment: Judgment::Perfect,
                    ..
                }
            )),
            2
        );
        assert_eq!(
            count(|k| matches!(
                k,
                JudgeEventKind::Judged {
                    judgment: Judgment::Perfect,
                    release: Some(TimeUs(0)),
                    ..
                }
            )),
            2
        );
        if saved {
            assert!(matches!(report.saved, Some(Ok(_))), "{:?}", report.saved);
            assert!(root.records().is_dir());
        } else {
            assert!(report.saved.is_none(), "{:?}", report.saved);
            assert!(!root.records().exists());
        }
    }
}

/// G2: whatever `device.toml` binds (no keyboard lanes, or lanes on other
/// keys, with a controller config playing the scratch on an axis),
/// autoplay plays all eight lanes with its own keys, and the settings
/// written back keep `device.toml`'s bindings.
#[test]
fn autoplay_keys_play_every_lane_whatever_device_toml_binds() {
    let chart = chart();
    let controller = KeyConfig {
        lanes: vec![
            LaneKey {
                control: 0x1_0030,
                lane: 0,
            },
            LaneKey {
                control: 3,
                lane: 1,
            },
        ],
        retry: None,
        quit: None,
    };
    let empty = KeyConfig {
        lanes: Vec::new(),
        retry: Some(0x0e),
        quit: Some(0x01),
    };
    // Q W E R T Y U I.
    let remapped = KeyConfig {
        lanes: (0..8)
            .map(|lane| LaneKey {
                control: 0x10 + u32::from(lane),
                lane,
            })
            .collect(),
        retry: Some(0x0e),
        quit: Some(0x01),
    };
    for keyboard in [empty, remapped] {
        let (_dir, root) = temp_root();
        let device = DeviceSettings {
            keyboard,
            controller: controller.clone(),
            controller_device: Some(ControllerDevice {
                vendor_id: 0x1234,
                product_id: 0x5678,
                interface: None,
            }),
            ..DeviceSettings::default()
        };
        let mut loaded = load(&root, &chart, &device, false);
        let mut rig = AutoRig::new(&loaded);
        let report = play(&mut loaded, &mut rig);
        assert_all_perfect(&report, judgeable(&chart));
        let lanes: std::collections::BTreeSet<usize> = report
            .live
            .iter()
            .filter_map(|e| match e.kind {
                JudgeEventKind::Judged { lane, .. } => Some(lane.index()),
                _ => None,
            })
            .collect();
        assert_eq!(lanes.len(), 8);
        // What the player writes when a play ends.
        loaded.settings().save(&root).unwrap();
        let back = Settings::load(&root).unwrap().device;
        assert_eq!(back, device);
    }
}

/// G3: with an input offset of 50 ms and an audio offset of -30 ms set,
/// and the input offset raised ten steps (F8) between two plays, every
/// note of both plays is judged with a deviation of exactly 0, and the
/// new input offset is what the settings keep.
#[test]
fn autoplay_ignores_the_input_offset_and_keeps_its_changes() {
    let chart = chart();
    let notes = judgeable(&chart);
    let (_dir, root) = temp_root();
    let device = DeviceSettings {
        input_offset_us: 50 * MS,
        audio_offset_us: -30 * MS,
        ..DeviceSettings::default()
    };
    let mut loaded = load(&root, &chart, &device, false);
    let mut rig = AutoRig::new(&loaded);
    rig.before = Box::new(|settings, play| {
        if play == 1 {
            let mut t = Tuning::of(settings);
            for _ in 0..10 {
                t = t.adjust(Adjust::InputOffsetUp);
            }
            t.apply_to(settings);
        }
    });
    for _ in 0..2 {
        let report = play(&mut loaded, &mut rig);
        assert_all_perfect(&report, notes);
        assert_eq!(
            report.record.as_ref().unwrap().devices.sources[0]
                .profile
                .input_offset,
            TimeUs(0)
        );
    }
    assert_eq!(loaded.settings().device.input_offset_us, 60 * MS);
    loaded.settings().save(&root).unwrap();
    assert_eq!(
        Settings::load(&root).unwrap().device.input_offset_us,
        60 * MS
    );
    // The logic sent no key sound: autoplay's are on the schedule.
    assert_eq!(rig.key_commands, 0);
}

/// A retry mid-chart, while the scratch's long note is held: the next
/// play starts from the first event on a released source and judges
/// every note PERFECT again.
#[test]
fn a_retry_mid_long_note_starts_over_released() {
    let chart = chart();
    let (_dir, root) = temp_root();
    let mut loaded = load(&root, &chart, &DeviceSettings::default(), false);
    let mut rig = AutoRig::new(&loaded);
    rig.retry_at = Some(TimeUs(1_200 * MS));
    let first = play(&mut loaded, &mut rig);
    assert_eq!(
        first.end,
        PlayEnd::Aborted(game::AbortCause::Retry),
        "{:?}",
        first.end
    );
    let second = play(&mut loaded, &mut rig);
    assert!(second.record.as_ref().unwrap().start.held.is_empty());
    assert_all_perfect(&second, judgeable(&chart));
}

/// The key sounds on the schedule are exactly the sounds the evaluator
/// plays for the generated input (ids and multiplicities, at the same
/// times), with taps, long note heads, mines, soundless notes and chords;
/// BGM stays as it was, first at equal times.
#[test]
fn scheduled_key_sounds_are_the_sounds_the_evaluator_plays() {
    let chart = chart();
    let (_dir, root) = temp_root();
    let loaded = load(&root, &chart, &DeviceSettings::default(), false);
    let made = loaded.autoplay().unwrap();
    assert_eq!(made.schedule.capacity(), made.schedule.len());
    let plan = loaded.plan(&[DeviceKind::Keyboard], &[Vec::new()]).unwrap();
    let evaluation = eval::evaluate(
        &chart,
        &EvalInput {
            events: made.events.clone(),
            device: plan.device,
            start: InputStart::default(),
            finalized: None,
            section: None,
        },
        loaded.preset(),
    )
    .unwrap();
    let mut played: HashMap<(i64, u16), usize> = HashMap::new();
    for e in &evaluation.events {
        if let JudgeEventKind::PlaySound { sound, .. } = e.kind {
            *played.entry((e.time.0, sound.0)).or_default() += 1;
        }
    }
    let mut scheduled: HashMap<(i64, u16), usize> = HashMap::new();
    for b in &made.schedule {
        *scheduled.entry((b.time.0, b.sound.0)).or_default() += 1;
    }
    for b in &chart.bgm {
        *scheduled.get_mut(&(b.time.0, b.sound.0)).unwrap() -= 1;
    }
    scheduled.retain(|_, n| *n > 0);
    assert_eq!(scheduled, played);
    // 15 pressed notes, one soundless.
    assert_eq!(made.schedule.len(), chart.bgm.len() + 14);
    assert!(made.schedule.windows(2).all(|w| w[0].time <= w[1].time));
    // At 1.0 s: the BGM, then the scratch's long note head (sound 0).
    let at_one: Vec<u16> = made
        .schedule
        .iter()
        .filter(|b| b.time == TimeUs(1_000 * MS))
        .map(|b| b.sound.0)
        .collect();
    assert_eq!(at_one, [2, 0]);
}

/// The audio side of an autoplay session: the merged schedule in the
/// engine, a session that sends no judgment-driven key sound.
type Out = RecordingOutput<SimDevice>;

fn session(
    root: &DataRoot,
    chart: &Chart,
    device: &DeviceSettings,
) -> (game::Session<Out>, Vec<eval::PhysicalInputEvent>) {
    let loaded = load(root, chart, device, false);
    let made = loaded.autoplay().unwrap();
    let engine = AudioEngine::with_schedule(
        common::bank(chart),
        made.schedule,
        RATE,
        PERIOD,
        MAX_VOICES,
        256,
    )
    .unwrap();
    let out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let sources = SourceSet::new(common::sources()).unwrap();
    (
        loaded.start(RunnerAudio::new(engine, out), sources),
        made.events,
    )
}

fn script(events: &[eval::PhysicalInputEvent], commands: Vec<(TimeUs, Command)>) -> PlayScript {
    PlayScript {
        input: ScriptedInput::from_log(&InputStart::default(), events, &[DeviceKind::Keyboard]),
        commands,
    }
}

/// AU1: a key sound and a BGM sound at the same chart time (2.0 s, sounds
/// 1 and 2) start on the same frame with an audio offset of -30 ms, and no
/// key sound comes from a judgment; again after a retry that cancelled a
/// play mid-chart.
#[test]
fn autoplay_key_sounds_start_with_the_bgm() {
    let chart = chart();
    let (_dir, root) = temp_root();
    let device = DeviceSettings {
        audio_offset_us: -30 * MS,
        ..DeviceSettings::default()
    };
    let (mut session, events) = session(&root, &chart, &device);
    let frames = |session: &mut game::Session<Out>| {
        let mut at_two = Vec::new();
        session.drain_scheduled(|s| match s.kind {
            ScheduledKind::Key { .. } => panic!("a judgment-driven key sound: {s:?}"),
            ScheduledKind::Bgm { time } if time == TimeUs(2_000 * MS) => {
                at_two.push((s.sound.0, s.frame));
            }
            ScheduledKind::Bgm { .. } => {}
        });
        at_two
    };
    let first = session.play(script(&events, Vec::new())).unwrap();
    assert_eq!(first.end, PlayEnd::Completed);
    // The BGM's sound 2, then lane 4's tap with its sound 1.
    let pair = frames(&mut session);
    assert_eq!(pair.iter().map(|p| p.0).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(pair[0].1, pair[1].1, "{pair:?}");
    let cancelled = session
        .play(script(&events, vec![(TimeUs(1_500 * MS), Command::Retry)]))
        .unwrap();
    assert_eq!(cancelled.end, PlayEnd::Aborted(game::AbortCause::Retry));
    assert!(frames(&mut session).is_empty());
    let again = session.play(script(&events, Vec::new())).unwrap();
    assert_eq!(again.end, PlayEnd::Completed);
    let pair = frames(&mut session);
    assert_eq!(pair.len(), 2, "{pair:?}");
    assert_eq!(pair[0].1, pair[1].1, "{pair:?}");
}
