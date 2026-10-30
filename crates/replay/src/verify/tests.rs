use bms_model::{Lane, TimeUs};
use bms_parser::{LOADER_VERSION, LoadError, LoadOptions, load_bms};
use eval::{
    Binding, ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile,
};
use rules::{CanonicalError, RulePreset, Window};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::{Coverage, VerifyError, evaluate_record, load_recorded_chart};
use crate::{
    BuildProvenance, ChartRecord, DecodeError, DecodeLimits, DeviceKind, DeviceRecord, EndStatus,
    Incomplete, PASSTHROUGH, PlayRecord, PlayType, PresetRecord, ReceiveMethod, SourceRecord,
    Status, TransformRecord, Versions, decode, encode,
};

/// Taps on key 1 at 2 s and 4 s (120 BPM, a measure is 2 s).
const CHART: &str = "#BPM 120\n#WAV01 a.wav\n#00111:01\n#00211:01\n";

const MS: i64 = 1_000;

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

/// A completed play of `CHART` under the custom preset, both notes hit on
/// time, with the folder its chart loads from.
fn played() -> (TempDir, PlayRecord) {
    let dir = tempfile::tempdir().unwrap();
    let chart = load_bms(CHART.as_bytes(), dir.path(), &LoadOptions::default())
        .unwrap()
        .chart;
    let record = PlayRecord {
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
                    bindings: vec![Binding {
                        control: ControlId(1),
                        lane: Lane::new(1).unwrap(),
                    }],
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
    };
    (dir, record)
}

/// `record` with its preset copy replaced by `bytes`, hashed so that it
/// decodes.
fn with_preset_bytes(mut record: PlayRecord, bytes: Vec<u8>) -> PlayRecord {
    record.preset.sha256 = Sha256::digest(&bytes).into();
    record.preset.canonical_bytes = bytes;
    record
}

/// Bytes the loader refuses.
const UNLOADABLE: &[u8] = b"#LNTYPE 2\n#00111:01\n";

#[test]
fn a_recorded_play_verifies_and_evaluates_as_played() {
    let (dir, record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    let r = evaluate_record(&record, &chart, None).unwrap();
    assert!(r.as_played);
    assert_eq!(r.coverage, Coverage::Recorded);
    assert!(r.evaluation.completed);
    assert_eq!(r.evaluation.tally.counts.perfect, 2);
}

#[test]
fn decode_checks_precede_verify_checks() {
    let (_dir, record) = played();
    let mut other = record.clone();
    other.versions.evaluator = 3;
    // With only the evaluator version wrong, the record decodes and
    // verification refuses it.
    let decoded = decode(&encode(&other), &DecodeLimits::STANDARD).unwrap();
    assert_eq!(
        load_recorded_chart(&decoded, UNLOADABLE, std::path::Path::new("missing")),
        Err(VerifyError::UnsupportedEvaluator(3))
    );
    // With the preset hash wrong too, decoding refuses it first.
    other.preset.sha256[0] ^= 1;
    assert_eq!(
        decode(&encode(&other), &DecodeLimits::STANDARD),
        Err(DecodeError::PresetHashMismatch)
    );
}

#[test]
fn refuses_another_evaluator_before_loading() {
    let (dir, record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    for version in [1, 3] {
        let mut other = record.clone();
        other.versions.evaluator = version;
        // Every later check would fail too: the preset copy, the loader
        // version and the load.
        other.preset.canonical_bytes.clear();
        other.chart.identity.loader_version = LOADER_VERSION + 1;
        let refused = VerifyError::UnsupportedEvaluator(version);
        assert_eq!(
            load_recorded_chart(&other, UNLOADABLE, dir.path()),
            Err(refused.clone())
        );
        assert_eq!(
            evaluate_record(&other, &chart, Some(&rules::custom())).map(|_| ()),
            Err(refused)
        );
    }
}

#[test]
fn old_preset_copy_is_refused_even_with_an_override() {
    let (dir, record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    // The copy in an older canonical encoding: its version is the first
    // byte, a varint below 128.
    let mut bytes = record.preset.canonical_bytes.clone();
    bytes[0] -= 1;
    let old = u64::from(bytes[0]);
    let other = with_preset_bytes(record, bytes);
    let refused = VerifyError::PresetCopy(CanonicalError::UnsupportedVersion(old));
    let custom = rules::custom();
    for preset_override in [None, Some(&custom)] {
        assert_eq!(
            evaluate_record(&other, &chart, preset_override).map(|_| ()),
            Err(refused.clone())
        );
    }
    // Before the loader version and the load.
    let mut other = other;
    other.chart.identity.loader_version = LOADER_VERSION + 1;
    assert_eq!(
        load_recorded_chart(&other, UNLOADABLE, dir.path()),
        Err(refused)
    );
}

#[test]
fn refuses_another_loader_version() {
    let (dir, record) = played();
    for version in [LOADER_VERSION - 1, LOADER_VERSION + 1] {
        let mut other = record.clone();
        other.chart.identity.loader_version = version;
        assert_eq!(
            load_recorded_chart(&other, CHART.as_bytes(), dir.path()),
            Err(VerifyError::UnsupportedLoader(version))
        );
        // Before the load.
        assert_eq!(
            load_recorded_chart(&other, UNLOADABLE, dir.path()),
            Err(VerifyError::UnsupportedLoader(version))
        );
    }
}

#[test]
fn refuses_a_chart_that_does_not_load() {
    let (dir, mut record) = played();
    // Before the resolved-chart hash and the transform.
    record.chart.identity.resolved_hash[0] ^= 1;
    record.chart.transform.id = "mirror".to_owned();
    let refused = load_recorded_chart(&record, UNLOADABLE, dir.path());
    assert!(
        matches!(
            refused,
            Err(VerifyError::Load(LoadError::Unsupported { .. }))
        ),
        "{refused:?}"
    );
}

#[test]
fn refuses_another_resolved_chart() {
    let (dir, record) = played();
    // The second note a measure later.
    let moved = "#BPM 120\n#WAV01 a.wav\n#00111:01\n#00311:01\n";
    assert_eq!(
        load_recorded_chart(&record, moved.as_bytes(), dir.path()),
        Err(VerifyError::ResolvedChartMismatch)
    );
    // Before the transform.
    let mut other = record;
    other.chart.transform.id = "mirror".to_owned();
    assert_eq!(
        load_recorded_chart(&other, moved.as_bytes(), dir.path()),
        Err(VerifyError::ResolvedChartMismatch)
    );
}

#[test]
fn refuses_an_unknown_transform() {
    let (dir, record) = played();
    for (id, version) in [("mirror", 1), ("passthrough", 2), ("passthrough", 0)] {
        let mut other = record.clone();
        other.chart.transform = TransformRecord {
            id: id.to_owned(),
            version,
        };
        assert_eq!(
            load_recorded_chart(&other, CHART.as_bytes(), dir.path()),
            Err(VerifyError::UnknownTransform {
                id: id.to_owned(),
                version
            })
        );
    }
}

#[test]
fn edited_file_resolving_the_same_chart_verifies() {
    let (dir, record) = played();
    let edited = format!("; edited\r\n{CHART}\n\n");
    let chart = load_recorded_chart(&record, edited.as_bytes(), dir.path()).unwrap();
    // Another file, the same resolved chart.
    assert_ne!(chart.identity.sha256, record.chart.identity.sha256);
    assert_eq!(
        chart.identity.resolved_hash,
        record.chart.identity.resolved_hash
    );
    assert!(evaluate_record(&record, &chart, None).unwrap().as_played);
}

#[test]
fn an_override_is_a_what_if_and_its_own_copy_is_as_played() {
    let (dir, record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    let mut wide = rules::custom();
    wide.windows.bad = Window::symmetric(220 * MS);
    let r = evaluate_record(&record, &chart, Some(&wide)).unwrap();
    assert!(!r.as_played);
    assert_eq!(r.coverage, Coverage::NoInputAfter(TimeUs(4_180 * MS)));
    let own = RulePreset::from_canonical_bytes(&record.preset.canonical_bytes).unwrap();
    assert!(
        evaluate_record(&record, &chart, Some(&own))
            .unwrap()
            .as_played
    );
}

#[test]
fn verify_errors_say_what_was_refused() {
    let cases = [
        (
            VerifyError::UnsupportedEvaluator(3),
            "the record was made by evaluator version 3, which this build does not run",
        ),
        (
            VerifyError::UnsupportedLoader(7),
            "the record was made by loader version 7, which this build does not run",
        ),
        (
            VerifyError::ResolvedChartMismatch,
            "the chart resolves to another chart than the record's",
        ),
        (
            VerifyError::UnknownTransform {
                id: "mirror".to_owned(),
                version: 1,
            },
            "the record's chart transform \"mirror\" version 1 is unknown",
        ),
    ];
    for (e, message) in cases {
        assert_eq!(e.to_string(), message);
    }
}

#[test]
fn what_if_ignores_input_recorded_after_the_watermark() {
    let (dir, mut record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    // The 4 s note is missed at 4.180 s, where the play completes; a press
    // 200 ms late was recorded after it and never evaluated.
    record.events.truncate(2);
    record
        .events
        .extend([press(4_200 * MS, 1), press(4_250 * MS, 0)]);
    let as_played = evaluate_record(&record, &chart, None).unwrap();
    assert_eq!(as_played.evaluation.tally.counts.poor, 1);
    // Under a 220 ms BAD edge the press would be a BAD: it is not input of
    // the play, so the note is still missed.
    let mut wide = rules::custom();
    wide.windows.bad = Window::symmetric(220 * MS);
    let r = evaluate_record(&record, &chart, Some(&wide)).unwrap();
    assert_eq!(r.coverage, Coverage::NoInputAfter(TimeUs(4_180 * MS)));
    let counts = r.evaluation.tally.counts;
    assert_eq!((counts.perfect, counts.bad, counts.poor), (1, 0, 1));
}
