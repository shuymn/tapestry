use bms_model::{Lane, Speed, TimeUs};
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

/// Key `n` (bound to lane `n`) set to `value`, received at `at`.
fn key(n: u32, at: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(n),
            value,
        },
    }
}

fn press(at: i64, value: i32) -> PhysicalInputEvent {
    key(1, at, value)
}

/// A completed play of `CHART` under the custom preset, both notes hit on
/// time, with the folder its chart loads from.
fn played() -> (TempDir, PlayRecord) {
    played_from(CHART, 0, 0)
}

/// A play of `text` loaded with `seed`, from one keyboard whose keys 1-7
/// are bound to lanes 1-7 with input offset `offset`. Its input hits
/// `CHART`'s notes on time (received `offset` later), and it completed at
/// 4.180 s.
fn played_from(text: &str, seed: u64, offset: i64) -> (TempDir, PlayRecord) {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        seed,
        ..LoadOptions::default()
    };
    let chart = load_bms(text.as_bytes(), dir.path(), &opts).unwrap().chart;
    let record = PlayRecord {
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
                    input_offset: TimeUs(offset),
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
            wake: crate::WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: Speed::NORMAL,
        start: InputStart {
            opened_at: TimeUs(0),
            held: Vec::new(),
        },
        events: vec![
            press(2_000 * MS + offset, 1),
            press(2_050 * MS + offset, 0),
            press(4_000 * MS + offset, 1),
            press(4_050 * MS + offset, 0),
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

/// `bytes` (a current preset copy) labeled canonical encoding 4, the layout
/// before the judgment-header model, and without the header policy's tag.
/// The decoder refuses it at the version byte, before any other field.
fn encoding_4(bytes: &[u8]) -> Vec<u8> {
    let mut other = rules::custom();
    other.headers = rules::HeaderPolicy::DefExRankFirst;
    let tag = bytes
        .iter()
        .zip(other.canonical_bytes())
        .position(|(&a, b)| a != b)
        .unwrap();
    let mut old = bytes.to_vec();
    old.remove(tag);
    assert_eq!(old[0], 7, "the current encoding");
    old[0] = 4;
    old
}

/// Verifies as `eval-cli` does: loads the chart, then evaluates the record
/// under `preset_override` or its own preset.
fn verify(
    record: &PlayRecord,
    dir: &std::path::Path,
    preset_override: Option<&RulePreset>,
) -> Result<(), VerifyError> {
    let chart = load_recorded_chart(record, CHART.as_bytes(), dir)?;
    evaluate_record(record, &chart, preset_override).map(|_| ())
}

#[test]
fn record_before_the_header_model_is_refused_at_its_first_failing_check() {
    let (dir, record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    // As made before the judgment-header model: the preset copy in encoding
    // 4, loader version 6 and that loader's resolved-chart hash.
    let bytes = encoding_4(&record.preset.canonical_bytes);
    let mut old = with_preset_bytes(record, bytes);
    old.chart.identity.loader_version = 6;
    old.chart.identity.resolved_hash[0] ^= 1;
    // It decodes: the preset hash matches its bytes.
    let old = decode(&encode(&old), &DecodeLimits::STANDARD).unwrap();
    // The copy fails to decode before the loader version is compared, on
    // every path and under any override.
    let refused = VerifyError::PresetCopy(CanonicalError::UnsupportedVersion(4));
    let style = rules::beatoraja_style();
    for preset_override in [None, Some(&rules::custom()), Some(&style)] {
        assert_eq!(
            verify(&old, dir.path(), preset_override),
            Err(refused.clone())
        );
        assert_eq!(
            evaluate_record(&old, &chart, preset_override).map(|_| ()),
            Err(refused.clone())
        );
    }
    assert_eq!(
        load_recorded_chart(&old, UNLOADABLE, dir.path()),
        Err(refused)
    );
}

#[test]
fn refuses_loader_version_6_with_current_fields() {
    let (dir, record) = played();
    // The evaluator version and the preset copy are current.
    let mut old = record;
    old.chart.identity.loader_version = 6;
    let refused = VerifyError::UnsupportedLoader(6);
    let style = rules::beatoraja_style();
    for preset_override in [None, Some(&rules::custom()), Some(&style)] {
        assert_eq!(
            verify(&old, dir.path(), preset_override),
            Err(refused.clone())
        );
    }
    // Before the load and the resolved-chart hash.
    assert_eq!(
        load_recorded_chart(&old, UNLOADABLE, dir.path()),
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
    wide.tables.keys.windows.bad = Window::symmetric(220 * MS);
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
    wide.tables.keys.windows.bad = Window::symmetric(220 * MS);
    let r = evaluate_record(&record, &chart, Some(&wide)).unwrap();
    assert_eq!(r.coverage, Coverage::NoInputAfter(TimeUs(4_180 * MS)));
    let counts = r.evaluation.tally.counts;
    assert_eq!((counts.perfect, counts.bad, counts.poor), (1, 0, 1));
}

/// The custom preset with the BAD window `ms` each side.
fn bad_edge(ms: i64) -> RulePreset {
    let mut p = rules::custom();
    p.tables.keys.windows.bad = Window::symmetric(ms * MS);
    p
}

#[test]
fn what_if_stops_at_its_own_final_deadline() {
    let (dir, mut record) = played();
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    // A second press on key 1 at 4.120 s, before the recorded watermark
    // (4.180 s): 120 ms after the last note, an empty POOR in the play.
    record
        .events
        .extend([press(4_120 * MS, 1), press(4_140 * MS, 0)]);
    let as_played = evaluate_record(&record, &chart, None).unwrap();
    assert_eq!(as_played.evaluation.tally.empty_poor, 1);
    // A 110 ms edge ends the what-if at 4.110 s, before that press: it is
    // input of the play, but past the what-if's end, so it is not judged.
    let r = evaluate_record(&record, &chart, Some(&bad_edge(110))).unwrap();
    assert!(r.evaluation.completed);
    assert_eq!(r.coverage, Coverage::Recorded);
    let tally = r.evaluation.tally;
    assert_eq!(
        (tally.counts.perfect, tally.empty_poor, tally.bp),
        (2, 0, 0)
    );
    assert!(
        r.evaluation
            .events
            .iter()
            .all(|e| e.time <= TimeUs(4_110 * MS))
    );
}

#[test]
fn what_if_keeps_input_up_to_the_watermark_in_evaluation_time() {
    // Notes on keys 2-4 at 4 s besides `CHART`'s; the source's input offset
    // is 5 ms, so each input is received 5 ms after its evaluation time.
    let text = format!("{CHART}#00212:01\n#00213:01\n#00214:01\n");
    let offset = 5 * MS;
    let (dir, mut record) = played_from(&text, 0, offset);
    let chart = load_recorded_chart(&record, text.as_bytes(), dir.path()).unwrap();
    let w = record.status.finalized.0;
    // Presses at evaluation times W - 1, W and W + 1 µs on keys 2, 3, 4.
    // The input thread publishes `fresh - 1 µs`: every event received at
    // or before it is already queued, and the watermark is that
    // publication less the offset. The live merge moves each input whose
    // evaluation time is at or before the watermark, and the evaluator
    // processes them, so the inputs at W - 1 and at W were judged in the
    // play and the one at W + 1 was not.
    for (n, t) in [(2, w - 1), (3, w), (4, w + 1)] {
        record.events.push(key(n, t + offset, 1));
    }
    // Under a 220 ms edge the first two are BADs (about 180 ms late); the
    // press on key 4 is not input of the play, so its note is missed.
    let r = evaluate_record(&record, &chart, Some(&bad_edge(220))).unwrap();
    let judged: Vec<_> = r
        .evaluation
        .events
        .iter()
        .filter_map(|e| match e.kind {
            eval::JudgeEventKind::Judged { lane, judgment, .. } if lane.index() >= 2 => {
                Some((lane.index(), judgment))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        judged,
        [
            (2, rules::Judgment::Bad),
            (3, rules::Judgment::Bad),
            (4, rules::Judgment::Poor)
        ]
    );
}

#[test]
fn the_load_uses_the_recorded_seed() {
    // `CHART` plus a 2 s note on key 2 or key 3, by the `#RANDOM` draw.
    let text = format!("{CHART}#RANDOM 2\n#IF 1\n#00112:01\n#ENDIF\n#IF 2\n#00113:01\n#ENDIF\n");
    let dir = tempfile::tempdir().unwrap();
    let resolved = |seed| {
        let opts = LoadOptions {
            seed,
            ..LoadOptions::default()
        };
        load_bms(text.as_bytes(), dir.path(), &opts)
            .unwrap()
            .chart
            .identity
            .resolved_hash
    };
    // A nonzero seed resolving otherwise than seed 0, and one resolving
    // otherwise than it.
    let seed = (1..64).find(|&s| resolved(s) != resolved(0)).unwrap();
    let other = (0..64).find(|&s| resolved(s) != resolved(seed)).unwrap();
    let (dir, record) = played_from(&text, seed, 0);
    let chart = load_recorded_chart(&record, text.as_bytes(), dir.path()).unwrap();
    assert_eq!(chart.identity.resolved_hash, resolved(seed));
    let mut reseeded = record;
    reseeded.chart.random_seed = other;
    assert_eq!(
        load_recorded_chart(&reseeded, text.as_bytes(), dir.path()),
        Err(VerifyError::ResolvedChartMismatch)
    );
}

/// Source: beatoraja's scratch BAD late edge is 290 ms
/// (`play/JudgeProperty.java:24`), 217.5 ms at beatoraja-style's absent
/// rank (75 %), past the keys' 210 ms.
#[test]
fn final_deadline_has_one_source() {
    // `CHART` plus a scratch tap at 4 s, beside the key 1 tap there.
    let text = format!("{CHART}#00216:01\n");
    let (dir, record) = played_from(&text, 0, 0);
    let chart = load_recorded_chart(&record, text.as_bytes(), dir.path()).unwrap();
    let style = rules::beatoraja_style();
    let scratch_miss = TimeUs(4_217_500);
    let device = record.devices.profile();
    let song_end = |e: &eval::Evaluation| {
        e.events
            .iter()
            .filter(|ev| ev.kind == eval::JudgeEventKind::SongEnd)
            .map(|ev| ev.time)
            .collect::<Vec<_>>()
    };
    // Completion: a play with no input ends at the scratch's miss.
    let input = eval::EvalInput {
        events: Vec::new(),
        device: device.clone(),
        start: InputStart::default(),
        finalized: None,
        section: None,
    };
    let done = eval::evaluate(&chart, &input, &style).unwrap();
    assert!(done.completed);
    assert_eq!(song_end(&done), [scratch_miss]);
    // The what-if of the play, recorded to 4.180 s, runs to it.
    let r = evaluate_record(&record, &chart, Some(&style)).unwrap();
    assert!(r.evaluation.completed);
    assert_eq!(song_end(&r.evaluation), [scratch_miss]);
    assert_eq!(r.coverage, Coverage::NoInputAfter(TimeUs(4_180 * MS)));
    // The section probe sizing a recording window.
    let section = eval::Section {
        start: TimeUs(0),
        end: TimeUs(5_000 * MS),
        lead_in: TimeUs(0),
        initial_gauges: style.gauges.iter().map(|g| g.start).collect(),
    };
    let start = InputStart::default();
    let probe = eval::Evaluator::new(&chart, &style, &device, &start, Some(&section), 0).unwrap();
    assert_eq!(probe.final_deadline(), scratch_miss);
}

#[test]
fn the_load_uses_the_recorded_speed() {
    // `CHART` played at 50 %: its notes at 2 s and 4 s are at 4 s and 8 s
    // in real time, where the input hits them.
    let (dir, mut record) = played();
    let opts = LoadOptions {
        speed: Speed::SLOWEST,
        ..LoadOptions::default()
    };
    let half = load_bms(CHART.as_bytes(), dir.path(), &opts).unwrap().chart;
    // The resolved hash is the 1× chart's; the played hash is the 50 %
    // chart's.
    assert_eq!(
        half.identity.resolved_hash,
        record.chart.identity.resolved_hash
    );
    assert_ne!(half.identity.played_hash, record.chart.identity.played_hash);
    record.chart.identity = half.identity;
    record.speed = Speed::SLOWEST;
    for e in &mut record.events {
        e.received = TimeUs(e.received.0 * 2);
    }
    record.status.finalized = TimeUs(8_180 * MS);
    let chart = load_recorded_chart(&record, CHART.as_bytes(), dir.path()).unwrap();
    let times: Vec<i64> = chart.lanes[1].iter().map(|n| n.time.0).collect();
    assert_eq!(times, [4_000 * MS, 8_000 * MS]);
    let r = evaluate_record(&record, &chart, None).unwrap();
    assert_eq!(r.evaluation.tally.counts.perfect, 2);
    // At another speed the chart plays otherwise than it was judged.
    record.speed = Speed::NORMAL;
    assert_eq!(
        load_recorded_chart(&record, CHART.as_bytes(), dir.path()),
        Err(VerifyError::PlayedChartMismatch)
    );
}

#[test]
fn refuses_a_chart_that_resolves_alike_but_plays_otherwise_at_the_speed() {
    // The two BPMs differ by 10^-18: at 1× the note and the scroll map
    // floor alike, so the resolved hash is one, and at 50 % the note floors
    // to 2,000,001 µs and to 2,000,000 µs.
    let text = |bpm| format!("#BPM {bpm}\n#WAV01 a.wav\n#00111:01\n");
    let (played_text, edited) = (
        text("239.999880000059999970"),
        text("239.999880000059999971"),
    );
    let dir = tempfile::tempdir().unwrap();
    let load = |text: &str, speed| {
        let opts = LoadOptions {
            speed,
            ..LoadOptions::default()
        };
        load_bms(text.as_bytes(), dir.path(), &opts).unwrap().chart
    };
    let (one, edited_one) = (
        load(&played_text, Speed::NORMAL),
        load(&edited, Speed::NORMAL),
    );
    assert_eq!(
        one.identity.resolved_hash,
        edited_one.identity.resolved_hash
    );
    assert_eq!(one.scroll, edited_one.scroll);
    let (half, edited_half) = (
        load(&played_text, Speed::SLOWEST),
        load(&edited, Speed::SLOWEST),
    );
    assert_eq!(half.lanes[1][0].time, TimeUs(2_000_001));
    assert_eq!(edited_half.lanes[1][0].time, TimeUs(2_000_000));
    // A play at 50 % pressing 20.000 ms after its note: PERFECT under
    // custom, and GREAT 20.001 ms after the edited file's.
    let (_, mut record) = played();
    record.speed = Speed::SLOWEST;
    record.chart.identity = half.identity.clone();
    record.events = vec![press(2_020_001, 1), press(2_070_001, 0)];
    record.status.finalized = TimeUs(2_400_000);
    let first = |chart: &bms_model::Chart| {
        let r = evaluate_record(&record, chart, None).unwrap();
        r.evaluation.events.iter().find_map(|e| match e.kind {
            eval::JudgeEventKind::Judged { judgment, .. } => Some(judgment),
            _ => None,
        })
    };
    assert_eq!(first(&half), Some(rules::Judgment::Perfect));
    assert_eq!(first(&edited_half), Some(rules::Judgment::Great));
    let chart = load_recorded_chart(&record, played_text.as_bytes(), dir.path()).unwrap();
    assert_eq!(chart, half);
    assert_eq!(
        load_recorded_chart(&record, edited.as_bytes(), dir.path()),
        Err(VerifyError::PlayedChartMismatch)
    );
}
