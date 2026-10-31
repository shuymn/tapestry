//! Plays at a practice speed, headless, through the BMS loader (design.md
//! "Practice Speed Changes"): the chart's times are real times, so
//! judgment windows stay in real time, and the saved record re-evaluates
//! to the live result.

mod common;

use std::fs;
use std::path::PathBuf;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{Speed, TimeUs};
use common::{MS, PERIOD, RATE, judgments, tapestry_settings};
use eval::{ControlId, JudgeEventKind};
use game::{
    Command, DataRoot, DeviceSettings, Loaded, Parts, PlayEnd, PlayReport, PlayScript, RunnerAudio,
    SessionConfig,
};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};
use replay::{BuildProvenance, DecodeLimits, ReceiveMethod};

fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("speed")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    (dir, root)
}

fn config(speed: Speed) -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 7,
        speed,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

/// Writes `text` as a chart in `root` and returns its path.
fn chart_file(root: &DataRoot, text: &str) -> PathBuf {
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("song.bms");
    fs::write(&path, text).unwrap();
    path
}

/// Keyboard 0 sets lane `n`'s key to `value` at `at` µs.
fn key(at: i64, n: u8, value: i32) -> ScriptItem {
    let keys = DeviceSettings::default().keyboard;
    let control = ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control);
    ScriptItem {
        at: TimeUs(at),
        action: Action::Packet(Packet {
            device: DeviceId(0),
            control,
            value,
            background: false,
        }),
    }
}

/// Each `(lane, µs)` pressed, released 40 ms later.
fn script(presses: &[(u8, i64)]) -> PlayScript {
    let mut items = vec![ScriptItem {
        at: TimeUs(i64::MIN),
        action: Action::Connect(DeviceId(0), DeviceKind::Keyboard),
    }];
    for &(n, t) in presses {
        items.extend([key(t, n, 1), key(t + 40 * MS, n, 0)]);
    }
    let recorded = DeviceSettings::default().keyboard.recorded();
    PlayScript {
        input: ScriptedInput::new(items).with_recorded(DeviceId(0), recorded),
        commands: Vec::<(TimeUs, Command)>::new(),
    }
}

/// Loads `text` at `speed` with the standard parts (the BMS loader, saved
/// records), then plays it with `presses(chart)`.
fn play(
    root: &DataRoot,
    text: &str,
    speed: Speed,
    presses: impl FnOnce(&bms_model::Chart) -> Vec<(u8, i64)>,
) -> (bms_model::Chart, PlayReport) {
    let settings = tapestry_settings(root);
    let parts = Parts::standard(root, &settings.player);
    let path = chart_file(root, text);
    let loaded = Loaded::load(root.clone(), settings, &path, config(speed), parts).unwrap();
    let chart = loaded.chart().clone();
    let engine = AudioEngine::new(
        common::bank(&chart),
        &chart.bgm,
        RATE,
        PERIOD,
        MAX_VOICES,
        64,
    )
    .unwrap();
    let device = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let audio = RunnerAudio::new(engine, RecordingOutput::new(device, 0));
    let mut session = loaded.start(audio, SourceSet::new(common::sources()).unwrap());
    let presses = presses(&chart);
    let report = session.play(script(&presses)).unwrap();
    (chart, report)
}

/// 140 BPM: key 1 and key 2 alternate at sevenths of measures 1 and 2, so
/// their times fall between whole µs.
const FRACTIONAL: &str = "#BPM 140\n#WAV01 a.wav\n#00111:01000100010001\n#00112:00010001000100\n\
                          #00211:01000100010001\n#00212:00010001000100\n";

/// Every note of `chart` on lanes 1 and 2, pressed `late` µs after its time.
fn hit_all(chart: &bms_model::Chart, late: i64) -> Vec<(u8, i64)> {
    let mut presses: Vec<(u8, i64)> = (1..=2_u8)
        .flat_map(|n| {
            chart.lanes[usize::from(n)]
                .iter()
                .map(move |note| (n, note.time.0 + late))
        })
        .collect();
    presses.sort_by_key(|&(_, t)| t);
    presses
}

#[test]
fn speed_play_reevaluates_equal() {
    for k in [10, 13, 29] {
        let speed = Speed::from_twentieths(k).unwrap();
        let (_dir, root) = temp_root();
        let (chart, report) = play(&root, FRACTIONAL, speed, |c| hit_all(c, 4 * MS));
        assert_eq!(report.end, PlayEnd::Completed, "{k}/20");
        let record = report.record.as_ref().unwrap();
        assert_eq!(record.speed, speed);
        // Every note judged 4 ms late in real time.
        let judged = judgments(&report.live);
        assert_eq!(judged.len(), 14, "{k}/20");
        assert!(judged.iter().all(|j| j.3 == Some(4 * MS)), "{judged:?}");
        // The result screen re-evaluates the chart just played.
        assert!(report.result.as_ref().unwrap().matches_live, "{k}/20");
        // The saved record, decoded and verified against the chart file,
        // loads the chart at its speed and re-evaluates to the live stream.
        let saved = report.saved.as_ref().unwrap().as_ref().unwrap();
        let decoded =
            replay::decode(&fs::read(&saved.path).unwrap(), &DecodeLimits::STANDARD).unwrap();
        assert_eq!(&decoded, record);
        let path = root.path().join("song.bms");
        let reloaded =
            replay::verify::load_recorded_chart(&decoded, &fs::read(&path).unwrap(), root.path())
                .unwrap();
        assert_eq!(reloaded.lanes, chart.lanes, "{k}/20");
        assert_eq!(reloaded.scroll, chart.scroll, "{k}/20");
        let again = replay::verify::evaluate_record(&decoded, &reloaded, None).unwrap();
        assert!(again.as_played);
        assert_eq!(again.evaluation.events, report.live, "{k}/20");
        assert_eq!(again.evaluation.tally, report.tally, "{k}/20");
    }
}

/// 60 BPM: key 1 every second from 4 s to 12 s.
const SECONDS: &str = "#BPM 60\n#WAV01 a.wav\n#00111:01010101\n#00211:01010101\n#00311:01\n";

/// Presses `DEVIATIONS[i]` µs off note `i` of key 1.
const DEVIATIONS: [i64; 9] = [
    -150 * MS,
    -60 * MS,
    -25 * MS,
    -10 * MS,
    0,
    12 * MS,
    30 * MS,
    90 * MS,
    170 * MS,
];

#[test]
fn windows_stay_in_real_time_at_half_speed() {
    let run = |speed| {
        let (_dir, root) = temp_root();
        let (chart, report) = play(&root, SECONDS, speed, |c| {
            c.lanes[1]
                .iter()
                .zip(DEVIATIONS)
                .map(|(note, d)| (1, note.time.0 + d))
                .collect()
        });
        let last = chart.lanes[1].last().unwrap().time;
        let end = report
            .live
            .iter()
            .find(|e| e.kind == JudgeEventKind::SongEnd)
            .unwrap()
            .time;
        (judgments(&report.live), end.0 - last.0, last)
    };
    let (normal, normal_tail, normal_last) = run(Speed::NORMAL);
    let (half, half_tail, half_last) = run(Speed::SLOWEST);
    // The chart plays at half speed: its last note, at 12 s, is at 24 s.
    assert_eq!(
        (normal_last, half_last),
        (TimeUs(12_000 * MS), TimeUs(24_000 * MS))
    );
    // The same presses, as far off each note in real time, are judged
    // alike, with the same deviations: nothing is scaled by the speed.
    assert_eq!(half, normal);
    let deviations: Vec<Option<i64>> = half.iter().map(|j| j.3).collect();
    assert_eq!(deviations, DEVIATIONS.map(Some));
    // The late BAD edge after the last note, 180 ms, is the same real
    // time too.
    assert_eq!((half_tail, normal_tail), (180 * MS, 180 * MS));
}

/// A speed change reads the chart again from its file at the new speed,
/// with the same seed and transform, and keeps the session: the attempt
/// numbering, the preset and the settings go on, and the practice set up
/// at the old speed is cleared. A file whose SHA-256 changed, or one that
/// cannot be read, is refused with nothing changed.
#[test]
fn set_speed_reloads_the_chart_and_keeps_the_session() {
    let (_dir, root) = temp_root();
    let settings = tapestry_settings(&root);
    let parts = Parts::standard(&root, &settings.player);
    let path = chart_file(&root, SECONDS);
    let mut loaded =
        Loaded::load(root.clone(), settings, &path, config(Speed::NORMAL), parts).unwrap();
    let practice = game::mid_chart_start(
        loaded.chart(),
        Speed::NORMAL,
        loaded.preset(),
        TimeUs(4_000 * MS),
    );
    loaded.set_practice(Some(practice)).unwrap();
    assert_eq!(loaded.begin_play().attempt, 1);
    let normal = loaded.chart().clone();
    let (preset, settings) = (loaded.preset().clone(), loaded.settings().clone());
    loaded.set_speed(Speed::SLOWEST).unwrap();
    assert_eq!(loaded.config().speed, Speed::SLOWEST);
    assert_eq!(loaded.path(), path);
    assert_eq!(loaded.practice(), None);
    assert_eq!(loaded.section(), None);
    let times = |c: &bms_model::Chart| c.lanes[1].iter().map(|n| n.time.0).collect::<Vec<_>>();
    let doubled: Vec<i64> = times(&normal).iter().map(|t| t * 2).collect();
    assert_eq!(times(loaded.chart()), doubled);
    let id = |c: &bms_model::Chart| (c.identity.sha256, c.identity.resolved_hash);
    assert_eq!(id(loaded.chart()), id(&normal));
    assert_eq!(loaded.preset(), &preset);
    assert_eq!(loaded.settings(), &settings);
    assert_eq!(loaded.begin_play().attempt, 2);
    // The file changed: refused, nothing changed.
    let slowest = loaded.chart().clone();
    fs::write(&path, format!("{SECONDS}#00411:01\n")).unwrap();
    let e = loaded.set_speed(Speed::NORMAL).unwrap_err();
    assert_eq!(e, game::SessionError::ChartChanged(path.clone()));
    assert_eq!(loaded.config().speed, Speed::SLOWEST);
    assert_eq!(loaded.chart(), &slowest);
    // The file gone: refused, nothing changed.
    fs::remove_file(&path).unwrap();
    let e = loaded.set_speed(Speed::NORMAL).unwrap_err();
    assert!(matches!(e, game::SessionError::Io { .. }), "{e:?}");
    assert_eq!(loaded.config().speed, Speed::SLOWEST);
    assert_eq!(loaded.chart(), &slowest);
    // The same file again: the speed changes, the numbering goes on.
    fs::write(&path, SECONDS).unwrap();
    loaded.set_speed(Speed::NORMAL).unwrap();
    assert_eq!(loaded.chart().lanes, normal.lanes);
    assert_eq!(loaded.begin_play().attempt, 3);
}
