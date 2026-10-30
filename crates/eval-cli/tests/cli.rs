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
    played_with(chart, 0)
}

/// [`played`], with the chart loaded with `seed`.
fn played_with(chart: &str, seed: u64) -> PlayRecord {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        seed,
        ..LoadOptions::default()
    };
    let chart = load_bms(chart.as_bytes(), dir.path(), &opts).unwrap().chart;
    PlayRecord {
        versions: Versions {
            evaluator: rules::Implementation::STANDARD_VERSION,
            app: "test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord {
            identity: chart.identity,
            random_seed: seed,
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
            wake: replay::WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
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
        "other.tprl | recorded preset | refused: the chart resolves to another chart than the \
         record's"
    );
    // A refusal does not stop the records after it.
    assert!(
        lines[1].starts_with("play.tprl | recorded preset | as played | "),
        "{out}"
    );
    assert_eq!(
        lines[2],
        format!(
            "old.tprl | recorded preset | refused: the record was made by evaluator version {}, \
             which this build does not run",
            old.versions.evaluator
        )
    );
    assert!(
        lines[3].starts_with("junk.tprl | recorded preset | refused: "),
        "{out}"
    );
    assert!(
        lines[4].starts_with("missing.tprl | recorded preset | refused: "),
        "{out}"
    );
}

/// A chart with `#RANDOM 2`: branch `bad` holds a measure past the loader's
/// limit, so the chart loads only when the other branch, `CHART`'s two
/// taps, is drawn.
fn random_chart(bad: u32) -> String {
    let good = 3 - bad;
    format!(
        "#BPM 120\n#WAV01 a.wav\n#RANDOM 2\n#IF {good}\n#00111:01\n#00211:01\n#ENDIF\n\
         #IF {bad}\n#100011:01\n#ENDIF\n#ENDRANDOM\n"
    )
}

fn loads(text: &str, seed: u64) -> bool {
    let opts = LoadOptions {
        seed,
        ..LoadOptions::default()
    };
    load_bms(text.as_bytes(), Path::new("."), &opts).is_ok()
}

/// The random chart whose bad branch the default seed draws, a seed that
/// loads it, and one that does not.
fn random_failing_at_default_seed() -> (String, u64, u64) {
    let text = [random_chart(1), random_chart(2)]
        .into_iter()
        .find(|t| !loads(t, 0))
        .unwrap();
    let good = (1..64).find(|&s| loads(&text, s)).unwrap();
    let bad = (1..64).find(|&s| !loads(&text, s)).unwrap();
    (text, good, bad)
}

#[test]
fn a_random_chart_is_checked_under_the_recorded_seeds() {
    let (text, good, bad) = random_failing_at_default_seed();
    let dir = folder();
    fs::write(dir.path().join("random.bms"), &text).unwrap();
    save(dir.path(), "good.tprl", &played_with(&text, good));
    let mut failing = played_with(&text, good);
    failing.chart.random_seed = bad;
    save(dir.path(), "bad.tprl", &failing);
    let presets = ["--preset", "own.toml", "--preset", "wide.toml"];
    let with = |records: &[&str]| {
        let mut args = vec!["--chart", "random.bms"];
        args.extend(presets);
        args.extend(records);
        run(dir.path(), &args)
    };
    // The chart fails under the default seed, but loads under the record's:
    // the record is evaluated.
    let o = with(&["good.tprl"]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(
        stdout(&o),
        format!(
            "good.tprl | preset own.toml | as played | {CUSTOM_TOTALS}\n\
             good.tprl | preset wide.toml | what-if, no input after 4.180 s | {CUSTOM_TOTALS}\n"
        )
    );
    // A record whose own seed fails, beside one whose seed loads, is
    // refused on its own.
    let o = with(&["bad.tprl", "good.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(o.stderr.is_empty(), "{o:?}");
    let reason = "refused: the chart cannot be loaded: line 9: measure 1000 is out of range";
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[..2],
        [
            format!("bad.tprl | preset own.toml | {reason}"),
            format!("bad.tprl | preset wide.toml | {reason}"),
        ],
        "{out}"
    );
    assert_eq!(lines.len(), 4, "{out}");
    // Under every recorded seed the chart fails: the run ends before any
    // line, with the first seed's error. An undecodable record adds no seed.
    fs::write(dir.path().join("junk.tprl"), b"not a record").unwrap();
    let o = with(&["junk.tprl", "bad.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(o.stdout.is_empty(), "{o:?}");
    assert_eq!(
        String::from_utf8(o.stderr).unwrap(),
        "eval-cli: random.bms: line 9: measure 1000 is out of range\n"
    );
    // With no record that decodes, the default seed decides.
    let o = with(&["junk.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(o.stdout.is_empty(), "{o:?}");
    let o = run(dir.path(), &["--chart", "chart.bms", "junk.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert!(
        stdout(&o).starts_with("junk.tprl | recorded preset | refused: "),
        "{o:?}"
    );
}

#[test]
fn incomplete_record_is_refused() {
    type Set = fn(&mut Incomplete);
    let reasons: [(Set, &str); 4] = [
        (|i| i.overflow = true, "overflow"),
        (|i| i.source_limit = true, "source limit"),
        (|i| i.suspend = true, "suspend"),
        (|i| i.input_failed = true, "input failed"),
    ];
    let dir = folder();
    for (set, why) in reasons {
        let mut record = played(CHART);
        set(&mut record.status.incomplete);
        save(dir.path(), "play.tprl", &record);
        let o = run(dir.path(), &["--chart", "chart.bms", "play.tprl"]);
        assert_eq!(o.status.code(), Some(1), "{why}: {o:?}");
        assert_eq!(
            stdout(&o),
            format!(
                "play.tprl | recorded preset | refused: the record's input is incomplete ({why})\n"
            )
        );
    }
    // Every reason holds, on a record of another chart: incompleteness is
    // checked before verification, so it is the refusal, and the chart is
    // not loaded for a record that would be refused anyway.
    let mut record = played(OTHER_CHART);
    for (set, _) in reasons {
        set(&mut record.status.incomplete);
    }
    save(dir.path(), "play.tprl", &record);
    let o = run(dir.path(), &["--chart", "chart.bms", "play.tprl"]);
    assert_eq!(o.status.code(), Some(1), "{o:?}");
    assert_eq!(
        stdout(&o),
        "play.tprl | recorded preset | refused: the record's input is incomplete (overflow, \
         source limit, suspend, input failed)\n"
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

/// `text` padded with a comment line to exactly `len` bytes.
fn padded(text: &str, len: usize) -> String {
    let mut s = format!("{text}#");
    s.push_str(&"x".repeat(len - s.len()));
    s
}

#[test]
fn an_unusable_chart_or_preset_exits_one_before_any_line() {
    let dir = folder();
    save(dir.path(), "play.tprl", &played(CHART));
    fs::write(dir.path().join("bad.toml"), "id = 1\n").unwrap();
    let own = rules_toml::preset_to_toml(&rules::custom());
    let limit = rules_toml::MAX_PRESET_FILE_BYTES;
    fs::write(dir.path().join("big.toml"), padded(&own, limit + 1)).unwrap();
    fs::write(dir.path().join("bad.bms"), "#LNTYPE 2\n#00111:01\n").unwrap();
    let chart_limit = LoadOptions::default().limits.max_file_bytes;
    fs::write(dir.path().join("big.bms"), padded(CHART, chart_limit + 1)).unwrap();
    for (args, reason) in [
        (&["--chart", "missing.bms", "play.tprl"][..], None),
        (
            &["--chart", "bad.bms", "play.tprl"],
            Some("eval-cli: bad.bms: line 1: unsupported #LNTYPE 2"),
        ),
        (
            &["--chart", "big.bms", "play.tprl"],
            Some(
                "eval-cli: big.bms: the file is larger than 16777216 bytes, the largest chart \
                 accepted",
            ),
        ),
        (
            &[
                "--chart",
                "chart.bms",
                "--preset",
                "missing.toml",
                "play.tprl",
            ],
            None,
        ),
        (
            &[
                "--chart",
                "chart.bms",
                "--preset",
                "own.toml",
                "--preset",
                "bad.toml",
                "play.tprl",
            ],
            None,
        ),
        (
            &["--chart", "chart.bms", "--preset", "big.toml", "play.tprl"],
            Some(
                "eval-cli: big.toml: the file is larger than 1048576 bytes, the largest preset \
                 file accepted",
            ),
        ),
    ] {
        let o = run(dir.path(), args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {o:?}");
        assert!(o.stdout.is_empty(), "{args:?}: {o:?}");
        let err = String::from_utf8(o.stderr).unwrap();
        let named = args
            .iter()
            .find(|a| ["missing", "bad", "big"].iter().any(|n| a.contains(n)))
            .unwrap();
        assert!(err.starts_with(&format!("eval-cli: {named}: ")), "{err}");
        if let Some(reason) = reason {
            assert_eq!(err.trim_end(), reason);
        }
    }
    // A preset file of exactly the limit is read.
    fs::write(dir.path().join("big.toml"), padded(&own, limit)).unwrap();
    let o = run(
        dir.path(),
        &["--chart", "chart.bms", "--preset", "big.toml", "play.tprl"],
    );
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(
        stdout(&o),
        format!("play.tprl | preset big.toml | as played | {CUSTOM_TOTALS}\n")
    );
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
    // Records, then presets, in the order given, each line labelled.
    let out = stdout(&first);
    let heads: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.splitn(4, " | ").take(3).collect())
        .collect();
    let refused = "refused: the chart resolves to another chart than the record's";
    assert_eq!(
        heads,
        [
            [
                "a.tprl",
                "preset wide.toml",
                "what-if, no input after 4.180 s"
            ],
            ["a.tprl", "preset own.toml", "as played"],
            [
                "b.tprl",
                "preset wide.toml",
                "what-if, ended midway at 3.000 s"
            ],
            [
                "b.tprl",
                "preset own.toml",
                "as played, ended midway at 3.000 s"
            ],
            ["c.tprl", "preset wide.toml", refused],
            ["c.tprl", "preset own.toml", refused],
        ],
        "{out}"
    );
}
