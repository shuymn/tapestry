//! Runs the built `eval-cli` on charts, presets and records made here.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use bms_model::{Lane, TimeUs};
use bms_parser::{LoadOptions, load_bms};
use eval::{
    Binding, ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile,
};
use replay::{
    BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete, PASSTHROUGH,
    PlayRecord, PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};
use rules::{RulePreset, Window};
use tempfile::TempDir;

/// Taps on key 1 at 2 s and 4 s (120 BPM, a measure is 2 s).
const CHART: &str = "#BPM 120\n#WAV01 a.wav\n#00111:01\n#00211:01\n";

/// The same taps a measure later, so it resolves to another chart.
const OTHER_CHART: &str = "#BPM 120\n#WAV01 a.wav\n#00211:01\n#00311:01\n";

const MS: i64 = 1_000;

/// The custom preset with its BAD window widened to 220 ms, whose final
/// deadline comes after the recording's end.
fn wide() -> RulePreset {
    let mut p = rules::custom();
    p.id = "wide".to_owned();
    p.windows.bad = Window::symmetric(220 * MS);
    p
}

fn press(at: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(1),
            value,
        },
    }
}

/// A completed play of `chart` under the custom preset, from one keyboard
/// whose keys 1-7 are bound to lanes 1-7, hitting taps at 2 s and 4 s on
/// time. It completed at 4.180 s.
fn played(chart: &str) -> PlayRecord {
    let dir = tempfile::tempdir().unwrap();
    let chart = load_bms(chart.as_bytes(), dir.path(), &LoadOptions::default())
        .unwrap()
        .chart;
    PlayRecord {
        versions: Versions {
            evaluator: rules::Implementation::STANDARD_VERSION,
            app: "test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord {
            identity: chart.identity,
            random_seed: 0,
            transform: PASSTHROUGH.record(),
        },
        preset: PresetRecord::of(&rules::custom()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: SourceProfile {
                    input_offset: TimeUs(0),
                    bindings: (1..=7)
                        .map(|n| Binding {
                            control: ControlId(n),
                            lane: Lane::new(u8::try_from(n).unwrap()).unwrap(),
                        })
                        .collect(),
                    axes: Vec::new(),
                },
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
        },
        play_type: PlayType::Full,
        start: InputStart {
            opened_at: TimeUs(0),
            held: Vec::new(),
        },
        events: vec![
            press(2_000 * MS, 1),
            press(2_050 * MS, 0),
            press(4_000 * MS, 1),
            press(4_050 * MS, 0),
        ],
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized: TimeUs(4_180 * MS),
            incomplete: Incomplete::default(),
        },
    }
}

/// A folder holding `chart.bms`, `own.toml` (the custom preset) and
/// `wide.toml`.
fn folder() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("chart.bms"), CHART).unwrap();
    let own = rules_toml::preset_to_toml(&rules::custom());
    fs::write(dir.path().join("own.toml"), own).unwrap();
    fs::write(
        dir.path().join("wide.toml"),
        rules_toml::preset_to_toml(&wide()),
    )
    .unwrap();
    dir
}

fn save(dir: &Path, name: &str, record: &PlayRecord) {
    fs::write(dir.join(name), replay::encode(record)).unwrap();
}

/// Runs eval-cli in `dir` with `args`.
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eval-cli"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

/// The totals after two PERFECTs, with the custom preset's gauges cleared.
const CUSTOM_TOTALS: &str = "perfect 2 great 0 good 0 bad 0 poor 0 | ex 4 | bp 0 | max combo 2 \
                             | normal 100.000% cleared | survival 100.000% cleared";

#[test]
fn prints_as_played_and_what_if() {
    let dir = folder();
    save(dir.path(), "play.tprl", &played(CHART));
    let o = run(
        dir.path(),
        &[
            "--chart",
            "chart.bms",
            "--preset",
            "own.toml",
            "--preset",
            "wide.toml",
            "play.tprl",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    // Under 220 ms the final deadline is 4.220 s, after the recording's
    // end: the what-if completes and clears, with no input after 4.180 s.
    assert_eq!(
        stdout(&o),
        format!(
            "play.tprl | preset own.toml | as played | {CUSTOM_TOTALS}\n\
             play.tprl | preset wide.toml | what-if, no input after 4.180 s | {CUSTOM_TOTALS}\n"
        )
    );
    // Without a preset, the record's own copy is used.
    let o = run(dir.path(), &["--chart", "chart.bms", "play.tprl"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(
        stdout(&o),
        format!("play.tprl | recorded preset | as played | {CUSTOM_TOTALS}\n")
    );
}

#[test]
fn refuses_with_the_reason_and_exits_one() {
    let dir = folder();
    save(dir.path(), "play.tprl", &played(CHART));
    save(dir.path(), "other.tprl", &played(OTHER_CHART));
    let mut old = played(CHART);
    old.versions.evaluator = rules::Implementation::STANDARD_VERSION + 1;
    save(dir.path(), "old.tprl", &old);
    fs::write(dir.path().join("junk.tprl"), b"not a record").unwrap();
    let o = run(
        dir.path(),
        &[
            "--chart",
            "chart.bms",
            "other.tprl",
            "play.tprl",
            "old.tprl",
            "junk.tprl",
            "missing.tprl",
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(
        lines[0],
        "other.tprl | refused: the chart resolves to another chart than the record's"
    );
    // A refusal does not stop the records after it.
    assert!(
        lines[1].starts_with("play.tprl | recorded preset | as played | "),
        "{out}"
    );
    assert_eq!(
        lines[2],
        format!(
            "old.tprl | refused: the record was made by evaluator version {}, which this \
             build does not run",
            old.versions.evaluator
        )
    );
    assert!(lines[3].starts_with("junk.tprl | refused: "), "{out}");
    assert!(lines[4].starts_with("missing.tprl | refused: "), "{out}");
}

#[test]
fn incomplete_record_is_refused() {
    let dir = folder();
    let mut record = played(CHART);
    record.status.incomplete.suspend = true;
    record.status.incomplete.overflow = true;
    save(dir.path(), "play.tprl", &record);
    let o = run(dir.path(), &["--chart", "chart.bms", "play.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert_eq!(
        stdout(&o),
        "play.tprl | refused: the record's input is incomplete (overflow, suspend)\n"
    );
}

#[test]
fn usage_error_exits_two() {
    let dir = folder();
    save(dir.path(), "play.tprl", &played(CHART));
    for args in [
        &[][..],
        &["play.tprl"],
        &["--chart", "chart.bms"],
        &["--chart", "chart.bms", "--preset"],
        &["--chart", "chart.bms", "--chart", "chart.bms", "play.tprl"],
        &["--chart", "chart.bms", "--verbose", "play.tprl"],
    ] {
        let o = run(dir.path(), args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {o:?}");
        assert!(o.stdout.is_empty(), "{args:?}: {o:?}");
        let err = String::from_utf8(o.stderr).unwrap();
        assert!(
            err.trim_end().ends_with(
                "usage: eval-cli --chart <chart> [--preset <file.toml>]... <record.tprl>..."
            ),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn an_unusable_chart_or_preset_exits_one_before_any_line() {
    let dir = folder();
    save(dir.path(), "play.tprl", &played(CHART));
    fs::write(dir.path().join("bad.toml"), "id = 1\n").unwrap();
    for args in [
        &["--chart", "missing.bms", "play.tprl"][..],
        &[
            "--chart",
            "chart.bms",
            "--preset",
            "missing.toml",
            "play.tprl",
        ],
        &[
            "--chart",
            "chart.bms",
            "--preset",
            "own.toml",
            "--preset",
            "bad.toml",
            "play.tprl",
        ],
    ] {
        let o = run(dir.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
        assert!(o.stdout.is_empty(), "{args:?}: {o:?}");
        let err = String::from_utf8(o.stderr).unwrap();
        let named = args
            .iter()
            .find(|a| a.contains("missing") || a.contains("bad"));
        assert!(
            err.starts_with(&format!("eval-cli: {}: ", named.unwrap())),
            "{err}"
        );
    }
}

#[test]
fn output_is_deterministic() {
    let dir = folder();
    save(dir.path(), "a.tprl", &played(CHART));
    let mut aborted = played(CHART);
    aborted.status.end = EndStatus::EndedMidway;
    aborted.status.finalized = TimeUs(3_000 * MS);
    save(dir.path(), "b.tprl", &aborted);
    save(dir.path(), "c.tprl", &played(OTHER_CHART));
    let args = [
        "--chart",
        "chart.bms",
        "--preset",
        "wide.toml",
        "--preset",
        "own.toml",
        "a.tprl",
        "b.tprl",
        "c.tprl",
    ];
    let first = run(dir.path(), &args);
    let second = run(dir.path(), &args);
    assert_eq!(first.status.code(), Some(1), "{first:?}");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.status, second.status);
    // Records, then presets, in the order given.
    let out = stdout(&first);
    let heads: Vec<&str> = out
        .lines()
        .map(|l| l.splitn(4, " | ").take(3).last().unwrap())
        .collect();
    assert_eq!(
        heads,
        [
            "what-if, no input after 4.180 s",
            "as played",
            "what-if, ended midway at 3.000 s",
            "as played, ended midway at 3.000 s",
            "refused: the chart resolves to another chart than the record's",
        ],
        "{out}"
    );
}
