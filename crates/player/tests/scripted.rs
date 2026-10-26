//! The whole game on its real threads, driven by an input log instead of
//! devices (dev-tools): scripted input, a recording audio output, and WARP
//! offscreen. The saved record must replay the log exactly and re-evaluate
//! to the live result.

#![cfg(feature = "dev-tools")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bms_model::TimeUs;
use eval::{EvalInput, InputStart, JudgeEventKind};
use game::DeviceSettings;
use input::DeviceKind;
use replay::{
    BuildProvenance, ChartRecord, DecodeLimits, DeviceRecord, EndStatus, PlayRecord, PlayType,
    PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};
use rules::Judgment;

/// 120 BPM, 2 s a measure. Measure 1: lane 1 on every beat (2.0, 2.5,
/// 3.0, 3.5 s), lane 3 on beats 2 and 4 (2.5, 3.5 s), the scratch on beat
/// 1 (2.0 s): 7 notes. Measure 2: one BGM sound at 4.0 s.
const CHART: &str = "#PLAYER 1\n#TITLE scripted\n#BPM 120\n#WAV01 click.wav\n\
                     #00111:01010101\n#00113:00010001\n#00116:01000000\n#00201:01\n";
const NOTES: usize = 7;

/// A 50 ms mono 16-bit click at 44.1 kHz.
fn write_click(path: &Path) {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { 8_000 } else { -8_000 })
        .collect();
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
    // PCM, mono, 44.1 kHz, 88 200 bytes/s, 2-byte frames of 16 bits.
    b.extend_from_slice(&[1, 0, 1, 0]);
    b.extend_from_slice(&44_100_u32.to_le_bytes());
    b.extend_from_slice(&88_200_u32.to_le_bytes());
    b.extend_from_slice(&[2, 0, 16, 0]);
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, b).unwrap();
}

/// An input log that hits every note on time from one keyboard with the
/// default key config: what the scripted run replays.
fn write_log(chart: &bms_model::Chart, path: &Path) -> Vec<eval::PhysicalInputEvent> {
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard]);
    let events = input::autoplay(chart, &profile.sources[0]).unwrap();
    let rec = PlayRecord {
        versions: Versions {
            evaluator: 1,
            app: "test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(&rules::custom()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: replay::DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: profile.sources[0].clone(),
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
        },
        play_type: PlayType::Full,
        start: InputStart {
            opened_at: TimeUs(-1_000_000),
            held: Vec::new(),
        },
        events: events.clone(),
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized: TimeUs(4_000_000),
            incomplete: replay::Incomplete::default(),
        },
    };
    fs::write(path, replay::encode(&rec)).unwrap();
    events
}

/// A temporary folder with the chart, its sound, and an input log that hits
/// every note; `appdata` is the data root's parent for the run.
struct Fixture {
    dir: tempfile::TempDir,
    chart: bms_model::Chart,
    chart_path: PathBuf,
    log: PathBuf,
    events: Vec<eval::PhysicalInputEvent>,
    appdata: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let charts = dir.path().join("chart");
    fs::create_dir_all(&charts).unwrap();
    let chart_path = charts.join("scripted.bms");
    fs::write(&chart_path, CHART).unwrap();
    write_click(&charts.join("click.wav"));
    let chart = bms_parser::load_bms(CHART.as_bytes(), &charts, &Default::default())
        .unwrap()
        .chart;
    let log = dir.path().join("input.tprl");
    let events = write_log(&chart, &log);
    let appdata = dir.path().join("appdata");
    Fixture {
        dir,
        chart,
        chart_path,
        log,
        events,
        appdata,
    }
}

fn records(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for chart in fs::read_dir(root.join("tapestry").join("records")).unwrap() {
        for f in fs::read_dir(chart.unwrap().path()).unwrap() {
            found.push(f.unwrap().path());
        }
    }
    found
}

#[test]
fn scripted_play_writes_a_record() {
    let Fixture {
        dir,
        chart,
        chart_path,
        log,
        events,
        appdata,
    } = fixture();
    assert_eq!(events.len(), 2 * NOTES);
    let frames = dir.path().join("frames");

    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(&chart_path)
        .arg("--dev-scripted-input")
        .arg(&log)
        .arg("--dev-frames")
        .arg(&frames)
        .env("LOCALAPPDATA", &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout.contains(&format!("perfect {NOTES} great 0 good 0 bad 0 poor 0")),
        "{stdout}"
    );
    assert!(stdout.contains("re-evaluation matches live: true"));
    for f in ["countdown.png", "play.png", "end.png"] {
        assert!(frames.join(f).is_file(), "{f}");
    }

    let saved = records(&appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert_eq!(rec.versions.provenance, BuildProvenance::Development);
    assert_eq!(rec.status.end, EndStatus::PlayedToEnd);
    assert!(!rec.status.incomplete.any());
    // The play recorded exactly the log it replayed.
    assert_eq!(rec.events, events);
    let evaluation = eval::evaluate(
        &chart,
        &EvalInput {
            events: rec.events.clone(),
            device: rec.devices.profile(),
            start: rec.start.clone(),
            finalized: Some(rec.status.finalized),
        },
        &rules::custom(),
    )
    .unwrap();
    let perfect = evaluation
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                JudgeEventKind::Judged {
                    judgment: Judgment::Perfect,
                    ..
                }
            )
        })
        .count();
    assert_eq!(perfect, NOTES);
}

/// A key config binding one control to two lanes cannot be evaluated: the
/// run ends with the reason and a nonzero exit, and nothing is recorded.
#[test]
fn an_invalid_key_config_exits_nonzero() {
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    let mut device = DeviceSettings::default();
    device.keyboard.lanes[2].control = device.keyboard.lanes[1].control;
    game::save_settings(&appdata.join("tapestry").join("device.toml"), &device).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(&chart_path)
        .arg("--dev-scripted-input")
        .arg(&log)
        .env("LOCALAPPDATA", &appdata)
        .output()
        .expect("run tapestry");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("the play cannot start"), "{stderr}");
    assert!(!appdata.join("tapestry").join("records").exists());
}

/// With `--dev-auto-retry` the run retries once by itself: the retried
/// play replays the log from its start on devices in their first state, so
/// both plays hit every note, and both are saved.
#[test]
fn an_auto_retried_scripted_play_replays_the_log_again() {
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();

    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(&chart_path)
        .arg("--dev-scripted-input")
        .arg(&log)
        .arg("--dev-auto-retry")
        .env("LOCALAPPDATA", &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    for play in [1, 2] {
        let line = format!("play {play}: Completed; perfect {NOTES} great 0 good 0 bad 0 poor 0");
        assert!(stdout.contains(&line), "{line} in {stdout}");
    }
    assert!(stdout.contains("retry "), "{stdout}");
    assert_eq!(records(&appdata).len(), 2);
}
