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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::TimeUs;
use common::{PERIOD, RATE, count_judged, judgeable};
use eval::EvalInput;
use game::{
    BmsIngest, ChartIngest, DataRoot, DeviceSettings, Loaded, Parts, PlayEnd, PlayScript,
    RunnerAudio, SessionConfig, Settings,
};
use input::{DeviceKind, ScriptedInput, SourceSet};
use replay::{BuildProvenance, DecodeLimits, ReceiveMethod};
use rules::Judgment;

/// Every file under `dir` the BMS adapter reads, in a stable order.
fn charts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            charts(&path, out);
        } else if BmsIngest.accepts(&path) {
            out.push(path);
        }
    }
}

/// Plays one chart; returns its note and warning counts, or `None` when the
/// loader refuses it.
fn sweep_one(path: &Path) -> Option<(usize, usize)> {
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
    let loaded = Loaded::load(root, settings, path, config, parts).ok()?;
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
    let profile = DeviceSettings::default().profile(&[DeviceKind::Keyboard]);
    let events = input::autoplay(&chart, &profile.sources[0]).unwrap();
    let input = ScriptedInput::from_log(&Default::default(), &events, &[DeviceKind::Keyboard]);
    let report = session
        .play(PlayScript {
            input,
            commands: Vec::new(),
        })
        .unwrap();

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
    Some((notes, warnings))
}

#[test]
#[ignore = "reads the owner's charts from TAPESTRY_CHARTS_DIR"]
fn local_charts_sweep() {
    let dir = std::env::var_os("TAPESTRY_CHARTS_DIR").expect("set TAPESTRY_CHARTS_DIR");
    let mut found = Vec::new();
    charts(Path::new(&dir), &mut found);
    let mut played = 0;
    for (i, path) in found.iter().enumerate() {
        match sweep_one(path) {
            Some((notes, warnings)) => {
                played += 1;
                println!("chart {}: notes {notes} warnings {warnings}", i + 1);
            }
            None => println!("chart {}: refused by the loader", i + 1),
        }
    }
    println!("{played} of {} charts played", found.len());
    assert!(played > 0, "no chart could be played");
}
