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
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let events = input::autoplay(chart, &profile.sources[0]).unwrap();
    let rec = PlayRecord {
        versions: Versions {
            evaluator: 1,
            app: "test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(&rules::tapestry()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: replay::DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: profile.sources[0].clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: replay::WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
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
    fixture_of(CHART)
}

/// The fixture with chart `text`; its sounds folder also holds
/// `broken.wav`, which is not a sound.
fn fixture_of(text: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let charts = dir.path().join("chart");
    fs::create_dir_all(&charts).unwrap();
    let chart_path = charts.join("scripted.bms");
    fs::write(&chart_path, text).unwrap();
    write_click(&charts.join("click.wav"));
    fs::write(charts.join("broken.wav"), b"not a sound").unwrap();
    let chart = bms_parser::load_bms(text.as_bytes(), &charts, &Default::default())
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
            let path = f.unwrap().path();
            if path.extension().is_some_and(|e| e == "tprl") {
                found.push(path);
            }
        }
    }
    found
}

/// The `.measure.toml` beside `record`, parsed.
fn measured(record: &Path) -> toml::Table {
    let path = record.with_extension("measure.toml");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.parse().unwrap()
}

/// `tapestry play <chart> --dev-scripted-input <log>` with its data root
/// under `appdata`; the caller adds its own arguments.
/// `--dev-render-timeout`'s seconds for every headless run: WARP frames
/// on a loaded machine can take tens of seconds, past the production
/// bounds on the render thread's answers and end, so the bound here only
/// catches a render thread that never answers.
const RENDER_TIMEOUT: [&str; 2] = ["--dev-render-timeout", "600"];

fn scripted_play(chart: &Path, log: &Path, appdata: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tapestry"));
    command
        .arg("play")
        .arg(chart)
        .arg("--dev-scripted-input")
        .arg(log)
        .args(RENDER_TIMEOUT)
        .env("LOCALAPPDATA", appdata);
    command
}

fn int(t: &toml::Table, table: &str, key: &str) -> Option<i64> {
    t.get(table)?.get(key)?.as_integer()
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

    let out = scripted_play(&chart_path, &log, &appdata)
        .arg("--dev-frames")
        .arg(&frames)
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
            section: None,
        },
        &rules::RulePreset::from_canonical_bytes(&rec.preset.canonical_bytes).unwrap(),
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

    // The play's measurement (design.md "Latency Figures") beside the record:
    // one press → sound per key sound, input → judgment for the steps that
    // judged presses, and every judgment's on-screen latency N/A offscreen,
    // where no frame statistics exist; never a 0.
    let m = measured(&saved[0]);
    assert_eq!(
        int(&m, "press_to_sound_scheduled_us", "count"),
        Some(NOTES as i64)
    );
    // The simulated device: 10 ms periods, two queued.
    assert_eq!(
        int(&m, "press_to_sound_scheduled_us", "device_period_us"),
        Some(10_000)
    );
    assert_eq!(
        int(&m, "press_to_sound_scheduled_us", "queued_periods"),
        Some(2)
    );
    // One input to judgment sample per judged press.
    assert_eq!(int(&m, "input_to_judgment_us", "count"), Some(NOTES as i64));
    assert_eq!(int(&m, "press_to_sound_scheduled_us", "dropped"), Some(0));
    // Offscreen: no swap chain, so no presentation to count refreshes under.
    assert_eq!(
        int(&m, "lost_refreshes_frame_statistics", "sync_interval"),
        None
    );
    assert!(int(&m, "finalization_delay_us", "count").unwrap() > 0);
    assert_eq!(int(&m, "press_to_screen_us", "count"), Some(0));
    assert_eq!(int(&m, "press_to_screen_us", "p50"), None);
    assert_eq!(
        int(&m, "press_to_screen_us", "unavailable"),
        Some(NOTES as i64)
    );
    let refreshes = "lost_refreshes_frame_statistics";
    assert_eq!(int(&m, refreshes, "lost"), None);
    assert_eq!(int(&m, refreshes, "intervals"), Some(0));
    assert!(int(&m, "render_cpu_us", "count").unwrap() > 0);
    assert!(int(&m, "audio", "underruns").is_some());
    assert_eq!(int(&m, "audio", "deferred_wakes"), Some(0));
    assert_eq!(int(&m, "audio", "voices_stolen"), Some(0));
    assert_eq!(int(&m, "sprite_cap", "full_frames"), Some(0));
    assert!(stdout.contains(
        "limits: sprite cap full in 0 frames; starts deferred in 0 audio wakes; 0 voices stolen"
    ));
    assert!(int(&m, "load_ms", "first").is_some());
    assert!(int(&m, "allocations", "render").is_some());
    // The frame dump reads back, encodes and writes files, outside the
    // render thread's app region, which allocates nothing.
    assert_eq!(int(&m, "allocations", "render_app"), Some(0));
}

/// A scripted play with the overlay shown and no frame dump: the render
/// thread's app region (design.md "Scope of No Allocation") allocates
/// nothing in any frame of the play. The whole-thread counts are printed,
/// not checked: they include wgpu and the driver.
#[test]
fn render_app_region_does_not_allocate() {
    let fx = fixture();
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .arg("--dev-overlay")
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let saved = records(&fx.appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let m = measured(&saved[0]);
    println!("allocations: {:?}", m.get("allocations"));
    // Play frames were drawn, so the count covers them.
    assert!(int(&m, "render_cpu_us", "count").unwrap() > 0);
    assert_eq!(int(&m, "allocations", "render_app"), Some(0));
}

/// The play's setup, measured as the game thread hands the play to audio
/// and the screen, reaches its `.measure.toml` and the console line printed
/// when the play ends, with the first frame after it and the process's page
/// faults from the handover until the play was over.
#[test]
fn the_play_setup_reaches_the_figures() {
    let fx = fixture();
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let saved = records(&fx.appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let m = measured(&saved[0]);
    let setup = int(&m, "countdown", "setup_us").unwrap();
    let first = int(&m, "countdown", "first_frame_us").unwrap();
    // The setup takes time and ends within the 1 s lead-in; the first
    // frame comes after it.
    assert!(0 < setup && setup < 1_000_000, "{setup}");
    assert!(first > setup, "{first} {setup}");
    assert!(stdout.contains(&format!(
        "countdown: setup {setup} us, first frame {first} us after it started"
    )));
    // Seconds of play that fill buffers and draw frames: some page faults.
    assert!(int(&m, "faults", "process").unwrap() > 0);
}

/// The perfect measurement (`--dev-perfect`, headless here): with a
/// `device.toml` whose keys and input offset differ from the built-in ones,
/// every note is still PERFECT, each key sound is sent from its judgment
/// (press → sound has a sample per note), and the play is saved as
/// generated input with the built-in profile and its `.measure.toml`.
#[test]
fn perfect_measurement_judges_key_sounds_and_saves() {
    let fx = fixture();
    let mut device = DeviceSettings {
        input_offset_us: 30_000,
        ..DeviceSettings::default()
    };
    // Other physical keys (Q, W): the generated keys must still be the
    // ones recorded.
    let lanes = &mut device.keyboard.lanes;
    (lanes[1].control, lanes[2].control) = (0x10, 0x11);
    game::save_settings(&fx.appdata.join("tapestry").join("device.toml"), &device).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(&fx.chart_path)
        .args(["--dev-perfect", "--dev-headless"])
        .args(RENDER_TIMEOUT)
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("input: generated, every note hit on time"));
    assert!(
        stdout.contains(&format!("perfect {NOTES} great 0 good 0 bad 0 poor 0")),
        "{stdout}"
    );
    assert!(stdout.contains("re-evaluation matches live: true"));
    let saved = records(&fx.appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    let source = &rec.devices.sources[0];
    assert_eq!(source.receive, ReceiveMethod::Scripted);
    assert_eq!(source.profile.input_offset, TimeUs(0));
    let m = measured(&saved[0]);
    assert_eq!(
        int(&m, "press_to_sound_scheduled_us", "count"),
        Some(NOTES as i64)
    );
}

/// A `device.toml` binding one control to two lanes, binding a reserved
/// settings key, or naming a lane the game does not have is refused when
/// it is loaded: the run ends with the reason and a nonzero exit, and
/// nothing is recorded.
#[test]
fn an_invalid_key_config_exits_nonzero() {
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    type Change = fn(&mut DeviceSettings);
    let bad: [(Change, &str); 3] = [
        (
            |d| d.keyboard.lanes[2].control = d.keyboard.lanes[1].control,
            "control 0x2c is bound to both key 1 and key 2",
        ),
        (
            |d| d.keyboard.lanes[3].control = 0x3d,
            "key 0x3d is reserved for volume",
        ),
        (
            |d| d.keyboard.lanes[4].lane = 9,
            "lane 9 does not exist (lanes are 0 to 7)",
        ),
    ];
    for (change, refusal) in bad {
        let mut device = DeviceSettings::default();
        change(&mut device);
        game::save_settings(&appdata.join("tapestry").join("device.toml"), &device).unwrap();
        let out = scripted_play(&chart_path, &log, &appdata)
            .output()
            .expect("run tapestry");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        // The whole diagnostic: the data folder, then each cause once.
        let line = format!(
            "{}: the settings cannot be used: device.toml: keyboard key config: {refusal}",
            appdata.join("tapestry").display()
        );
        assert!(stderr.lines().any(|l| l == line), "{line:?} in {stderr}");
        assert!(!appdata.join("tapestry").join("records").exists());
    }
}
/// A scripted run that retries once by itself (`--dev-auto-retry`), with
/// `extra` arguments: the retried play replays the log from its start on
/// devices in their first state, so both plays hit every note, both
/// re-evaluate to their live streams, and both are saved. Returns stdout
/// and the records in the order they were saved.
fn run_retried(fx: &Fixture, extra: &[&str]) -> (String, Vec<PlayRecord>) {
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .arg("--dev-auto-retry")
        .args(extra)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    for play in [1, 2] {
        let line = format!(
            "play {play}: Completed; perfect {NOTES} great 0 good 0 bad 0 poor 0; {}",
            "re-evaluation matches live: true"
        );
        assert!(stdout.contains(&line), "{line} in {stdout}");
    }
    let mut paths = records(&fx.appdata);
    // `<n>-<id>.tprl`: by number.
    let number = |p: &PathBuf| -> u64 {
        let stem = p.file_stem().unwrap().to_str().unwrap();
        stem.split_once('-').unwrap().0.parse().unwrap()
    };
    paths.sort_by_key(number);
    let recs = paths
        .iter()
        .map(|p| replay::decode(&fs::read(p).unwrap(), &DecodeLimits::default()).unwrap())
        .collect();
    (stdout, recs)
}

/// The judgments a record re-evaluates to; the run itself checked that
/// they equal its live stream ("re-evaluation matches live").
fn judged(chart: &bms_model::Chart, rec: &PlayRecord) -> Vec<eval::JudgeEvent> {
    let evaluation = eval::evaluate(
        chart,
        &EvalInput {
            events: rec.events.clone(),
            device: rec.devices.profile(),
            start: rec.start.clone(),
            finalized: Some(rec.status.finalized),
            section: None,
        },
        &rules::RulePreset::from_canonical_bytes(&rec.preset.canonical_bytes).unwrap(),
    )
    .unwrap();
    evaluation
        .events
        .into_iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }))
        .collect()
}

/// Every `settings: wrote` line comes while no play is running: after a
/// play's end line and before the next play starts.
fn writes_only_between_plays(stdout: &str) {
    let mut running = false;
    let mut writes = 0;
    for line in stdout.lines() {
        if line == "play started" {
            running = true;
        } else if line.starts_with("play ") {
            running = false;
        } else if line.starts_with("settings: wrote") {
            writes += 1;
            assert!(!running, "a write during a play: {stdout}");
        }
    }
    assert!(writes > 0, "{stdout}");
}

/// Hi-speed (F2) and the display offset (F6), changed during the first
/// play through the settings keys' path, move notes only: a note 300 ms
/// ahead of the display time sits 800 * 300 * (1/575 - 1/600) = 17.391
/// layout pixels higher, and both plays judge exactly as a run without the
/// change. The second play starts with the new display offset; neither
/// play's input offset moves.
#[test]
fn hi_speed_change_moves_notes_not_judgment() {
    let plain = fixture();
    let (plain_out, without) = run_retried(&plain, &[]);
    // The retried play's load time is reported.
    assert!(plain_out.contains("retry "), "{plain_out}");
    let changed = fixture();
    let (stdout, with) = run_retried(
        &changed,
        &["--dev-adjust", "F2,F6", "--dev-adjust-at", "2250"],
    );
    writes_only_between_plays(&stdout);
    let root = game::DataRoot::new(changed.appdata.join("tapestry"));
    let s = game::Settings::load(&root).unwrap();
    assert_eq!(s.player.visible_time_ms, 569);
    assert_eq!(s.device.display_offset_us, 1_000);

    assert_eq!((without.len(), with.len()), (2, 2));
    for (a, b) in without.iter().zip(&with) {
        assert_eq!(a.events, b.events);
        assert_eq!(a.devices.sources, b.devices.sources);
        let (ja, jb) = (judged(&plain.chart, a), judged(&changed.chart, b));
        assert_eq!(ja.len(), NOTES);
        assert_eq!(ja, jb);
    }
    let display = |r: &PlayRecord| r.devices.display_offset;
    assert_eq!(display(&with[0]), TimeUs(0));
    assert_eq!(display(&with[1]), TimeUs(1_000));

    // The same note at the same display time with 600 and 569 ms.
    let scroll = &changed.chart.scroll;
    let y = |visible| {
        render::layout::note_y(
            scroll,
            scroll.position_at(TimeUs(2_300_000)),
            TimeUs(2_000_000),
            TimeUs(visible),
        )
    };
    // By hand: 880 - 800 * 300 / 600 = 480; 880 - 800 * 300 / 569 =
    // 458.2074.
    assert!((y(600_000) - 480.0).abs() < 1e-3);
    let after = i64::from(s.player.visible_time_ms) * 1_000;
    assert!((y(after) - 458.207_4).abs() < 1e-3);
}

/// Settings keys pressed in the first play (hi-speed up twice, volume down,
/// display offset up three times, and input offset up, which a play
/// refuses) and on its end screen (input offset up twice) are written to
/// `player.toml` and `device.toml` when a play ends, never during one.
/// Each play is recorded with the offsets it started with: the input
/// offset changed on the end screen is the retried play's, never the
/// first one's.
#[test]
fn settings_keys_persist_and_the_input_offset_waits_for_the_next_play() {
    let fx = fixture();
    let (stdout, recs) = run_retried(
        &fx,
        &[
            "--dev-adjust",
            "F2,F2,F3,F6,F6,F6,F8",
            "--dev-adjust-at",
            "2250",
            "--dev-adjust-end",
            "F8,F8",
        ],
    );
    writes_only_between_plays(&stdout);
    // Play 1's changes are written when it ends, before play 2 starts.
    let first_write = stdout.find("settings: wrote").unwrap();
    assert!(stdout.find("play 1:").unwrap() < first_write);
    assert!(first_write < stdout.find("play 2:").unwrap());

    let root = game::DataRoot::new(fx.appdata.join("tapestry"));
    let s = game::Settings::load(&root).unwrap();
    // Two hi-speed steps from 600 ms (569, then 540 ms); 100 - 5 %; 3 * 1 ms; F8 during play does nothing,
    // 2 * 1 ms on the end screen.
    assert_eq!(s.player.visible_time_ms, 540);
    assert_eq!(s.device.master_volume, 0.95);
    assert_eq!(s.device.display_offset_us, 3_000);
    assert_eq!(s.device.input_offset_us, 2_000);
    assert_eq!(s.device.keyboard, DeviceSettings::default().keyboard);

    let offsets: Vec<(i64, i64)> = recs
        .iter()
        .map(|r| {
            (
                r.devices.sources[0].profile.input_offset.0,
                r.devices.display_offset.0,
            )
        })
        .collect();
    // Play 1 starts before any key; play 2 starts with both offsets.
    assert_eq!(offsets, [(0, 0), (2_000, 3_000)]);
}

/// The chart with `unknown` unknown headers, each a loading warning, and,
/// with `broken`, a BGM sound whose file does not decode, a sound bank
/// warning.
fn warned_chart(unknown: usize, broken: bool) -> String {
    let mut text = CHART.to_owned();
    for i in 0..unknown {
        text.push_str(&format!("#XUNKNOWN{i:04} 1\n"));
    }
    if broken {
        // Beside the BGM sound at 4.0 s: the play's times stay the same.
        text.push_str("#WAV02 broken.wav\n#00201:02\n");
    }
    text
}

/// The messages of the run's log, after the time and the level.
fn log_lines(appdata: &Path) -> Vec<String> {
    let text = fs::read_to_string(log_path(appdata)).unwrap();
    text.lines().map(|l| l[15..].to_owned()).collect()
}

fn log_path(appdata: &Path) -> PathBuf {
    appdata.join("tapestry").join("logs").join("tapestry.log")
}

/// The text of each `warning:` line on stderr.
fn stderr_warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .collect()
}

/// Loading and sound bank warnings within the cap go to the log, each as
/// stderr prints it, and stderr still prints them all.
#[test]
fn chart_and_sound_warnings_are_logged() {
    let fx = fixture_of(&warned_chart(3, true));
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let warned = stderr_warnings(&stderr);
    let (sounds, chart): (Vec<&str>, Vec<&str>) = warned
        .iter()
        .copied()
        .partition(|w| w.starts_with("Undecodable"));
    assert_eq!(sounds.len(), 1, "{stderr}");
    assert_eq!(
        chart.iter().filter(|w| w.contains("XUNKNOWN")).count(),
        3,
        "{stderr}"
    );
    let logged = log_lines(&fx.appdata);
    let loaded = format!(
        "loaded {} ({} warnings)",
        fx.chart_path.display(),
        chart.len()
    );
    assert!(logged.contains(&loaded), "{loaded:?} in {logged:?}");
    let of = |what: &str| -> Vec<String> {
        logged
            .iter()
            .filter_map(|l| l.strip_prefix(what).map(str::to_owned))
            .collect()
    };
    assert_eq!(of("chart: "), chart);
    assert_eq!(of("sounds: "), sounds);
    assert_eq!(logged.last().map(String::as_str), Some("exit 0"));
}

/// Past the cap, the log keeps the first 100 warnings and a count of the
/// rest within its size budget; stderr still prints every one.
#[test]
fn warnings_past_the_cap_are_counted() {
    let fx = fixture_of(&warned_chart(1_000, false));
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .output()
        .expect("run tapestry");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let warned = stderr_warnings(&stderr);
    let unknown = warned.iter().filter(|w| w.contains("XUNKNOWN")).count();
    assert_eq!(unknown, 1_000, "{stderr}");
    let n = warned.len();
    let logged = log_lines(&fx.appdata);
    let summary = format!("chart: {} more warnings not logged ({n} in all)", n - 100);
    assert_eq!(
        logged.iter().filter(|l| **l == summary).count(),
        1,
        "{summary:?} in {logged:?}"
    );
    let records = logged
        .iter()
        .filter(|l| l.starts_with("chart: ") && **l != summary)
        .count();
    assert_eq!(records, 100);
    assert_eq!(logged.last().map(String::as_str), Some("exit 0"));
    let size = fs::metadata(log_path(&fx.appdata)).unwrap().len();
    assert!(size <= 1 << 20, "{size}");
}

/// A log that cannot be opened (`logs` is a file) leaves the play as it
/// is: the same exit code, one saved record, the same judgment.
#[test]
fn an_unwritable_log_does_not_change_the_play() {
    let fx = fixture();
    let root = fx.appdata.join("tapestry");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("logs"), "").unwrap();
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let logs = root.join("logs");
    let prefix = format!("log: {}: ", logs.display());
    assert_eq!(
        stderr.lines().filter(|l| l.starts_with("log: ")).count(),
        1,
        "{stderr}"
    );
    assert!(stderr.starts_with(&prefix), "{stderr}");
    assert!(
        stdout.contains(&format!("perfect {NOTES} great 0 good 0 bad 0 poor 0")),
        "{stdout}"
    );
    assert_eq!(records(&fx.appdata).len(), 1);
    assert!(logs.is_file());
}

/// `tapestry keys` cancelled with Escape through the real command writes
/// no settings file and opens no recording: no records either.
#[test]
#[ignore = "opens a window: needs an interactive desktop"]
fn keys_cancelled_writes_nothing() {
    let fx = fixture();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args(["keys", "--dev-scripted-keys", "2c,1f,01"])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("keys: cancelled, nothing written"),
        "{stdout}"
    );
    let root = fx.appdata.join("tapestry");
    assert!(!root.join("device.toml").exists());
    assert!(!root.join("player.toml").exists());
    assert!(!root.join("records").exists());
}

/// The scripted chart with a lane 3 long note from 4.0 s to 5.0 s, which
/// starts with the BGM sound: 8 notes.
const AUTO_CHART: &str = "#PLAYER 1\n#TITLE autoplay\n#BPM 120\n#WAV01 click.wav\n\
                          #00111:01010101\n#00113:00010001\n#00116:01000000\n#00201:01\n\
                          #00253:0101\n";
const AUTO_NOTES: usize = 8;

/// A temporary folder with the autoplay chart and its sound; returns it,
/// the chart's path and the data root's parent.
fn auto_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let charts = dir.path().join("chart");
    fs::create_dir_all(&charts).unwrap();
    let chart_path = charts.join("autoplay.bms");
    fs::write(&chart_path, AUTO_CHART).unwrap();
    write_click(&charts.join("click.wav"));
    let appdata = dir.path().join("appdata");
    (dir, chart_path, appdata)
}

/// `play --auto <chart> --dev-headless` with `extra` arguments.
fn auto_run(chart: &Path, appdata: &Path, extra: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg("--auto")
        .arg(chart)
        .arg("--dev-headless")
        .args(RENDER_TIMEOUT)
        .args(extra)
        .env("LOCALAPPDATA", appdata)
        .output()
        .expect("run tapestry")
}

/// `tapestry play <chart>` switches presets on its result too: a scripted
/// right arrow shows the play under lr2-style as a what-if, after the
/// default beatoraja-style as played; the play preset is unchanged.
#[test]
fn direct_play_result_switches_presets() {
    let (_dir, chart, appdata) = auto_fixture();
    let out = auto_run(
        &chart,
        &appdata,
        &["--dev-result".as_ref(), "next,back".as_ref()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let viewing: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("result: viewing "))
        .collect();
    assert_eq!(viewing.len(), 2, "{stdout}");
    assert_eq!(viewing[0], "result: viewing beatoraja-style (as played)");
    assert!(
        viewing[1].starts_with("result: viewing lr2-style (what-if, "),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .lines()
            .filter(|l| l.starts_with("play started"))
            .count(),
        1,
        "{stdout}"
    );
}

/// A result script step that has no place on a result fails the direct
/// play's run with a nonzero exit that names it.
#[test]
fn direct_play_result_script_failure_fails_the_run() {
    let (_dir, chart, appdata) = auto_fixture();
    let out = auto_run(
        &chart,
        &appdata,
        &["--dev-result".as_ref(), "enter".as_ref()],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("the result script failed"), "{stderr}");
    assert!(stderr.contains("Enter on a result"), "{stderr}");
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// H1: autoplay on the game's threads (a clocked input thread, the
/// merged schedule, WARP) plays the chart to its end with every note
/// PERFECT, re-evaluates to the live play, shows its end screen and saves
/// nothing: no record, no measurement, only the settings and the preset
/// a first start writes.
#[test]
fn auto_play_saves_nothing_and_judges_every_note_perfect() {
    let (dir, chart, appdata) = auto_fixture();
    let frames = dir.path().join("frames");
    let out = auto_run(
        &chart,
        &appdata,
        &["--dev-frames".as_ref(), frames.as_os_str()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(0));
    let line = format!(
        "play 1: Completed; perfect {AUTO_NOTES} great 0 good 0 bad 0 poor 0; {}",
        "re-evaluation matches live: true"
    );
    assert!(stdout.contains(&line), "{line} in {stdout}");
    assert!(
        stdout.lines().any(|l| l == "autoplay: not saved"),
        "{stdout}"
    );
    assert!(!stdout.lines().any(|l| l.starts_with("saved:")), "{stdout}");
    assert!(!stdout.contains("measure:"), "{stdout}");
    assert!(frames.join("end.png").is_file());
    // The key sounds came from the merged schedule: a start every 0.5 s
    // from 2.0 s (the notes, chords on one frame) to 4.0 s (the long note
    // with the BGM), each exactly 24 000 frames after the last at 48 kHz.
    let onsets: Vec<i64> = stdout
        .lines()
        .find_map(|l| l.strip_prefix("dev: audio onsets "))
        .and_then(|l| l.split_once(": "))
        .map(|(_, frames)| frames.split(' ').map(|f| f.parse().unwrap()).collect())
        .unwrap_or_default();
    let gaps: Vec<i64> = onsets.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(gaps, [24_000; 4], "{onsets:?}");
    let root = appdata.join("tapestry");
    assert!(!root.join("records").exists());
    let mut kept: Vec<String> = files_under(&root)
        .iter()
        .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
        .collect();
    kept.sort();
    // The settings and the run's log are kept; no play is (design.md
    // "Autoplay", "Log File"), and the built-in preset is never written
    // (design.md "Standard Presets").
    assert_eq!(
        kept,
        ["device.toml", "logs\\tapestry.lock", "logs\\tapestry.log"],
        "{kept:?}"
    );
}

/// H2: retried by itself after ten input offset steps on the end screen
/// (`F8`, 10 ms), autoplay plays the chart over from its first note with
/// its own input offset of 0: both plays judge every note PERFECT, neither
/// is saved, and the new input offset is written to `device.toml`.
#[test]
fn auto_retry_ignores_the_changed_input_offset() {
    let (_dir, chart, appdata) = auto_fixture();
    let f8 = ["F8"; 10].join(",");
    let out = auto_run(
        &chart,
        &appdata,
        &[
            "--dev-auto-retry".as_ref(),
            "--dev-adjust-end".as_ref(),
            f8.as_ref(),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    for play in [1, 2] {
        let line = format!(
            "play {play}: Completed; perfect {AUTO_NOTES} great 0 good 0 bad 0 poor 0; {}",
            "re-evaluation matches live: true"
        );
        assert!(stdout.contains(&line), "{line} in {stdout}");
    }
    assert_eq!(stdout.matches("autoplay: not saved").count(), 2, "{stdout}");
    writes_only_between_plays(&stdout);
    let root = game::DataRoot::new(appdata.join("tapestry"));
    let s = game::Settings::load(&root).unwrap();
    assert_eq!(s.device.input_offset_us, 10_000);
    assert_eq!(s.device.keyboard, DeviceSettings::default().keyboard);
    assert!(!root.records().exists());
}

/// Autoplay plays with its own keys whatever `device.toml` binds: with no
/// keyboard lane bound, every note is still PERFECT, and the settings
/// written when the run ends (an input offset step on the end screen)
/// keep `device.toml`'s own bindings.
#[test]
fn auto_play_uses_its_own_keys_and_keeps_device_toml() {
    let (_dir, chart, appdata) = auto_fixture();
    let device = DeviceSettings {
        keyboard: game::KeyConfig {
            lanes: Vec::new(),
            ..DeviceSettings::default().keyboard
        },
        ..DeviceSettings::default()
    };
    let path = appdata.join("tapestry").join("device.toml");
    game::save_settings(&path, &device).unwrap();
    let out = auto_run(
        &chart,
        &appdata,
        &["--dev-adjust-end".as_ref(), "F8".as_ref()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let line = format!("play 1: Completed; perfect {AUTO_NOTES} great 0 good 0 bad 0 poor 0;");
    assert!(stdout.contains(&line), "{line} in {stdout}");
    assert!(stdout.contains("settings: wrote"), "{stdout}");
    let written: DeviceSettings = game::load_settings(&path).unwrap();
    assert_eq!(written.input_offset_us, 1_000);
    assert_eq!(written.keyboard, device.keyboard);
}

/// H3: autoplay and an input log are two ways to receive input, refused
/// together before anything loads; `--dev-headless` is for autoplay
/// alone.
#[test]
fn auto_with_scripted_input_is_refused() {
    let fx = fixture();
    let run = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_tapestry"))
            .args(args)
            .env("LOCALAPPDATA", &fx.appdata)
            .output()
            .expect("run tapestry")
    };
    let chart = fx.chart_path.as_os_str();
    let log = fx.log.as_os_str();
    let both = "--auto and --dev-scripted-input cannot be used together";
    let headless = "--dev-headless needs --auto or --dev-perfect";
    for (args, refusal) in [
        (
            vec![
                "play".as_ref(),
                "--auto".as_ref(),
                chart,
                "--dev-scripted-input".as_ref(),
                log,
            ],
            both,
        ),
        (
            vec![
                "play".as_ref(),
                chart,
                "--dev-scripted-input".as_ref(),
                log,
                "--auto".as_ref(),
            ],
            both,
        ),
        (
            vec!["play".as_ref(), chart, "--dev-headless".as_ref()],
            headless,
        ),
        (
            vec![
                "play".as_ref(),
                chart,
                "--dev-scripted-input".as_ref(),
                log,
                "--dev-headless".as_ref(),
            ],
            headless,
        ),
    ] {
        let out = run(&args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.lines().any(|l| l == refusal), "{args:?}: {stderr}");
        assert!(!fx.appdata.join("tapestry").exists(), "{args:?}");
    }
}

/// W1: autoplay on the real window, audio output and renderer, retried
/// once by itself: both plays judge every note PERFECT and nothing is
/// saved.
#[test]
#[ignore = "opens a fullscreen window and the audio device: needs an interactive desktop"]
fn auto_play_on_the_window() {
    let (_dir, chart, appdata) = auto_fixture();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg("--auto")
        .arg(&chart)
        .arg("--dev-auto-retry")
        .env("LOCALAPPDATA", &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert_eq!(
        stdout
            .matches(&format!("perfect {AUTO_NOTES} great 0 good 0 bad 0 poor 0"))
            .count(),
        2,
        "{stdout}"
    );
    assert_eq!(stdout.matches("autoplay: not saved").count(), 2, "{stdout}");
    assert!(!appdata.join("tapestry").join("records").exists());
}

/// A chart's missing sound files are one warning line, on stderr and in the
/// log alike.
#[test]
fn missing_sounds_are_summed_up_once() {
    let mut text = CHART.to_owned();
    text.push_str("#WAV03 gone1.wav\n#WAV04 gone2.wav\n#WAV05 gone3.wav\n#WAV06 gone4.wav\n");
    let fx = fixture_of(&text);
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let line = "4 sound files are missing: gone1.wav, gone2.wav, gone3.wav and 1 more";
    let missing: Vec<&str> = stderr_warnings(&stderr)
        .into_iter()
        .filter(|w| w.contains("missing"))
        .collect();
    assert_eq!(missing, [line], "{stderr}");
    let logged = log_lines(&fx.appdata);
    assert!(
        logged.contains(&format!("chart: {line}")),
        "{line:?} in {logged:?}"
    );
}

/// A mono 16-bit sound at 48 kHz, the output's rate, so it is not
/// converted and its clicks keep their frames: `frames` long, silent but
/// for a 2 ms click from each frame of `clicks`.
fn write_clicks(path: &Path, frames: usize, clicks: &[usize]) {
    let samples: Vec<i16> = (0..frames)
        .map(|i| {
            let into = clicks
                .iter()
                .find_map(|&c| i.checked_sub(c).filter(|&d| d < 96));
            match into {
                Some(d) if d % 8 < 4 => 8_000,
                Some(_) => -8_000,
                None => 0,
            }
        })
        .collect();
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
    // PCM, mono, 48 kHz, 96 000 bytes/s, 2-byte frames of 16 bits.
    b.extend_from_slice(&[1, 0, 1, 0]);
    b.extend_from_slice(&48_000_u32.to_le_bytes());
    b.extend_from_slice(&96_000_u32.to_le_bytes());
    b.extend_from_slice(&[2, 0, 16, 0]);
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, b).unwrap();
}

/// 120 BPM, 2 s a measure. BGM: `long.wav` at 0 (4 s, silent but for a
/// click 3.0 s in), `click.wav` at 1.0 s, 2.05 s and 3.5 s. Targets: lane 1
/// at 4.0 and 4.5 s.
const START_CHART: &str = "#PLAYER 1\n#TITLE start\n#BPM 120\n#WAV01 long.wav\n\
                           #WAV02 click.wav\n#00001:01\n#00001:0002\n\
                           #00101:00020000000000000000000000000000000000000000000000000000000000000000000000000000\n\
                           #00101:00000002\n#00211:0202\n";

/// The fixture of `text`, with `long.wav` and `click.wav` beside it.
fn start_fixture(text: &str) -> Fixture {
    let fx = fixture_of(text);
    let dir = fx.chart_path.parent().unwrap();
    write_clicks(&dir.join("long.wav"), 4 * 48_000, &[3 * 48_000]);
    write_clicks(&dir.join("click.wav"), 2_400, &[0]);
    fx
}

/// The onsets the run printed, in output frames.
fn printed_onsets(stdout: &str) -> Vec<i64> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("dev: audio onsets "))
        .and_then(|l| l.split_once(": "))
        .map(|(_, frames)| frames.split(' ').map(|f| f.parse().unwrap()).collect())
        .unwrap_or_default()
}

#[test]
fn dev_start_starts_the_play_at_that_time() {
    let fx = start_fixture(START_CHART);
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .args(["--dev-start", "4000"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(0));
    // One round, both targets hit, saved as a mid-chart start from 4 s with
    // a 2 s lead-in, recording from 1.8 s.
    assert_eq!(stdout.matches("play started").count(), 1, "{stdout}");
    assert!(
        stdout.contains("play 1: Completed; perfect 2 great 0 good 0 bad 0 poor 0"),
        "{stdout}"
    );
    assert!(stdout.contains("re-evaluation matches live: true"));
    let saved = records(&fx.appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    let PlayType::MidChartStart(practice) = &rec.play_type else {
        panic!("{:?}", rec.play_type);
    };
    assert_eq!(practice.start, TimeUs(4_000_000));
    assert_eq!(practice.lead_in, TimeUs(2_000_000));
    assert_eq!(practice.attempt, 1);
    assert_eq!(rec.start.opened_at, TimeUs(1_800_000));
    // The restored long voice's click (3.0 s) and the marker (3.5 s) sound
    // exactly 0.5 s apart, whichever plan was placed: both restore before
    // 3.0 s. Nothing due before the restore sounds (no 1.0 s click, nor
    // the schedule from chart time 0), and the 2.05 s click only after a
    // restore at the lead-in's start.
    let first = stdout.contains("audio: restored at 2000 ms (first)");
    let fallback = stdout.contains("audio: restored at 2500 ms (fallback)");
    assert!(first ^ fallback, "{stdout}");
    let onsets = printed_onsets(&stdout);
    let gaps: Vec<i64> = onsets.windows(2).map(|w| w[1] - w[0]).collect();
    let near = |gap: i64, want: i64| (gap - want).abs() <= 1;
    let lead = usize::from(first);
    assert!(gaps.len() > lead + 1, "{onsets:?}");
    if first {
        // 2.05 s to 3.0 s.
        assert!(near(gaps[0], 45_600), "{onsets:?}");
    }
    assert!(near(gaps[lead], 24_000), "{onsets:?}");
    // Then only the targets' key sounds, sent from their judgments.
    assert_eq!(onsets.len(), lead + 4, "{onsets:?}");
}

#[test]
fn dev_start_inside_a_lone_long_note_exits_one() {
    // One long note on lane 1 from 1 s to 6 s: a start at 4 s leaves no
    // tap or long note to play.
    let text = "#PLAYER 1\n#TITLE lone\n#BPM 120\n#LNTYPE 1\n#WAV01 click.wav\n\
                #00051:0001\n#00351:01\n";
    let fx = fixture_of(text);
    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .args(["--dev-start", "4000"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr.contains("the section has no notes to play"),
        "{stderr}"
    );
    assert!(!stdout.contains("play started"), "{stdout}");
    assert!(!fx.appdata.join("tapestry").join("records").exists());
}
