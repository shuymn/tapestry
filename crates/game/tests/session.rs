//! Whole sessions, headless: load → countdown → play → tail → save →
//! result, with retries and aborts.

mod common;

use std::cell::RefCell;

use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{Chart, ChartBuilder, ModelError, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, count_judged, custom_settings, judgments, lane};
use eval::{ControlId, InputKind, SourceId};
use game::{
    AbortCause, ChartIngest, Command, DataRoot, DefaultPolicy, DeviceSettings, FailureAction,
    GaugeIndex, IngestError, Ingested, Loaded, Parts, PlayEnd, PlayReport, PlayScript, PresetError,
    ReEvaluate, ReloadError, RunnerAudio, SaveError, Session, SessionConfig, SessionError,
    SessionEvent, SessionEventSink, SessionPolicy, Settings, preset_to_toml,
};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};
use replay::{BuildProvenance, DecodeLimits, EndStatus, ReceiveMethod};
use rules::Judgment;

type Out = RecordingOutput<SimDevice>;

/// A data root in the build's temporary directory, never the real one.
fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("session")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    (dir, root)
}

/// The scan code the default key config binds to lane `n`.
fn code(n: u8) -> ControlId {
    let keys = DeviceSettings::default().keyboard;
    ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control)
}

/// Taps at `(lane, ms)`.
fn taps(notes: &[(u8, i64)]) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for &(l, t) in notes {
        b = b.tap(lane(l), TimeUs(t * MS));
    }
    b.build().unwrap()
}

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

/// Collects session events.
#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<SessionEvent>>>);

impl SessionEventSink for Events {
    fn on_event(&mut self, ev: &SessionEvent) {
        self.0.borrow_mut().push(*ev);
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 7,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

fn parts(root: &DataRoot, chart: &Chart, events: &Events) -> Parts {
    Parts {
        ingest: Box::new(Built(chart.clone())),
        events: Box::new(events.clone()),
        ..Parts::standard(root, &custom_settings(root).player)
    }
}

fn chart_file(root: &DataRoot) -> PathBuf {
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("play.chart");
    fs::write(&path, b"built in code").unwrap();
    path
}

fn steady_audio(chart: &Chart, bank: Arc<SoundBank>) -> RunnerAudio<Out> {
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, PERIOD, MAX_VOICES, 64).unwrap();
    let device = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    RunnerAudio::new(engine, RecordingOutput::new(device, 0))
}

fn session_with(root: &DataRoot, parts: Parts) -> Session<Out> {
    let settings = custom_settings(root);
    let loaded = Loaded::load(root.clone(), settings, &chart_file(root), config(), parts).unwrap();
    let audio = steady_audio(loaded.chart(), common::bank(loaded.chart()));
    loaded.start(audio, SourceSet::new(common::sources()).unwrap())
}

fn session(root: &DataRoot, chart: &Chart, events: &Events) -> Session<Out> {
    session_with(root, parts(root, chart, events))
}

fn item(at: i64, action: Action) -> ScriptItem {
    ScriptItem {
        at: TimeUs(at),
        action,
    }
}

/// Keyboard `d` sets lane `n`'s key to `value`.
fn key(at: i64, d: u64, n: u8, value: i32) -> ScriptItem {
    item(
        at,
        Action::Packet(Packet {
            device: DeviceId(d),
            control: code(n),
            value,
            background: false,
        }),
    )
}

fn connect(d: u64) -> ScriptItem {
    item(i64::MIN, Action::Connect(DeviceId(d), DeviceKind::Keyboard))
}

/// Keyboard 0 pressing each `(lane, ms)` and releasing 40 ms later.
fn presses(notes: &[(u8, i64)]) -> Vec<ScriptItem> {
    notes
        .iter()
        .flat_map(|&(n, t)| [key(t * MS, 0, n, 1), key(t * MS + 40 * MS, 0, n, 0)])
        .collect()
}

fn script(items: Vec<ScriptItem>, commands: &[(i64, Command)]) -> PlayScript {
    let recorded = DeviceSettings::default().keyboard.recorded();
    let mut input = ScriptedInput::new(items);
    for d in 0..2 {
        input = input.with_recorded(DeviceId(d), recorded.clone());
    }
    PlayScript {
        input,
        commands: commands.iter().map(|&(at, c)| (TimeUs(at), c)).collect(),
    }
}

fn status(report: &PlayReport) -> (EndStatus, TimeUs) {
    let s = report.record.as_ref().unwrap().status;
    (s.end, s.finalized)
}

fn file_name(report: &PlayReport) -> String {
    let saved = report.saved.as_ref().unwrap().as_ref().unwrap();
    saved.path.file_name().unwrap().to_str().unwrap().to_owned()
}

const NOTES: [(u8, i64); 6] = [
    (1, 1_000),
    (2, 1_200),
    (3, 1_400),
    (4, 1_600),
    (5, 1_800),
    (6, 2_000),
];

#[test]
fn retry_resets_judgment_and_starts_a_new_log() {
    let (_dir, root) = temp_root();
    let chart = taps(&NOTES);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut first = vec![connect(0)];
    first.extend(presses(&NOTES));
    // Retry at 1.5 s: the notes at 1.0, 1.2 and 1.4 s are judged.
    let one = s
        .play(script(first, &[(1_500 * MS, Command::Retry)]))
        .unwrap();
    let two = s.play(script(presses(&NOTES), &[])).unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    // The retry is seen at the step at 1.5 s, before it advances: the last
    // step, at 1.499 s, published 1 498 999.
    assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(1_498_999)));
    assert_eq!(count_judged(&one.live, Judgment::Perfect), 3);
    assert_eq!(one.tally.counts.perfect, 3);
    assert_eq!(two.end, PlayEnd::Completed);
    assert_eq!(status(&two).0, EndStatus::PlayedToEnd);
    assert_eq!(count_judged(&two.live, Judgment::Perfect), 6);
    assert_eq!(two.tally.counts.perfect, 6);
    assert_eq!(two.live[0].seq, 0);
    // Two logs, numbered in order, each decoding to its record.
    assert_eq!(
        (file_name(&one), file_name(&two)),
        ("1.tprl".to_owned(), "2.tprl".to_owned())
    );
    for r in [&one, &two] {
        let path = &r.saved.as_ref().unwrap().as_ref().unwrap().path;
        let decoded = replay::decode(&fs::read(path).unwrap(), &DecodeLimits::STANDARD).unwrap();
        assert_eq!(Some(decoded), r.record);
    }
    assert_eq!(
        *events.0.borrow(),
        vec![
            SessionEvent::Load,
            SessionEvent::Start { attempt: 1 },
            SessionEvent::Retry { attempt: 1 },
            SessionEvent::End {
                attempt: 1,
                status: EndStatus::EndedMidway
            },
            SessionEvent::Start { attempt: 2 },
            SessionEvent::End {
                attempt: 2,
                status: EndStatus::PlayedToEnd
            },
        ]
    );
}

#[test]
fn held_control_across_retry_then_released() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(3, 1_000), (3, 2_000)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    // Lane 3's key goes down at 0.5 s and stays down through the retry.
    let first = vec![connect(0), key(500 * MS, 0, 3, 1)];
    let one = s
        .play(script(first, &[(600 * MS, Command::Retry)]))
        .unwrap();
    // In the second play it is released, then pressed for each note.
    let mut second = vec![key(300 * MS, 0, 3, 0)];
    second.extend(presses(&[(3, 1_000), (3, 2_000)]));
    let two = s.play(script(second, &[])).unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    let rec = two.record.as_ref().unwrap();
    assert_eq!(rec.start.held, vec![(SourceId(0), code(3), 1)]);
    assert_eq!(rec.events[0].received, TimeUs(300 * MS));
    assert_eq!(
        rec.events[0].kind,
        InputKind::Control {
            control: code(3),
            value: 0
        }
    );
    // The release judged nothing; each press judged its note.
    assert_eq!(
        judgments(&two.live),
        vec![
            (3, 0, Judgment::Perfect, Some(0)),
            (3, 1, Judgment::Perfect, Some(0))
        ]
    );
    let result = two.result.as_ref().unwrap();
    assert!(result.matches_live);
    assert_eq!(result.evaluation.as_ref().unwrap().events, two.live);
}

#[test]
fn aborted_play_reevaluates_over_prefix() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(2, 500), (1, 1_000)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    // The press on the 1 s note arrives in the same step as the retry: it is
    // recorded, beyond the finalized watermark, and never evaluated.
    items.extend(presses(&[(2, 500), (1, 1_000)]));
    let one = s
        .play(script(items, &[(1_000 * MS, Command::Retry)]))
        .unwrap();

    assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(998_999)));
    let rec = one.record.as_ref().unwrap();
    assert_eq!(
        rec.events.last().map(|e| e.received),
        Some(TimeUs(1_000 * MS))
    );
    assert_eq!(
        judgments(&one.live),
        vec![(2, 0, Judgment::Perfect, Some(0))]
    );
    let result = one.result.as_ref().unwrap();
    let evaluation = result.evaluation.as_ref().unwrap();
    assert_eq!(judgments(&evaluation.events), judgments(&one.live));
    assert!(result.matches_live);
    assert!(!evaluation.completed);
}

#[test]
fn last_device_removal_aborts() {
    let (_dir, root) = temp_root();
    let chart = taps(&NOTES);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    items.extend(presses(&NOTES[..2]));
    items.push(item(1_300 * MS, Action::Disconnect(DeviceId(0))));
    let one = s.play(script(items, &[])).unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::LastSourceRemoved));
    // The device's final watermark would have finalized 1.3 s; the play
    // ends at the step before, which published 1 298 999.
    assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(1_298_999)));
    assert_eq!(count_judged(&one.live, Judgment::Perfect), 2);
    let rec = one.record.as_ref().unwrap();
    assert_eq!(rec.events.last().unwrap().kind, InputKind::Disconnected);
}

#[test]
fn suspend_abort_is_ended_midway_and_incomplete() {
    let (_dir, root) = temp_root();
    let chart = taps(&NOTES);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    items.extend(presses(&NOTES));
    let one = s
        .play(script(items, &[(1_300 * MS, Command::Suspend)]))
        .unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Suspend));
    let rec = one.record.as_ref().unwrap();
    assert_eq!(rec.status.end, EndStatus::EndedMidway);
    assert!(rec.status.incomplete.suspend);
    assert!(!rec.status.incomplete.overflow);
    assert!(!rec.status.incomplete.source_limit);
    assert_eq!(count_judged(&one.live, Judgment::Perfect), 2);
}

/// The tapestry preset with PERFECT widened to 30 ms each side.
fn wide_perfect() -> rules::RulePreset {
    let mut p = rules::tapestry();
    p.id = "wide".to_owned();
    for t in p.tables.each_mut() {
        t.windows.perfect = rules::Window::symmetric(30 * MS);
    }
    p
}

fn rewrite(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    // A modification time clearly after the one read at load.
    let later = SystemTime::now() + Duration::from_secs(60);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(later)
        .unwrap();
}

#[test]
fn changed_preset_file_applies_from_next_retry_only() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    // Changed after loading, before the first play.
    rewrite(
        &root.preset("tapestry.toml"),
        &preset_to_toml(&wide_perfect()),
    );
    // A press 25 ms late: GREAT under the tapestry preset, PERFECT under the
    // changed one.
    let late = |items: Vec<ScriptItem>| {
        let mut v = items;
        v.push(key(1_025 * MS, 0, 1, 1));
        v.push(key(1_060 * MS, 0, 1, 0));
        v
    };
    let one = s.play(script(late(vec![connect(0)]), &[])).unwrap();
    let two = s.play(script(late(Vec::new()), &[])).unwrap();

    assert_eq!(
        judgments(&one.live),
        vec![(1, 0, Judgment::Great, Some(25_000))]
    );
    assert_eq!(
        judgments(&two.live),
        vec![(1, 0, Judgment::Perfect, Some(25_000))]
    );
    assert_eq!(
        one.record.as_ref().unwrap().preset.sha256,
        replay::preset_hash(&rules::tapestry())
    );
    assert_eq!(
        two.record.as_ref().unwrap().preset.sha256,
        replay::preset_hash(&wide_perfect())
    );
    assert_eq!(two.reload_error, None);
}

#[test]
fn malformed_preset_at_retry_keeps_the_old_one() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let one = s
        .play(script(vec![connect(0)], &[(-500 * MS, Command::Retry)]))
        .unwrap();
    rewrite(&root.preset("tapestry.toml"), "windows = [");
    let two = s
        .play(script(Vec::new(), &[(-500 * MS, Command::Retry)]))
        .unwrap();
    fs::remove_file(root.preset("tapestry.toml")).unwrap();
    let three = s.play(script(Vec::new(), &[])).unwrap();

    assert_eq!(one.reload_error, None);
    assert!(matches!(
        two.reload_error,
        Some(ReloadError::Preset(PresetError::Parse(_)))
    ));
    assert_eq!(
        three.reload_error,
        Some(ReloadError::Io(std::io::ErrorKind::NotFound))
    );
    for r in [&two, &three] {
        assert_eq!(
            r.record.as_ref().unwrap().preset.sha256,
            replay::preset_hash(&rules::tapestry())
        );
    }
    assert_eq!(three.end, PlayEnd::Completed);
}

#[test]
fn missing_custom_preset_is_written_as_custom() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let _s = session(&root, &chart, &Events::default());
    let text = fs::read_to_string(root.preset("tapestry.toml")).unwrap();
    assert_eq!(game::load_preset_toml(&text).unwrap(), rules::tapestry());
}

/// One play of a tap at 1 s pressed 17 ms late, with the settings in
/// `root`: PERFECT under the tapestry preset (±20 ms), GREAT under
/// beatoraja-style (±15 ms at a chart's default rank).
fn play_17_ms_late(root: &DataRoot) -> PlayReport {
    let chart = taps(&[(1, 1_000)]);
    let settings = Settings::load(root).unwrap();
    let parts = parts(root, &chart, &Events::default());
    let loaded = Loaded::load(root.clone(), settings, &chart_file(root), config(), parts).unwrap();
    let audio = steady_audio(loaded.chart(), common::bank(loaded.chart()));
    let mut s = loaded.start(audio, SourceSet::new(common::sources()).unwrap());
    let items = vec![
        connect(0),
        key(1_017 * MS, 0, 1, 1),
        key(1_060 * MS, 0, 1, 0),
    ];
    s.play(script(items, &[])).unwrap()
}

/// The judgment of the one tap, and the hash of the preset recorded.
fn played_under(report: &PlayReport) -> (Judgment, [u8; 32]) {
    let judged = judgments(&report.live);
    assert_eq!(judged.len(), 1, "{judged:?}");
    (judged[0].2, report.record.as_ref().unwrap().preset.sha256)
}

#[test]
fn default_preset_is_beatoraja_style() {
    let (_dir, root) = temp_root();
    assert!(!root.player_settings().exists());
    fs::create_dir_all(root.preset("")).unwrap();
    let report = play_17_ms_late(&root);
    let text = fs::read_to_string(root.preset("beatoraja-style.toml")).unwrap();
    assert_eq!(
        game::load_preset_toml(&text).unwrap(),
        rules::beatoraja_style()
    );
    assert!(!root.preset("tapestry.toml").exists());
    assert_eq!(
        played_under(&report),
        (
            Judgment::Great,
            replay::preset_hash(&rules::beatoraja_style())
        )
    );
}

#[test]
fn explicit_custom_choice_is_kept() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.path()).unwrap();
    fs::write(root.player_settings(), "preset = \"tapestry.toml\"\n").unwrap();
    let report = play_17_ms_late(&root);
    assert_eq!(
        played_under(&report),
        (Judgment::Perfect, replay::preset_hash(&rules::tapestry()))
    );
    Settings::load(&root).unwrap().save(&root).unwrap();
    let saved = fs::read_to_string(root.player_settings()).unwrap();
    assert!(saved.contains("preset = \"tapestry.toml\""), "{saved}");
}

#[test]
fn missing_preset_key_takes_the_new_default() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.path()).unwrap();
    fs::write(root.player_settings(), "visible_time_ms = 600\n").unwrap();
    let report = play_17_ms_late(&root);
    assert_eq!(
        played_under(&report),
        (
            Judgment::Great,
            replay::preset_hash(&rules::beatoraja_style())
        )
    );
}

#[test]
fn missing_non_standard_preset_is_refused() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let mut settings = Settings::load(&root).unwrap();
    settings.player.preset = "wide.toml".to_owned();
    let parts = parts(&root, &chart, &Events::default());
    let e = Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).unwrap_err();
    assert_eq!(e, SessionError::MissingPreset(root.preset("wide.toml")));
    assert!(e.to_string().contains("wide.toml"), "{e}");
    // Nothing written: not the named file, nor the presets folder.
    assert!(!root.preset("").exists());
}

#[test]
fn reevaluation_matches_live_summary() {
    let (_dir, root) = temp_root();
    // 40 notes 150 ms apart over lanes 1-7, pressed 4 ms late each.
    let notes: Vec<(u8, i64)> = (0..40)
        .map(|i| ((i % 7) as u8 + 1, 1_000 + i * 150))
        .collect();
    let chart = taps(&notes);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    for &(n, t) in &notes {
        items.push(key(t * MS + 4 * MS, 0, n, 1));
        items.push(key(t * MS + 44 * MS, 0, n, 0));
    }
    let one = s.play(script(items, &[])).unwrap();

    assert_eq!(one.end, PlayEnd::Completed);
    let result = one.result.as_ref().unwrap();
    let evaluation = result.evaluation.as_ref().unwrap();
    assert_eq!(count_judged(&one.live, Judgment::Perfect), 40);
    assert!(result.matches_live);
    assert_eq!(evaluation.tally, one.tally);
    assert!(evaluation.completed);
    let offset = result.recommended_offset.unwrap();
    assert_eq!(
        (offset.median, offset.spread, offset.samples),
        (TimeUs(4_000), TimeUs(0), 40)
    );
    assert_eq!(one.displayed_gauge, GaugeIndex(0));
}

/// A chart with two taps on lane 1 at 1.0 and 1.1 s and a BGM sound at
/// 1.0 s that lasts 3 s, and a bank where sound 0 is that long sound.
fn tail_chart() -> (Chart, Arc<SoundBank>) {
    let chart = ChartBuilder::new(150_000)
        .sound("long.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_100 * MS))
        .bgm(TimeUs(1_000 * MS), SoundId(0))
        .build()
        .unwrap();
    let frames = 3 * RATE as usize;
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![Sound::mono(vec![900; frames])],
    ));
    (chart, bank)
}

fn tail_session(root: &DataRoot, events: &Events) -> Session<Out> {
    let (chart, bank) = tail_chart();
    let settings = custom_settings(root);
    let parts = parts(root, &chart, events);
    let loaded = Loaded::load(root.clone(), settings, &chart_file(root), config(), parts).unwrap();
    loaded.start(
        steady_audio(&chart, bank),
        SourceSet::new(common::sources()).unwrap(),
    )
}

/// The two notes pressed, and lane 7 (no notes) tapped every 20 ms from
/// the countdown to 5 s: a source that reports continuously.
fn busy_items() -> Vec<ScriptItem> {
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000), (1, 1_100)]));
    for k in 0..300 {
        let t = -999 * MS + k * 20 * MS;
        items.push(key(t, 0, 7, 1));
        items.push(key(t + 5 * MS, 0, 7, 0));
    }
    items
}

#[test]
fn session_waits_for_audio_tail_after_completion() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = tail_session(&root, &events);
    let one = s.play(script(busy_items(), &[])).unwrap();

    assert_eq!((one.end, one.tail_cut), (PlayEnd::Completed, None));
    // Evaluation completes when the watermark passes the last miss
    // deadline, 1.28 s: at the step at 1.281 s (watermark 1 280 999).
    // Recording closes at that step, so the record holds nothing received
    // after it; the press received at 1.281 s is in it, unevaluated.
    assert_eq!(status(&one), (EndStatus::PlayedToEnd, TimeUs(1_280_999)));
    let rec = one.record.as_ref().unwrap();
    assert!(rec.events.iter().all(|e| e.received.0 <= 1_281_000));
    // Lane 7 from -0.999 s to 1.281 s: 115 presses (k = 0..=114, 20 ms
    // apart) and the 114 releases 5 ms after the first 114; plus the four
    // note inputs.
    assert_eq!(rec.events.len(), 4 + 115 + 114);
    // The session waited for the 3 s BGM sound started at chart 1.0 s
    // (session time 2.0 s), which plays until 5.0 s. The engine mixes a
    // block one period after its wake, two periods ahead of playback, so
    // the last block of it is written at the wake at 5.0 s − 3 × 10 ms.
    assert_eq!(s.now(), TimeUs(4_970 * MS));

    // Retry during the next play's tail: it still restarts.
    let two = s
        .play(script(busy_items(), &[(2_500 * MS, Command::Retry)]))
        .unwrap();
    assert_eq!(two.tail_cut, Some(AbortCause::Retry));
    assert_eq!(status(&two).0, EndStatus::PlayedToEnd);
    let three = s
        .play(script(Vec::new(), &[(-900 * MS, Command::Quit)]))
        .unwrap();
    assert_eq!(three.attempt, 3);
    assert!(
        events
            .0
            .borrow()
            .contains(&SessionEvent::Retry { attempt: 2 })
    );
}

#[test]
fn retry_in_the_countdown() {
    let (_dir, root) = temp_root();
    let chart = taps(&NOTES);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    // At the first step nothing was finalized yet: no record.
    let one = s
        .play(script(vec![connect(0)], &[(-999 * MS, Command::Retry)]))
        .unwrap();
    // Later in the countdown it ends midway with nothing judged.
    let two = s
        .play(script(Vec::new(), &[(-500 * MS, Command::Retry)]))
        .unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    assert_eq!((one.record.as_ref(), one.saved.as_ref()), (None, None));
    assert_eq!(status(&two), (EndStatus::EndedMidway, TimeUs(-501_001)));
    assert!(judgments(&two.live).is_empty());
    assert_eq!(file_name(&two), "1.tprl");
}

#[test]
fn retry_in_the_step_of_song_end_ends_midway() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000), (1, 1_100)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000), (1, 1_100)]));
    // The song would end at the step at 1.281 s; the retry comes first.
    let one = s
        .play(script(items, &[(1_281 * MS, Command::Retry)]))
        .unwrap();

    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(1_279_999)));
    assert_eq!(count_judged(&one.live, Judgment::Perfect), 2);
}

#[test]
fn unwritable_records_still_complete_and_report() {
    let (_dir, root) = temp_root();
    // `records` is a file, so no directory can be made under it.
    fs::create_dir_all(root.path()).unwrap();
    fs::write(root.records(), b"not a directory").unwrap();
    let chart = taps(&[(1, 1_000)]);
    let events = Events::default();
    let mut s = session(&root, &chart, &events);
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000)]));
    let one = s.play(script(items, &[])).unwrap();

    assert_eq!(one.end, PlayEnd::Completed);
    assert!(matches!(one.saved, Some(Err(SaveError::Io { .. }))));
    assert!(one.result.as_ref().unwrap().matches_live);
}

/// A policy that ends the play when a survival gauge fails.
struct Strict;

impl SessionPolicy for Strict {
    fn id(&self) -> &str {
        "strict"
    }
    fn displayed_gauge(&self, _gauges: &[eval::GaugeState]) -> GaugeIndex {
        GaugeIndex(1)
    }
    fn on_survival_failure(&self) -> FailureAction {
        FailureAction::EndPlay
    }
}

#[test]
fn policy_decides_what_a_survival_failure_does() {
    // Sixteen unpressed notes 100 ms apart. The survival gauge loses 9 % per
    // miss, halved below 30 %: 100, 91, ... 37, 28 after eight misses, then
    // 23.5, 19, 14.5, 10, 5.5, 1 and 0 at the fifteenth, at 2.58 s.
    let notes: Vec<(u8, i64)> = (0..16).map(|i| (1, 1_000 + i * 100)).collect();
    let chart = taps(&notes);
    let (_dir, root) = temp_root();
    let events = Events::default();
    let strict = Parts {
        policy: Box::new(Strict),
        ..parts(&root, &chart, &events)
    };
    let mut s = session_with(&root, strict);
    let one = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
    assert_eq!(count_judged(&one.live, Judgment::Poor), 15);
    assert_eq!(one.displayed_gauge, GaugeIndex(1));
    assert_eq!(one.record.as_ref().unwrap().session_policy, "strict");

    // The default policy keeps playing to the end.
    let (_dir2, root2) = temp_root();
    let mut s = session(&root2, &chart, &events);
    let two = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(two.end, PlayEnd::Completed);
    assert_eq!(count_judged(&two.live, Judgment::Poor), 16);
}

#[test]
fn a_failure_in_the_completing_advance_leaves_the_play_complete() {
    // Fifteen unpressed notes: the fifteenth miss, at 2.58 s, fails the
    // survival gauge and is the chart's last deadline, so SongEnd comes in
    // the same advance. Completion wins over the policy's end.
    let notes: Vec<(u8, i64)> = (0..15).map(|i| (1, 1_000 + i * 100)).collect();
    let chart = taps(&notes);
    let (_dir, root) = temp_root();
    let strict = Parts {
        policy: Box::new(Strict),
        ..parts(&root, &chart, &Events::default())
    };
    let mut s = session_with(&root, strict);
    let one = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(status(&one), (EndStatus::PlayedToEnd, TimeUs(2_580_999)));
    assert_eq!(count_judged(&one.live, Judgment::Poor), 15);
    let survival = &one
        .result
        .as_ref()
        .unwrap()
        .evaluation
        .as_ref()
        .unwrap()
        .gauges[1];
    assert_eq!(survival.failed_at, Some(TimeUs(2_580_000)));
}

#[test]
fn default_policy_shows_the_chosen_gauge_or_the_first() {
    let gauges = vec![
        eval::GaugeState {
            value: rules::GaugeValue(1),
            reference: rules::GaugeValue(2),
            failed_at: None,
        };
        2
    ];
    assert_eq!(
        DefaultPolicy { displayed: 1 }.displayed_gauge(&gauges),
        GaugeIndex(1)
    );
    assert_eq!(
        DefaultPolicy { displayed: 2 }.displayed_gauge(&gauges),
        GaugeIndex(0)
    );
}

#[test]
fn load_refuses_what_it_cannot_use() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let events = Events::default();
    let load = |path: &Path| {
        let settings = custom_settings(&root);
        Loaded::load(
            root.clone(),
            settings,
            path,
            config(),
            parts(&root, &chart, &events),
        )
    };
    let file = chart_file(&root);
    let other = root.path().join("play.txt");
    assert_eq!(
        load(&other).err(),
        Some(SessionError::UnsupportedChart(other.clone()))
    );
    let missing = root.path().join("missing.chart");
    assert!(matches!(
        load(&missing),
        Err(SessionError::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        })
    ));
    fs::write(root.preset("tapestry.toml"), "id = 3").unwrap();
    assert!(matches!(
        load(&file),
        Err(SessionError::Preset(PresetError::Parse(_)))
    ));
    fs::remove_file(root.preset("tapestry.toml")).unwrap();
    fs::create_dir(root.preset("tapestry.toml")).unwrap();
    assert!(matches!(load(&file), Err(SessionError::Io { .. })));
}

#[test]
fn bad_key_config_refuses_to_play_and_closes_recording() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let mut settings = custom_settings(&root);
    // Lanes 1 and 2 on one key: loading the file refuses it, and a
    // session given it all the same refuses to play.
    settings.device.keyboard.lanes[2].control = settings.device.keyboard.lanes[1].control;
    settings.save(&root).unwrap();
    assert!(matches!(
        Settings::load(&root),
        Err(game::SettingsError::InvalidDevice(
            game::DeviceSettingsError::KeyboardKeys(game::KeyError::Overlap { .. })
        ))
    ));
    let events = Events::default();
    let parts = Parts {
        ingest: Box::new(Built(chart.clone())),
        events: Box::new(events.clone()),
        ..Parts::standard(&root, &settings.player)
    };
    let loaded = Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).unwrap();
    let audio = steady_audio(loaded.chart(), common::bank(loaded.chart()));
    let mut s = loaded.start(audio, SourceSet::new(common::sources()).unwrap());
    let err = s.play(script(vec![connect(0)], &[])).unwrap_err();
    assert!(matches!(
        err,
        game::LogicError::Eval(eval::EvalError::DuplicateBinding { .. })
    ));
}

#[test]
fn passthrough_transform_is_recorded() {
    let (_dir, root) = temp_root();
    let path = common::write_bms(&root.path().join("charts"));
    let bytes = fs::read(&path).unwrap();
    let settings = custom_settings(&root);
    let parts = Parts::standard(&root, &settings.player);
    let loaded = Loaded::load(root.clone(), settings, &path, config(), parts).unwrap();
    // The chart is the loader's, unchanged.
    let direct = bms_parser::load_bms(
        &bytes,
        path.parent().unwrap(),
        &bms_parser::LoadOptions {
            seed: 7,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(*loaded.chart(), direct.chart);
    assert_eq!(common::judgeable(loaded.chart()), 8);
    let chart = loaded.chart().clone();
    let audio = steady_audio(&chart, common::bank(&chart));
    let mut s = loaded.start(audio, SourceSet::new(common::sources()).unwrap());
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let events = input::autoplay(&chart, &profile.sources[0]).unwrap();
    let input = ScriptedInput::from_log(&Default::default(), &events, &[DeviceKind::Keyboard]);
    let one = s
        .play(PlayScript {
            input,
            commands: Vec::new(),
        })
        .unwrap();

    assert_eq!(count_judged(&one.live, Judgment::Perfect), 8);
    let saved = &one.saved.as_ref().unwrap().as_ref().unwrap().path;
    let rec = replay::decode(&fs::read(saved).unwrap(), &DecodeLimits::STANDARD).unwrap();
    assert_eq!(
        rec.chart.transform,
        replay::TransformRecord {
            id: "passthrough".to_owned(),
            version: 1
        }
    );
    assert_eq!(rec.session_policy, "default");
    assert_eq!(rec.chart.random_seed, 7);
    let sha: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    assert_eq!(rec.chart.identity.sha256, sha);
}

/// A steady simulated device whose writes fail from the wake at `fail_from`
/// on.
struct Failing {
    inner: SimDevice,
    last: TimeUs,
    fail_from: TimeUs,
}

impl audio::AudioOutput for Failing {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }
    fn period(&self) -> u32 {
        self.inner.period()
    }
    fn wait(&mut self) -> audio::Wake {
        let w = self.inner.wait();
        self.last = w.at;
        w
    }
    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), audio::OutputError> {
        if self.last >= self.fail_from {
            return Err(audio::OutputError::DeviceLost);
        }
        self.inner.write(f)
    }
}

impl game::Wakes for Failing {
    fn next_wake(&self) -> TimeUs {
        self.inner.next_wake()
    }
}

/// A session over `chart` and `bank` whose output fails from `fail_from`
/// (session time), stepping `step`.
fn failing_session(
    root: &DataRoot,
    chart: &Chart,
    bank: Arc<SoundBank>,
    fail_from: i64,
    step: i64,
) -> Session<Failing> {
    let settings = custom_settings(root);
    let config = SessionConfig {
        step: TimeUs(step),
        ..config()
    };
    let parts = parts(root, chart, &Events::default());
    let loaded = Loaded::load(root.clone(), settings, &chart_file(root), config, parts).unwrap();
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let out = Failing {
        inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        last: TimeUs(i64::MIN),
        fail_from: TimeUs(fail_from),
    };
    loaded.start(
        RunnerAudio::new(engine, out),
        SourceSet::new(common::sources()).unwrap(),
    )
}

#[test]
fn audio_failure_ends_the_play_where_it_is_seen() {
    let chart = taps(&NOTES);
    let bank = common::bank(&chart);
    // Steps of 3 ms and wakes every 10 ms; play 1's chart time 0 is at
    // session time 1 s. The wake at 1.51 s falls between the steps at
    // 1.509 and 1.512 s, so the step at 1.512 s runs it before advancing:
    // the play ends at the watermark of the step at 1.509 s,
    // 1 509 000 − 1 000 000 − 1.
    let (_dir, root) = temp_root();
    let mut s = failing_session(&root, &chart, bank.clone(), 1_510 * MS, 3 * MS);
    let one = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::OutputFailed));
    assert_eq!(one.output_error, Some(audio::OutputError::DeviceLost));
    assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(508_999)));
    assert_eq!(s.now(), TimeUs(1_512 * MS));

    // The wake at 1.5 s falls on a step: it runs after that step advanced,
    // and the play ends in that same step.
    let (_dir, root) = temp_root();
    let mut s = failing_session(&root, &chart, bank, 1_500 * MS, 3 * MS);
    let two = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(two.end, PlayEnd::Aborted(AbortCause::OutputFailed));
    assert_eq!(status(&two), (EndStatus::EndedMidway, TimeUs(499_999)));
    assert_eq!(s.now(), TimeUs(1_500 * MS));
}

#[test]
fn a_play_aborted_while_a_long_note_is_held_reevaluates_to_live() {
    // The output fails at session 1.5 s, stepping 1 ms: the play ends at
    // watermark 499 999. A long note on key 1 from 300 ms is pressed on
    // time and released at 700 ms, after the abort. Ending at 500 000, it
    // is still held at the watermark (its end − 1): not judged or counted.
    // Ending at 499 999, its end deadline judges it PERFECT at the
    // watermark. A tap at 2 s keeps the play going. The re-evaluation of
    // the record matches live either way.
    for (end, judged) in [(500_000, 0), (499_999, 1)] {
        let chart = ChartBuilder::new(150_000)
            .long(lane(1), TimeUs(300 * MS), TimeUs(end))
            .tap(lane(2), TimeUs(2_000 * MS))
            .build()
            .unwrap();
        let bank = common::bank(&chart);
        let (_dir, root) = temp_root();
        let mut s = failing_session(&root, &chart, bank, 1_500 * MS, MS);
        let items = vec![connect(0), key(300 * MS, 0, 1, 1), key(700 * MS, 0, 1, 0)];
        let one = s.play(script(items, &[])).unwrap();
        assert_eq!(one.end, PlayEnd::Aborted(AbortCause::OutputFailed));
        assert_eq!(status(&one), (EndStatus::EndedMidway, TimeUs(499_999)));
        let count =
            |f: fn(&eval::JudgeEventKind) -> bool| one.live.iter().filter(|e| f(&e.kind)).count();
        assert_eq!(
            count(|k| matches!(k, eval::JudgeEventKind::LnHeld { .. })),
            1
        );
        assert_eq!(
            count(|k| matches!(k, eval::JudgeEventKind::Judged { .. })),
            judged,
            "end {end}"
        );
        let summary = one.result.as_ref().unwrap();
        assert!(summary.matches_live, "end {end}");
        let evaluation = summary.evaluation.as_ref().unwrap();
        assert_eq!(
            evaluation.tally.counts.perfect,
            u32::try_from(judged).unwrap()
        );
        assert_eq!(evaluation.tally, one.tally);
    }
}

#[test]
fn audio_failure_in_the_tail_keeps_the_play_complete() {
    let (chart, bank) = tail_chart();
    let (_dir, root) = temp_root();
    // The BGM plays until 5.0 s; the device fails at 3.0 s.
    let mut s = failing_session(&root, &chart, bank, 3_000 * MS, MS);
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000), (1, 1_100)]));
    let one = s.play(script(items, &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(status(&one).0, EndStatus::PlayedToEnd);
    assert_eq!(one.output_error, Some(audio::OutputError::DeviceLost));
    assert_eq!(s.now(), TimeUs(3_000 * MS));
}

#[test]
fn reevaluation_refuses_a_record_of_another_preset() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let mut s = session(&root, &chart, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000)]));
    let one = s.play(script(items, &[])).unwrap();
    let other = wide_perfect();
    let summary = game::ResultExtension::on_play_end(
        &mut ReEvaluate,
        &game::ResultContext {
            record: one.record.as_ref().unwrap(),
            chart: &chart,
            preset: &other,
            live: &one.live,
        },
    );
    assert_eq!(
        summary.evaluation,
        Err(game::ReEvaluationError::PresetMismatch)
    );
    assert!(!summary.matches_live);
    assert_eq!(summary.recommended_offset, None);
}

#[test]
fn reevaluation_refuses_a_record_of_another_evaluator_version() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let mut s = session(&root, &chart, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_000)]));
    let one = s.play(script(items, &[])).unwrap();
    let preset = rules::tapestry();
    let record = one.record.as_ref().unwrap();
    // The session records the version the preset runs on, and its preset.
    assert_eq!(record.versions.evaluator, 3);
    assert_eq!(record.preset.sha256, replay::preset_hash(&preset));
    let reevaluate = |record: &replay::PlayRecord| {
        game::ResultExtension::on_play_end(
            &mut ReEvaluate,
            &game::ResultContext {
                record,
                chart: &chart,
                preset: &preset,
                live: &one.live,
            },
        )
    };
    assert!(reevaluate(record).matches_live);
    // The same record under another evaluator version, its preset hash
    // still matching: refused before the hash is compared.
    for version in [2, 4] {
        let mut other = record.clone();
        other.versions.evaluator = version;
        let summary = reevaluate(&other);
        assert_eq!(
            summary.evaluation,
            Err(game::ReEvaluationError::UnsupportedEvaluator(version))
        );
        assert!(!summary.matches_live);
        // With its preset hash not matching either, the version is still
        // what is reported: it is checked first.
        other.preset.sha256[0] ^= 1;
        assert_eq!(
            reevaluate(&other).evaluation,
            Err(game::ReEvaluationError::UnsupportedEvaluator(version))
        );
    }
    // A supported version with a hash that does not match is the preset
    // mismatch.
    let mut other = record.clone();
    other.preset.sha256[0] ^= 1;
    assert_eq!(
        reevaluate(&other).evaluation,
        Err(game::ReEvaluationError::PresetMismatch)
    );
}

/// Accepts any `.chart` file up to `max` bytes and records how many bytes
/// it was handed.
struct Capped {
    chart: Chart,
    max: usize,
    seen: Rc<RefCell<Option<usize>>>,
}

impl ChartIngest for Capped {
    fn max_bytes(&self) -> usize {
        self.max
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(
        &self,
        bytes: &[u8],
        _dir: &Path,
        _seed: u64,
        _speed: bms_model::Speed,
    ) -> Result<Ingested, IngestError> {
        *self.seen.borrow_mut() = Some(bytes.len());
        Ok(Ingested {
            chart: self.chart.clone(),
            warnings: Vec::new(),
        })
    }
}

#[test]
fn chart_is_read_one_byte_past_the_adapter_limit_at_most() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let path = chart_file(&root);
    fs::write(&path, vec![b'x'; 1_000]).unwrap();
    for (max, expected) in [(10, 11), (999, 1_000), (1_000, 1_000)] {
        let seen = Rc::new(RefCell::new(None));
        let parts = Parts {
            ingest: Box::new(Capped {
                chart: chart.clone(),
                max,
                seen: Rc::clone(&seen),
            }),
            ..parts(&root, &chart, &Events::default())
        };
        let settings = custom_settings(&root);
        Loaded::load(root.clone(), settings, &path, config(), parts).unwrap();
        assert_eq!(*seen.borrow(), Some(expected), "limit {max}");
    }
    // The BMS adapter refuses the cut file as too large.
    let big = root.path().join("big.bms");
    let limit = game::BmsIngest.max_bytes();
    fs::write(&big, vec![b'*'; limit + 5]).unwrap();
    let settings = custom_settings(&root);
    let parts = Parts::standard(&root, &settings.player);
    assert_eq!(
        Loaded::load(root.clone(), settings, &big, config(), parts).err(),
        Some(SessionError::Ingest(IngestError::Load(
            bms_parser::LoadError::FileTooLarge { bytes: limit + 1 }
        )))
    );
}

#[test]
fn a_step_that_does_not_move_the_clock_is_refused() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    for step in [0, -1] {
        let config = SessionConfig {
            step: TimeUs(step),
            ..config()
        };
        let settings = custom_settings(&root);
        let parts = parts(&root, &chart, &Events::default());
        assert_eq!(
            Loaded::load(root.clone(), settings, &chart_file(&root), config, parts).err(),
            Some(SessionError::InvalidStep(TimeUs(step)))
        );
    }
}

#[test]
fn audio_failure_at_the_completing_step_keeps_the_play_complete() {
    // The last miss deadline is 0.999 + 0.18 s: evaluation completes at the
    // step at 1.18 s of play time, session time 2.18 s, where a wake falls.
    let chart = taps(&[(1, 999)]);
    let (_dir, root) = temp_root();
    let mut s = failing_session(&root, &chart, common::bank(&chart), 2_180 * MS, MS);
    let one = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(status(&one), (EndStatus::PlayedToEnd, TimeUs(1_179_999)));
    assert_eq!(one.tail_cut, Some(AbortCause::OutputFailed));
    assert_eq!(one.output_error, Some(audio::OutputError::DeviceLost));
    assert!(
        one.result
            .as_ref()
            .unwrap()
            .evaluation
            .as_ref()
            .unwrap()
            .completed
    );
    assert_eq!(s.now(), TimeUs(2_180 * MS));
}

#[test]
fn focus_loss_and_device_removal_cut_the_tail() {
    let gone = Action::Disconnect(DeviceId(0));
    let back = Action::Connect(DeviceId(0), DeviceKind::Keyboard);
    for (actions, cause) in [
        (vec![Action::FocusLoss], AbortCause::FocusLoss),
        (vec![gone], AbortCause::LastSourceRemoved),
        // Removed and back in one poll: still a removal.
        (vec![gone, back], AbortCause::LastSourceRemoved),
    ] {
        let (_dir, root) = temp_root();
        let mut s = tail_session(&root, &Events::default());
        let mut items = busy_items();
        // At 2.0 s of play time, inside the BGM tail (it plays to 4.0 s).
        items.extend(actions.into_iter().map(|a| item(2_000 * MS, a)));
        let one = s.play(script(items, &[])).unwrap();
        assert_eq!(one.end, PlayEnd::Completed);
        assert_eq!(one.tail_cut, Some(cause));
        assert_eq!(status(&one), (EndStatus::PlayedToEnd, TimeUs(1_280_999)));
        // Play time 2.0 s is session time 3.0 s, and the audio stops there.
        assert_eq!(s.now(), TimeUs(3_000 * MS));
        assert_eq!(s.audio().engine().mixer().sounding(), 0);
    }
}

#[test]
fn a_reconnect_during_play_does_not_hide_a_removal_in_the_tail() {
    let (_dir, root) = temp_root();
    let mut s = tail_session(&root, &Events::default());
    let mut items = busy_items();
    items.push(connect(1));
    // Keyboard 0 is removed and connected again during play: from then on
    // it is a new connection, not the one that took part.
    items.push(item(500 * MS, Action::Disconnect(DeviceId(0))));
    items.push(item(
        500 * MS,
        Action::Connect(DeviceId(0), DeviceKind::Keyboard),
    ));
    // Keyboard 1, the last participant still connected, goes in the tail.
    items.push(item(2_000 * MS, Action::Disconnect(DeviceId(1))));
    let one = s.play(script(items, &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Completed);
    assert_eq!(one.tail_cut, Some(AbortCause::LastSourceRemoved));
    // Play time 2.0 s is session time 3.0 s, and the audio stops there.
    assert_eq!(s.now(), TimeUs(3_000 * MS));
}

/// Moves every note 500 ms later, its display position with it.
struct Later;

impl game::ChartTransform for Later {
    fn id(&self) -> game::TransformId {
        game::TransformId {
            id: "later",
            version: 3,
        }
    }

    fn apply(&self, mut chart: Chart, _seed: u64) -> Chart {
        for note in chart.lanes.iter_mut().flatten() {
            note.time = TimeUs(note.time.0 + 500 * MS);
            note.pos = chart.scroll.position_at(note.time);
        }
        chart
    }
}

/// Moves the first note of lane 1 past the second's display position but
/// keeps its time: the lane's positions run backwards.
struct Backwards;

impl game::ChartTransform for Backwards {
    fn id(&self) -> game::TransformId {
        game::TransformId {
            id: "backwards",
            version: 1,
        }
    }

    fn apply(&self, mut chart: Chart, _seed: u64) -> Chart {
        let lane = &mut chart.lanes[1];
        lane[0].pos = lane[1].pos;
        lane[0].pos.0 += 1;
        chart
    }
}

#[test]
fn a_transformed_chart_that_breaks_an_invariant_is_refused() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000), (1, 2_000)]);
    let load = |transform: Box<dyn game::ChartTransform>| {
        let parts = Parts {
            transform,
            ..parts(&root, &chart, &Events::default())
        };
        let settings = custom_settings(&root);
        Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).err()
    };
    assert_eq!(
        load(Box::new(Backwards)),
        Some(SessionError::Transform(ModelError::PositionsUnsorted {
            lane: lane(1),
            index: 1
        }))
    );
    // The same chart moved whole, times and positions together, loads.
    assert_eq!(load(Box::new(Later)), None);
}

#[test]
fn the_transformed_chart_is_played_and_recorded() {
    let (_dir, root) = temp_root();
    let chart = taps(&[(1, 1_000)]);
    let parts = Parts {
        transform: Box::new(Later),
        ..parts(&root, &chart, &Events::default())
    };
    let mut s = session_with(&root, parts);
    let mut items = vec![connect(0)];
    items.extend(presses(&[(1, 1_500)]));
    let one = s.play(script(items, &[])).unwrap();

    assert_eq!(
        judgments(&one.live),
        vec![(1, 0, Judgment::Perfect, Some(0))]
    );
    let rec = one.record.as_ref().unwrap();
    assert_eq!(
        rec.chart.transform,
        replay::TransformRecord {
            id: "later".to_owned(),
            version: 3
        }
    );
    assert!(one.result.as_ref().unwrap().matches_live);
}

#[test]
fn key_sounds_and_bgm_follow_each_play_across_a_retry() {
    let (_dir, root) = temp_root();
    // A sounded tap at 1.0 s, an unsounded one at 2.0 s that keeps play 1
    // going past its retry, and a BGM event at 1.2 s, with a 3 ms input
    // offset and a 5 ms audio offset.
    let mut chart = ChartBuilder::new(150_000)
        .sound("k.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(2_000 * MS))
        .bgm(TimeUs(1_200 * MS), SoundId(0))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let mut settings = custom_settings(&root);
    settings.device.input_offset_us = 3 * MS;
    settings.device.audio_offset_us = 5 * MS;
    settings.save(&root).unwrap();
    let mut s = session(&root, &chart, &Events::default());
    let press = |items: Vec<ScriptItem>| {
        let mut v = items;
        v.push(key(1_003 * MS, 0, 1, 1));
        v.push(key(1_043 * MS, 0, 1, 0));
        v
    };
    // Play 1's time 0 is session time 1.0 s. The retry at 1.5 s of play
    // time (session 2.5 s) starts play 2, whose time 0 is session 3.5 s.
    let one = s
        .play(script(
            press(vec![connect(0)]),
            &[(1_500 * MS, Command::Retry)],
        ))
        .unwrap();
    let mut first = Vec::new();
    s.drain_scheduled(|x| first.push(*x));
    let two = s.play(script(press(Vec::new()), &[])).unwrap();
    let mut second = Vec::new();
    s.drain_scheduled(|x| second.push(*x));

    for (report, scheduled, origin) in [(&one, &first, 1_000 * MS), (&two, &second, 3_500 * MS)] {
        assert_eq!(
            judgments(&report.live)[0],
            (1, 0, Judgment::Perfect, Some(0))
        );
        let keys: Vec<_> = scheduled
            .iter()
            .filter_map(|x| match x.kind {
                audio::ScheduledKind::Key { origin } => Some((origin, x.at.unwrap())),
                audio::ScheduledKind::Bgm { .. } => None,
            })
            .collect();
        // The press, received 1.003 s into the play, on the session clock;
        // its sound starts two to three 10 ms periods later.
        assert_eq!(keys.len(), 1);
        let (key_origin, key_at) = keys[0];
        assert_eq!(key_origin, TimeUs(origin + 1_003 * MS));
        let delay = key_at.0 - key_origin.0;
        assert!(
            (20 * MS..=30 * MS).contains(&delay),
            "key sound {delay} us late"
        );
        // The BGM at 1.2 s, 5 ms early for the output latency, within a
        // frame (1/48 000 s).
        let bgm: Vec<_> = scheduled
            .iter()
            .filter(|x| matches!(x.kind, audio::ScheduledKind::Bgm { .. }))
            .map(|x| x.at.unwrap().0)
            .collect();
        assert_eq!(bgm.len(), 1);
        let expected = origin + 1_195 * MS;
        assert!(
            (bgm[0] - expected).abs() <= 21,
            "BGM at {} not {expected}",
            bgm[0]
        );
    }
}

#[test]
fn plan_sizes_axes_at_1khz_and_buttons_per_source() {
    let (_dir, root) = temp_root();
    // The final deadline is 1180 ms; with the 1 s lead-in the window is
    // 2.18 s, 3 whole seconds.
    let chart = taps(&[(1, 1_000)]);
    let mut settings = custom_settings(&root);
    settings.device.controller.lanes = vec![
        game::LaneKey {
            control: 7,
            lane: 1,
        },
        game::LaneKey {
            control: 0x1_0030,
            lane: 0,
        },
    ];
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        ..Parts::standard(&root, &settings.player)
    };
    let loaded = Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).unwrap();
    // The keyboard records its 8 lanes, capped at 240 edges a second
    // together, and retry and quit, a press and a release each in all. The
    // controller records a button (60) and the axis it declares (1000
    // reports a second). Over 3 s.
    let kinds = [DeviceKind::Keyboard, DeviceKind::Controller];
    let plan = loaded.plan(&kinds, &[vec![], vec![x_axis()]]).unwrap();
    assert_eq!(plan.device.sources[1].axes.len(), 1);
    assert_eq!(plan.logic.inputs, 3 * (240 + 60 + 1000) + 2 * 2);
    // An axis the controller does not declare is never read: no room.
    let plan = loaded.plan(&kinds, &[]).unwrap();
    assert_eq!(plan.logic.inputs, 3 * (240 + 60) + 2 * 2);
}

/// The X axis, declared absolute 0..=255 in 8 bits.
fn x_axis() -> input::hid::AxisCap {
    input::hid::axis_cap(ControlId(0x1_0030), true, 0, 255, 8).unwrap()
}

#[test]
fn keyboards_bound_only_to_retry_and_quit_add_no_rate() {
    let (_dir, root) = temp_root();
    // The controller plays: 7 buttons (min(7 × 60, 240) = 240 edges a
    // second) and the axis (1000 reports a second). Keyboards bind only
    // retry and quit: a press and a release each per keyboard, whatever
    // the window's length.
    let window = |chart: Chart, keyboards: usize| {
        let mut settings = custom_settings(&root);
        settings.device.keyboard.lanes = Vec::new();
        settings.device.controller.lanes = (1..=7)
            .map(|lane| game::LaneKey {
                control: u32::from(lane),
                lane,
            })
            .chain([game::LaneKey {
                control: 0x1_0030,
                lane: 0,
            }])
            .collect();
        let parts = Parts {
            ingest: Box::new(Built(chart)),
            ..Parts::standard(&root, &settings.player)
        };
        let loaded =
            Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).unwrap();
        let mut kinds = vec![DeviceKind::Controller];
        kinds.extend(vec![DeviceKind::Keyboard; keyboards]);
        let mut axes = vec![vec![x_axis()]];
        axes.extend(vec![vec![]; keyboards]);
        loaded.plan(&kinds, &axes).unwrap().logic.inputs
    };
    // Final deadline 1.18 s, 3 whole seconds with the lead-in; 100.18 s,
    // 102 whole seconds.
    assert_eq!(window(taps(&[(1, 1_000)]), 1), 3 * 1240 + 4);
    assert_eq!(window(taps(&[(1, 1_000)]), 14), 3 * 1240 + 14 * 4);
    assert_eq!(window(taps(&[(1, 100_000)]), 1), 102 * 1240 + 4);
    assert_eq!(window(taps(&[(1, 100_000)]), 14), 102 * 1240 + 14 * 4);
}

/// A loaded session of `chart` whose controller binds `axes` axes.
fn loaded_with_axes(root: &DataRoot, chart: Chart, axes: u32) -> Loaded {
    let mut settings = custom_settings(root);
    settings.device.controller.lanes = (0..axes)
        .map(|i| game::LaneKey {
            control: 0x1_0030 + i,
            lane: u8::try_from(i).unwrap(),
        })
        .collect();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        ..Parts::standard(root, &settings.player)
    };
    Loaded::load(root.clone(), settings, &chart_file(root), config(), parts).unwrap()
}

#[test]
fn a_play_too_long_to_record_is_refused_before_it_starts() {
    let (_dir, root) = temp_root();
    // A 15-minute chart: final deadline 900.18 s, with the lead-in 902
    // whole seconds. The keyboard's 10 keys get 240 edges a second, the
    // controller's axis 1000 reports.
    let loaded = loaded_with_axes(&root, taps(&[(1, 900_000)]), 1);
    let both = [DeviceKind::Keyboard, DeviceKind::Controller];
    let axes = [vec![], vec![x_axis()]];
    let err = loaded.plan(&both, &axes).unwrap_err();
    let device = DeviceSettings {
        controller: loaded.settings().device.controller.clone(),
        ..DeviceSettings::default()
    }
    .profile(&both, &axes);
    let allowed = game::Reservation::of(loaded.chart(), loaded.preset(), &device, config().effects)
        .unwrap()
        .capacity();
    assert_eq!(
        err,
        game::LogicError::RecordingTooLarge {
            needed: 902 * (240 + 1000) + 2 * 2,
            allowed,
            section: false,
        }
    );
    // The keyboard alone fits: its 902 × 240 + 4 inputs are reserved
    // whole.
    let plan = loaded.plan(&[DeviceKind::Keyboard], &[]).unwrap();
    assert_eq!(plan.logic.inputs, 902 * 240 + 4);
}

#[test]
fn recording_policy_fits_the_standard_decoder() {
    // For any number of sources the most a play may record, with each
    // source's disconnect, decodes under the standard limits.
    let chart = taps(&[(1, 1_000)]);
    let decodable = replay::max_record_events(&replay::DecodeLimits::STANDARD);
    for n in 1..=8 {
        let device = DeviceSettings::default().profile(&vec![DeviceKind::Keyboard; n], &[]);
        let r =
            game::Reservation::of(&chart, &rules::tapestry(), &device, config().effects).unwrap();
        assert!(r.capacity() + n <= decodable, "{n} sources");
        assert!(r.bytes(r.capacity()) <= game::RECORDING_MEMORY);
        assert!(r.bytes(r.capacity() + 1) > game::RECORDING_MEMORY);
    }
    // Indicator: how long a play one keyboard, and one keyboard with a
    // controller's axis, can record.
    let one = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let r = game::Reservation::of(&chart, &rules::tapestry(), &one, config().effects).unwrap();
    println!(
        "indicator: {} bytes per input, {} inputs: {} s for a keyboard, {} s with an axis",
        r.per_input,
        r.capacity(),
        r.capacity() / 240,
        r.capacity() / 1240
    );
}

#[test]
fn fixed_costs_alone_can_refuse_a_play() {
    let r = game::Reservation {
        fixed: game::RECORDING_MEMORY - 10,
        per_input: 5,
    };
    assert!(r.fits(0));
    assert!(r.fits(2));
    assert!(!r.fits(3));
    // A chart whose fixed part alone is beyond the budget fits nothing,
    // not even a play that records no input.
    let r = game::Reservation {
        fixed: game::RECORDING_MEMORY + 1,
        per_input: 5,
    };
    assert!(!r.fits(0));
    assert_eq!(r.capacity(), 0);
}

#[test]
fn a_chart_too_large_to_reserve_is_refused_without_inputs() {
    let (_dir, root) = temp_root();
    // 600 thousand notes: their share of the output alone is beyond the
    // budget. No control is recorded, so the window needs no input.
    let mut b = ChartBuilder::new(150_000);
    for i in 0..600_000_i64 {
        b = b.tap(
            lane(u8::try_from(i % 7).unwrap() + 1),
            TimeUs(1_000 * MS + i * MS),
        );
    }
    let mut settings = custom_settings(&root);
    settings.device.keyboard = game::KeyConfig {
        lanes: Vec::new(),
        retry: None,
        quit: None,
    };
    let parts = Parts {
        ingest: Box::new(Built(b.build().unwrap())),
        ..Parts::standard(&root, &settings.player)
    };
    let loaded = Loaded::load(root.clone(), settings, &chart_file(&root), config(), parts).unwrap();
    let err = loaded.plan(&[DeviceKind::Keyboard], &[]).unwrap_err();
    assert_eq!(
        err,
        game::LogicError::RecordingTooLarge {
            needed: 0,
            allowed: 0,
            section: false,
        }
    );
}

#[test]
fn bms_ingest_leaves_missing_sounds_to_the_sound_table() {
    let dir = tempfile::tempdir().unwrap();
    // The loader only resolves the path; the bytes need not be audio.
    fs::write(dir.path().join("kick.wav"), b"RIFF").unwrap();
    let text =
        "#BPM 120\n#WAV01 kick.wav\n#WAV02 snare.wav\n#WAV03 hat.wav\n#FOO 1\n#00111:010203\n";
    let ingested = game::BmsIngest
        .ingest(text.as_bytes(), dir.path(), 0, bms_model::Speed::NORMAL)
        .unwrap();
    // One warning, the unknown header; none per missing file.
    assert_eq!(ingested.warnings.len(), 1);
    assert!(ingested.warnings[0].contains("UnknownHeader"));
    let missing: Vec<&str> = ingested
        .chart
        .sounds
        .iter()
        .filter(|s| s.path.is_none())
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(missing, ["snare.wav", "hat.wav"]);
}

/// The tapestry preset with the BAD window `ms` each side: its final deadline
/// is the last note plus `ms`.
fn bad_edge(ms: i64) -> rules::RulePreset {
    let mut p = rules::tapestry();
    p.id = format!("bad-{ms}");
    for t in p.tables.each_mut() {
        t.windows.bad = rules::Window::symmetric(ms * MS);
    }
    p
}

/// Re-evaluates `report`'s record of `chart` under `preset`.
fn under(
    report: &PlayReport,
    chart: &Chart,
    preset: &rules::RulePreset,
) -> replay::verify::ReEvaluation {
    let tapestry = rules::tapestry();
    let ctx = game::ResultContext {
        record: report.record.as_ref().unwrap(),
        chart,
        preset: &tapestry,
        live: &report.live,
    };
    ReEvaluate::under(&ctx, preset).unwrap()
}

/// When `events` reach the final deadline.
fn song_end(events: &[eval::JudgeEvent]) -> Option<TimeUs> {
    events
        .iter()
        .find(|e| e.kind == eval::JudgeEventKind::SongEnd)
        .map(|e| e.time)
}

fn all_cleared(e: &eval::Evaluation) -> bool {
    e.gauges.iter().all(|g| g.cleared)
}

#[test]
fn as_played_matches_live() {
    use replay::verify::Coverage;
    let chart = taps(&NOTES);
    // Played to the end, and retried at 1.5 s.
    for retry in [None, Some(1_500 * MS)] {
        let (_dir, root) = temp_root();
        let mut s = session(&root, &chart, &Events::default());
        let mut items = vec![connect(0)];
        items.extend(presses(&NOTES));
        let commands: Vec<_> = retry.map(|t| (t, Command::Retry)).into_iter().collect();
        let one = s.play(script(items, &commands)).unwrap();
        let (end, watermark) = status(&one);
        let r = under(&one, &chart, &rules::tapestry());
        assert!(r.as_played);
        assert_eq!(r.evaluation.events, one.live, "retry {retry:?}");
        assert_eq!(r.evaluation.tally, one.tally);
        if retry.is_none() {
            assert_eq!(end, EndStatus::PlayedToEnd);
            assert_eq!(r.coverage, Coverage::Recorded);
            assert!(r.evaluation.completed && all_cleared(&r.evaluation));
        } else {
            assert_eq!(end, EndStatus::EndedMidway);
            assert_eq!(r.coverage, Coverage::EndedMidway(watermark));
            assert!(!r.evaluation.completed);
        }
    }
}

/// A play of taps at 1 s and 60 s, both hit on time, under the tapestry
/// preset, played to the end.
fn sixty_seconds() -> (Chart, PlayReport) {
    let notes = [(1, 1_000), (2, 60_000)];
    let chart = taps(&notes);
    let (_dir, root) = temp_root();
    let mut s = session(&root, &chart, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(presses(&notes));
    let one = s.play(script(items, &[])).unwrap();
    assert_eq!(one.end, PlayEnd::Completed);
    // The tapestry preset's late BAD edge, 180 ms, puts the final deadline
    // at 60.180 s, and the recording ends there, within a step.
    assert_eq!(song_end(&one.live), Some(TimeUs(60_180 * MS)));
    let (_, watermark) = status(&one);
    assert!(
        (TimeUs(60_180 * MS)..TimeUs(60_181 * MS)).contains(&watermark),
        "{watermark:?}"
    );
    (chart, one)
}

#[test]
fn what_if_with_a_later_final_deadline_completes_and_clears() {
    use replay::verify::Coverage;
    let (chart, one) = sixty_seconds();
    let (_, watermark) = status(&one);
    // A 220 ms edge: the final deadline is 60.220 s, after the recording.
    let r = under(&one, &chart, &bad_edge(220));
    assert!(!r.as_played);
    assert!(r.evaluation.completed);
    assert!(all_cleared(&r.evaluation));
    assert_eq!(song_end(&r.evaluation.events), Some(TimeUs(60_220 * MS)));
    assert_eq!(r.coverage, Coverage::NoInputAfter(watermark));
    assert_eq!(r.evaluation.tally.counts.perfect, 2);
}

#[test]
fn what_if_with_an_equal_or_earlier_final_deadline_is_covered_by_the_recording() {
    use replay::verify::Coverage;
    let (chart, one) = sixty_seconds();
    // Another preset with the same 180 ms edge, and one with 150 ms.
    let mut equal = bad_edge(180);
    for t in equal.tables.each_mut() {
        t.windows.great = rules::Window::symmetric(50 * MS);
    }
    for (preset, end) in [(equal, 60_180), (bad_edge(150), 60_150)] {
        let r = under(&one, &chart, &preset);
        assert!(!r.as_played);
        assert!(r.evaluation.completed && all_cleared(&r.evaluation));
        assert_eq!(song_end(&r.evaluation.events), Some(TimeUs(end * MS)));
        assert_eq!(r.coverage, Coverage::Recorded, "{}", preset.id);
    }
}

#[test]
fn what_if_of_an_aborted_play_stops_at_the_watermark() {
    use replay::verify::Coverage;
    // Taps at 1 s and 2 s hit on time; retried at 2.170 s, before the
    // tapestry preset's final deadline at 2.180 s.
    let notes = [(1, 1_000), (2, 2_000)];
    let chart = taps(&notes);
    let (_dir, root) = temp_root();
    let mut s = session(&root, &chart, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(presses(&notes));
    let one = s
        .play(script(items, &[(2_170 * MS, Command::Retry)]))
        .unwrap();
    let (end, watermark) = status(&one);
    assert_eq!(end, EndStatus::EndedMidway);
    assert!(
        (TimeUs(2_150 * MS)..TimeUs(2_170 * MS)).contains(&watermark),
        "{watermark:?}"
    );
    // Under 220 ms the final deadline (2.220 s) is beyond the watermark:
    // not complete. Under 150 ms it (2.150 s) is before it: complete, but
    // an aborted play still clears nothing.
    for (ms, completed) in [(220, false), (150, true)] {
        let r = under(&one, &chart, &bad_edge(ms));
        assert!(!r.as_played);
        assert_eq!(r.coverage, Coverage::EndedMidway(watermark));
        assert_eq!(r.evaluation.completed, completed, "{ms} ms");
        assert!(r.evaluation.gauges.iter().all(|g| !g.cleared), "{ms} ms");
        assert!(r.evaluation.events.iter().all(|e| e.time <= watermark));
        assert_eq!(r.evaluation.tally.counts.perfect, 2);
    }
}

#[test]
fn under_checks_the_evaluator_version_first() {
    let chart = taps(&NOTES);
    let (_dir, root) = temp_root();
    let mut s = session(&root, &chart, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(presses(&NOTES));
    let one = s.play(script(items, &[])).unwrap();
    let mut record = one.record.clone().unwrap();
    record.versions.evaluator = 4;
    let tapestry = rules::tapestry();
    let ctx = game::ResultContext {
        record: &record,
        chart: &chart,
        preset: &tapestry,
        live: &one.live,
    };
    for preset in [tapestry.clone(), bad_edge(220)] {
        assert_eq!(
            ReEvaluate::under(&ctx, &preset).map(|_| ()),
            Err(game::ReEvaluationError::UnsupportedEvaluator(4))
        );
    }
}
