//! The local chart sweep (stage1.md section 5), on the owner's PC only:
//!
//! ```sh
//! TAPESTRY_CHARTS_DIR=<dir> mise exec -- cargo test -p game --test local_charts -- --ignored --nocapture
//! ```
//!
//! Every BMS/BME chart under the directory is loaded and played headlessly
//! with autoplay through a whole session. Each must judge every judgeable
//! note PERFECT, live evaluation must equal batch evaluation, and the saved
//! log must decode to the record. One line per chart is printed with its
//! number, note count and warning count; titles and paths never are.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::TimeUs;
use bms_parser::LoadError;
use common::{PERIOD, RATE, count_judged, judgeable};
use eval::{EvalError, EvalInput};
use game::{
    BmsIngest, ChartIngest, DataRoot, DeviceSettings, IngestError, Loaded, LogicError, Parts,
    PlayEnd, PlayScript, RunnerAudio, SessionConfig, SessionError, Settings,
};
use input::{DeviceKind, ScriptedInput, SourceSet};
use replay::{BuildProvenance, DecodeLimits, ReceiveMethod};
use rules::Judgment;

/// Every file under `dir` the BMS adapter reads, in a stable order. Linked
/// folders are followed, each folder at most once, so a link cycle ends. A
/// folder or entry that cannot be read counts in `unreadable`, so no part
/// of the collection is skipped silently.
fn charts(dir: &Path, out: &mut Vec<PathBuf>, unreadable: &mut usize) {
    charts_in(dir, &mut BTreeSet::new(), out, unreadable);
}

fn charts_in(
    dir: &Path,
    visited: &mut BTreeSet<PathBuf>,
    out: &mut Vec<PathBuf>,
    unreadable: &mut usize,
) {
    let Ok(real) = fs::canonicalize(dir) else {
        *unreadable += 1;
        return;
    };
    if !visited.insert(real) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        *unreadable += 1;
        return;
    };
    let mut paths = Vec::new();
    for entry in entries {
        // `metadata` follows links, so a linked folder counts as a folder.
        match entry.and_then(|e| {
            let path = e.path();
            let is_dir = fs::metadata(&path)?.is_dir();
            Ok((path, is_dir))
        }) {
            Ok(p) => paths.push(p),
            Err(_) => *unreadable += 1,
        }
    }
    paths.sort();
    for (path, is_dir) in paths {
        if is_dir {
            charts_in(&path, visited, out, unreadable);
        } else if BmsIngest.accepts(&path) {
            out.push(path);
        }
    }
}

/// Links `link` to the folder `target`, or `None` when this system does not
/// let the test create links.
fn link_dir(target: &Path, link: &Path) -> Option<()> {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    match made {
        Ok(()) => Some(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
        // ERROR_PRIVILEGE_NOT_HELD: links need Developer Mode or elevation.
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(1314) => None,
        Err(e) => panic!("cannot create a folder link: {e}"),
    }
}

#[test]
fn linked_folders_are_followed_once() {
    let base = tempfile::Builder::new()
        .prefix("links")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    fs::create_dir_all(root.join("plain")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("plain").join("a.bms"), b"").unwrap();
    fs::write(outside.join("b.bme"), b"").unwrap();
    fs::write(outside.join("notes.txt"), b"").unwrap();
    if link_dir(&outside, &root.join("linked")).is_none() {
        println!("skipped: this system does not allow creating folder links");
        return;
    }
    // A link back to the root would recurse without end if followed again.
    link_dir(&root, &root.join("plain").join("loop")).unwrap();

    let mut found = Vec::new();
    let mut unreadable = 0;
    charts(&root, &mut found, &mut unreadable);
    assert_eq!(
        found,
        [
            root.join("linked").join("b.bme"),
            root.join("plain").join("a.bms")
        ]
    );
    assert_eq!(unreadable, 0);
}

/// Why a chart was not played, as a category with no detail of the chart.
/// Only [`allowed`] ones let the sweep pass.
fn category(e: &SessionError) -> &'static str {
    match e {
        SessionError::Ingest(IngestError::Load(l)) => match l {
            LoadError::Unsupported { .. } => "unsupported directive",
            LoadError::FileTooLarge { .. }
            | LoadError::TooManyLines { .. }
            | LoadError::TooManyObjects { .. }
            | LoadError::MeasureOutOfRange { .. }
            | LoadError::NestingTooDeep { .. }
            | LoadError::DurationExceeded { .. } => "loading limit",
            LoadError::InvalidValue { .. } => "invalid value",
            LoadError::TimeOverflow { .. } => "time overflow",
            LoadError::SoundPathRejected { .. } | LoadError::SoundPathEscapes { .. } => {
                "sound path outside the folder"
            }
            LoadError::ChartDir { .. } => "chart folder unreadable",
            LoadError::Model(_) => "loader bug",
        },
        SessionError::UnsupportedChart(_) => "unsupported format",
        SessionError::InvalidStep(_) => "invalid step",
        SessionError::Io { .. } => "file error",
        SessionError::Preset(_) => "preset error",
    }
}

/// Categories of chart the stage 1 player refuses by design.
fn allowed(category: &str) -> bool {
    matches!(
        category,
        "unsupported directive" | "unsupported long-note mode" | "no judgeable notes"
    )
}

/// Plays one chart; returns its note and warning counts, or why it was not
/// played.
fn sweep_one(path: &Path) -> Result<(usize, usize), &'static str> {
    let dir = tempfile::Builder::new()
        .prefix("sweep")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    let settings = Settings::load(&root).unwrap();
    let parts = Parts::standard(&root, &settings.player);
    let config = SessionConfig {
        lead_in: TimeUs(1_000_000),
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: 1 << 16,
        seed: 0,
        app_version: "sweep".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
    };
    let loaded = Loaded::load(root, settings, path, config, parts).map_err(|e| category(&e))?;
    let chart = loaded.chart().clone();
    let notes = judgeable(&chart);
    let warnings = loaded.warnings().len();
    // Silence for every sound: the sweep checks judgment, not the mix.
    let bank = SoundBank::from_sounds(
        RATE,
        chart
            .sounds
            .iter()
            .map(|_| Sound::mono(vec![0; 1]))
            .collect(),
    );
    let engine = AudioEngine::new(Arc::new(bank), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let device = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let audio = RunnerAudio::new(engine, RecordingOutput::new(device, 0));
    let mut session = loaded.start(audio, SourceSet::new(common::sources()).unwrap());
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
    let events = input::autoplay(&chart, &profile.sources[0]).unwrap();
    let input = ScriptedInput::from_log(&Default::default(), &events, &[DeviceKind::Keyboard]);
    let report = session
        .play(PlayScript {
            input,
            commands: Vec::new(),
        })
        .map_err(|e| match e {
            LogicError::Eval(EvalError::UnsupportedLnMode(_)) => "unsupported long-note mode",
            LogicError::Eval(EvalError::NoJudgeableNotes) => "no judgeable notes",
            LogicError::Eval(_) => "evaluator refusal",
            _ => "logic refusal",
        })?;

    assert_eq!(report.end, PlayEnd::Completed);
    assert!(notes > 0);
    assert_eq!(count_judged(&report.live, Judgment::Perfect), notes);
    let record = report.record.as_ref().unwrap();
    let batch = eval::evaluate(
        &chart,
        &EvalInput {
            events: record.events.clone(),
            device: record.devices.profile(),
            start: record.start.clone(),
            finalized: Some(record.status.finalized),
        },
        session.preset(),
    )
    .unwrap();
    assert_eq!(batch.events, report.live);
    assert!(report.result.as_ref().unwrap().matches_live);
    let saved = &report.saved.as_ref().unwrap().as_ref().unwrap().path;
    let decoded = replay::decode(&fs::read(saved).unwrap(), &DecodeLimits::STANDARD).unwrap();
    assert_eq!(&decoded, record);
    Ok((notes, warnings))
}

#[test]
#[ignore = "reads the owner's charts from TAPESTRY_CHARTS_DIR"]
fn local_charts_sweep() {
    let dir = std::env::var_os("TAPESTRY_CHARTS_DIR").expect("set TAPESTRY_CHARTS_DIR");
    let mut found = Vec::new();
    let mut unreadable = 0;
    charts(Path::new(&dir), &mut found, &mut unreadable);
    let mut played = 0;
    let mut refused: BTreeMap<&str, usize> = BTreeMap::new();
    if unreadable > 0 {
        refused.insert("folder or entry unreadable", unreadable);
    }
    for (i, path) in found.iter().enumerate() {
        match sweep_one(path) {
            Ok((notes, warnings)) => {
                played += 1;
                println!("chart {}: notes {notes} warnings {warnings}", i + 1);
            }
            Err(category) => {
                println!("chart {}: not played: {category}", i + 1);
                *refused.entry(category).or_default() += 1;
            }
        }
    }
    println!("{played} of {} charts played", found.len());
    for (category, n) in &refused {
        println!("not played, {category}: {n}");
    }
    let unexpected: Vec<_> = refused.keys().filter(|c| !allowed(c)).collect();
    assert!(unexpected.is_empty(), "unexpected failures: {unexpected:?}");
    assert!(played > 0, "no chart could be played");
}
