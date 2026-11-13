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
         #WAV01 key.wav\n#00138:01\n#00111:01010101\n"
    )
}

/// A 50 ms mono 16-bit square wave at 44.1 kHz.
fn write_wav(path: &Path) {
    write_wav_at(path, 8_000);
}

/// [`write_wav`] of `amplitude`.
fn write_wav_at(path: &Path, amplitude: i16) {
    let samples: Vec<i16> = (0..2_205)
        .map(|i| if i % 50 < 25 { amplitude } else { -amplitude })
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
        "#TITLE 壊れた譜面\n#LNTYPE 2\n#00138:01\n#00111:01\n",
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
        history: Default::default(),
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
        self.output(&mut self.command(text))
    }

    /// [`run`](Self::run) with the result's injected refusals `faults`
    /// (`TAPESTRY_DEV_RESULT_FAULTS`).
    fn run_with(&self, text: &str, faults: &str) -> (Output, String, String) {
        self.output(self.command(text).env("TAPESTRY_DEV_RESULT_FAULTS", faults))
    }

    /// [`run`](Self::run), stopped at `limit`: `None` when the run did
    /// not end by then, the child then killed and waited for.
    fn run_bounded(
        &self,
        text: &str,
        limit: std::time::Duration,
    ) -> Option<(Output, String, String)> {
        let mut child = self
            .command(text)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let begun = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if begun.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        Some((out, stdout, stderr))
    }

    fn output(&self, command: &mut Command) -> (Output, String, String) {
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        (out, stdout, stderr)
    }

    /// The saved records of a played attempt, oldest first, and how many
    /// captures were cancelled before playback (a quit in READY saves
    /// one).
    fn played_records(&self) -> (Vec<PathBuf>, usize) {
        let all = self.records();
        let (played, cancelled): (Vec<PathBuf>, Vec<PathBuf>) = all.into_iter().partition(|p| {
            replay::decode(&fs::read(p).unwrap(), &DecodeLimits::default())
                .unwrap()
                .played()
        });
        (played, cancelled.len())
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
    let mut text = "version = 8\noutput = \"out\"\n[library]\nroots = [\"library\"]\n".to_owned();
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
/// result, retried during the play, quit during the play to its result and
/// left there, then B found by search, then one practice round of A at
/// 75 % with the log, left before the next round opens; the steps run in
/// scene order, the log's plays are saved, re-evaluate equal and are
/// counted into the bests, the round is saved as a section repeat at 75 %
/// played to the end, and the capture is the log's first play's frame.
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
        // Where a cut lands depends on the machine's timing: what the
        // aborted plays judged is not pinned here (`until_fires_once_at_its_time`
        // pins the time a cut is sent).
        "play 2: Aborted(Retry); ",
        "dev: scenario: step 12 (play)",
        "play started",
        "play 3: Aborted(Quit); ",
        // The quit while the chart plays closes it, then shows its result.
        "dev: scenario: step 13 (result)",
        "dev: scenario: step 14 (select)",
        "select: chose b.bms",
        "dev: scenario: step 15 (play)",
        "play started",
        "play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0",
        "dev: scenario: step 16 (result)",
        "dev: scenario: step 17 (practice)",
        "select: chose a.bms to practice ",
        "play started",
        // The log's presses fall on 1× times: at 75 % they miss.
        "play 1: Completed; ",
        // The rounds are left before the next opens, back to the page.
        "dev: scenario: step 18 (quit)",
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

    // The log's three plays, none of autoplay's: the completed one holds
    // the whole log, and each aborted one the log replayed from its first
    // event up to where it was cut; the figures of each were measured
    // with no allocation in the render thread's app region.
    let (records, cancelled) = fx.played_records();
    assert_eq!(records.len(), 4, "{records:?}");
    // The rounds are left before the next opens: no capture is cancelled.
    assert_eq!(cancelled, 0);
    let decoded: Vec<PlayRecord> = records
        .iter()
        .map(|r| replay::decode(&fs::read(r).unwrap(), &DecodeLimits::default()).unwrap())
        .collect();
    let statuses = [
        EndStatus::PlayedToEnd,
        EndStatus::EndedMidway,
        EndStatus::EndedMidway,
    ];
    assert_eq!(decoded[0].events, log);
    // Each starts with the tuning its opening took from the adjustment
    // controls: a solved multiplier, which only the reducer has.
    for rec in &decoded {
        let tuning = rec.start_tuning.expect("a played record's tuning");
        assert!(tuning.hispeed.is_some(), "{tuning:?}");
    }
    for ((rec, path), status) in decoded.iter().zip(&records).zip(statuses) {
        assert_eq!(rec.status.end, status, "{}", path.display());
        assert!(
            !rec.events.is_empty() && log.starts_with(&rec.events),
            "{}: {:?}",
            path.display(),
            rec.events
        );
    }
    // The practice round: a section repeat of the whole chart at 75 %,
    // played to the end, holding the log replayed from its start.
    let round = &decoded[3];
    let replay::PlayType::SectionRepeat(practice) = &round.play_type else {
        panic!("{:?}", round.play_type);
    };
    assert_eq!(practice.attempt, 1);
    assert_eq!(practice.lead_in, TimeUs(2_000_000));
    let twenty = rules::GaugeValue(20 * rules::GaugeValue::PERCENT);
    assert_eq!(practice.initial_gauges[0].value, twenty);
    assert_eq!(round.speed, bms_model::Speed::from_twentieths(15).unwrap());
    assert_eq!(round.status.end, EndStatus::PlayedToEnd);
    assert_eq!(round.events, log);
    for (rec, path) in decoded.iter().zip(&records) {
        assert_eq!(rec.devices.sources[0].receive, ReceiveMethod::Scripted);
        let m: toml::Table = fs::read_to_string(path.with_extension("measure.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(int(&m, "render_cpu_us", "count").unwrap() > 0, "{m:?}");
        assert_eq!(int(&m, "allocations", "render_app"), Some(0), "{m:?}");
    }

    // The bests: four plays of A counted live, the best set by the
    // completed full play (a round at 75 % sets none); B's autoplay counts
    // nothing.
    assert_eq!(
        lines(&stdout, "bests: the play was counted").len(),
        4,
        "{stdout}"
    );
    let (library, _) = library::Library::open(&fx.data().join("library.db")).unwrap();
    let scores = library
        .bests(&decoded[0].preset.sha256, library::LN_MODE)
        .unwrap();
    assert_eq!(scores.len(), 1, "{scores:?}");
    let score = scores.values().next().unwrap();
    assert_eq!(score.plays, 4);
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

/// Chart `tail/tail.bms`: [`chart_text`]'s four notes, and a 4 s BGM sound
/// from 2.0 s, so its audio tail sounds from completion (about 3.7 s) to
/// 6.0 s.
fn write_tail_chart(library: &Path) {
    let dir = library.join("tail");
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"));
    let samples: Vec<i16> = (0..4 * 44_100)
        .map(|i| if i % 100 < 50 { 2_000 } else { -2_000 })
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
    fs::write(dir.join("bgm.wav"), b).unwrap();
    let text = format!("{}#WAV02 bgm.wav\n#00101:02\n", chart_text("余韻"));
    fs::write(dir.join("tail.bms"), text).unwrap();
}

/// The quit key timed after the play completes, its BGM still sounding
/// (`command = "skip"`), skips the tail to the play's result, which the
/// next step leaves; the play is complete and saved as played to the end.
#[test]
fn a_skip_after_completion_lands_on_the_result() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"auto\", until = { at_ms = 4500, command = \"skip\" } }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let plays = lines(&stdout, "play 1: ");
    assert_eq!(plays.len(), 1, "{stdout}");
    assert!(plays[0].starts_with("play 1: Completed"), "{stdout}");
    assert!(!lines(&stdout, "result: viewing ").is_empty(), "{stdout}");
}

/// The quit key timed while the chart plays (`command = "quit"`) ends the
/// play midway on its result, which the next step leaves for selection.
#[test]
fn a_quit_while_playing_lands_on_the_result() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"auto\", until = { at_ms = 2250, command = \"quit\" } }",
        "result = \"back\"",
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let plays = lines(&stdout, "play 1: ");
    assert_eq!(plays.len(), 2, "{stdout}");
    assert!(plays[0].starts_with("play 1: Aborted(Quit)"), "{stdout}");
    assert!(plays[1].starts_with("play 1: Completed"), "{stdout}");
    let at = |prefix: &str| stdout.lines().position(|l| l.starts_with(prefix));
    let viewing = at("result: viewing ").unwrap();
    assert!(at("play 1: Aborted(Quit)").unwrap() < viewing, "{stdout}");
    assert!(
        viewing < at("dev: scenario: step 4 (select)").unwrap(),
        "{stdout}"
    );
}

/// A `quit` timed after the play completes lands on its result, as a skip
/// does, but the play is not ended midway: the run fails at that step,
/// naming `skip`.
#[test]
fn a_quit_after_completion_fails_naming_skip() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"auto\", until = { at_ms = 4500, command = \"quit\" } }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "step 2 (play): the quit came after the play completed, which skips to its result"
        ),
        "{stderr}"
    );
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
    // The run's error leads; what else went wrong follows it.
    let at = |prefix: &str| stderr.lines().position(|l| l.starts_with(prefix));
    let primary = at("dev: scenario: step 2 (play): the chart chosen at step 1").unwrap();
    let also = at("dev: scenario: also: step 3 (quit) did not run").unwrap();
    assert!(primary < also, "{stderr}");
}

/// A render thread stuck writing a capture (stalled by the test hook) ends
/// the run with 1 within the render bound: the play's end is not
/// answered, the capture fails, and the thread is left to the process's
/// exit.
#[test]
fn a_stalled_capture_exits_one_within_the_bound() {
    use std::time::{Duration, Instant};
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let file = fx.base.join("scenario.toml");
    fs::write(
        &file,
        scenario(&[
            "capture = { at = 0, file = \"a.png\" }",
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]),
    )
    .unwrap();
    let begun = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("--dev-scenario")
        .arg(&file)
        .args(["--dev-render-timeout", "20"])
        .env("LOCALAPPDATA", &fx.appdata)
        .env("TAPESTRY_DEV_CAPTURE_STALL_MS", "600000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Far below the stall: the run must not wait for it.
    let bound = Duration::from_secs(300);
    while child.try_wait().unwrap().is_none() {
        if begun.elapsed() > bound {
            let _ = child.kill();
            panic!("the run did not end within {} s", bound.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("step 1 (capture): the render thread did not answer"),
        "{stderr}"
    );
    assert!(
        stderr.contains("the render thread did not end cleanly"),
        "{stderr}"
    );
    assert!(!fx.base.join("out").join("a.png").exists());
}

/// A file of another version is refused for its version, whatever else it
/// holds, before the library opens; one without a version too.
#[test]
fn other_scenario_version_is_refused() {
    let fx = Fixture::new();
    let (out, _, stderr) = fx.run("version = 9\nwhat_v9_adds = true\n");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("version 9 is not supported; this build reads version 8"),
        "{stderr}"
    );
    assert!(!stderr.contains("what_v9_adds"), "{stderr}");
    assert!(!fx.data().join("library.db").exists());
    // The previous version too: its files are refused, never read.
    let old = scenario(&["quit = {}"]).replacen("version = 8", "version = 7", 1);
    let (out, _, stderr) = fx.run(&old);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("version 7 is not supported; this build reads version 8"),
        "{stderr}"
    );
    assert!(!fx.data().join("library.db").exists());
    let (out, _, stderr) = fx.run("[library]\nroots = [\"library\"]\n");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("no version; this build reads version 8"),
        "{stderr}"
    );
    assert!(!fx.data().join("library.db").exists());
}

/// A scenario's `size` sets the frames' size, and its captures keep their
/// axes: a loading frame, a READY frame by time since READY, two chart
/// times on one play, each with what its frame was beside it. A later
/// chart time sits on the play's own axis, past READY.
#[test]
fn a_sized_scenario_captures_on_each_axis() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let text = scenario(&[
        "capture = { loading = true, file = \"load.png\" }",
        "capture = { ready_at = 200, file = \"ready.png\" }",
        "capture = { at = 200, file = \"start.png\" }",
        "capture = { at = 2500, file = \"note.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ])
    .replacen(
        "output = \"out\"\n",
        "output = \"out\"\nsize = [640, 360]\n",
        1,
    );
    let (out, stdout, stderr) = fx.run(&text);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let dir = fx.base.join("out");
    for (name, axis, phase) in [
        ("load", "loading", "loading"),
        ("ready", "ready", "ready"),
        ("start", "chart", "play"),
        ("note", "chart", "play"),
    ] {
        let png = fs::read(dir.join(format!("{name}.png"))).unwrap();
        assert_eq!(&png[..4], b"\x89PNG", "{name}");
        // 8-bit RGBA at the file's size.
        assert_eq!(
            (
                u32::from_be_bytes(png[16..20].try_into().unwrap()),
                u32::from_be_bytes(png[20..24].try_into().unwrap()),
                png[24],
                png[25]
            ),
            (640, 360, 8, 6),
            "{name}"
        );
        let side = fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
        assert_eq!(side_value(&side, "axis"), axis, "{side}");
        assert_eq!(side_value(&side, "phase"), phase, "{side}");
    }
    let side = |name: &str| fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
    assert!(side_num(&side("ready"), "ready_us") >= 200_000);
    assert!(side_num(&side("ready"), "chart_us") < 0);
    assert!(side_num(&side("start"), "chart_us") >= 0);
    let note = side("note");
    assert!(side_num(&note, "chart_us") >= 2_500_000);
    // Its view's own: the evaluator had passed a note by then, and judged
    // the notes at 2.0 s and later.
    let finalized = side_num(&note, "finalized_us");
    assert!(
        (1..=side_num(&note, "chart_us")).contains(&finalized),
        "{note}"
    );
    assert!(side_num(&note, "events") >= 1, "{note}");
    assert_ne!(side_num(&side("start"), "frame"), side_num(&note, "frame"));
}

/// `play --dev-headless --dev-size` writes its frames at that size.
#[test]
fn a_headless_play_takes_a_size() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let frames = fx.base.join("frames");
    fs::create_dir_all(&frames).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(fx.library().join("a").join("a.bms"))
        .args(["--auto", "--dev-headless", "--dev-size", "320x180"])
        .arg("--dev-frames")
        .arg(&frames)
        .args(["--dev-render-timeout", "600"])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut seen = 0;
    for entry in fs::read_dir(&frames).unwrap() {
        let png = fs::read(entry.unwrap().path()).unwrap();
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 320);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 180);
        seen += 1;
    }
    assert!(seen > 0);
}

/// `key` of a sidecar's JSON, as written.
fn side_value(side: &str, key: &str) -> String {
    let rest = side
        .split(&format!("\"{key}\":"))
        .nth(1)
        .unwrap_or_else(|| panic!("no {key}: {side}"));
    rest.split([',', '}'])
        .next()
        .unwrap()
        .trim_matches('"')
        .to_owned()
}

/// A number of a sidecar's JSON.
fn side_num(side: &str, key: &str) -> i64 {
    side_value(side, key)
        .parse()
        .unwrap_or_else(|_| panic!("{key} in {side}"))
}

/// A READY capture in a practice round runs on that round's READY: its
/// lead-in of other than a second and its start at any chart time, at
/// chart time past 0 too, its elapsed time counted from its own origin.
#[test]
fn a_ready_capture_follows_a_practices_lead_in_and_start() {
    for (from, start) in [(1_i64, 0_i64), (2, 2_000_000)] {
        let fx = Fixture::new();
        write_chart(&fx.library(), "a", "譜面A");
        let key7 = bms_parser::load_bms(
            b"#BPM 120\n#00119:0000000000010000\n",
            &fx.base,
            &Default::default(),
        )
        .unwrap()
        .chart;
        write_log(
            &fx.base.join("logs").join("k7.tprl"),
            &hits(&key7, |_| true),
            1,
        );
        let practice = format!(
            "practice = {{ path = \"library/a/a.bms\", from = {from}, to = 2, speed = 100, \
             lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/k7.tprl\", rounds = 1 }}"
        );
        let (out, stdout, stderr) = fx.run(&scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "capture = { ready_at = 1500, file = \"r.png\" }",
            &practice,
            "quit = {}",
        ]));
        assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
        let side = fs::read_to_string(fx.base.join("out").join("r.json")).unwrap();
        assert_eq!(side_value(&side, "phase"), "ready", "{side}");
        // A round's READY begins 200 ms before its lead-in (design.md
        // "Mid-Chart Start and Section Repeat").
        let origin = start - 2_000_000 - 200_000;
        assert_eq!(side_num(&side, "ready_origin_us"), origin, "{side}");
        assert_eq!(side_num(&side, "play_at_us"), start, "{side}");
        let elapsed = side_num(&side, "ready_us");
        assert!((1_500_000..2_200_000).contains(&elapsed), "{side}");
        assert_eq!(elapsed, side_num(&side, "chart_us") - origin, "{side}");
    }
}

/// A frame of the audio tail says so, its chart time past the end of
/// evaluation; `completed_us` names when the frame's drawing ended.
#[test]
fn a_tail_frames_sidecar_says_tail() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { at = 1000, file = \"play.png\" }",
        "capture = { at = 4500, file = \"tail.png\" }",
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let read = |n: &str| fs::read_to_string(fx.base.join("out").join(n)).unwrap();
    assert_eq!(side_value(&read("play.json"), "phase"), "play");
    let tail = read("tail.json");
    assert_eq!(side_value(&tail, "phase"), "tail", "{tail}");
    assert!(side_num(&tail, "completed_us") > 0, "{tail}");
}

/// A frame of Closing says so: a log that presses nothing fails the HAZARD
/// gauge at its first miss, and the first frame drawn from a view with
/// Closing's start names that phase. No frame of Closing is guaranteed: a
/// render thread starved through all of it draws none, and the capture
/// then fails the run, which still reports the play's Closing. Which
/// chart time first shows Closing is the real threads' timing; the phase
/// for each view is fixed in `a_phase_is_one_decision_over_its_inputs`
/// (src/dev/scenario.rs).
#[test]
fn a_closing_frames_sidecar_says_closing() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    write_log(&fx.base.join("logs").join("none.tprl"), &[], 1);
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        "displayed_gauge = 5\non_failure = \"close\"\n",
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { closing_at = 0, file = \"c.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/none.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(lines(&stdout, "closing: ").len(), 1, "{stdout}");
    if out.status.code() == Some(1) {
        let missed = "dev: scenario: step 1 (capture): \
                      the play ended before a frame at 0 ms on its closing axis";
        assert!(
            stdout.lines().chain(stderr.lines()).any(|l| l == missed),
            "{stdout}\n{stderr}"
        );
        assert!(!fx.base.join("out").join("c.json").exists());
        return;
    }
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let side = fs::read_to_string(fx.base.join("out").join("c.json")).unwrap();
    assert_eq!(side_value(&side, "phase"), "closing", "{side}");
}

/// A loading capture says where on its loading axis it was sampled: the
/// origin and the elapsed time agree, and the completion time comes later.
/// Without a pending skin it draws its presentation: settled.
#[test]
fn a_loading_frames_sidecar_has_its_sampled_time() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { loading = true, file = \"l.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let side = fs::read_to_string(fx.base.join("out").join("l.json")).unwrap();
    let elapsed = side_num(&side, "loading_us");
    let origin = side_num(&side, "loading_origin_us");
    assert!(elapsed >= 0, "{side}");
    assert!(origin > 0, "{side}");
    assert!(
        side_num(&side, "completed_us") >= origin + elapsed,
        "{side}"
    );
    assert_eq!(side_value(&side, "settled"), "true", "{side}");
}

/// With a negative input offset the evaluation's watermark passes chart
/// time 0 while the frames are still READY's, and the phase stays `ready`.
#[test]
fn a_negative_input_offset_keeps_a_ready_frame_ready() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    write_log(&fx.base.join("logs").join("none.tprl"), &[], 1);
    fs::create_dir_all(fx.data()).unwrap();
    let device = DeviceSettings {
        input_offset_us: -900_000,
        ..DeviceSettings::default()
    };
    game::save_settings(&fx.data().join("device.toml"), &device).unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { ready_at = 300, file = \"r.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/none.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let side = fs::read_to_string(fx.base.join("out").join("r.json")).unwrap();
    assert!(side_num(&side, "chart_us") < 0, "{side}");
    assert!(side_num(&side, "finalized_us") > 0, "{side}");
    assert_eq!(side_value(&side, "phase"), "ready", "{side}");
}

/// A file taken by a folder on a capture's sidecar fails the run, its PNG
/// written or not.
#[test]
fn a_sidecar_that_cannot_be_written_exits_one() {
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
    let target = fx.base.join("out").join("a.json");
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
    assert!(stderr.contains("a.png"), "{stderr}");
}

/// Without a `size`, frames keep the default offscreen target.
#[test]
fn a_scenario_without_a_size_captures_at_the_default() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { at = 0, file = \"a.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let png = fs::read(fx.base.join("out").join("a.png")).unwrap();
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1280);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 720);
}

/// A play frame of an imported skin has its display list beside it: the
/// frame's state, the skin's element with what the draw evaluated, and its
/// quad on the canvas with the file and rectangle it shows. A
/// loading frame and a frame of the built-in skin have none.
#[test]
fn a_captured_play_frame_of_a_skin_has_its_display_list() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
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
            "image": [{"id": "b", "src": "b", "x": 4, "y": 2, "w": 8, "h": 8}],
            "destination": [{"id": "b", "dst": [{"x": 0, "y": 0, "w": 1920, "h": 1080}]}]}"#,
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 's.json'\n",
            skin.display()
        ),
    )
    .unwrap();
    let text = scenario(&[
        "capture = { loading = true, file = \"load.png\" }",
        "capture = { at = 3200, file = \"note.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ])
    .replacen(
        "output = \"out\"\n",
        "output = \"out\"\nsize = [640, 360]\n",
        1,
    );
    let (out, stdout, stderr) = fx.run(&text);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let dir = fx.base.join("out");
    assert!(!dir.join("load.jsonl").exists());
    let list = fs::read_to_string(dir.join("note.jsonl")).unwrap();
    let mut lines = list.lines();
    let state = lines.next().unwrap();
    assert_eq!(side_num(state, "element_count"), 1, "{state}");
    assert_eq!(side_num(state, "ex_score") % 2, 0, "{state}");
    assert!(side_num(state, "combo") >= 1, "{state}");
    assert_eq!(side_value(state, "look"), "Normal", "{state}");
    let element = lines.next().unwrap();
    assert_eq!(side_num(element, "element"), 0, "{element}");
    assert_eq!(side_value(element, "outcome"), "Sampled", "{element}");
    assert!(side_num(element, "t_us") > 3_200_000, "{element}");
    let quad = lines.next().unwrap();
    assert_eq!(side_num(quad, "element_of"), 0, "{quad}");
    // On the skin's canvas, whatever the frame's size.
    assert!(quad.contains("\"rect\":[0,0,1920,1080]"), "{quad}");
    assert!(quad.contains("\"file\":\"blue.png\""), "{quad}");
    assert!(quad.contains("\"src\":[4,2,8,8]"), "{quad}");
    assert!(quad.contains("\"blend\":\"alpha\""), "{quad}");
}

/// A READY capture past READY's end fails the run rather than show play.
#[test]
fn a_ready_capture_past_ready_fails_the_run() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { ready_at = 1500, file = \"late.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("its READY state ended first"), "{stderr}");
    assert!(!fx.base.join("out").join("late.png").exists());
}

/// A skin with a 2 500 ms loading time, installed as the play skin.
fn install_slow_loading_skin(fx: &Fixture) {
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    fs::write(
        skin.join("s.json"),
        r#"{"type": 0, "w": 1920, "h": 1080, "loadend": 2500, "destination": []}"#,
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 's.json'\n",
            skin.display()
        ),
    )
    .unwrap();
}

/// A `loading_at` capture takes a frame on its loading state's clock, its
/// screen clock the same; the play's frames run that screen clock on
/// (docs/stage3.md D3). One past the loading state fails the run.
#[test]
fn a_loading_at_capture_runs_on_the_loading_clock() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    install_slow_loading_skin(&fx);
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { loading_at = 1500, file = \"mid.png\" }",
        "capture = { at = 0, file = \"start.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let dir = fx.base.join("out");
    assert!(dir.join("mid.png").exists());
    let mid = fs::read_to_string(dir.join("mid.json")).unwrap();
    assert_eq!(side_value(&mid, "axis"), "loading_at", "{mid}");
    assert_eq!(side_value(&mid, "phase"), "loading", "{mid}");
    let loading_us = side_num(&mid, "loading_us");
    // At or after 1.5 s, and inside the 2.5 s loading state.
    assert!((1_500_000..2_500_000).contains(&loading_us), "{mid}");
    assert_eq!(side_num(&mid, "screen_us"), loading_us, "{mid}");
    // The play's screen clock runs on past the whole loading time.
    let start = fs::read_to_string(dir.join("start.json")).unwrap();
    assert!(side_num(&start, "screen_us") >= 2_500_000, "{start}");
    assert_eq!(side_value(&start, "loading_us"), "null", "{start}");

    let late = scenario(&[
        "capture = { loading_at = 60000, file = \"late.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ])
    .replacen("output = \"out\"", "output = \"late\"", 1);
    let (out, stdout, stderr) = fx.run(&late);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(stderr.contains("its loading state ended first"), "{stderr}");
}

/// The adapter lines of `stdout`.
fn adapter_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("dev: adapter asked "))
        .collect()
}

/// A run asking for the hardware adapter builds its screen with that
/// choice and says what it got: a CPU adapter refuses the run, any other
/// draws it and names itself in each capture's sidecar. CI has no
/// hardware GPU, so either outcome is checked as it comes.
#[test]
fn a_hardware_scenario_builds_on_the_hardware_adapter() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let text = small(&[
        "capture = { at = 0, file = \"a.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ])
    .replacen(
        "size = [640, 360]\n",
        "size = [640, 360]\nadapter = \"hardware\"\n",
        1,
    );
    let (out, stdout, stderr) = fx.run(&text);
    let lines = adapter_lines(&stdout);
    assert!(!lines.is_empty(), "{stdout}\n{stderr}");
    assert!(
        lines
            .iter()
            .all(|l| l.starts_with("dev: adapter asked hardware, got ")),
        "{lines:?}"
    );
    if lines[0].contains(" (Cpu, ") {
        assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
        assert!(
            stderr.contains("hardware was asked for, and the adapter is a CPU adapter"),
            "{stderr}"
        );
        return;
    }
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let name = lines[0]
        .trim_start_matches("dev: adapter asked hardware, got ")
        .rsplit_once(" (")
        .unwrap()
        .0;
    assert_eq!(side_value(&side_of(&fx, "a"), "adapter"), name);
}

/// Every build of the screen is admitted against the file's choice: a
/// hardware run whose screen, built again after a device loss on the
/// result, gets a CPU adapter (injected) is refused there, as at startup.
#[test]
fn a_rebuilt_screen_s_cpu_adapter_is_refused_on_a_hardware_choice() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let text = small(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ])
    .replacen(
        "size = [640, 360]\n",
        "size = [640, 360]\nadapter = \"hardware\"\n",
        1,
    );
    let (out, stdout, stderr) = fx.output(
        fx.command(&text)
            .env("TAPESTRY_DEV_RESULT_FAULTS", "lostresult1")
            .env("TAPESTRY_DEV_ADAPTER_FAULTS", "cpu2"),
    );
    let lines = adapter_lines(&stdout);
    assert!(!lines.is_empty(), "{stdout}\n{stderr}");
    let refusal = "hardware was asked for, and the adapter is a CPU adapter";
    // No hardware GPU (CI): refused at startup, before any loss.
    if lines[0].contains(" (Cpu, ") {
        assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
        assert!(stderr.contains(refusal), "{stderr}");
        return;
    }
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("dev: adapter asked hardware, got injected CPU adapter (Cpu, "),
        "{lines:?}"
    );
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(&format!("{refusal}: injected CPU adapter")),
        "{stderr}"
    );
}

/// Every build of the screen takes the file's choice: the one after a
/// device loss on the result says so again.
#[test]
fn a_screen_built_again_keeps_the_adapter_choice() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run_with(
        &small(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ])
        .replacen(
            "size = [640, 360]\n",
            "size = [640, 360]\nadapter = \"warp\"\n",
            1,
        ),
        "lostresult1",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stderr.contains("render: "), "no loss: {stderr}");
    let lines = adapter_lines(&stdout);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|l| l.starts_with("dev: adapter asked warp, got ")),
        "{lines:?}"
    );
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

/// Chart A by autoplay, then a practice of A with a log that presses only
/// key 7, at 3.25 s: the practice's chart is set up with the log, not
/// autoplay, so the round's record holds exactly the log's events and none
/// generated. Autoplay's key sounds, which no BGM holds, play before the
/// practice: its four, and nothing else sounds in the run.
#[test]
fn practice_after_autoplay_plays_its_log() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let key7 = bms_parser::load_bms(
        b"#BPM 120\n#00119:0000000000010000\n",
        &fx.base,
        &Default::default(),
    )
    .unwrap()
    .chart;
    let log = hits(&key7, |_| true);
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].received, TimeUs(3_250_000));
    write_log(&fx.base.join("logs").join("k7.tprl"), &log, 1);
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "practice = { path = \"library/a/a.bms\", from = 1, to = 2, speed = 100, \
         lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/k7.tprl\", rounds = 1 }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let (records, cancelled) = fx.played_records();
    assert_eq!(records.len(), 1, "{records:?}");
    // The rounds are left at the boundary after the last: no round after
    // it opens, so no capture is cancelled.
    assert_eq!(cancelled, 0, "{stdout}");
    // Autoplay's play and the round: no second round reports.
    assert_eq!(lines(&stdout, "play 1: ").len(), 2, "{stdout}");
    assert!(lines(&stdout, "play 2: ").is_empty(), "{stdout}");
    let round = replay::decode(&fs::read(&records[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert!(
        matches!(round.play_type, PlayType::SectionRepeat(_)),
        "{:?}",
        round.play_type
    );
    assert_eq!(round.events, log);
    assert_eq!(round.status.end, EndStatus::PlayedToEnd);
    assert_eq!(
        lines(
            &stdout,
            "play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0"
        )
        .len(),
        1,
        "{stdout}"
    );
    // And the decision tone at each of the two choices.
    assert_eq!(lines(&stdout, "dev: audio onsets 6:").len(), 1, "{stdout}");
}

/// Skins are imported at startup only: with a Lua play skin configured,
/// a run of an autoplay, a retry from its result, a return to selection
/// and a practice Start with its round imports nothing more than
/// startup's one import (its two Lua passes).
#[test]
fn no_skin_is_imported_after_startup_through_practice() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let key7 = bms_parser::load_bms(
        b"#BPM 120\n#00119:0000000000010000\n",
        &fx.base,
        &Default::default(),
    )
    .unwrap()
    .chart;
    write_log(
        &fx.base.join("logs").join("k7.tprl"),
        &hits(&key7, |_| true),
        1,
    );
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    let file = fs::File::create(skin.join("blue.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let blue: Vec<u8> = (0..16 * 16).flat_map(|_| [0, 0, 255, 255]).collect();
    e.write_header().unwrap().write_image_data(&blue).unwrap();
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"blue\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"b\", path = \"blue.png\" } }\n\
         skin.image = { { id = \"b\", src = \"b\", x = 0, y = 0, w = 16, h = 16 } }\n\
         skin.destination = { { id = \"b\", dst = { { x = 0, y = 0, w = 1920, h = 1080 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"retry\"",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "practice = { path = \"library/a/a.bms\", from = 1, to = 2, speed = 100, \
         lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/k7.tprl\", rounds = 1 }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        lines(&stdout, "skin:"),
        ["skin: play: installed"],
        "{stdout}"
    );
    assert!(
        !lines(&stdout, "practice: on the practice screen").is_empty(),
        "{stdout}"
    );
    // The two plays and the round.
    assert_eq!(lines(&stdout, "play started").len(), 3, "{stdout}");
    assert_eq!(
        lines(&stdout, "dev: skin imports "),
        ["dev: skin imports 1 lua passes 2"],
        "{stdout}"
    );
}

/// A practice Start with a pending play skin that cannot be built for the
/// chart (its configured pass, after reading the judgment class, loads a
/// missing module) returns to the practice page, which shows the reason;
/// no round opens and the skin in use never stands in. The scenario's
/// rounds then fail naming that refusal.
#[test]
fn a_refused_specialization_returns_to_the_practice_page() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let key7 = bms_parser::load_bms(
        b"#BPM 120\n#00119:0000000000010000\n",
        &fx.base,
        &Default::default(),
    )
    .unwrap()
    .chart;
    write_log(
        &fx.base.join("logs").join("k7.tprl"),
        &hits(&key7, |_| true),
        1,
    );
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"class\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local easy = ms.option(183)\n\
         local m = require(\"missing\")\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "practice = { path = \"library/a/a.bms\", from = 1, to = 2, speed = 100, \
         lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/k7.tprl\", rounds = 1 }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("skin: play: pending: reads the chart's judgment class"),
        "{stdout}"
    );
    let reason = "the play skin cannot be built for this chart: ";
    let opened = stdout
        .find("practice: on the practice screen")
        .unwrap_or_else(|| panic!("{stdout}"));
    let refused = stdout
        .find(&format!("practice: refused: {reason}"))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(opened < refused, "{stdout}");
    assert!(
        stdout[refused..]
            .lines()
            .next()
            .unwrap()
            .contains("missing"),
        "{stdout}"
    );
    assert!(
        format!("{stdout}{stderr}").contains(&format!("the practice was refused: {reason}")),
        "{stdout}\n{stderr}"
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
    assert!(fx.played_records().0.is_empty());
}

/// The quit key while a pending skin's specialization loops, started from
/// a practice Start, cancels it and returns to the practice page only once
/// the job's thread has ended (inside the child: no VM open, every job
/// started joined), with no round and no recording opened; the
/// scenario's rounds end there, and the run leaves the page and quits.
#[test]
fn the_quit_key_during_a_practice_specialization_returns_to_the_page() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let key7 = bms_parser::load_bms(
        b"#BPM 120\n#00119:0000000000010000\n",
        &fx.base,
        &Default::default(),
    )
    .unwrap()
    .chart;
    write_log(
        &fx.base.join("logs").join("k7.tprl"),
        &hits(&key7, |_| true),
        1,
    );
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"class\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local easy = require(\"main_state\").option(183)\n\
         while true do end\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let out = fx
        .command(&scenario(&[
            "practice = { path = \"library/a/a.bms\", from = 1, to = 2, speed = 100, \
             lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/k7.tprl\", rounds = 1 }",
            "quit = {}",
        ]))
        .args(["--dev-specializing", "quit"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let at = |from: usize, line: &str| {
        stdout[from..]
            .find(line)
            .map(|i| i + from)
            .unwrap_or_else(|| panic!("no {line}:\n{stdout}"))
    };
    let page = at(0, "practice: on the practice screen");
    let quit = at(page, "dev: quit pressed while specializing");
    let left = at(quit, "dev: specialization wait left");
    let ended = at(left, "dev: specialization ended: live 0 joined 2 of 2");
    let back = at(ended, "practice: on the practice screen");
    at(
        back,
        "dev: the rounds ended at the Start's cancelled specialization",
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
    assert!(lines(&stdout, "dev: lua at open").is_empty(), "{stdout}");
    assert!(lines(&stdout, "skin: play: refused").is_empty(), "{stdout}");
    assert!(fx.records().is_empty());
}

/// The peaks the recording output found after each sound's onset.
fn onset_peaks(stdout: &str) -> Vec<f64> {
    let line = lines(stdout, "dev: audio onset peaks:")[0];
    line.trim_start_matches("dev: audio onset peaks:")
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// [`onset_peaks`] after the decision tones of the run's first `tones`
/// choices, which sound before any chart's sound.
fn chart_peaks(stdout: &str, tones: usize) -> Vec<f64> {
    onset_peaks(stdout).split_off(tones)
}

/// A practice at 75 % of a quiet chart the library misses measures the
/// chart once at 1×, on the practice page or during the Start's load
/// (whichever comes first), before the round's READY state, so its first
/// round plays at that gain, not the headroom; a later 1× play
/// finds that measurement by the chart's 1× key and measures nothing: its
/// sounds peak as loud as the round's, about four times the headroom's.
#[test]
fn a_practice_at_a_speed_measures_the_charts_1x_gain() {
    let fx = Fixture::new();
    let a = write_chart(&fx.library(), "a", "譜面A");
    write_wav_at(&fx.library().join("a").join("key.wav"), 1_000);
    write_log(&fx.base.join("logs").join("a.tprl"), &hits(&a, |_| true), 1);
    // The practice: measured on its page or at its Start, its round's four
    // presses at the gain.
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "practice = { path = \"library/a/a.bms\", from = 1, to = 2, speed = 75, \
         lead_in_ms = 2000, gauge = 20, repeat = true, input = \"logs/a.tprl\", rounds = 1 }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        lines(&stdout, "loudness: measuring a.bms").len(),
        1,
        "{stdout}"
    );
    let measured = lines(&stdout, "loudness: a.bms measured in ");
    assert_eq!(measured.len(), 1, "{stdout}");
    let db: f64 = measured[0]
        .split("; gain ")
        .nth(1)
        .and_then(|r| r.split(" dB").next())
        .unwrap()
        .parse()
        .unwrap();
    // The +6 dB cap: four times the headroom.
    assert!(db > 5.5, "{stdout}");
    // The Start left the practice page for the play screen's loading state,
    // which was drawn up to the round's READY state; the one measurement came
    // before the chart was ready, on the page or in that loading state.
    let all: Vec<&str> = stdout.lines().collect();
    let at = |from: usize, line: &str| {
        (from..all.len())
            .find(|&i| all[i].starts_with(line))
            .unwrap_or_else(|| panic!("no {line} after line {from}:\n{stdout}"))
    };
    let page = at(0, "practice: on the practice screen");
    let shown = at(page, "load: the play screen loads a.bms");
    let ready = at(shown, "load: the chart is ready ");
    let measured_at = at(0, "loudness: a.bms measured in ");
    assert!(
        measured_at < ready,
        "measured after the chart was ready:\n{stdout}"
    );
    // Measured by the Start (after the play screen began loading), the
    // loading state said so before the chart was ready.
    if measured_at > shown {
        let measuring = at(shown, "load: measuring the loudness");
        assert!(measuring < ready, "{stdout}");
    }
    at(ready, "play started");
    assert!(
        !lines(&stdout, "dev: loading frames ").is_empty(),
        "{stdout}"
    );
    let round = chart_peaks(&stdout, 1);
    assert_eq!(round.len(), 4, "{stdout}");
    // A 1× play in another run: the stored measurement, nothing measured.
    let root = game::DataRoot::new(fx.data());
    fs::create_dir_all(root.path()).unwrap();
    let device = DeviceSettings {
        library: game::LibrarySettings {
            roots: vec![fx.library().display().to_string()],
        },
        ..DeviceSettings::default()
    };
    game::save_settings(&root.device_settings(), &device).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args(["--auto", "--dev-headless", "--dev-select"])
        .arg("pick=a.bms,enter,back,close")
        .args(["--dev-render-timeout", "600"])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .unwrap();
    let played = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{played}");
    assert!(lines(&played, "loudness: measuring").is_empty(), "{played}");
    assert_eq!(
        lines(&played, "loudness: a.bms: gain ").len(),
        1,
        "{played}"
    );
    let peaks = chart_peaks(&played, 1);
    assert_eq!(peaks.len(), 4, "{played}");
    for p in round {
        assert!(
            (p / peaks[0] - 1.0).abs() < 0.15,
            "{p} against {}: {stdout}",
            peaks[0]
        );
    }
}

/// A practice of the middle of a chart: measure 3, whose lead-in holds
/// measure 2's notes on key 2, which are not targets and sound a loud
/// sound. The round's schedule plays them as autoplay in the lead-in,
/// before the four quiet targets the log hits; the screen is given the
/// section and draws only its four targets of the eight notes.
#[test]
fn a_middle_measure_round_sounds_its_lead_in_and_draws_its_targets() {
    let fx = Fixture::new();
    let dir = fx.library().join("m");
    fs::create_dir_all(&dir).unwrap();
    write_wav_at(&dir.join("key.wav"), 1_000);
    write_wav_at(&dir.join("loud.wav"), 8_000);
    let text = "#PLAYER 1\n#TITLE 譜面M\n#BPM 120\n#WAV01 key.wav\n#WAV02 loud.wav\n\
                #00138:01\n#00112:02020202\n#00211:01010101\n";
    fs::write(dir.join("m.bms"), text).unwrap();
    let chart = bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart;
    let log = hits(&chart, |t| t.0 >= 4_000_000);
    assert_eq!(log.len(), 8);
    write_log(&fx.base.join("logs").join("m.tprl"), &log, 1);
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "practice = { path = \"library/m/m.bms\", from = 3, to = 3, speed = 100, \
         lead_in_ms = 2500, gauge = 20, repeat = true, input = \"logs/m.tprl\", rounds = 1 }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        lines(
            &stdout,
            "dev: render: a section's 4 targets of 8 notes are drawn"
        )
        .len(),
        1,
        "{stdout}"
    );
    assert_eq!(
        lines(
            &stdout,
            "play 1: Completed; perfect 4 great 0 good 0 bad 0 poor 0"
        )
        .len(),
        1,
        "{stdout}"
    );
    // The lead-in's four loud sounds, then the four targets' quiet ones.
    let peaks = chart_peaks(&stdout, 1);
    assert_eq!(peaks.len(), 8, "{peaks:?}\n{stdout}");
    let (loud, quiet) = peaks.split_at(4);
    let least_loud = loud.iter().copied().fold(f64::MAX, f64::min);
    let most_quiet = quiet.iter().copied().fold(0.0, f64::max);
    assert!(least_loud > most_quiet * 4.0, "{peaks:?}");
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

/// A preset chosen on the select screen (F12, from the default
/// beatoraja-style) is the one the next launch plays: a scripted run
/// chooses it and closes, then a scenario on the same data root plays a
/// log, saved under lr2-style.
#[test]
fn restart_keeps_the_chosen_preset() {
    let fx = Fixture::new();
    let a = write_chart(&fx.library(), "a", "譜面A");
    write_log(&fx.base.join("logs").join("a.tprl"), &hits(&a, |_| true), 1);
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .args([
            "--auto",
            "--dev-headless",
            "--dev-select",
            "next-preset,sleep=300,close",
        ])
        .args(["--dev-render-timeout", "600"])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert_eq!(
        lines(&stdout, "select: preset "),
        ["select: preset lr2-style"]
    );

    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/a.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(lines(&stdout, "select: preset ").is_empty(), "{stdout}");
    let records = fx.records();
    let [record] = &records[..] else {
        panic!("{records:?}\n{stdout}");
    };
    let decoded = replay::decode(&fs::read(record).unwrap(), &DecodeLimits::default()).unwrap();
    assert_eq!(decoded.preset.id, "lr2-style");
    assert_eq!(
        decoded.preset.sha256,
        replay::preset_hash(&rules::lr2_style())
    );
}

/// Input generated to press nothing, with the HAZARD gauge shown and a
/// failure closing the play, played and saved as a log's play is: one
/// keyboard, nothing held at the start, no press, its re-evaluation the
/// live result. Its outcome is any the real clock permits (design.md "Play
/// Lifecycle"): only POORs, as many as the notes missed by its end; the
/// gauge's failure aborts it and enters Closing, or a late wake completes
/// it first, all four notes missed and no Closing; or a pause before its
/// transport commits lets that wake cancel it before playback, nothing
/// recorded and no played record. The exact count and Closing are the unit
/// test's on a virtual clock
/// (`generated_no_press_input_misses_into_closing_on_the_play_path`); a
/// frame of Closing is not asserted: none is guaranteed to be presented.
#[test]
fn a_generated_no_press_play_misses_into_closing() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        "displayed_gauge = 5\non_failure = \"close\"\n",
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = { miss = \"all\" }, until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let [play] = lines(&stdout, "play 1: ")[..] else {
        panic!("{stdout}");
    };
    let (records, _) = fx.played_records();
    let closing = lines(&stdout, "closing: ").len();
    if play == "play 1: Completed; nothing recorded" {
        // Its first scoring wake came before its transport committed, so
        // the candidate was cancelled before playback.
        assert_eq!(closing, 0, "{stdout}");
        assert!(records.is_empty(), "{records:?}");
        return;
    }
    // Only misses, and the re-evaluation agrees.
    let poor: u32 = play
        .split_once("; perfect 0 great 0 good 0 bad 0 poor ")
        .and_then(|(_, rest)| rest.split_once(';'))
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("{play}"));
    assert!(play.contains("re-evaluation matches live: true"), "{play}");
    if play.starts_with("play 1: Aborted(GaugeFailed);") {
        // Every miss the wake that failed it finalized, then Closing.
        assert!((1..=4).contains(&poor), "{play}");
        assert_eq!(closing, 1, "{stdout}");
    } else {
        assert!(play.starts_with("play 1: Completed;"), "{play}");
        assert_eq!(poor, 4, "{play}");
        assert_eq!(closing, 0, "{stdout}");
    }
    assert_eq!(records.len(), 1, "{records:?}");
    let rec = replay::decode(&fs::read(&records[0]).unwrap(), &DecodeLimits::default()).unwrap();
    assert!(
        rec.events
            .iter()
            .all(|e| !matches!(e.kind, eval::InputKind::Control { value, .. } if value != 0)),
        "{:?}",
        rec.events.first()
    );
    assert_eq!(rec.devices.sources.len(), 1);
    assert_eq!(rec.devices.sources[0].kind, replay::DeviceKind::Keyboard);
    assert!(rec.start.held.is_empty(), "{:?}", rec.start);
}

/// A chart with no judgeable note is refused, generated input or not
/// (design.md "The tapestry Preset", zero judgeable notes: nothing to
/// play): the library never lists it, so its `play` never starts and
/// nothing is saved.
#[test]
fn a_generated_no_press_play_of_a_chart_without_notes_is_refused() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let dir = fx.library().join("tail");
    fs::write(
        dir.join("tail.bms"),
        "#PLAYER 1\n#TITLE none\n#BPM 120\n#WAV02 bgm.wav\n#00101:02\n",
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = { miss = \"all\" }, until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("tail.bms was not listed"),
        "{stdout}\n{stderr}"
    );
    assert!(lines(&stdout, "play started").is_empty(), "{stdout}");
    assert!(fx.records().is_empty());
}

/// A generated input is no input file: an unknown word is refused by its
/// line, and a `play` of it after one of autoplay needs the chart again.
#[test]
fn a_generated_input_is_checked() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, _, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = { miss = \"some\" }, until = \"end\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    // The `play` step's own line.
    assert!(stderr.contains("line 8"), "{stderr}");
    let (out, _, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = { at_ms = 1, command = \"retry\" } }",
        "play = { input = { miss = \"all\" }, until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("input differs from step 2's"), "{stderr}");
}

/// A log that presses nothing, played with the HAZARD gauge shown: its
/// first miss fails it. Under `on_failure = "close"` the play enters
/// Closing, which lasts at least the 1 s floor before the result; under
/// `"play-to-end"` it plays to its end.
#[test]
fn a_shown_survival_failure_closes_or_plays_to_the_end() {
    for (on_failure, closes) in [("close", true), ("play-to-end", false)] {
        let fx = Fixture::new();
        write_chart(&fx.library(), "a", "譜面A");
        write_log(&fx.base.join("logs").join("none.tprl"), &[], 1);
        fs::create_dir_all(fx.data()).unwrap();
        fs::write(
            fx.data().join("player.toml"),
            format!("displayed_gauge = 5\non_failure = \"{on_failure}\"\n"),
        )
        .unwrap();
        let (out, stdout, stderr) = fx.run(&scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"logs/none.tprl\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]));
        assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
        let closing = lines(&stdout, "closing: ");
        if closes {
            assert_eq!(
                lines(&stdout, "play 1: Aborted(GaugeFailed)").len(),
                1,
                "{stdout}"
            );
            let [line] = closing[..] else {
                panic!("{stdout}");
            };
            let lasted: i64 = line
                .split(", ")
                .nth(1)
                .and_then(|d| d.strip_suffix(" us"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(lasted >= 1_000_000, "{line}");
            assert!(line.ends_with("ran out"), "{line}");
        } else {
            assert_eq!(lines(&stdout, "play 1: Completed").len(), 1, "{stdout}");
            assert!(closing.is_empty(), "{stdout}");
        }
        let (records, _) = fx.played_records();
        let record =
            replay::decode(&fs::read(&records[0]).unwrap(), &DecodeLimits::default()).unwrap();
        let (status, policy) = if closes {
            (EndStatus::EndedMidway, "close-on-failure")
        } else {
            (EndStatus::PlayedToEnd, "play-to-end")
        };
        assert_eq!(record.status.end, status);
        assert_eq!(record.session_policy, policy);
    }
}

/// The pixel at column `x`, row `y` of the 1280×720 capture at `path`.
fn pixel(path: &Path, x: usize, y: usize) -> [u8; 3] {
    let mut reader = png::Decoder::new(BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut rgba = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut rgba).unwrap();
    let i = (y * info.width as usize + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2]]
}

/// A play skin with a full-canvas image on the last note's timer, a read of
/// the combo break its condition, opaque from 750 to 1250 ms on that timer:
/// the shape of a skin's end-of-play image, in blue, and an untimed red
/// corner on the same read. A play ends with its audio, so the chart's BGM lasts through
/// the captures. A frame shows the corner exactly when the view it drew
/// from holds the miss, its sidecar's event count: a frame may draw from a
/// view older than its time, so no time alone decides it, and none is
/// required to hold it here: the renderer's own test draws a view that
/// does. After a perfect play there is no corner. A
/// retry of the miss starts from a clean snapshot: its first READY frame
/// has no corner, with no tally set by hand. The blue window runs on the
/// last-note timer, which only a frame at an exact time shows for certain:
/// `lua_bridges` and the renderer's tests test it at those times.
#[test]
fn a_missed_note_draws_the_finish_window() {
    let fx = Fixture::new();
    write_tail_chart(&fx.library());
    let dir = fx.library().join("tail");
    let text = "#PLAYER 1\n#TITLE finish\n#BPM 120\n#WAV01 key.wav\n#WAV02 bgm.wav\n\
                #00138:01\n#00111:01000000\n#00101:02\n";
    fs::write(dir.join("tail.bms"), text).unwrap();
    let chart = bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart;
    write_log(&fx.base.join("logs").join("miss.tprl"), &[], 1);
    write_log(
        &fx.base.join("logs").join("hit.tprl"),
        &hits(&chart, |_| true),
        1,
    );
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    let file = fs::File::create(skin.join("white.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..16 * 16).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"finish\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = { { id = \"b\", path = \"white.png\" } }\n\
         skin.image = { { id = \"b\", src = \"b\", x = 0, y = 0, w = 16, h = 16 } }\n\
         skin.destination = { { id = \"b\", timer = 143, loop = -1,\n\
           draw = function() local cb = ms.number(425) return cb > 0 end, dst = {\n\
             { time = 500, x = 0, y = 0, w = 1920, h = 1080, a = 0, r = 0, g = 0 },\n\
             { time = 750, a = 255 }, { time = 1250, a = 255 }, { time = 1450, a = 0 } } },\n\
           { id = \"b\", draw = function() return ms.number(425) > 0 end,\n\
             dst = { { x = 0, y = 0, w = 192, h = 108, g = 0, b = 0 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { at = 2900, file = \"miss.png\" }",
        "capture = { at = 3600, file = \"miss2.png\" }",
        "capture = { at = 4300, file = \"miss3.png\" }",
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"logs/miss.tprl\", until = \"end\" }",
        "capture = { ready_at = 0, file = \"retry.png\" }",
        "result = \"retry\"",
        "play = { input = \"logs/miss.tprl\", until = \"end\" }",
        "result = \"back\"",
        "capture = { at = 2900, file = \"hit.png\" }",
        "select = { path = \"library/tail/tail.bms\" }",
        "play = { input = \"logs/hit.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let plays: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("play ") && l.contains(": Completed"))
        .collect();
    let scores = [
        "perfect 0 great 0 good 0 bad 0 poor 1",
        "perfect 0 great 0 good 0 bad 0 poor 1",
        "perfect 1 great 0 good 0 bad 0 poor 0",
    ];
    assert_eq!(plays.len(), 3, "{stdout}");
    for (play, score) in plays.iter().zip(scores) {
        assert!(play.contains(score), "{stdout}");
    }
    let out = fx.base.join("out");
    let corner = |p: &Path| pixel(p, 20, 700);
    // The events the frame's view had published: the miss is the play's
    // only one.
    let events = |name: &str| {
        let side = fs::read_to_string(out.join(format!("{name}.json"))).unwrap();
        let rest = side.split("\"events\":").nth(1).unwrap();
        rest.split(['}', ','])
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    for name in ["miss", "miss2", "miss3"] {
        let expected = if events(name) > 0 {
            [255, 0, 0]
        } else {
            [0, 0, 0]
        };
        assert_eq!(corner(&out.join(format!("{name}.png"))), expected, "{name}");
    }
    assert_eq!(
        corner(&out.join("retry.png")),
        [0, 0, 0],
        "the retry's first frame"
    );
    assert_eq!(corner(&out.join("hit.png")), [0, 0, 0]);
}

/// A play skin reads the attempt's autoplay while loading and playing: a
/// full-canvas blue image on op 33 (autoplay) and a red corner on op 32
/// (not autoplay). An autoplay draws the blue, a log's play the corner.
#[test]
fn a_play_skin_reads_the_attempts_autoplay() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    write_log(&fx.base.join("logs").join("none.tprl"), &[], 1);
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    let file = fs::File::create(skin.join("white.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white: Vec<u8> = (0..16 * 16).flat_map(|_| [255; 4]).collect();
    e.write_header().unwrap().write_image_data(&white).unwrap();
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"autoplay\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"b\", path = \"white.png\" } }\n\
         skin.image = { { id = \"b\", src = \"b\", x = 0, y = 0, w = 16, h = 16 } }\n\
         skin.destination = {\n\
           { id = \"b\", op = { 33 }, dst = { { x = 0, y = 0, w = 1920, h = 1080, r = 0, g = 0 } } },\n\
           { id = \"b\", op = { 32 }, dst = { { x = 0, y = 0, w = 192, h = 108, g = 0, b = 0 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "capture = { loading = true, file = \"auto_load.png\" }",
        "capture = { at = 200, file = \"auto_play.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "capture = { loading = true, file = \"log_load.png\" }",
        "capture = { at = 200, file = \"log_play.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/none.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let out = fx.base.join("out");
    let (centre, corner) = (|p: &Path| pixel(p, 640, 360), |p: &Path| pixel(p, 20, 700));
    for name in ["auto_load", "auto_play"] {
        let png = out.join(format!("{name}.png"));
        assert_eq!(centre(&png), [0, 0, 255], "{name}");
        assert_eq!(corner(&png), [0, 0, 255], "{name}");
    }
    for name in ["log_load", "log_play"] {
        let png = out.join(format!("{name}.png"));
        assert_eq!(centre(&png), [0, 0, 0], "{name}");
        assert_eq!(corner(&png), [255, 0, 0], "{name}");
    }
}

/// The recent timing's chart at 120 BPM (a measure is 2 s): key 1 at
/// 125 ms, 4, 6 and 8 s, key 2's long note from 10 to 12 s, and a sound at
/// 14 s the play lasts to. Every capture is a second or more from the
/// next judgment, so a late frame still shows the one it names.
const TIMING_CHART: &str = "#PLAYER 1\n#TITLE timing\n#BPM 120\n#WAV01 key.wav\n#LNTYPE 1\n#00138:01\n\
    #00011:00010000000000000000000000000000\n#00211:01\n#00311:01\n#00411:01\n\
    #00552:01\n#00652:01\n#00701:01\n";

/// The rearm's chart: key 2's long note from 0 to 3 s.
const REARM_CHART: &str = "#PLAYER 1\n#TITLE rearm\n#BPM 120\n#WAV01 key.wav\n#LNTYPE 1\n#00138:01\n\
    #00052:01\n#00152:00000100\n";

/// What a captured frame of the recent-timing skin shows, read from its
/// display list: number 525's value from its glyphs (`n.png`, 24 glyphs
/// 10 wide: digits, padding, a sign, then the same for a negative), `None`
/// when undrawn, and whether the 1242 (30 wide) and 1243 (40 wide)
/// images drew.
fn timing_shown(list: &str) -> (Option<i32>, bool, bool) {
    let nums = |line: &str, key: &str| -> Vec<i64> {
        let rest = line.split(&format!("\"{key}\":[")).nth(1).unwrap();
        rest.split(']')
            .next()
            .unwrap()
            .split(',')
            .map(|n| n.trim().parse::<f64>().unwrap().round() as i64)
            .collect()
    };
    let quads: Vec<&str> = list.lines().filter(|l| l.contains("\"src\":[")).collect();
    let mut cells: Vec<(i64, i64)> = quads
        .iter()
        .filter(|l| l.contains("\"file\":\"n.png\""))
        .map(|l| (nums(l, "rect")[0], nums(l, "src")[0] / 10))
        .collect();
    cells.sort_unstable();
    let ms = (!cells.is_empty()).then(|| {
        let (sign, digits) = cells.split_first().unwrap();
        assert_eq!(sign.1 % 12, 11, "{cells:?}");
        let magnitude = digits.iter().fold(0, |v, &(_, g)| {
            assert!(g % 12 < 10 && g / 12 == sign.1 / 12, "{cells:?}");
            v * 10 + i32::try_from(g % 12).unwrap()
        });
        if sign.1 >= 12 { -magnitude } else { magnitude }
    });
    let wide = |w: i64| {
        quads
            .iter()
            .any(|l| l.contains("\"file\":\"white.png\"") && nums(l, "rect")[2] == w)
    };
    (ms, wide(30), wide(40))
}

/// Writes chart `name` of `text` into the library, with its sound.
fn write_text_chart(fx: &Fixture, name: &str, text: &str) -> bms_model::Chart {
    let dir = fx.library().join(name);
    fs::create_dir_all(&dir).unwrap();
    write_wav(&dir.join("key.wav"));
    fs::write(dir.join(format!("{name}.bms")), text).unwrap();
    bms_parser::load_bms(text.as_bytes(), &dir, &Default::default())
        .unwrap()
        .chart
}

/// A play skin's recent timing agrees with the view each captured frame
/// drew, on the game's real threads (a consistency check: which judgment a
/// frame shows depends on when it acquired its view, so none is required):
/// number 525's digits and the images on ops 1242 (early) and 1243
/// (late), from a log's play under the default beatoraja-style preset.
///
/// The play: key 1's note at 125 ms missed (−210, its late BAD edge); 4 s
/// pressed 30 ms early (+30, early); 6 s on time, then a press 140 ms
/// after it, an empty POOR against it (−140, late); 8 s pressed 30 ms late
/// (−30, late); the long note's head 10 ms late, held, which leaves −30;
/// its release 40 ms before its end, PERFECT (+40, neither); the play is
/// retried. Then the rearm's chart: its first READY takes an empty
/// POOR 480 ms before the long note's head and is cancelled by START held
/// before chart time 0, its release rearming a fresh READY, which holds
/// the long note: a held head leaves the timing, so that candidate reads 0
/// only if the rearm started it over.
#[test]
fn a_play_skin_reads_the_recent_timing() {
    const E: u32 = 0x12;
    let fx = Fixture::new();
    let chart = write_text_chart(&fx, "t", TIMING_CHART);
    let rearm_chart = write_text_chart(&fx, "r", REARM_CHART);
    let at = |e: PhysicalInputEvent, ms: i64| PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        ..e
    };
    // Each note is a press and its release, in note order.
    let all = hits(&chart, |_| true);
    assert_eq!(all.len(), 10);
    let (key1, key2) = ((all[0], all[1]), (all[8], all[9]));
    let played = [
        at(key1.0, 3_970),
        at(key1.1, 3_990),
        at(key1.0, 6_000),
        at(key1.1, 6_020),
        at(key1.0, 6_140),
        at(key1.1, 6_150),
        at(key1.0, 8_030),
        at(key1.1, 8_050),
        at(key2.0, 10_010),
        at(key2.1, 11_960),
    ];
    write_log(&fx.base.join("logs").join("timing.tprl"), &played, 1);
    let q = |ms: i64, value: i32| PhysicalInputEvent {
        received: TimeUs(ms * 1_000),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(E),
            value,
        },
    };
    let held = hits(&rearm_chart, |_| true);
    assert_eq!(held.len(), 2);
    // START released 250 ms before chart time 0 rearms a READY whose chart
    // time 0 lies about 1 s later on the log's clock (the lead-in after
    // the release, as in
    // `the_start_gate_cancels_and_rearms_ready_on_the_real_threads`): the
    // long note is pressed there and held to its end.
    let rearmed = [
        at(held[0], -480),
        at(held[1], -470),
        q(-450, 1),
        q(-250, 0),
        at(held[0], 760),
        at(held[1], 3_760),
    ];
    let path = fx.base.join("logs").join("rearm.tprl");
    write_log(&path, &rearmed, 1);
    let skin = fx.base.join("skin");
    fs::create_dir_all(&skin).unwrap();
    let image = |name: &str, w: u32| {
        let file = fs::File::create(skin.join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, 20);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let white: Vec<u8> = (0..w * 20).flat_map(|_| [255; 4]).collect();
        e.write_header().unwrap().write_image_data(&white).unwrap();
    };
    image("white.png", 16);
    image("n.png", 240);
    fs::write(
        skin.join("play.luaskin"),
        "local skin = { type = 0, name = \"timing\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         skin.source = { { id = \"b\", path = \"white.png\" }, { id = \"n\", path = \"n.png\" } }\n\
         skin.image = { { id = \"b\", src = \"b\", x = 0, y = 0, w = 16, h = 16 } }\n\
         skin.value = { { id = \"v\", src = \"n\", x = 0, y = 0, w = 240, h = 20, divx = 24, digit = 4,\n\
           value = function() return ms.number(525) end } }\n\
         skin.destination = {\n\
           { id = \"v\", dst = { { x = 0, y = 500, w = 12, h = 20 } } },\n\
           { id = \"b\", op = { 1242 }, dst = { { x = 0, y = 100, w = 30, h = 10 } } },\n\
           { id = \"b\", draw = function() return ms.option(1243) end,\n\
             dst = { { x = 0, y = 200, w = 40, h = 10 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.play]\nroot = '{}'\nentry = 'play.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let captures = [
        ("miss", 1_000),
        ("early", 4_500),
        ("empty", 6_700),
        ("late", 8_500),
        ("held", 10_500),
        ("release", 12_500),
    ];
    let mut steps: Vec<String> = captures
        .iter()
        .map(|(name, ms)| format!("capture = {{ at = {ms}, file = \"{name}.png\" }}"))
        .collect();
    steps.extend(
        [
            "select = { path = \"library/t/t.bms\" }",
            "play = { input = \"logs/timing.tprl\", until = \"end\" }",
            "result = \"retry\"",
            "play = { input = \"logs/timing.tprl\", until = \"end\" }",
            "result = \"back\"",
            "capture = { at = 1500, file = \"rearmed.png\" }",
            "select = { path = \"library/r/r.bms\" }",
            "play = { input = \"logs/rearm.tprl\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]
        .map(str::to_owned),
    );
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let (out, stdout, stderr) = fx.run(&scenario(&steps));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let out = fx.base.join("out");
    let shown = |name: &str| {
        let list = fs::read_to_string(out.join(format!("{name}.jsonl")))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        timing_shown(&list)
    };
    let side = |name: &str| fs::read_to_string(out.join(format!("{name}.json"))).unwrap();
    // Each play's events, re-evaluated from its record: the main play,
    // its retry, then the rearm's winning candidate.
    let (records, _) = fx.played_records();
    assert_eq!(records.len(), 3, "{records:?}");
    let played: Vec<(PlayRecord, Vec<eval::JudgeEvent>)> = records
        .iter()
        .zip([&chart, &chart, &rearm_chart])
        .map(|(path, chart)| {
            let rec = replay::decode(&fs::read(path).unwrap(), &DecodeLimits::default()).unwrap();
            let input = eval::EvalInput {
                events: rec.events.clone(),
                device: eval::DeviceProfile {
                    sources: rec
                        .devices
                        .sources
                        .iter()
                        .map(|s| s.profile.clone())
                        .collect(),
                },
                start: rec.start.clone(),
                ..eval::EvalInput::default()
            };
            let events = eval::evaluate(chart, &input, &rules::beatoraja_style())
                .unwrap()
                .events;
            (rec, events)
        })
        .collect();
    assert_eq!(played[2].0.history.openings.len(), 1, "the rearm");
    // What a frame shows after `events`' first `n` (docs/stage3.md R3, the
    // recent timing), each value from its own re-evaluated event, so a
    // play whose input landed elsewhere on its clock still agrees: a
    // press's deviation, negated, in ms truncated toward 0; a miss, its
    // note's time minus the miss's; a long note finished by a release, the
    // farther of its head and its release, the release on a tie, and the
    // release's when that one is early and the note BAD; an empty POOR,
    // its own deviation. A held head leaves it.
    let expected = |chart: &bms_model::Chart, events: &[eval::JudgeEvent], n: usize| {
        let ms = |us: i64| Some(i32::try_from(us / 1_000).unwrap());
        let mut want = (Some(0), false, false);
        for e in &events[..n] {
            let (us, imperfect) = match e.kind {
                eval::JudgeEventKind::Judged {
                    lane,
                    note,
                    judgment,
                    deviation,
                    release,
                    ..
                } => {
                    let us = match (deviation, release) {
                        (None, _) => chart.lanes[lane.index()][note].time.0 - e.time.0,
                        (Some(d), None) => -d.0,
                        (Some(d), Some(r)) => {
                            let chosen = if d.0.abs() > r.0.abs() { -d.0 } else { -r.0 };
                            if judgment >= rules::Judgment::Bad && chosen > 0 {
                                -r.0
                            } else {
                                chosen
                            }
                        }
                    };
                    (us, judgment != rules::Judgment::Perfect)
                }
                eval::JudgeEventKind::EmptyPoor { deviation, .. } => (-deviation.0, true),
                _ => continue,
            };
            want = (
                ms(us),
                imperfect && us / 1_000 > 0,
                imperfect && us / 1_000 < 0,
            );
        }
        want
    };
    // Every capture against what its own frame drew from: its play's
    // generation and candidate, and the events its view had published,
    // all of which the frame applied (the fixture's few events fit the
    // frame's effect buffer and queue, 2^14 each, so none is split or
    // dropped). No capture must show a judgment: a frame may draw a view
    // older than its time (the deterministic test,
    // `tests/recent_timing.rs`, checks each value).
    let charts = [&chart, &chart, &rearm_chart];
    let mut judged = 0;
    for name in [
        "miss", "early", "empty", "late", "held", "release", "rearmed",
    ] {
        let s = side(name);
        let generation = usize::try_from(side_num(&s, "generation")).unwrap();
        // The rearm's cancelled candidate is not its record's.
        if generation == 3 && side_num(&s, "view_generation") == 0 {
            continue;
        }
        let n = usize::try_from(side_num(&s, "events")).unwrap();
        let (_, events) = &played[generation - 1];
        assert!(n <= events.len() && n < 1 << 14, "{name}: {s}");
        if let Some(t) = events[..n].last() {
            assert!(t.time.0 <= side_num(&s, "finalized_us"), "{name}: {s}");
        }
        let want = expected(charts[generation - 1], events, n);
        assert_eq!(shown(name), want, "{name}: {s}");
        judged += usize::from(want != (Some(0), false, false));
    }
    println!("captures that showed a judgment: {judged}");
}

/// The S10-09i design's Q1 and Q2 through the real flow: a Lua result
/// skin that reads numbers 154 and 178 while it loads is laid out for the
/// own view once its best is compared (the request takes the compared
/// best, the play before's), on each play's result; and the result skin
/// settles once for each leaving command the result acts on, backs and a
/// retry, never on taking a command alone.
#[test]
fn the_result_layout_takes_the_compared_best() {
    let fx = Fixture::new();
    let a = write_chart(&fx.library(), "a", "譜面A");
    // The first play hits notes 1 and 3 (BP 2), the retry every note.
    let half =
        |t: TimeUs| (2_000_000..2_250_000).contains(&t.0) || (3_000_000..3_250_000).contains(&t.0);
    write_log(&fx.base.join("logs").join("half.tprl"), &hits(&a, half), 1);
    write_log(
        &fx.base.join("logs").join("all.tprl"),
        &hits(&a, |_| true),
        1,
    );
    let skin = fx.base.join("result");
    fs::create_dir_all(&skin).unwrap();
    let file = fs::File::create(skin.join("s.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 64, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels: Vec<u8> = (0..64 * 16).flat_map(|_| [9, 9, 9, 255]).collect();
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    fs::write(
        skin.join("result.luaskin"),
        "local skin = { type = 7, name = \"r\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         local ms = require(\"main_state\")\n\
         local nr = ms.number(154)\n\
         local y = 300\n\
         if ms.number(178) == 0 then y = 400 end\n\
         skin.source = { { id = \"s\", path = \"s.png\" } }\n\
         skin.image = { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }\n\
         skin.value = { { id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, digit = 4, ref = 71 } }\n\
         skin.destination = { { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
           { id = \"dot\", op = { 90 }, dst = { { x = 0, y = y, w = 8, h = 8 } } } }\n\
         return skin\n",
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.result]\nroot = '{}'\nentry = 'result.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/half.tprl\", until = \"end\" }",
        "result = \"back\"",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"logs/all.tprl\", until = \"end\" }",
        "result = \"retry\"",
        "play = { input = \"logs/all.tprl\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let skins = lines(&stdout, "skin: result:");
    assert!(
        skins
            .first()
            .is_some_and(|l| l.starts_with("skin: result: pending")),
        "{stdout}"
    );
    // Each play's P asked for its layout once, with the best compared; a
    // request before the comparison would see none.
    assert_eq!(
        lines(&stdout, "dev: result layout requested: "),
        ["dev: result layout requested: best known"; 3],
        "{stdout}"
    );
    // The layout read the compared view: the first play has no best
    // before it (178 hidden); the second's best is the first's BP 2, so
    // its BP 0 is 2 better; the retry's best is the second's.
    assert_eq!(
        lines(&stdout, "dev: result layout read:"),
        [
            "dev: result layout read: ResultNumber(154)=Int(1) ResultNumber(178)=Int(-2147483648)",
            "dev: result layout read: ResultNumber(154)=Int(0) ResultNumber(178)=Int(-2)",
            "dev: result layout read: ResultNumber(154)=Int(0) ResultNumber(178)=Int(0)",
        ],
        "{stdout}"
    );
    // The rig's whole path: each attempt's Preparing ran the image stage
    // and prepared the probe's images (prepare_attempt); each P laid the
    // own view out and published it (begin_result, the comparison,
    // poll_result); the result's showing asked again and reused that
    // layout (show_end, idle_wait).
    assert_eq!(
        lines(&stdout, "dev: result image stage: "),
        ["dev: result image stage: per view, images prepared"; 3],
        "{stdout}"
    );
    assert_eq!(
        lines(&stdout, "dev: result skin published: "),
        ["dev: result skin published: skin"; 6],
        "{stdout}"
    );
    // The result skin settles once for each leaving command acted on;
    // taking a command settles nothing.
    let settles = stdout
        .lines()
        .filter(|l| *l == "dev: result skin settle")
        .count();
    assert_eq!(settles, 3, "{stdout}");
    assert_eq!(
        lines(&stdout, "dev: result skin settled: "),
        ["dev: result skin settled: accepted"; 3],
        "{stdout}"
    );
}

/// What a self-made result skin reads while it loads.
#[derive(Clone, Copy, Debug)]
enum Reads {
    /// Nothing: fixed, installed at startup.
    Nothing,
    /// The chart's play level (number 96): one bundle for every view of
    /// the attempt, uploaded after its commit.
    Chart,
    /// The view's next-rank difference (number 154): each view laid out on
    /// the pages uploaded after the attempt's commit.
    View,
    /// The view's best-relative values (numbers 150, 152, 170, 172, 176,
    /// 178 and 371, options 330 and 332), raising an error unless each is
    /// the source's empty score's for a view that scored: EX 0, the
    /// difference the view's EX score (number 71), BP and its difference
    /// `i32::MIN`, no lamp, both beaten.
    EmptyBest,
}

/// A self-made Lua result skin under `fx`, chosen in `player.toml`: its EX
/// number and a status image, reading `reads` while it loads.
fn result_skin(fx: &Fixture, reads: Reads) {
    result_skin_with(fx, reads, "");
}

/// [`result_skin`] with `header` in its header table (its timing).
fn result_skin_with(fx: &Fixture, reads: Reads, header: &str) {
    let skin = fx.base.join("result");
    fs::create_dir_all(&skin).unwrap();
    let file = fs::File::create(skin.join("s.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 64, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels: Vec<u8> = (0..64 * 16).flat_map(|_| [9, 9, 9, 255]).collect();
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    let reads = match reads {
        Reads::Nothing => "",
        Reads::Chart => "local ms = require(\"main_state\")\nlocal lv = ms.number(96)\n",
        Reads::View => "local ms = require(\"main_state\")\nlocal nr = ms.number(154)\n",
        Reads::EmptyBest => {
            "local ms = require(\"main_state\")\n\
             local ex = ms.number(71)\n\
             local min = -2147483648\n\
             if ex <= 0 or ms.number(150) ~= 0 or ms.number(170) ~= 0\n\
               or ms.number(152) ~= ex or ms.number(172) ~= ex\n\
               or ms.number(176) ~= min or ms.number(178) ~= min\n\
               or ms.number(371) ~= 0\n\
               or not ms.option(330) or not ms.option(332) then\n\
               error(\"not the empty score\")\n\
             end\n"
        }
    };
    fs::write(
        skin.join("result.luaskin"),
        format!(
            "local skin = {{ type = 7, name = \"r\", w = 1920, h = 1080{header} }}\n\
             if not skin_config then return skin end\n\
             {reads}\
             skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
             skin.image = {{ {{ id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
             skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, digit = 4, ref = 71 }} }}\n\
             skin.destination = {{ {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
               {{ id = \"dot\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }} }}\n\
             return skin\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(fx.data()).unwrap();
    fs::write(
        fx.data().join("player.toml"),
        format!(
            "[skins.result]\nroot = '{}'\nentry = 'result.luaskin'\n",
            skin.display()
        ),
    )
    .unwrap();
}

/// The result's stagings and uploads printed at each point of the flow,
/// in order: `(point, stagings, uploads)`.
fn result_work(stdout: &str) -> Vec<(String, u64, u64)> {
    lines(stdout, "dev: result work at ")
        .iter()
        .map(|l| {
            let rest = l.strip_prefix("dev: result work at ").unwrap();
            let (at, counts) = rest.split_once(": ").unwrap();
            let n: Vec<u64> = counts
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            (at.to_owned(), n[0], n[1])
        })
        .collect()
}

/// The S10-09i design's C6 and C7 through the real flow, for a fixed, a
/// chart-only and a pending result skin: the result's texture is uploaded
/// once, at startup (fixed) or after the first attempt's commit; from
/// READY through the play's end and its Closing nothing of the result is
/// staged or uploaded; the views on the result stage with no upload; and
/// the retry, its atlas installed, stages and uploads nothing at its
/// commit (G4's warm reuse).
#[test]
fn the_result_uploads_once_and_never_in_play() {
    for reads in [Reads::Nothing, Reads::Chart, Reads::View] {
        let fx = Fixture::new();
        write_chart(&fx.library(), "a", "譜面A");
        result_skin(&fx, reads);
        let (out, stdout, stderr) = fx.run(&scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"next-preset\"",
            "result = \"retry\"",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]));
        assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
        let work = result_work(&stdout);
        let at: Vec<&str> = work.iter().map(|(a, ..)| a.as_str()).collect();
        assert_eq!(
            at,
            [
                "commit", "ready", "ended", "left", "commit", "ready", "ended", "left"
            ],
            "{stdout}"
        );
        let (commit, ready, ended, left) = (&work[0], &work[1], &work[2], &work[3]);
        // One upload, before READY: the startup's selection, or the
        // attempt's pages after its commit.
        assert_eq!((commit.1, commit.2), (1, 1), "{reads:?}: {stdout}");
        // Nothing from READY through the play's end and its Closing.
        assert_eq!((ready.1, ready.2), (commit.1, commit.2), "{stdout}");
        assert_eq!((ended.1, ended.2), (ready.1, ready.2), "{stdout}");
        // The views staged on the result, with no upload.
        assert!(left.1 > ended.1, "P staged: {stdout}");
        assert_eq!(left.2, ended.2, "no upload on the result: {stdout}");
        // The retry: its atlas installed, its commit does nothing; nothing
        // in its play either.
        for w in &work[4..7] {
            assert_eq!((w.1, w.2), (left.1, left.2), "{} {reads:?}: {stdout}", w.0);
        }
        assert_eq!(work[7].2, left.2, "{stdout}");
        assert!(
            lines(&stdout, "dev: result shown: built-in").is_empty(),
            "{stdout}"
        );
    }
}

/// The S10-09i design's §8.1 probes through the real flow, one line per
/// sample: G1 at each upload of the result's pages (the first attempt's;
/// with `cold`, the retry's too, its installation removed and nothing
/// retained first), G2 at each opening with what the result step did, G3
/// at each result's first frame drawn with the skin, and G5 at the step's
/// first frame of the new view.
#[test]
fn the_probes_fire_once_per_sample() {
    for (faults, uploads, steps) in [
        ("", 1, ["uploaded", "reused"]),
        ("cold", 2, ["uploaded", "uploaded"]),
    ] {
        let fx = Fixture::new();
        write_chart(&fx.library(), "a", "譜面A");
        result_skin(&fx, Reads::View);
        let (out, stdout, stderr) = fx.run_with(
            &scenario(&[
                "select = { path = \"library/a/a.bms\" }",
                "play = { input = \"auto\", until = \"end\" }",
                "result = \"next-preset\"",
                "result = \"retry\"",
                "play = { input = \"auto\", until = \"end\" }",
                "result = \"back\"",
                "quit = {}",
            ]),
            faults,
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{stdout}
{stderr}"
        );
        let g1 = lines(&stderr, "dev: probe G1 upload: ");
        assert_eq!(g1.len(), uploads, "{faults}: {stderr}");
        let cold = format!("cold {} retained 0 ", faults == "cold");
        for l in &g1 {
            assert!(l.contains(&cold), "{l}");
            assert!(!l.ends_with("done none"), "the queue's report: {l}");
        }
        let g2: Vec<&str> = lines(&stderr, "dev: probe G2 ready to open: ")
            .iter()
            .map(|l| l.rsplit(' ').next().unwrap())
            .collect();
        assert_eq!(g2, steps, "{faults}: {stderr}");
        assert_eq!(
            lines(&stderr, "dev: probe G3 save to first result: ").len(),
            2,
            "{faults}: {stderr}"
        );
        assert_eq!(
            lines(&stderr, "dev: probe G5 switch: ").len(),
            1,
            "{faults}: {stderr}"
        );
    }
}

/// C17a through the real flow: a view whose staging is refused shows the
/// built-in result with why, the next view the skin again; the run is not
/// disabled.
#[test]
fn a_refused_view_staging_shows_that_view_built_in() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::Nothing);
    let (out, stdout, stderr) = fx.run_with(
        &scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"next-preset\"",
            "result = \"prev-preset\"",
            "result = \"back\"",
            "quit = {}",
        ]),
        "stage2",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(
        lines(&stdout, "dev: result shown: "),
        [
            "dev: result shown: skin",
            "dev: result shown: built-in: dev: the view's staging was refused",
            "dev: result shown: skin",
        ],
        "{stdout}"
    );
    assert!(!stderr.contains("draws from now on"), "{stderr}");
}

/// C17b through the real flow: the after-commit upload
/// refused shows the built-in result for every view of the attempt, with
/// no layout asked for; the retry uploads again and shows the skin.
#[test]
fn a_refused_upload_shows_the_attempt_built_in() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::View);
    let (out, stdout, stderr) = fx.run_with(
        &scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"next-preset\"",
            "result = \"retry\"",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]),
        "select1",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let why = "dev: result shown: built-in: dev: the result's upload was refused";
    assert_eq!(
        lines(&stdout, "dev: result shown: "),
        [why, why, "dev: result shown: skin"],
        "{stdout}"
    );
    // Only the retry's P asked for a layout.
    assert_eq!(
        lines(&stdout, "dev: result layout requested: ").len(),
        1,
        "{stdout}"
    );
    let work = result_work(&stdout);
    assert_eq!((work[0].0.as_str(), work[0].2), ("commit", 0), "{stdout}");
    assert_eq!((work[4].0.as_str(), work[4].2), ("commit", 1), "{stdout}");
}

/// The run's fallback through the real flow (C17c, P5): a view's text
/// refused disables the result skin for the run; the views of that result
/// and of the next attempt are still presented, each the built-in result
/// with why, and no skin work is done for the retry (no image stage, no
/// layout, no upload).
#[test]
fn the_run_fallback_keeps_presenting_the_built_in_result() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::View);
    let (out, stdout, stderr) = fx.run_with(
        &scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"next-preset\"",
            "result = \"retry\"",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"next-preset\"",
            "result = \"back\"",
            "quit = {}",
        ]),
        "text1",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let why = "dev: result shown: built-in: dev: the view's text was refused";
    assert_eq!(lines(&stdout, "dev: result shown: "), [why; 4], "{stdout}");
    // The retry did no skin work.
    assert_eq!(
        lines(&stdout, "dev: result image stage: ").len(),
        1,
        "{stdout}"
    );
    assert_eq!(
        lines(&stdout, "dev: result layout requested: ").len(),
        1,
        "{stdout}"
    );
    let work = result_work(&stdout);
    assert_eq!(work[4].0, "commit", "{stdout}");
    assert_eq!((work[4].1, work[4].2), (work[3].1, work[3].2), "{stdout}");
}

/// How long after its outro began each result was left (dev-tools'
/// line), in µs.
fn left_after_outro(stdout: &str) -> Vec<i64> {
    lines(stdout, "dev: result left ")
        .iter()
        .map(|l| {
            l.strip_prefix("dev: result left ")
                .and_then(|r| r.split_once(" us"))
                .unwrap()
                .0
                .parse()
                .unwrap()
        })
        .collect()
}

/// An autoplay's result, which has no best lookup (`Parts::unsaved`),
/// through the real flow: a result skin reading its best-relative values
/// while it loads reads the source's empty score for the own view and the
/// next preset's, and draws (design.md "Personal Bests", skin
/// projections).
#[test]
fn an_autoplay_result_skin_reads_the_empty_best() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::EmptyBest);
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"next-preset\"",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let shown = lines(&stdout, "dev: result shown: ");
    assert!(shown.len() >= 2, "{stdout}");
    assert!(
        shown.iter().all(|l| *l == "dev: result shown: skin"),
        "{stdout}"
    );
}

/// C14 through the real flow (P4): a skin's declared `fadeout` delays back
/// and retry until its outro has played, never less; the view is pinned
/// when each is accepted.
#[test]
fn the_skins_outro_delays_back_and_retry() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin_with(&fx, Reads::Nothing, ", fadeout = 400, input = 0");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"retry\"",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let left = left_after_outro(&stdout);
    assert_eq!(left.len(), 2, "{stdout}");
    assert!(left.iter().all(|&us| us >= 400_000), "{left:?}");
    assert_eq!(
        lines(&stdout, "dev: result outro began, view Some(").len(),
        2,
        "{stdout}"
    );
}

/// C16 through the real flow: a positive `scene` with no command leaves
/// the result (back) after its outro, so selection takes the next step.
#[test]
fn the_skins_scene_time_leaves_the_result() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin_with(&fx, Reads::Nothing, ", fadeout = 200, scene = 1500");
    let (out, stdout, stderr) = fx.run(&scenario(&[
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"stay\"",
        // Reached only once the first result left on its own.
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let left = left_after_outro(&stdout);
    assert_eq!(left.len(), 2, "{stdout}");
    assert!(left.iter().all(|&us| us >= 200_000), "{left:?}");
}

/// C17f, C23 joined: a screen lost under a 5 s outro's barrier whose
/// result restore is refused falls back for the run, and the flow serves
/// back (accepted by the scene time) once, the result shown built-in. When
/// it is served (the first look after the rebuild, before the outro's
/// end, T16) is `game`'s fixed-clock test
/// `a_screen_lost_mid_outro_serves_back_after_its_refused_rebuild`: the
/// rebuild on WARP takes about 0.5 s idle and up to 5 s under load.
#[test]
fn a_screen_lost_mid_outro_with_its_restore_refused_serves_back() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin_with(&fx, Reads::Nothing, ", fadeout = 5000, scene = 500");
    let (out, stdout, stderr) = fx.run_with(
        &scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            // Back is accepted by the scene time; the result shown again
            // after the rebuild takes no step.
            "result = \"stay\"",
            "quit = {}",
        ]),
        "lostpinned3",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("dev: the result's restore was refused"),
        "the screen was built again, its result restore refused: {stderr}"
    );
    assert_eq!(left_after_outro(&stdout).len(), 1, "{stdout}");
    assert_eq!(
        lines(&stdout, "dev: result shown: built-in").len(),
        1,
        "{stdout}"
    );
}

/// C17f through the real flow (P5): a frame refused on a healthy device
/// while back waits on a 5 s outro disables the skin for the run, and back
/// is served once, the result shown built-in. When it is served (the next
/// look, not the outro's end) is `game`'s fixed-clock test
/// `a_fallback_during_an_outro_serves_the_command`.
#[test]
fn a_fallback_during_the_outro_serves_back_at_once() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin_with(&fx, Reads::Nothing, ", fadeout = 5000");
    let (out, stdout, stderr) = fx.run_with(
        &scenario(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]),
        "pinned3",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!(left_after_outro(&stdout).len(), 1, "{stdout}");
    assert_eq!(
        lines(&stdout, "dev: result shown: built-in").len(),
        1,
        "{stdout}"
    );
}

/// The sidecar of capture `name` under the run's output.
fn side_of(fx: &Fixture, name: &str) -> String {
    fs::read_to_string(fx.base.join("out").join(format!("{name}.json"))).unwrap()
}

/// [`scenario`] at 640×360.
fn small(steps: &[&str]) -> String {
    scenario(steps).replacen(
        "output = \"out\"\n",
        "output = \"out\"\nsize = [640, 360]\n",
        1,
    )
}

/// I1, I2: each holding selection capture takes a frame of the update its
/// own cursor step published, before the next step's: its revision is at
/// least its stamp's and below the next capture's stamp.
#[test]
fn held_select_captures_follow_their_own_cursor_steps() {
    let fx = Fixture::new();
    for (c, title) in [("a", "譜面A"), ("b", "譜面B"), ("c", "譜面C")] {
        write_chart(&fx.library(), c, title);
    }
    let (out, stdout, stderr) = fx.run(&small(&[
        "cursor = { path = \"library/b/b.bms\" }",
        "capture = { select_at = 250, file = \"b.png\" }",
        "cursor = { path = \"library/c/c.bms\" }",
        "capture = { select_at = 250, file = \"c.png\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let (b, c) = (side_of(&fx, "b"), side_of(&fx, "c"));
    for side in [&b, &c] {
        assert_eq!(side_value(side, "screen"), "selection", "{side}");
        assert_eq!(side_value(side, "axis"), "select", "{side}");
        assert!(side_num(side, "select_us") >= 250_000, "{side}");
        assert!(
            side_num(side, "selection_revision") >= side_num(side, "stamp_revision"),
            "{side}"
        );
    }
    assert!(
        side_num(&b, "selection_revision") < side_num(&c, "stamp_revision"),
        "{b}\n{c}"
    );
    let png = |n: &str| fs::read(fx.base.join("out").join(format!("{n}.png"))).unwrap();
    assert_ne!(png("b"), png("c"));
}

/// A selection capture that does not hold fails when the selection is
/// left before its frame, saying so.
#[test]
fn a_select_capture_left_before_its_frame_fails() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { select_at = 600000, hold = false, file = \"s.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "step 1 (capture): the selection left before a frame at 600000 ms on its select axis"
        ),
        "{stderr}"
    );
}

/// I2 on a result: back waits for a holding capture of the built-in
/// result, which is taken 400 ms after the result's first frame.
#[test]
fn held_result_capture_defers_back() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 400, file = \"r.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = { miss = \"all\" }, until = { at_ms = 1500, command = \"quit\" } }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let side = side_of(&fx, "r");
    assert_eq!(side_value(&side, "phase"), "result", "{side}");
    assert_eq!(side_value(&side, "result_drawn"), "built-in", "{side}");
    assert!(side_num(&side, "result_us") >= 400_000, "{side}");
    assert_eq!(side_num(&side, "result_seq"), 1, "{side}");
}

/// I3: a result's origin is its first presented frame, whatever view it
/// shows later; a capture written on the result after a view command
/// takes a frame of the view that command made.
#[test]
fn a_result_origin_survives_a_view_change() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::Nothing);
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 0, file = \"r0.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"next-preset\"",
        "capture = { result_at = 400, file = \"r1.png\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let (r0, r1) = (side_of(&fx, "r0"), side_of(&fx, "r1"));
    assert_eq!(
        side_num(&r0, "presented_us"),
        side_num(&r1, "result_origin_us"),
        "{r0}\n{r1}"
    );
    assert_eq!(side_num(&r0, "result_seq"), side_num(&r1, "result_seq"));
    assert!(
        side_num(&r1, "result_view") >= side_num(&r1, "stamp_view"),
        "{r1}"
    );
    assert!(
        side_num(&r1, "stamp_view") > side_num(&r0, "result_view"),
        "{r0}\n{r1}"
    );
    assert!(side_num(&r1, "result_us") >= 400_000, "{r1}");
    assert_eq!(side_value(&r0, "result_drawn"), "skin", "{r0}");
}

/// A capture that does not hold, past the skin's scene time, fails when
/// the result leaves on its own.
#[test]
fn a_result_capture_past_the_scene_fails() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin_with(&fx, Reads::Nothing, ", fadeout = 200, scene = 1500");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 600000, hold = false, file = \"r.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"stay\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "step 1 (capture): the result left before a frame at 600000 ms on its result axis"
        ),
        "{stderr}"
    );
}

/// `play --dev-frames` writes a skinned result's first frame as
/// `end.png`.
#[test]
fn the_frame_dump_writes_a_skinned_results_end_frame() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::Nothing);
    let frames = fx.base.join("frames");
    fs::create_dir_all(&frames).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapestry"))
        .arg("play")
        .arg(fx.library().join("a").join("a.bms"))
        .args(["--auto", "--dev-headless", "--dev-size", "320x180"])
        .arg("--dev-frames")
        .arg(&frames)
        .args(["--dev-render-timeout", "600"])
        .env("LOCALAPPDATA", &fx.appdata)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        !lines(&stdout, "dev: result shown: skin").is_empty(),
        "{stdout}"
    );
    assert!(frames.join("end.png").exists(), "{stdout}");
    assert!(!frames.join("select.png").exists(), "{stdout}");
}

/// I3 through a device loss: a built-in result's screen lost and built
/// again keeps its identity and origin.
#[test]
fn a_built_in_result_keeps_its_identity_through_recovery() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run_with(
        &small(&[
            "capture = { result_at = 0, file = \"r0.png\" }",
            "capture = { result_at = 800, file = \"r1.png\" }",
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]),
        "lostresult2",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stderr.contains("render: "), "no loss: {stderr}");
    let (r0, r1) = (side_of(&fx, "r0"), side_of(&fx, "r1"));
    assert_eq!(side_num(&r0, "result_seq"), side_num(&r1, "result_seq"));
    assert_eq!(
        side_num(&r0, "result_origin_us"),
        side_num(&r1, "result_origin_us")
    );
    assert!(side_num(&r1, "result_us") >= 800_000, "{r1}");
}

/// A refused result skin's built-in page names the same result.
#[test]
fn a_refused_result_skin_keeps_its_identity() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    result_skin(&fx, Reads::Chart);
    let (out, stdout, stderr) = fx.run_with(
        &small(&[
            "capture = { result_at = 300, file = \"r.png\" }",
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]),
        "select1",
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let side = side_of(&fx, "r");
    assert_eq!(side_num(&side, "result_seq"), 1, "{side}");
    assert_ne!(side_value(&side, "result_drawn"), "skin", "{side}");
}

/// Two results of a run have two numbers and two origins.
#[test]
fn two_results_have_two_identities() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 0, file = \"r1.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "capture = { result_at = 0, file = \"r2.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "result = \"back\"",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let (r1, r2) = (side_of(&fx, "r1"), side_of(&fx, "r2"));
    assert_eq!(side_num(&r1, "result_seq"), 1, "{r1}");
    assert_eq!(side_num(&r2, "result_seq"), 2, "{r2}");
    assert!(
        side_num(&r2, "result_origin_us") > side_num(&r1, "presented_us"),
        "{r1}\n{r2}"
    );
}

/// A close on a result fails its capture whose frame was not taken,
/// saying the run ended.
#[test]
fn a_close_on_a_result_fails_its_capture() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 600000, hold = false, file = \"r.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(
            "step 1 (capture): the run ended before a frame at 600000 ms on its result axis"
        ),
        "{stderr}"
    );
}

/// A scripted quit on a result waits for its holding capture.
#[test]
fn held_result_capture_defers_quit() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.run(&small(&[
        "capture = { result_at = 400, file = \"r.png\" }",
        "select = { path = \"library/a/a.bms\" }",
        "play = { input = \"auto\", until = \"end\" }",
        "quit = {}",
    ]));
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(side_num(&side_of(&fx, "r"), "result_us") >= 400_000);
}

/// `stay` on a result with no scene time in force fails at once: it would
/// never leave.
#[test]
fn stay_without_scene_time_fails() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    // Bounded: a stay that waits for good never ends the run.
    let (out, stdout, stderr) = fx
        .run_bounded(
            &small(&[
                "select = { path = \"library/a/a.bms\" }",
                "play = { input = \"auto\", until = \"end\" }",
                "result = \"stay\"",
                "quit = {}",
            ]),
            std::time::Duration::from_secs(300),
        )
        .expect("the run did not end within 300 s");
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("stay needs a result skin's scene time; none is in force"),
        "{stderr}"
    );
}

/// A capture whose frame is being written when its result is left is not
/// failed by the leaving.
#[test]
fn a_saving_capture_survives_departure() {
    let fx = Fixture::new();
    write_chart(&fx.library(), "a", "譜面A");
    let (out, stdout, stderr) = fx.output(
        fx.command(&small(&[
            "capture = { result_at = 0, hold = false, file = \"r.png\" }",
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"auto\", until = \"end\" }",
            "result = \"back\"",
            "quit = {}",
        ]))
        .env("TAPESTRY_DEV_CAPTURE_STALL_MS", "2000"),
    );
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(fx.base.join("out").join("r.png").exists());
}
