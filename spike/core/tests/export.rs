//! P1 test (i): export round trip and the leak scan.

use serde_json::{Map, Value, json};
use spike_core::conditions::{DisplayMode, GpuState, HybridPresent, PstateShare, PublicConditions};
use spike_core::evaluate::evaluate;
use spike_core::export::{
    CONDITIONS, Export, ExportError, FRAMES, Finding, INPUT, REPORT, RUN, SUMMARY, ScanContext,
    build, read, recompute, scan,
};
use spike_core::obslog::{WallWindow, derive};
use spike_core::run::RunData;
use spike_core::synth::{passing_obs_run, passing_run};

const QPC_LO: u64 = 52_000_000_000;
const QPC_HI: u64 = 52_000_000_000 + 700 * 10_000_000;

fn ctx() -> ScanContext {
    ScanContext {
        forbidden_names: vec![
            "DESKTOP-SYNTH01".into(),
            "synthuser".into(),
            "Speakers (Synth Audio)".into(),
        ],
        qpc_range: Some((QPC_LO, QPC_HI)),
    }
}

fn private_conditions() -> Map<String, Value> {
    let v = json!({
        "commit": "0123abcd",
        "dirty": false,
        "cpu_brand": "Synthetic CPU 8-Core",
        "refresh_num": 60,
        "refresh_den": 1,
        "input": "Synthetic only",
        // Private fields that must be dropped:
        "computer_name": "DESKTOP-SYNTH01",
        "user_name": "synthuser",
        "exe_path": "C:\\Users\\synthuser\\spike\\tapestry-spike.exe",
        "audio_device_name": "Speakers (Synth Audio)",
        "origin_qpc": QPC_LO + 5,
        "wall_clock_start": "2026-10-24T21:00:10+09:00",
        "gpu_adapters": [{ "name": "Synthetic GPU", "vendor_id": 1 }]
    });
    v.as_object().unwrap().clone()
}

fn conditions() -> PublicConditions {
    PublicConditions::from_conditions(&private_conditions()).unwrap()
}

/// A name planted in the public conditions shows up in both files that
/// carry them.
fn name_findings() -> Vec<Finding> {
    vec![
        Finding::Name(CONDITIONS.into()),
        Finding::Name(REPORT.into()),
    ]
}

fn obs_run() -> RunData {
    let mut run = passing_obs_run();
    let log = "21:00:00.000: [rtmp stream: 'adv_stream'] Connecting to RTMP URL rtmp://192.0.2.10/live...\n\
21:00:00.040: [rtmp stream: 'adv_stream'] Connection to rtmp://192.0.2.10/live successful\n\
21:00:00.060: [x264 encoder: 'advanced_video_stream'] settings:\n\
21:00:00.060: \trate_control: CBR\n\
21:00:00.060: \tbitrate:      6000\n\
21:00:00.060: \tfps_num:      60\n\
21:00:00.060: \tfps_den:      1\n\
21:00:00.060: \twidth:        1920\n\
21:00:00.060: \theight:       1080\n\
21:00:00.100: ==== Streaming Start ===============================================\n\
21:00:05.000: [game-capture: 'Game'] hooked to C:\\Users\\synthuser\\spike\\tapestry-spike.exe\n\
21:10:30.000: Output 'adv_stream': Total frames output: 37800\n\
21:10:30.000: Output 'adv_stream': Total drawn frames: 37600 (37800 attempted)\n\
21:10:30.000: Output 'adv_stream': Number of lagged frames due to rendering lag/stalls: 200 (0.5%)\n\
21:10:30.010: ==== Streaming Stop ================================================\n";
    let h = 3_600_000;
    run.obs = Some(derive(
        log,
        WallWindow {
            start_ms: 21 * h + 10_000,
            end_ms: 21 * h + 610_000,
        },
    ));
    run
}

#[test]
fn i_round_trip_gives_an_identical_summary() {
    // Also a run on a non-nominal (in-band) refresh rate with a cap.
    let mut odd = passing_run();
    odd.info.refresh = spike_core::run::Rational {
        num: 59_951,
        den: 1_000,
    };
    odd.info.kind = spike_core::run::RunKind::Diagnostic;
    odd.info.fps_cap = Some(30);
    for run in [passing_run(), obs_run(), odd] {
        let export = build(&run, &conditions(), &ctx()).expect("clean export");
        assert_eq!(read(&export).unwrap(), run);
        let r = recompute(&export).unwrap();
        assert!(r.matches_recorded);
        assert_eq!(r.summary, evaluate(&run));
    }
    // A tampered summary is detected.
    let mut export = build(&obs_run(), &conditions(), &ctx()).unwrap();
    let mut s: Value = serde_json::from_slice(&export.files[SUMMARY]).unwrap();
    s["exit_code"] = json!(0);
    export
        .files
        .insert(SUMMARY.into(), serde_json::to_vec(&s).unwrap());
    assert!(!recompute(&export).unwrap().matches_recorded);
}

#[test]
fn i_export_drops_private_condition_fields_and_the_raw_obs_log() {
    let export = build(&obs_run(), &conditions(), &ctx()).unwrap();
    let c: Value = serde_json::from_slice(&export.files[CONDITIONS]).unwrap();
    let set: Vec<&String> = c
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        set,
        [
            "commit",
            "cpu_brand",
            "dirty",
            "input",
            "refresh_den",
            "refresh_num"
        ]
    );
    for bytes in export.files.values() {
        let t = String::from_utf8_lossy(bytes);
        assert!(
            !t.contains("192.0.2.10") && !t.contains("synthuser") && !t.contains("game-capture")
        );
    }
}

/// Scans a copy of `base` with one JSON file edited.
fn planted(base: &Export, file: &str, edit: impl Fn(&mut Value)) -> Vec<Finding> {
    let mut export = base.clone();
    let mut v: Value = serde_json::from_slice(&export.files[file]).unwrap();
    edit(&mut v);
    export
        .files
        .insert(file.into(), serde_json::to_vec(&v).unwrap());
    scan(&export, &ctx())
}

#[test]
fn i_scan_finds_paths_names_raw_qpc_and_unknown_keys() {
    let base = build(&passing_run(), &conditions(), &ctx()).unwrap();
    let f = planted(&base, CONDITIONS, |v| {
        v["cpu_brand"] = json!("C:\\Users\\x\\a.txt")
    });
    assert!(f.contains(&Finding::Path(CONDITIONS.into())), "{f:?}");
    let f = planted(&base, CONDITIONS, |v| {
        v["cpu_brand"] = json!("/home/x/spike")
    });
    assert!(f.contains(&Finding::Path(CONDITIONS.into())));
    let f = planted(&base, CONDITIONS, |v| {
        v["cpu_brand"] = json!("built on desktop-synth01")
    });
    assert_eq!(f, vec![Finding::Name(CONDITIONS.into())]);
    // Raw QPC in a time field breaks the unit check; in free text it is
    // found by value.
    let f = planted(&base, RUN, |v| {
        v["info"]["timeline"]["play_start_us"] = json!(QPC_LO + 1_234)
    });
    assert_eq!(f, vec![Finding::OutOfRange(RUN.into())]);
    let f = planted(&base, RUN, |v| {
        v["info"]["presentmon_error"] = json!(format!("bad row at {}", QPC_LO + 9))
    });
    assert_eq!(f, vec![Finding::RawQpc(RUN.into())]);
    let f = planted(&base, CONDITIONS, |v| {
        v["cpu_brand"] = json!(format!("Synthetic {}", QPC_LO + 9))
    });
    assert_eq!(f, vec![Finding::RawQpc(CONDITIONS.into())]);
    let f = planted(&base, RUN, |v| v["info"]["computer"] = json!("x"));
    assert_eq!(
        f,
        vec![Finding::Key {
            file: RUN.into(),
            key: "computer".into()
        }]
    );
    let f = planted(&base, RUN, |v| {
        v["counters"]["library_allocs"]["some_new_scope"] = json!(1);
    });
    assert_eq!(f.len(), 1);

    // CSV: an extra column, an unknown file.
    let mut export = base.clone();
    let csv = String::from_utf8(export.files[FRAMES].clone()).unwrap();
    let csv = csv.replacen("present_us,", "present_us,raw_qpc,", 1);
    export.files.insert(FRAMES.into(), csv.into_bytes());
    export
        .files
        .insert("private-origin.json".into(), b"{}".to_vec());
    let f = scan(&export, &ctx());
    assert!(f.contains(&Finding::Key {
        file: FRAMES.into(),
        key: "raw_qpc".into()
    }));
    assert!(f.contains(&Finding::UnknownFile("private-origin.json".into())));
    // `recompute` refuses an export with findings.
    assert!(matches!(recompute(&export), Err(ExportError::Rejected(_))));
}

#[test]
fn i_build_refuses_a_run_that_would_leak() {
    let mut run = passing_run();
    run.info.timeline.play_start_us = QPC_LO as i64 + 77;
    assert!(matches!(
        build(&run, &conditions(), &ctx()),
        Err(ExportError::Rejected(f)) if f == vec![Finding::OutOfRange(RUN.into())]
    ));
    let mut run = passing_run();
    run.frames[100].render_us = QPC_LO as i64;
    assert!(matches!(
        build(&run, &conditions(), &ctx()),
        Err(ExportError::Rejected(f)) if f == vec![Finding::OutOfRange(FRAMES.into())]
    ));
}

#[test]
fn i_durations_and_counts_are_not_exempt() {
    // A raw QPC value in a duration field fails its range.
    let mut run = passing_run();
    run.input[10].latency_us = 52_000_000_009;
    assert!(matches!(
        build(&run, &conditions(), &ctx()),
        Err(ExportError::Rejected(f)) if f == vec![Finding::OutOfRange(INPUT.into())]
    ));
    let mut run = passing_run();
    run.frames[3].render_cpu_us = u32::MAX;
    assert!(build(&run, &conditions(), &ctx()).is_err());
    // A count is scanned by value.
    let mut run = passing_run();
    run.counters.voice_steals = QPC_LO + 5;
    assert!(matches!(
        build(&run, &conditions(), &ctx()),
        Err(ExportError::Rejected(f)) if f.contains(&Finding::RawQpc(RUN.into()))
    ));
}

#[test]
fn i_relative_times_inside_the_raw_qpc_range_are_accepted() {
    // A minute after boot the raw QPC range (10 MHz ticks, 60–260 s of
    // uptime) overlaps the run's relative µs times; those are legitimate.
    let ctx = ScanContext {
        qpc_range: Some((600_000_000, 2_600_000_000)),
        ..ScanContext::default()
    };
    // `build` scans the export and refuses it on any finding.
    assert!(build(&passing_run(), &conditions(), &ctx).is_ok());
}

#[test]
fn i_short_names_are_checked_as_whole_words() {
    let ctx = ScanContext {
        forbidden_names: vec!["jk".into()],
        ..ScanContext::default()
    };
    assert!(build(&passing_run(), &conditions(), &ctx).is_ok());
    let mut c = conditions();
    c.cpu_brand = Some("Synthetic CPU (jk)".into());
    assert!(matches!(
        build(&passing_run(), &c, &ctx),
        Err(ExportError::Rejected(f)) if f == name_findings()
    ));
    c.cpu_brand = Some("Synthetic JK-8 CPU".into());
    assert!(build(&passing_run(), &c, &ctx).is_err());
    // Inside a longer word it is not that name.
    c.cpu_brand = Some("Synthetic ajkx CPU".into());
    assert!(build(&passing_run(), &c, &ctx).is_ok());
    // Punctuation inside a short name is part of it.
    let ctx = ScanContext {
        forbidden_names: vec!["a-b".into()],
        ..ScanContext::default()
    };
    c.cpu_brand = Some("a-b performance plan".into());
    assert!(matches!(
        build(&passing_run(), &c, &ctx),
        Err(ExportError::Rejected(f)) if f == name_findings()
    ));
    c.cpu_brand = Some("xa-by performance plan".into());
    assert!(build(&passing_run(), &c, &ctx).is_ok());
    // Overlapping occurrences: the first `a-a` in "extra-a-a" is inside a
    // word, the overlapping second one is the name.
    let ctx = ScanContext {
        forbidden_names: vec!["a-a".into()],
        ..ScanContext::default()
    };
    c.cpu_brand = Some("extra-a-a performance plan".into());
    assert!(matches!(
        build(&passing_run(), &c, &ctx),
        Err(ExportError::Rejected(f)) if f == name_findings()
    ));
}

#[test]
fn i_every_condition_section_survives_the_export() {
    let mut c = conditions();
    c.displays = vec![
        DisplayMode {
            gdi_name: "DISPLAY1".into(),
            width: 2560,
            height: 1440,
            refresh_num: 240_085,
            refresh_den: 1_000,
        },
        DisplayMode {
            gdi_name: "DISPLAY2".into(),
            width: 1920,
            height: 1080,
            refresh_num: 59_951,
            refresh_den: 1_000,
        },
    ];
    c.presentmon_version = Some("2.6.0".into());
    c.scene_hash = Some("ab".repeat(32));
    c.hybrid_present = Some(HybridPresent {
        rows: 100,
        hybrid_rows: 0,
    });
    c.gpu_state = Some(GpuState {
        source: "nvidia-smi".into(),
        samples: 550,
        pstates: vec![PstateShare {
            pstate: "P0".into(),
            pct: 100.0,
        }],
        core_mhz_p50: Some(2550),
        mem_mhz_p50: Some(11501),
        ..GpuState::default()
    });
    c.gpu_power_mode_declared = Some("prefer-max-performance".into());
    c.power_mode = Some("balanced, game-mode".into());
    (c.audio_period_min, c.audio_period_max, c.audio_period_step) =
        (Some(384), Some(480), Some(48));
    c.audio_period_default = Some(480);
    let export = build(&passing_run(), &c, &ctx()).unwrap();
    let back: PublicConditions = serde_json::from_slice(&export.files[CONDITIONS]).unwrap();
    assert_eq!(back, c);
    let md = String::from_utf8(export.files[REPORT].clone()).unwrap();
    assert!(md.contains("DISPLAY2") && md.contains("nvidia-smi") && md.contains("game-mode"));
    assert!(recompute(&export).unwrap().matches_recorded);
}

#[test]
fn i_the_report_is_scanned_and_recomputed() {
    let base = build(&passing_run(), &conditions(), &ctx()).unwrap();
    let md = String::from_utf8(base.files[REPORT].clone()).unwrap();
    assert!(md.contains("## Gates") && md.contains("Present s"));
    let edit = |text: String| {
        let mut e = base.clone();
        e.files.insert(REPORT.into(), text.into_bytes());
        e
    };
    // A path, a name and a raw QPC value in the prose are each found.
    let f = scan(&edit(format!("{md}C:\\Users\\x\n")), &ctx());
    assert_eq!(f, vec![Finding::Path(REPORT.into())]);
    let f = scan(&edit(format!("{md}synthuser\n")), &ctx());
    assert_eq!(f, vec![Finding::Name(REPORT.into())]);
    let f = scan(&edit(format!("{md}{}\n", QPC_LO + 3)), &ctx());
    assert_eq!(f, vec![Finding::RawQpc(REPORT.into())]);
    // A report that does not match the data is detected.
    let tampered = edit(md.replace("pass", "PASS"));
    assert!(scan(&tampered, &ctx()).is_empty());
    assert!(!recompute(&tampered).unwrap().matches_recorded);
}

#[test]
fn i_escaped_quotes_and_backslashes_cannot_hide_a_name() {
    // A password-like name with a quote and a backslash: serialized JSON
    // and report.md's condition values escape both. (Any backslash is also
    // a path finding; the name must be found on its own all the same.)
    let secret = r#"Zq"8\xTest"#;
    let ctx = ScanContext {
        forbidden_names: vec![secret.into()],
        ..ctx()
    };
    let mut c = conditions();
    c.cpu_brand = Some(format!("Synthetic {secret} CPU"));
    match build(&passing_run(), &c, &ctx) {
        Err(ExportError::Rejected(f)) => {
            assert!(name_findings().iter().all(|n| f.contains(n)), "{f:?}")
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    // In any other JSON file's string, and in a CSV cell.
    let base = build(&passing_run(), &conditions(), &ctx).unwrap();
    let mut e = base.clone();
    let mut v: Value = serde_json::from_slice(&e.files[RUN]).unwrap();
    v["info"]["profile"] = json!(format!("standard {secret}"));
    e.files.insert(RUN.into(), serde_json::to_vec(&v).unwrap());
    assert!(scan(&e, &ctx).contains(&Finding::Name(RUN.into())));
    let mut e = base.clone();
    let csv = String::from_utf8(e.files[FRAMES].clone()).unwrap();
    let header = csv.lines().next().unwrap().to_owned();
    let cell = format!("\"{}\"", secret.replace('"', "\"\""));
    e.files
        .insert(FRAMES.into(), format!("{header}\n{cell}\n").into_bytes());
    assert!(scan(&e, &ctx).contains(&Finding::Name(FRAMES.into())));
    // In report.md as an escaped literal only, with a stray quote before
    // it in the prose.
    let md = String::from_utf8(base.files[REPORT].clone()).unwrap();
    for planted in [
        format!("- note = {}\n", serde_json::to_string(secret).unwrap()),
        format!("a \" stray quote\n- note = {:?}\n", format!("x{secret}y")),
        // A \u escape is found only by decoding the literal.
        "- note = \"Zq\\u00228\\\\xTest\"\n".to_owned(),
    ] {
        let mut e = base.clone();
        e.files
            .insert(REPORT.into(), format!("{md}{planted}").into_bytes());
        let f = scan(&e, &ctx);
        assert!(
            f.contains(&Finding::Name(REPORT.into())),
            "{planted}: {f:?}"
        );
    }
}
