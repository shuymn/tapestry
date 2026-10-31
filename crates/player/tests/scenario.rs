//! Scenario launch on the game's real threads (dev-tools): `tapestry
//! --dev-scenario <file>` over a library, input logs and sounds made here,
//! headless with a recording audio output and WARP offscreen.

#![cfg(feature = "dev-tools")]

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bms_model::TimeUs;
use eval::{InputStart, PhysicalInputEvent};
use game::DeviceSettings;
use input::DeviceKind;
use replay::{
    BuildProvenance, ChartRecord, DecodeLimits, DeviceRecord, EndStatus, PlayRecord, PlayType,
    PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};

/// The exit scenario, as committed.
const EXIT: &str = include_str!("scenarios/exit.toml");

/// A chart's text: 120 BPM, `#WAV01` its folder's `key.wav`, four notes on
/// lane 1 at 2.0, 2.5, 3.0 and 3.5 s.
fn chart_text(title: &str) -> String {
    format!(
        "#PLAYER 1\n#TITLE {title}\n#ARTIST テスト\n#PLAYLEVEL 3\n#BPM 120\n\
         #WAV01 key.wav\n#00111:01010101\n"
    )
}

/// A 50 ms mono 16-bit square wave at 44.1 kHz.
fn write_wav(path: &Path) {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { 8_000 } else { -8_000 })
        .collect();
    let data = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
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

/// Chart `name` titled `title` in its own folder of `library`.
fn write_chart(library: &Path, name: &str, title: &str) -> bms_model::Chart {
    let dir = library.join(name);
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"));
    let text = chart_text(title);
    fs::write(dir.join(format!("{name}.bms")), &text).unwrap();
    bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart
}

/// The refused chart `bad/bad.bms`: the loader does not read `#LNTYPE 2`.
fn write_refused(library: &Path) {
    let dir = library.join("bad");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("bad.bms"),
        "#TITLE 壊れた譜面\n#LNTYPE 2\n#00111:01\n",
    )
    .unwrap();
}

/// The events that hit `chart`'s notes on time from one keyboard with the
/// default key config, kept where `keep` says by press time.
fn hits(chart: &bms_model::Chart, keep: impl Fn(TimeUs) -> bool) -> Vec<PhysicalInputEvent> {
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let all = input::autoplay(chart, &profile.sources[0]).unwrap();
    // Each note is a press and its release.
    all.chunks(2)
        .filter(|pair| keep(pair[0].received))
        .flatten()
        .copied()
        .collect()
}

/// An input log of `events` from `sources` keyboards, each with the
/// default key config.
fn write_log(path: &Path, events: &[PhysicalInputEvent], sources: usize) {
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let source = SourceRecord {
        kind: replay::DeviceKind::Keyboard,
        receive: ReceiveMethod::Scripted,
        profile: profile.sources[0].clone(),
        axis_out_of_range: 0,
    };
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
            sources: vec![source; sources],
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
        events: events.to_vec(),
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized: TimeUs(4_000_000),
            incomplete: replay::Incomplete::default(),
        },
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, replay::encode(&rec)).unwrap();
}

/// A folder for a scenario file and what it names, and a data root.
struct Fixture {
    _dir: tempfile::TempDir,
    /// The scenario's folder.
    base: PathBuf,
    /// The data root's parent.
    appdata: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("scenario");
        fs::create_dir_all(base.join("library")).unwrap();
        let appdata = dir.path().join("appdata");
        Self {
            _dir: dir,
            base,
            appdata,
        }
    }

    fn library(&self) -> PathBuf {
        self.base.join("library")
    }

    fn data(&self) -> PathBuf {
        self.appdata.join("tapestry")
    }

    /// `tapestry --dev-scenario <file>` with `text` as the file, and a
    /// render bound that only catches a render thread that never answers.
    fn command(&self, text: &str) -> Command {
        let file = self.base.join("scenario.toml");
        fs::write(&file, text).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapestry"));
        command
            .arg("--dev-scenario")
            .arg(&file)
            .args(["--dev-render-timeout", "600"])
            .env("LOCALAPPDATA", &self.appdata);
        command
    }

    fn run(&self, text: &str) -> (Output, String, String) {
        let out = self.command(text).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (out, stdout, stderr)
    }

    /// Every saved record, oldest first.
    fn records(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(charts) = fs::read_dir(self.data().join("records")) else {
            return found;
        };
        for chart in charts {
            for f in fs::read_dir(chart.unwrap().path()).unwrap() {
                let path = f.unwrap().path();
                if path.extension().is_some_and(|e| e == "tprl") {
                    found.push(path);
                }
            }
        }
        found.sort_by_key(|p| fs::metadata(p).unwrap().modified().unwrap());
        found
    }
}

/// The scenario of `steps`, over `library`, with `out` as its output.
fn scenario(steps: &[&str]) -> String {
    let mut text = "version = 1\noutput = \"out\"\n[library]\nroots = [\"library\"]\n".to_owned();
    for s in steps {
        text.push_str("[[step]]\n");
        text.push_str(s);
        text.push('\n');
    }
    text
}

/// The lines of `stdout` that start with `prefix`.
fn lines<'a>(stdout: &'a str, prefix: &str) -> Vec<&'a str> {
    stdout.lines().filter(|l| l.starts_with(prefix)).collect()
}

fn int(t: &toml::Table, table: &str, key: &str) -> Option<i64> {
    t.get(table)?.get(key)?.as_integer()
}

/// The exit scenario: chart A by autoplay and its result under each
/// preset, A again with an input log played to the end, retried from the
/// result, retried and left during the play, then B found by search; the
/// steps run in scene order, the log's plays are saved, re-evaluate equal
/// and are counted into the bests, and the capture is the log's first
/// play's frame.
#[test]
fn exit_scenario_plays_select_result_retry_repeatedly() {
    let fx = Fixture::new();
    let a = write_chart(&fx.library(), "a", "譜面A");
    write_chart(&fx.library(), "b", "譜面B");
    write_refused(&fx.library());
    // Notes 1 and 3 hit, 2 and 4 missed.
    let first = |t: TimeUs| (2_000_000..2_250_000).contains(&t.0);
    let third = |t: TimeUs| (3_000_000..3_250_000).contains(&t.0);
    let log = hits(&a, |t| first(t) || third(t));
    assert_eq!(log.len(), 4);
    assert!(log.iter().all(|e| first(e.received) || third(e.received)));
    write_log(&fx.base.join("logs").join("a.tprl"), &log, 1);
    assert!(!fx.data().join("player.toml").exists());

    let (out, stdout, stderr) = fx.run(EXIT);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");

    // The scene order: each step as it starts, among the choices, the
    // plays' starts and their reports.
    let scene: Vec<&str> = stdout
        .lines()
        .filter(|l| {
            (l.starts_with("dev: scenario: step ") && !l.contains("): wrote "))
                || l.starts_with("select: chose ")
                || *l == "play started"
                || l.starts_with("play ") && l.as_bytes().get(5).is_some_and(u8::is_ascii_digit)
        })
        .collect();
    let expected = [
        "dev: scenario: step 1 (select)",
        "select: chose a.bms",
        "dev: scenario: step 2 (play)",
        "play started",
        "play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0",
        "dev: scenario: step 3 (result)",
        "dev: scenario: step 4 (result)",
        "dev: scenario: step 5 (result)",
        "dev: scenario: step 6 (result)",
        "dev: scenario: step 7 (capture)",
        "dev: scenario: step 8 (select)",
        "select: chose a.bms",
        "dev: scenario: step 9 (play)",
        "play started",
        "play 1: Completed; perfect 2 great 0 good 0 bad 0 poor 2",
        "dev: scenario: step 10 (result)",
        "dev: scenario: step 11 (play)",
        "play started",
        "play 2: Aborted(Retry); perfect 1 great 0 good 0 bad 0 poor 0",
        "dev: scenario: step 12 (play)",
        "play started",
        "play 3: Aborted(Quit); perfect 2 great 0 good 0 bad 0 poor 1",
        "dev: scenario: step 13 (select)",
        "select: chose b.bms",
        "dev: scenario: step 14 (play)",
        "play started",
        "play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0",
        "dev: scenario: step 15 (result)",
        "dev: scenario: step 16 (quit)",
    ];
    assert_eq!(
        scene.len(),
        expected.len(),
        "{scene:#?}\n{stdout}\n{stderr}"
    );
    for (line, start) in scene.iter().zip(expected) {
        assert!(line.starts_with(start), "{line} is not {start}:\n{stdout}");
    }
    for play in lines(&stdout, "play ")
        .iter()
        .filter(|l| l.contains("; perfect "))
    {
        assert!(play.ends_with("re-evaluation matches live: true"), "{play}");
    }
    // The default preset, then the what-ifs, labelled and evaluated.
    let viewing = lines(&stdout, "result: viewing ");
    assert!(
        viewing[0].starts_with("result: viewing beatoraja-style (as played)"),
        "{stdout}"
    );
    for (line, name) in viewing[1..3].iter().zip(["tapestry", "lr2-style"]) {
        assert!(
            line.starts_with(&format!("result: viewing {name} (what-if, ")),
            "{line}"
        );
    }
    let evaluated = lines(&stdout, "result: evaluated ");
    assert!(evaluated.len() >= 2, "{stdout}");

    // The log's three plays, none of autoplay's: each replays the log from
    // its first event up to its cut, and its figures were measured with
    // no allocation in the render thread's app region.
    let records = fx.records();
    assert_eq!(records.len(), 3, "{records:?}");
    let decoded: Vec<PlayRecord> = records
        .iter()
        .map(|r| replay::decode(&fs::read(r).unwrap(), &DecodeLimits::default()).unwrap())
        .collect();
    let cut = |at: i64| -> Vec<PhysicalInputEvent> {
        log.iter().filter(|e| e.received.0 < at).copied().collect()
    };
    let expected = [
        (EndStatus::PlayedToEnd, cut(i64::MAX)),
        (EndStatus::EndedMidway, cut(2_250_000)),
        (EndStatus::EndedMidway, cut(3_250_000)),
    ];
    for ((rec, path), (status, events)) in decoded.iter().zip(&records).zip(expected) {
        assert_eq!(rec.status.end, status, "{}", path.display());
        assert_eq!(rec.events, events, "{}", path.display());
        assert_eq!(rec.devices.sources[0].receive, ReceiveMethod::Scripted);
        let m: toml::Table = fs::read_to_string(path.with_extension("measure.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(int(&m, "render_cpu_us", "count").unwrap() > 0, "{m:?}");
        assert_eq!(int(&m, "allocations", "render_app"), Some(0), "{m:?}");
    }

    // The bests: three plays of A counted live, the best set by the
    // completed one; B's autoplay counts nothing.
    assert_eq!(
        lines(&stdout, "bests: the play was counted").len(),
        3,
        "{stdout}"
    );
    let (library, _) = library::Library::open(&fx.data().join("library.db")).unwrap();
    let scores = library
        .bests(&decoded[0].preset.sha256, library::LN_MODE)
        .unwrap();
    assert_eq!(scores.len(), 1, "{scores:?}");
    let score = scores.values().next().unwrap();
    assert_eq!(score.plays, 3);
    let best = score.best.as_ref().unwrap();
    assert_eq!((best.ex, best.bp, best.full_combo), (4, 2, false));

    // The capture: a PNG of the first play after its step, the log's.
    let png = fs::read(fx.base.join("out").join("a-log.png")).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let all: Vec<&str> = stdout.lines().collect();
    let at = |pred: &dyn Fn(&str) -> bool, from: usize| {
        all.iter()
            .skip(from)
            .position(|l| pred(l))
            .map(|i| i + from)
    };
    let step9 = at(&|l| l == "dev: scenario: step 9 (play)", 0).unwrap();
    let wrote = at(
        &|l| l.starts_with("dev: scenario: step 7 (capture): wrote "),
        0,
    )
    .unwrap();
    let reported = at(&|l| l.starts_with("play 1: "), step9).unwrap();
    assert!(step9 < wrote && wrote < reported, "{stdout}");
}

/// A play whose chart was refused fails the run at that step: exit 1,
/// naming it and the choice, and no step after it runs.
#[test]
fn unrunnable_step_exits_one() {
    let fx = Fixture::new();
    write_refused(&fx.library());
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/bad/bad.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("step 2 (play): the chart chosen at step 1 was refused"),
        "{stderr}"
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
    assert!(
        lines(&stdout, "dev: scenario: step 3").is_empty(),
        "{stdout}"
    );
}

/// A file of another version is refused for its version, whatever else it
/// holds, before the library opens; one without a version too.
#[test]
fn other_scenario_version_is_refused() {
    let fx = Fixture::new();
    let (out, _, stderr) = fx.run("version = 2\nwhat_v2_adds = true\n");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("version 2 is not supported; this build reads version 1"),
        "{stderr}"
    );
    assert!(!stderr.contains("what_v2_adds"), "{stderr}");
    assert!(!fx.data().join("library.db").exists());
    let (out, _, stderr) = fx.run("[library]\nroots = [\"library\"]\n");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("no version; this build reads version 1"),
        "{stderr}"
    );
    assert!(!fx.data().join("library.db").exists());
}

/// A capture whose file is a folder when its frame comes fails the run,
/// naming the step and the file, and leaves the folder as it was.
#[test]
fn a_capture_onto_a_directory_exits_one() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let mut child = fx
        .command(&scenario(&[
            "capture = { at = 0, file = \"a.png\" }",
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The run makes its output folder; once the chart is chosen, the
    // capture's file is taken by a folder before its frame comes.
    let target = fx.base.join("out").join("a.png");
    let mut stdout = String::new();
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if line == "dev: scenario: step 2 (select)" {
            fs::create_dir(&target).unwrap();
        }
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("step 1 (capture): "), "{stderr}");
    assert!(stderr.contains("a.png: "), "{stderr}");
    assert!(target.is_dir());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}

/// An output folder that is a link (a junction, which needs no privilege)
/// is refused before anything opens.
#[test]
fn a_linked_output_is_refused() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let elsewhere = fx.base.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let made = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(fx.base.join("out"))
        .arg(&elsewhere)
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { at = 0, file = \"a.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("is a link"), "{stderr}");
    assert!(lines(&stdout, "dev: scenario: step").is_empty(), "{stdout}");
    assert!(!fx.data().join("library.db").exists());
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// A log of as many keyboards as the input thread keeps devices for, then
/// another chart by autoplay: the input thread starts over for it, so its
/// generated keyboard plays every note.
#[test]
fn autoplay_after_a_full_log_chart_plays() {
    let fx = Fixture::new();
    let a = write_chart(&fx.library(), "a", "譜面A");
    write_chart(&fx.library(), "b", "譜面B");
    write_log(
        &fx.base.join("logs").join("a.tprl"),
        &hits(&a, |_| true),
        16,
    );
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/a.tprl\", until = \"end\" }",
        "result = \"back\"",
        "select = { path = \"library/b/b.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let plays = lines(&stdout, "play 1: ");
    assert_eq!(plays.len(), 2, "{stdout}");
    assert!(
        plays[1].starts_with("play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0"),
        "{stdout}"
    );
}
