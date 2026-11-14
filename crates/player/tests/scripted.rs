//! The whole game on its real threads, driven by an input log instead of
//! devices (dev-tools): scripted input, a recording audio output, and WARP
//! offscreen. The saved record must replay the log exactly and re-evaluate
//! to the live result.

#![cfg(feature = "dev-tools")]

mod text_skin;

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
    write_click_at(path, 8_000);
}

/// [`write_click`] of `amplitude`.
fn write_click_at(path: &Path, amplitude: i16) {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { amplitude } else { -amplitude })
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
        start_tuning: Some(replay::StartTuning::default()),
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
        history: Default::default(),
    };
    fs::write(path, replay::encode(&rec)).unwrap();
    events
}

/// `path` rewritten as a capture cancelled before playback: no winning
/// opening, its one READY quit at origin 0.
fn cancel_log(path: &Path) {
    let mut rec = replay::decode(&fs::read(path).unwrap(), &DecodeLimits::default()).unwrap();
    rec.start = InputStart::default();
    rec.events.clear();
    rec.status = Status {
        end: EndStatus::CancelledBeforePlayback,
        finalized: TimeUs(0),
        incomplete: replay::Incomplete::default(),
    };
    rec.history.openings = vec![replay::CancelledOpening {
        start: InputStart::default(),
        origin: TimeUs(0),
        closed: TimeUs(0),
        events: 0..0,
        arrivals: 0..0,
        reason: replay::CancelReason::Quit,
    }];
    fs::write(path, replay::encode(&rec)).unwrap();
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

/// The output frames a development run reports sounds starting at.
fn onsets(stdout: &str) -> Vec<i64> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("dev: audio onsets "))
        .and_then(|l| l.split_once(": "))
        .map(|(_, frames)| frames.split(' ').map(|f| f.parse().unwrap()).collect())
        .unwrap_or_default()
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
    for f in ["ready.png", "play.png", "end.png"] {
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
    let setup = int(&m, "ready", "setup_us").unwrap();
    let first = int(&m, "ready", "first_frame_us").unwrap();
    // The setup takes time and ends within the 1 s lead-in; the first
    // frame comes after it.
    assert!(0 < setup && setup < 1_000_000, "{setup}");
    assert!(first > setup, "{first} {setup}");
    assert!(stdout.contains(&format!(
        "ready: setup {setup} us, first frame {first} us after it started"
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
    // Other physical keys (Q and W, W being SELECT by default, so START
    // and SELECT are E and R): the generated keys must still be the ones
    // recorded.
    let lanes = &mut device.keyboard.lanes;
    (lanes[1].control, lanes[2].control) = (0x10, 0x11);
    (device.keyboard.start, device.keyboard.select) = (Some(0x12), Some(0x13));
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

/// The start gate on the real threads, which every play runs (design.md
/// "Play Lifecycle", the start gate): START (E) held 300 ms before chart
/// time 0 cancels the first READY through the input thread's latch, its
/// release at 200 ms rearms a fresh READY through the input thread's
/// boundary and the audio thread's transport, and the notes, pressed
/// 1.2 s later than the log has them, are played by that READY: every note
/// judged PERFECT, and the audio restarted for it, its key sounds and BGM
/// all sounding, as a play without the hold does. The rearm is answered
/// at least 30 ms after the release (design.md "Shortening Launch-Based
/// Checks", scripted input), and every press still lands on its note. The
/// saved record keeps the cancelled opening, E's press in it, and the
/// winning READY's input alone.
///
/// An input thread starved past the commit reads the hold only after it:
/// the design makes that an ordinary in-play adjustment, so the play is
/// not restarted and the record has no cancelled opening, E's press in its
/// input. Either way F2, pressed during the play, reaches the adjustments
/// and no record, and the record re-evaluates equal to the live play.
/// The exact outcomes are the controlled tests':
/// `a_late_rearm_opens_at_the_scripted_release` and
/// `a_hold_and_its_release_in_one_wake_hold_the_script_at_the_release`
/// (src/input_side.rs), and `a_hold_after_the_commit_is_an_ordinary_adjustment`
/// (game's tests/start_gate.rs).
#[test]
fn the_start_gate_cancels_and_rearms_ready_on_the_real_threads() {
    const E: u32 = 0x12;
    let Fixture {
        dir,
        chart,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    let unheld = scripted_play(&chart_path, &log, &dir.path().join("unheld"))
        .output()
        .expect("run tapestry");
    let unheld = onsets(&String::from_utf8_lossy(&unheld.stdout));
    let mut rec = replay::decode(&fs::read(&log).unwrap(), &DecodeLimits::default()).unwrap();
    let q = |ms: i64, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(E),
            value,
        },
    };
    let mut events = vec![q(-300, 1), q(200, 0)];
    events.extend(rec.events.iter().map(|e| eval::PhysicalInputEvent {
        received: TimeUs(e.received.0 + 1_200_000),
        ..*e
    }));
    rec.events = events;
    rec.status.finalized = TimeUs(rec.status.finalized.0 + 1_200_000);
    fs::write(&log, replay::encode(&rec)).unwrap();

    let out = scripted_play(&chart_path, &log, &appdata)
        .args(["--dev-speed", "F2", "--dev-adjust-at", "1500"])
        .args(["--dev-rearm-delay", "30"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout.contains("re-evaluation matches live: true"),
        "{stdout}"
    );
    let saved = records(&appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert_eq!(rec.status.end, EndStatus::PlayedToEnd);
    let h = &rec.history;
    let q_press = |e: &eval::PhysicalInputEvent| {
        e.kind
            == eval::InputKind::Control {
                control: eval::ControlId(E),
                value: 1,
            }
    };
    // F2 is in no opening.
    let f2 = |e: &eval::PhysicalInputEvent| matches!(e.kind, eval::InputKind::Control { control, .. } if control.0 == 0x3C);
    assert!(!rec.events.iter().any(f2) && !h.events.iter().any(f2));
    // Nor in the starting tuning: the winning opening started at 4.00,
    // before the play's step to 4.50 (design.md "Input Log Format").
    assert_eq!(
        rec.start_tuning.and_then(|t| t.hispeed),
        Some(replay::HiSpeedRatio { num: 4, den: 1 }),
        "{:?}",
        rec.start_tuning
    );
    assert!(
        stdout.contains("adjust: hi-speed 4.50 floating"),
        "{stdout}"
    );
    let Some(o) = h.openings.first() else {
        // The hold was read after the commit: no restart, E's press in the
        // played input.
        assert!(rec.events.iter().any(q_press), "{:?}", rec.events);
        return;
    };
    assert_eq!(h.openings.len(), 1, "{h:?}");
    // The same sounds as the play without the hold: the key sounds, and the BGM,
    // which starts only once the rearmed candidate's transport commits.
    // Where they fall is the real threads' timing, which the stepped
    // worker fixes exactly (start_gate.rs,
    // `a_rearmed_candidate_starts_its_bgm_at_its_own_origin`).
    let gated = onsets(&stdout);
    assert!(!unheld.is_empty());
    assert_eq!(gated.len(), unheld.len(), "{gated:?} {unheld:?}");
    assert!(
        stdout.contains(&format!(
            "play 1: Completed; perfect {NOTES} great 0 good 0 bad 0 poor 0; re-evaluation matches live: true"
        )),
        "{stdout}"
    );
    assert_eq!(o.reason, replay::CancelReason::Held);
    // On the clock the next READY's chart time 0 lies 1.2 s after the
    // first's and at least the rearm's delay more; how much more is the
    // scheduling's (the exact times are the unit test's,
    // `a_late_rearm_opens_at_the_scripted_release`).
    assert!(o.origin.0 <= -1_230_000, "{:?}", o.origin);
    // On the chart, every press is on its note: the rearm opened at the
    // release, not where it was answered.
    let late: Vec<_> = all_events(&chart, &rec)
        .into_iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { deviation: Some(d), .. } if d.0 != 0))
        .collect();
    assert!(late.is_empty(), "{late:?}");
    let cancelled = &h.events[o.events.start as usize..o.events.end as usize];
    assert!(cancelled.iter().any(q_press));
    // The winning READY's input: every note's press and release.
    let lanes = rec
        .events
        .iter()
        .filter(|e| !matches!(e.kind, eval::InputKind::Control { control, .. } if control.0 == E))
        .count();
    assert_eq!(lanes, 2 * NOTES);
}

/// A skin built per chart on the direct path, its READY cancelled by a hold
/// and rearmed: the candidate keeps the attempt's prepared text, so the play
/// that opens shows the title in its op-less and op-81 texts and nothing in
/// its op-80 one.
#[test]
fn a_rearmed_candidate_keeps_a_per_chart_skins_text() {
    use text_skin::{Band, bars};
    const E: u32 = 0x12;
    let Fixture {
        dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture_of(&CHART.replace("#TITLE scripted", "#TITLE aa"));
    let mut rec = replay::decode(&fs::read(&log).unwrap(), &DecodeLimits::default()).unwrap();
    let q = |ms: i64, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(E),
            value,
        },
    };
    let mut events = vec![q(-300, 1), q(200, 0)];
    events.extend(rec.events.iter().map(|e| eval::PhysicalInputEvent {
        received: TimeUs(e.received.0 + 1_200_000),
        ..*e
    }));
    rec.events = events;
    rec.status.finalized = TimeUs(rec.status.finalized.0 + 1_200_000);
    fs::write(&log, replay::encode(&rec)).unwrap();
    text_skin::write(&dir.path().join("skin"), &appdata);
    let frames = dir.path().join("frames");
    let out = scripted_play(&chart_path, &log, &appdata)
        .arg("--dev-frames")
        .arg(&frames)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let saved = records(&appdata);
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert_eq!(rec.history.openings.len(), 1, "a candidate was cancelled");
    let shown = |name: &str| {
        [Band::Always, Band::Loading, Band::Loaded].map(|b| bars(&frames.join(name), b))
    };
    assert_eq!(shown("ready.png"), [2, 0, 2], "{stdout}");
    assert_eq!(shown("play.png"), [2, 0, 2], "{stdout}");
}

/// Quit on E (0x12), the default START, is refused with the collision
/// when `device.toml` loads, before any play opens, so the run ends with
/// a nonzero exit, and nothing is recorded.
#[test]
fn a_colliding_device_toml_opens_no_play() {
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    let mut device = DeviceSettings::default();
    device.keyboard.quit = Some(0x12);
    game::save_settings(&appdata.join("tapestry").join("device.toml"), &device).unwrap();
    let out = scripted_play(&chart_path, &log, &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    let line = "the settings cannot be used: device.toml: keyboard key config: \
                control 0x12 is bound to both quit and START";
    assert!(
        stderr.lines().any(|l| l.ends_with(line)),
        "{line:?} in {stderr}"
    );
    assert!(!stdout.contains("play 1:"), "{stdout}");
    assert!(!appdata.join("tapestry").join("records").exists());
}

/// A self-made play skin in `skin`, `s.json`: one blue image over the
/// whole canvas; `player.toml` under `appdata` chooses it for play and
/// result, play's entry being `play_entry`.
fn configure_blue_skin(skin: &Path, appdata: &Path, play_entry: &str) {
    fs::create_dir_all(skin).unwrap();
    let file = fs::File::create(skin.join("blue.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let blue: Vec<u8> = (0..16 * 16).flat_map(|_| [0, 0, 255, 255]).collect();
    e.write_header().unwrap().write_image_data(&blue).unwrap();
    fs::write(
        skin.join("s.json"),
        r#"{"type": 0, "w": 1920, "h": 1080,
            "source": [{"id": "b", "path": "blue.png"}],
            "image": [{"id": "b", "src": "b", "x": 0, "y": 0, "w": 16, "h": 16}],
            "destination": [{"id": "b", "dst": [{"x": 0, "y": 0, "w": 1920, "h": 1080}]}]}"#,
    )
    .unwrap();
    let root = appdata.join("tapestry");
    fs::create_dir_all(&root).unwrap();
    let table = |kind: &str, entry: &str| {
        format!(
            "[skins.{kind}]\nroot = '{}'\nentry = '{entry}'\n",
            skin.display()
        )
    };
    fs::write(
        root.join("player.toml"),
        format!("{}{}", table("play", play_entry), table("result", "s.json")),
    )
    .unwrap();
}

/// `tapestry play` with a play skin configured installs it before the
/// chart's first preparation: the run reports it installed before the
/// play starts, the play draws in it (blue at the centre), and the chart
/// is played and recorded. The result kind, a play skin there, is
/// refused and keeps its built-in screen.
#[test]
fn a_configured_play_skin_is_installed_on_the_direct_path() {
    let Fixture {
        dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    configure_blue_skin(&dir.path().join("skin"), &appdata, "s.json");
    let frames = dir.path().join("frames");
    let out = scripted_play(&chart_path, &log, &appdata)
        .arg("--dev-frames")
        .arg(&frames)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    let installed = stdout.find("skin: play: installed").expect("installed");
    assert!(
        stdout.contains("skin: result: refused: a 7-key play skin in the single-chart result slot"),
        "{stdout}"
    );
    let started = stdout.find("play started").expect("played");
    assert!(installed < started, "{stdout}");
    assert!(stdout.contains("play 1:"), "{stdout}");
    assert_eq!(records(&appdata).len(), 1);
    let (w, h, rgba) = png_pixels(&frames.join("play.png"));
    let i = ((h / 2) * w + w / 2) * 4;
    assert_eq!(&rgba[i..i + 3], [0, 0, 255], "{stdout}");
}

/// `tapestry play` with a play skin that cannot be used fails at once:
/// the run says why and exits 1 before any play, nothing is recorded,
/// the result kind is not reached and no built-in skin stands in.
#[test]
fn a_refused_play_skin_fails_the_direct_path() {
    let Fixture {
        dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    configure_blue_skin(&dir.path().join("skin"), &appdata, "missing.json");
    let out = scripted_play(&chart_path, &log, &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    let skins: Vec<&str> = stdout.lines().filter(|l| l.starts_with("skin:")).collect();
    assert_eq!(skins.len(), 1, "{stdout}");
    assert!(
        skins[0].starts_with("skin: play: refused: cannot read missing.json"),
        "{stdout}"
    );
    assert!(
        stderr.contains("the play skin cannot be used: cannot read missing.json"),
        "{stderr}"
    );
    assert!(!stdout.contains("play started"), "{stdout}");
    assert!(!appdata.join("tapestry").join("records").exists());
}

/// A capture cancelled before playback is no script: the run ends with
/// the reason and a nonzero exit, and nothing is recorded.
#[test]
fn a_cancelled_capture_is_no_script() {
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    cancel_log(&log);
    let out = scripted_play(&chart_path, &log, &appdata)
        .output()
        .expect("run tapestry");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let refusal = replay::verify::VerifyError::NoPlayedAttempt.to_string();
    assert!(stderr.contains(&refusal), "{stderr}");
    assert!(!appdata.join("tapestry").join("records").exists());
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
    all_events(chart, rec)
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

/// One F2 press in each play (a scripted packet only the adjustment tap
/// observes) and the display offset (F6) in the first move notes only: F2 is
/// exactly one Floating step, 4.00 to 4.50 at 120 BPM and the target
/// green number 300, so a note 300 ms ahead of the display time sits
/// 120 · (4.5 − 4) = 60 layout pixels higher; both plays judge exactly as
/// a run without the change, and F2 is in neither record. The display
/// offset persists; neither play's input offset moves.
#[test]
fn one_f2_press_steps_hi_speed_once_and_moves_notes_not_judgment() {
    let plain = fixture();
    let (plain_out, without) = run_retried(&plain, &[]);
    // The retried play's load time is reported.
    assert!(plain_out.contains("retry "), "{plain_out}");
    assert_eq!(
        plain_out
            .matches("adjust: hi-speed 4.00 floating target 300")
            .count(),
        2,
        "{plain_out}"
    );
    let changed = fixture();
    let (stdout, with) = run_retried(
        &changed,
        &[
            "--dev-speed",
            "F2",
            "--dev-adjust",
            "F6",
            "--dev-adjust-at",
            "2250",
        ],
    );
    writes_only_between_plays(&stdout);
    // Each play's press steps once; a retry solves from the target again
    // before its own press.
    assert_eq!(
        stdout
            .matches("adjust: hi-speed 4.50 floating target 300")
            .count(),
        2,
        "{stdout}"
    );
    let root = game::DataRoot::new(changed.appdata.join("tapestry"));
    let s = game::Settings::load(&root).unwrap();
    assert_eq!(s.device.display_offset_us, 1_000);

    assert_eq!((without.len(), with.len()), (2, 2));
    for (a, b) in without.iter().zip(&with) {
        assert_eq!(a.events, b.events, "F2 is not recorded");
        assert_eq!(a.devices.sources, b.devices.sources);
        let (ja, jb) = (judged(&plain.chart, a), judged(&changed.chart, b));
        assert_eq!(ja.len(), NOTES);
        assert_eq!(ja, jb);
    }
    let display = |r: &PlayRecord| r.devices.display_offset;
    assert_eq!(display(&with[0]), TimeUs(0));
    assert_eq!(display(&with[1]), TimeUs(1_000));

    // The same note (lane 1's at 2.5 s) at the same display time (2.2 s)
    // at 4.00 and 4.50: its stored position, and the playhead the attempt
    // prepares, sampled there.
    let chart = &changed.chart;
    let playhead = skin_model::ExactPlayhead::prepare(chart, None).unwrap();
    let note = chart.lanes[1][1];
    assert_eq!(note.time, TimeUs(2_500_000));
    let y = |num, den| {
        render::layout::note_y(
            note.pos,
            playhead.position_at(TimeUs(2_200_000)),
            skin_model::Gain::new(num, den).unwrap(),
        )
    };
    // By hand: 0.6 beats ahead, 200 · H pixels a beat: 880 − 480 = 400
    // and 880 − 540 = 340.
    assert!((y(4, 1) - 400.0).abs() < 1e-3);
    assert!((y(9, 2) - 340.0).abs() < 1e-3);
}

/// Settings keys pressed in the first play (volume down, display offset up
/// three times, and input offset up, which a play refuses) and on its end
/// screen (input offset up twice) are written to `device.toml` when a play
/// ends, never during one.
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
            "F3,F6,F6,F6,F8",
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
    // The key and background volumes 100 - 5 %, the system volume
    // untouched; 3 * 1 ms; F8 during play does nothing, 2 * 1 ms on the
    // end screen.
    assert_eq!(s.device.key_volume, 0.95);
    assert_eq!(s.device.background_volume, 0.95);
    assert_eq!(s.device.system_volume, 1.0);
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
    // The end screen draws the view's graphs under its text: the first
    // gauge's trace (green) across the bottom left, and the histogram's
    // bar of on-time presses (late side, orange) bottom right.
    let (w, h, rgba) = png_pixels(&frames.join("end.png"));
    let count = |left: bool, keep: fn(i32, i32, i32) -> bool| {
        let mut n = 0;
        for y in h * 4 / 5..h {
            for x in 0..w {
                if (x < w / 2) != left {
                    continue;
                }
                let p = &rgba[(y * w + x) * 4..][..3];
                let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
                n += usize::from(keep(r, g, b));
            }
        }
        n
    };
    let green = count(true, |r, g, b| g > r + 30 && g > b + 30);
    let orange = count(false, |r, g, b| r > g + 30 && r > b + 60);
    assert!(green > 288, "{green} green pixels in {w}x{h}");
    assert!(orange > 0, "{orange} orange pixels in {w}x{h}");
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
    // The settings, the run's log and the system sounds' recovery state
    // with its lock are kept; no play is (design.md "Autoplay", "Log
    // File", "Threat Model" T18), and the built-in preset is never written
    // (design.md "Standard Presets").
    assert_eq!(
        kept,
        [
            "device.toml",
            "logs\\tapestry.lock",
            "logs\\tapestry.log",
            "sound-sets.lock",
            "sound-sets.toml"
        ],
        "{kept:?}"
    );
}

/// The size and RGBA8 pixels of a PNG file the frame dump wrote: one IDAT
/// chunk of stored deflate blocks, every row unfiltered.
fn png_pixels(path: &Path) -> (usize, usize, Vec<u8>) {
    let bytes = fs::read(path).unwrap();
    let mut at = 8;
    let (mut w, mut h, mut z) = (0, 0, Vec::new());
    while at + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let data = &bytes[at + 8..at + 8 + len];
        match &bytes[at + 4..at + 8] {
            b"IHDR" => {
                w = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
                h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
            }
            b"IDAT" => z.extend_from_slice(data),
            _ => {}
        }
        at += 12 + len;
    }
    let mut raw = Vec::new();
    let mut i = 2;
    loop {
        let last = z[i] & 1 == 1;
        let len = usize::from(u16::from_le_bytes([z[i + 1], z[i + 2]]));
        raw.extend_from_slice(&z[i + 5..i + 5 + len]);
        i += 5 + len;
        if last {
            break;
        }
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in raw.chunks(w * 4 + 1) {
        assert_eq!(row[0], 0, "unfiltered");
        rgba.extend_from_slice(&row[1..]);
    }
    assert_eq!(rgba.len(), w * h * 4);
    (w, h, rgba)
}

/// `tapestry play` has no library: each load measures the chart before its
/// first play, which sounds at the measured gain, as its retry does, and
/// nothing is stored. Two charts alike but for their click's amplitude
/// (the quiet one a quarter of the loud one): every onset's peak, the
/// click's resampling ripple included, scales with amplitude times gain,
/// so the loud over the quiet is four times the loud's gain over the
/// quiet's, which at the headroom would be 1.
#[test]
fn direct_play_measures_at_each_load() {
    let run = |amplitude: i16| {
        let (dir, chart, appdata) = auto_fixture();
        write_click_at(&chart.with_file_name("click.wav"), amplitude);
        let out = auto_run(&chart, &appdata, &["--dev-auto-retry".as_ref()]);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
        let line = |prefix: &str| -> Vec<String> {
            stdout
                .lines()
                .filter(|l| l.starts_with(prefix))
                .map(str::to_owned)
                .collect()
        };
        // Measured once, before the first READY state; the retry reuses it.
        assert_eq!(
            line("loudness: measuring autoplay.bms").len(),
            1,
            "{stdout}"
        );
        assert!(
            stdout.find("loudness: measuring") < stdout.find("play started"),
            "{stdout}"
        );
        let measured = line("loudness: autoplay.bms measured in ");
        assert_eq!(measured.len(), 1, "{stdout}");
        let db: f64 = measured[0]
            .split("; gain ")
            .nth(1)
            .and_then(|r| r.split(" dB").next())
            .unwrap()
            .parse()
            .unwrap();
        // Nothing stored: no library.
        assert!(
            !appdata.join("tapestry").join("library.db").exists(),
            "{stdout}"
        );
        let peaks: Vec<f64> = line("dev: audio onset peaks:")[0]
            .trim_start_matches("dev: audio onset peaks:")
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        // Both plays alike.
        assert_eq!(peaks.len() % 2, 0, "{peaks:?}");
        let (first, retry) = peaks.split_at(peaks.len() / 2);
        assert_eq!(first, retry, "{stdout}");
        (dir, chart, appdata, 10_f64.powf(db / 20.0), peaks)
    };
    let (_loud_dir, loud_chart, loud_appdata, loud_gain, loud) = run(8_000);
    let (_quiet_dir, _, _, quiet_gain, quiet) = run(2_000);
    assert!(loud_gain / quiet_gain < 0.8, "{loud_gain} {quiet_gain}");
    assert_eq!(loud.len(), quiet.len());
    let expected = 4.0 * loud_gain / quiet_gain;
    for (l, q) in loud.iter().zip(&quiet) {
        assert!(
            (l / q / expected - 1.0).abs() < 0.03,
            "{loud:?} {quiet:?} {loud_gain} {quiet_gain}"
        );
    }
    // Below the clipper's knee.
    assert!(loud.iter().all(|&p| p < 0.8), "{loud:?}");
    // The next run measures again.
    let out = auto_run(&loud_chart, &loud_appdata, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert_eq!(stdout.matches("loudness: measuring").count(), 1, "{stdout}");
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
            // No scratch key: no scratch direction either.
            scratch_positive: None,
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
    // The run is on real threads, so which plan lands depends on how fast
    // the game thread got from the opening to the start: the first plan
    // within 200 ms minus the queued output, the fallback within 700 ms;
    // past that the restore is refused as late and the round plays with
    // its background music stopped. Whichever it is, nothing due before
    // the restore point sounds: not the 1.0 s click, nor the schedule from
    // chart time 0.
    let first = stdout.contains("audio: restored at 2000 ms (first)");
    let fallback = stdout.contains("audio: restored at 2500 ms (fallback)");
    let refused = stdout.contains("audio: not restored (Late)");
    assert_eq!(
        u8::from(first) + u8::from(fallback) + u8::from(refused),
        1,
        "{stdout}"
    );
    let onsets = printed_onsets(&stdout);
    // Refused: only the targets' two key sounds, sent from their
    // judgments.
    if refused {
        assert_eq!(onsets.len(), 2, "{onsets:?}");
        return;
    }
    // The restored long voice's click (3.0 s) and the marker (3.5 s) sound
    // exactly 0.5 s apart, both restore points being before 3.0 s; the
    // 2.05 s click only after a restore at the lead-in's start; then the
    // two key sounds.
    let gaps: Vec<i64> = onsets.windows(2).map(|w| w[1] - w[0]).collect();
    let near = |gap: i64, want: i64| (gap - want).abs() <= 1;
    let lead = usize::from(first);
    assert_eq!(onsets.len(), lead + 4, "{onsets:?}");
    if first {
        // 2.05 s to 3.0 s.
        assert!(near(gaps[0], 45_600), "{onsets:?}");
    }
    assert!(near(gaps[lead], 24_000), "{onsets:?}");
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

/// The loading script on the real threads (`--dev-loading`; design.md
/// "Play Adjustments"): START (E) held from the start of loading for
/// 1.5 s, with recording closed, defers an opening asked meanwhile; the
/// loading script goes on feeding through the deferral, so its release
/// opens it, and the play's script takes over on the same devices.
/// None of the loading script's input packets is recorded, and one F2 edit
/// made under the hold (Classic 1.00, step 0.25) is the play's starting
/// tuning: 1.25, `H` 5/4 (design.md "Input Log Format"). A load that
/// outlasts the hold opens without a deferral; either way nothing is
/// cancelled and the play keeps the timeline of the opening first asked
/// for, so every recorded press is the log's, shifted by one amount of
/// zero or more.
/// The exact shift is
/// `a_deferred_opening_keeps_the_scripts_timeline` (src/input_side.rs).
#[test]
fn a_loading_script_press_defers_the_opening_and_its_release_opens_it() {
    const E: u32 = 0x12;
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        events,
        appdata,
        ..
    } = fixture();
    // Classic 1.00 in steps of 0.25: an edit acts before the tempo is
    // attached as after.
    let root = appdata.join("tapestry");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("player.toml"),
        "[hispeed]
mode = \"classic\"
classic_hispeed_x100 = 100
classic_step_x100 = 25
",
    )
    .unwrap();
    // START (0x12) holds the opening while F2 (0x3C) makes one edit.
    let out = scripted_play(&chart_path, &log, &appdata)
        .args(["--dev-loading", "12@0-1500,3c@100-150"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(stdout.contains("play 1:"), "{stdout}");
    let saved = records(&appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    // No cancelled candidate: an opening was deferred, not cancelled.
    assert!(rec.history.openings.is_empty(), "{:?}", rec.history);
    // The edit made while the opening was held is the play's starting
    // tuning (design.md "Input Log Format"), and F2 is recorded nowhere.
    let tuning = rec.start_tuning.expect("a played record's tuning");
    assert_eq!(tuning.classic_hispeed_x100, 125, "{tuning:?}");
    assert_eq!(
        tuning.hispeed,
        Some(replay::HiSpeedRatio { num: 5, den: 4 }),
        "{tuning:?}"
    );
    let f2 = |e: &eval::PhysicalInputEvent| matches!(e.kind, eval::InputKind::Control { control, .. } if control.0 == 0x3C);
    assert!(!rec.events.iter().any(f2) && !rec.history.events.iter().any(f2));
    // The script keeps the timeline of the opening first asked for; a
    // play opened later, at the release, has the log's presses that much
    // earlier on its chart time. A shift large enough to put the first
    // presses before the opening leaves them out of the record, all of
    // them when the play opened past the log's last one.
    assert!(rec.events.len() <= events.len(), "{:?}", rec.events);
    let kept = &events[events.len() - rec.events.len()..];
    if let (Some(e), Some(r)) = (kept.first(), rec.events.first()) {
        let early = e.received.0 - r.received.0;
        assert!(early >= 0, "{early} {stderr}");
        assert!(
            kept.iter()
                .zip(&rec.events)
                .all(|(e, r)| e.kind == r.kind && e.received.0 - r.received.0 == early),
            "{early} {kept:?} {:?}",
            rec.events
        );
    }
    assert!(!rec.events.iter().any(|e| matches!(
        e.kind,
        eval::InputKind::Control { control, .. } if control.0 == E
    )));
}

/// An autoplay session keeps F1 and F2 (design.md "Play Adjustments"): no
/// keyboard reaches the tap, so the window queues them for it; one F2
/// press during the play is exactly one Floating step, 4.00 to 4.50 at
/// 120 BPM and the target green number 300, and nothing is saved.
#[test]
fn autoplay_f2_steps_hi_speed_once() {
    let (_dir, chart, appdata) = auto_fixture();
    let out = auto_run(
        &chart,
        &appdata,
        &[
            "--dev-speed".as_ref(),
            "F2".as_ref(),
            "--dev-adjust-at".as_ref(),
            "1000".as_ref(),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("adjust: hi-speed 4.50 floating target 300"),
        "{stdout}"
    );
    assert!(stdout.contains("autoplay: not saved"), "{stdout}");
}

/// Every observation is stamped on the process clock (design.md "Play
/// Adjustments", clock domains): a scripted play's loading script, on its
/// own timeline from the start of loading, holds SELECT and taps a white
/// key, lowering G to 299 before the chart's tempo is known; the attempt
/// then solves once at 120 BPM, 144000 / (120 · 299) = 4.01, and the
/// play's script, on the timeline of the first opening, holds START
/// before the commit, so READY is cancelled and rearmed. Nothing the
/// reducer takes is out of order.
#[test]
fn presentation_times_follow_the_process_clock_under_a_script() {
    const E: u32 = 0x12;
    let Fixture {
        dir: _dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    let mut rec = replay::decode(&fs::read(&log).unwrap(), &DecodeLimits::default()).unwrap();
    let q = |ms: i64, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(E),
            value,
        },
    };
    let mut events = vec![q(-300, 1), q(200, 0)];
    events.extend(rec.events.iter().map(|e| eval::PhysicalInputEvent {
        received: TimeUs(e.received.0 + 1_200_000),
        ..*e
    }));
    rec.events = events;
    rec.status.finalized = TimeUs(rec.status.finalized.0 + 1_200_000);
    fs::write(&log, replay::encode(&rec)).unwrap();
    let out = scripted_play(&chart_path, &log, &appdata)
        .args(["--dev-loading", "11@0-300,2c@100-150"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("adjust: hi-speed 4.01 floating target 299"),
        "{stdout}"
    );
    assert!(stdout.contains("out-of-order 0"), "{stdout}");
    let saved = records(&appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert_eq!(
        rec.history.openings.len(),
        1,
        "the rearm: {:?}",
        rec.history
    );
}

/// A chart for the outcome mix at 120 BPM (a measure is 2 s): key 1, 2, 3
/// and 4 in measure 1; the scratch and key 5's long note (1/4 to 3/4) in
/// measure 2; key 6 and key 7 in measure 3.
const MIX_CHART: &str = "#PLAYER 1\n#TITLE mix\n#BPM 120\n#WAV01 click.wav\n#LNTYPE 1\n\
                         #00111:01000000\n#00112:00010000\n#00113:00000100\n#00114:00000001\n\
                         #00216:01\n#00255:00010001\n#00318:01\n#00319:00000100\n";

/// The winning capture re-evaluates equal to the live play, on the real
/// threads (design.md "Play Lifecycle", the start gate; B9): a READY
/// cancelled by START first, then the play with every outcome the
/// evaluator has: PERFECT, a GREAT pressed early (counted FAST), a POOR
/// miss, a long note judged at its release (the long notes judge their
/// release, so a separate CN mode adds nothing here), an empty POOR and a
/// scratch hit, with START, SELECT and the auxiliary keys pressed during
/// it, which are recorded, and F2, which is not. The cancelled opening
/// keeps START's press. The rearm is answered at least 30 ms after the
/// release, and each press keeps its offset from its note (design.md
/// "Shortening Launch-Based Checks", scripted input).
#[test]
fn the_winning_capture_reevaluates_equal_to_live() {
    const E: u32 = 0x12;
    const W: u32 = 0x11;
    const UP: u32 = 0xE048;
    const KEY6: u32 = 0x21;
    const KEY7: u32 = 0x2f;
    const KEY2: u32 = 0x1f;
    let fx = fixture_of(MIX_CHART);
    let mut rec = replay::decode(&fs::read(&fx.log).unwrap(), &DecodeLimits::default()).unwrap();
    let key = |ms: i64, control: u32, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(control),
            value,
        },
    };
    let key7 = fx.chart.lanes[7][0].time.0 / 1_000;
    let mut events: Vec<_> = rec
        .events
        .iter()
        .filter(
            |e| !matches!(e.kind, eval::InputKind::Control { control, .. } if control.0 == KEY6),
        )
        .map(|e| match e.kind {
            // Key 2 pressed 40 ms early: a GREAT, counted FAST.
            eval::InputKind::Control { control, value: 1 } if control.0 == KEY2 => {
                eval::PhysicalInputEvent {
                    received: TimeUs(e.received.0 - 40_000),
                    ..*e
                }
            }
            _ => *e,
        })
        .collect();
    // An empty POOR on key 7 400 ms before its note; START, SELECT and the
    // auxiliary key during the play.
    events.extend([
        key(key7 - 400, KEY7, 1),
        key(key7 - 380, KEY7, 0),
        key(2_500, W, 1),
        key(2_520, UP, 1),
        key(2_540, UP, 0),
        key(2_560, W, 0),
        key(3_300, E, 1),
        key(3_320, E, 0),
    ]);
    events.sort_by_key(|e| e.received);
    // START held before chart time 0 cancels the first READY; the play's
    // input comes 1.2 s later.
    let mut shifted = vec![key(-300, E, 1), key(200, E, 0)];
    shifted.extend(events.iter().map(|e| eval::PhysicalInputEvent {
        received: TimeUs(e.received.0 + 1_200_000),
        ..*e
    }));
    rec.events = shifted;
    rec.status.finalized = TimeUs(8_000_000);
    fs::write(&fx.log, replay::encode(&rec)).unwrap();

    let out = scripted_play(&fx.chart_path, &fx.log, &fx.appdata)
        .args(["--dev-speed", "F2", "--dev-adjust-at", "1500"])
        .args(["--dev-rearm-delay", "30"])
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
    assert!(
        stdout.contains("re-evaluation matches live: true"),
        "{stdout}"
    );
    let saved = records(&fx.appdata);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let rec = replay::decode(&fs::read(&saved[0]).unwrap(), &DecodeLimits::default()).unwrap();
    // The cancelled opening holds START's press.
    let h = &rec.history;
    assert_eq!(h.openings.len(), 1, "{h:?}");
    let o = &h.openings[0];
    let cancelled = &h.events[o.events.start as usize..o.events.end as usize];
    let has = |events: &[eval::PhysicalInputEvent], c: u32| {
        events.iter().any(|e| {
            e.kind
                == eval::InputKind::Control {
                    control: eval::ControlId(c),
                    value: 1,
                }
        })
    };
    assert!(has(cancelled, E));
    // The winning opening records START, SELECT and the auxiliary key, and
    // not F2.
    for c in [E, W, UP] {
        assert!(has(&rec.events, c), "{c:#x}");
    }
    assert!(!has(&rec.events, 0x3C) && !has(&h.events, 0x3C));
    // Every outcome, re-evaluated from the record.
    let all = all_events(&fx.chart, &rec);
    let judged: Vec<_> = all
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }))
        .collect();
    let count = |j: Judgment| {
        judged
            .iter()
            .filter(|e| matches!(e.kind, JudgeEventKind::Judged { judgment, .. } if judgment == j))
            .count()
    };
    // Each press as far from its note as the log has it: on time, and key
    // 2 40 ms early.
    let off = judged.iter().any(|e| {
        matches!(e.kind, JudgeEventKind::Judged { deviation: Some(d), .. } if d.0 != 0 && d.0 != -40_000)
    });
    assert!(!off, "{judged:?}");
    assert!(count(Judgment::Perfect) >= 1, "{judged:?}");
    assert_eq!(count(Judgment::Great), 1, "{judged:?}");
    assert_eq!(count(Judgment::Poor), 1, "{judged:?}");
    let fast = judged.iter().any(|e| {
        matches!(e.kind, JudgeEventKind::Judged { judgment: Judgment::Great, deviation: Some(d), .. } if d.0 < 0)
    });
    assert!(fast, "{judged:?}");
    let released = judged.iter().any(|e| {
        matches!(e.kind, JudgeEventKind::Judged { lane, release: Some(_), .. } if lane.index() == 5)
    });
    assert!(released, "{judged:?}");
    let scratch = judged.iter().any(|e| {
        matches!(e.kind, JudgeEventKind::Judged { lane, judgment: Judgment::Perfect, .. } if lane.index() == 0)
    });
    assert!(scratch, "{judged:?}");
    assert!(
        all.iter()
            .any(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. })),
        "{all:?}"
    );
}

/// Every event a record re-evaluates to.
fn all_events(chart: &bms_model::Chart, rec: &PlayRecord) -> Vec<eval::JudgeEvent> {
    eval::evaluate(
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
    .unwrap()
    .events
}

/// A Lua play skin in `skin` whose configured pass reads the chart's
/// judgment class (option 183, easy) and draws `blue.png` over the whole
/// canvas for an easy chart, `red.png` otherwise; `body` runs after the
/// read. `player.toml` under `appdata` chooses it for play.
fn configure_class_skin(skin: &Path, appdata: &Path, body: &str) {
    fs::create_dir_all(skin).unwrap();
    for (name, rgb) in [("blue.png", [0, 0, 255]), ("red.png", [255, 0, 0])] {
        let file = fs::File::create(skin.join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = (0..16 * 16)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect();
        e.write_header().unwrap().write_image_data(&data).unwrap();
    }
    fs::write(
        skin.join("play.luaskin"),
        format!(
            "local skin = {{ type = 0, name = \"class\", w = 1920, h = 1080 }}\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             local path = \"red.png\"\n\
             if ms.option(183) then path = \"blue.png\" end\n\
             {body}\n\
             skin.source = {{ {{ id = \"b\", path = path }} }}\n\
             skin.image = {{ {{ id = \"b\", src = \"b\", x = 0, y = 0, w = 16, h = 16 }} }}\n\
             skin.destination = {{ {{ id = \"b\", dst = {{ {{ x = 0, y = 0, w = 1920, h = 1080 }} }} }} }}\n\
             return skin\n"
        ),
    )
    .unwrap();
    let root = appdata.join("tapestry");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
}

/// The lines of `stdout` starting with `prefix`.
fn starting<'a>(stdout: &'a str, prefix: &str) -> Vec<&'a str> {
    stdout.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// `tapestry play` with a play skin reading the chart's judgment class:
/// startup reports it pending, the attempt builds it for the chart, whose
/// play draws the class's colour (blue for `#RANK 3`, red for `#RANK 2`),
/// and the play is recorded. Inside the child, the job's thread has ended
/// and no VM is open before the render thread prepares the attempt, and
/// from recording's open to the play's end no job, Lua pass or
/// instruction step runs and no VM is open; the counters at the open
/// (startup's job and two passes, the specialization's job and pass)
/// show they move.
#[test]
fn a_pending_play_skin_is_built_for_the_direct_chart() {
    for (rank, rgb) in [("3", [0, 0, 255]), ("2", [255, 0, 0])] {
        let Fixture {
            dir,
            chart_path,
            log,
            appdata,
            ..
        } = fixture_of(&format!("{CHART}#RANK {rank}\n"));
        configure_class_skin(&dir.path().join("skin"), &appdata, "");
        let frames = dir.path().join("frames");
        let out = scripted_play(&chart_path, &log, &appdata)
            .arg("--dev-frames")
            .arg(&frames)
            .output()
            .expect("run tapestry");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{stdout}{stderr}");
        let order = [
            "skin: play: pending: reads the chart's judgment class as it is built; \
             built for each chart before its play",
            "skin: play: specialized",
            "dev: specialization ended: live 0 joined 2 of 2",
            "play started",
        ];
        let at: Vec<usize> = order
            .iter()
            .map(|l| stdout.find(l).unwrap_or_else(|| panic!("{l}: {stdout}")))
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{stdout}");
        let open = starting(&stdout, "dev: lua at open: ");
        let end = starting(&stdout, "dev: lua at play end: ");
        assert_eq!(open.len(), 1, "{stdout}");
        assert!(
            open[0].starts_with("dev: lua at open: live 0 jobs 2 passes 3 steps "),
            "{stdout}"
        );
        assert_eq!(
            &open[0]["dev: lua at open: ".len()..],
            end.first()
                .map_or("", |l| &l["dev: lua at play end: ".len()..]),
            "{stdout}"
        );
        assert!(stdout.contains("play 1:"), "{stdout}");
        assert_eq!(records(&appdata).len(), 1);
        let (w, h, rgba) = png_pixels(&frames.join("play.png"));
        let i = ((h / 2) * w + w / 2) * 4;
        assert_eq!(&rgba[i..i + 3], rgb, "#RANK {rank}: {stdout}");
    }
}

/// `tapestry play` with a pending play skin that cannot be built for the
/// chart (its configured pass loads a missing module) fails with exit 1
/// and the reason, after the pending line, and records nothing; the skin
/// in use never stands in.
#[test]
fn a_refused_specialization_fails_the_direct_path() {
    let Fixture {
        dir,
        chart_path,
        log,
        appdata,
        ..
    } = fixture();
    configure_class_skin(
        &dir.path().join("skin"),
        &appdata,
        "local m = require(\"missing\")",
    );
    let out = scripted_play(&chart_path, &log, &appdata)
        .output()
        .expect("run tapestry");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(
        stdout.contains("skin: play: pending: reads the chart's judgment class"),
        "{stdout}"
    );
    let refused = starting(&stdout, "skin: play: refused for the chart: ");
    assert_eq!(refused.len(), 1, "{stdout}");
    assert!(refused[0].contains("missing"), "{stdout}");
    assert!(
        stderr.contains("the play skin cannot be built for this chart: "),
        "{stderr}"
    );
    assert!(!stdout.contains("play started"), "{stdout}");
    assert!(!appdata.join("tapestry").join("records").exists());
}
