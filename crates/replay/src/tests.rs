use bms_model::{ChartBuilder, ChartIdentity, Lane, TimeUs};
use eval::{
    Binding, ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile,
};
use sha2::{Digest, Sha256};

use crate::wire::Writer;
use crate::{
    Arrival, BuildProvenance, ChartRecord, DecodeError, DecodeLimits, DeviceKind, DeviceRecord,
    EndStatus, FORMAT_VERSION, Incomplete, MAGIC, PlayRecord, PlayType, PresetRecord,
    ReceiveMethod, Section, SourceRecord, Status, Versions, body_bytes, decode, encode,
    preset_hash, write_events,
};

fn lane(i: u8) -> Lane {
    Lane::new(i).unwrap()
}

fn control(source: u16, at: i64, control: u32, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(source),
        kind: InputKind::Control {
            control: ControlId(control),
            value,
        },
    }
}

fn source(kind: DeviceKind, offset: i64, bindings: &[(u32, u8)]) -> SourceRecord {
    SourceRecord {
        kind,
        receive: ReceiveMethod::RawInput,
        profile: SourceProfile {
            input_offset: TimeUs(offset),
            bindings: bindings
                .iter()
                .map(|&(c, l)| Binding {
                    control: ControlId(c),
                    lane: lane(l),
                })
                .collect(),
        },
    }
}

fn preset_record(bytes: &[u8]) -> PresetRecord {
    PresetRecord {
        id: "custom".to_owned(),
        sha256: Sha256::digest(bytes).into(),
        canonical_bytes: bytes.to_vec(),
    }
}

/// A record with three sources, a held control, a disconnect, an arrival
/// and a suspend abort, and `n_events` input events.
fn sample(n_events: usize) -> PlayRecord {
    let opened_at = 5_000_000;
    let mut events = Vec::with_capacity(n_events);
    // Source 2 disconnects halfway and sends nothing after; sources 0 and 1
    // interleave. Several events share a batch time.
    let half = n_events / 2;
    for i in 0..n_events {
        let at = opened_at + (i as i64 / 2) * 1_250;
        let src = if i < half {
            (i % 3) as u16
        } else {
            (i % 2) as u16
        };
        if i == half - 1 {
            events.push(PhysicalInputEvent {
                received: TimeUs(at),
                source: SourceId(2),
                kind: InputKind::Disconnected,
            });
            continue;
        }
        let value = i32::from(i % 4 < 2);
        events.push(control(src, at, 0x1e + (i % 8) as u32, value));
    }
    PlayRecord {
        versions: Versions {
            evaluator: 1,
            app: "0.1.0".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord {
            identity: ChartIdentity {
                sha256: [0x11; 32],
                md5: [0x22; 16],
                resolved_hash: [0x33; 32],
                loader_version: 1,
                random_results: vec![2, 1, 7],
            },
            random_seed: 0x0123_4567_89ab_cdef,
        },
        preset: PresetRecord::of(&rules::custom()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![
                source(DeviceKind::Keyboard, 3_000, &[(0x1e, 0), (0x1f, 1)]),
                source(DeviceKind::Keyboard, -1_500, &[(0x20, 2), (0x21, 3)]),
                SourceRecord {
                    receive: ReceiveMethod::Scripted,
                    ..source(DeviceKind::Controller, 0, &[(1, 4), (2, 5), (3, 6), (4, 7)])
                },
            ],
            audio_offset: TimeUs(-12_000),
            display_offset: TimeUs(8_000),
        },
        play_type: PlayType::Full,
        start: InputStart {
            opened_at: TimeUs(opened_at),
            held: vec![(SourceId(1), ControlId(0x21), 1)],
        },
        events,
        arrivals: vec![Arrival {
            at: TimeUs(opened_at + 700_000),
            kind: DeviceKind::Controller,
        }],
        status: Status {
            end: EndStatus::EndedMidway,
            finalized: TimeUs(opened_at + 3_000_000),
            incomplete: Incomplete {
                suspend: true,
                ..Incomplete::default()
            },
        },
    }
}

/// A file around `body`, as `encode` writes it, at zstd `level`.
fn wrap(body: &[u8], level: i32) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.push(1);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&zstd::bulk::compress(body, level).unwrap());
    out
}

fn limits() -> DecodeLimits {
    DecodeLimits::default()
}

#[test]
fn round_trip_preserves_every_field() {
    let rec = sample(5000);
    assert_eq!(rec.events.len(), 5000);
    assert_eq!(rec.devices.sources.len(), 3);
    assert!(rec.events.iter().any(|e| e.kind == InputKind::Disconnected));
    assert!(rec.status.incomplete.any());
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn extreme_and_empty_values_round_trip() {
    let mut rec = sample(30);
    // A source's first event at 0 with `opened_at` far above it, then the
    // extremes of the time range on interleaved sources.
    rec.events = vec![
        control(0, 0, 1, 1),
        control(1, i64::MIN, 2, i32::MIN),
        control(0, i64::MAX, 1, i32::MAX),
        control(1, i64::MAX, 2, 0),
        control(0, i64::MIN, 1, -1),
    ];
    rec.start.opened_at = TimeUs(i64::MAX);
    rec.status.finalized = TimeUs(i64::MIN);
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec.clone()));

    rec.events.clear();
    rec.arrivals.clear();
    rec.start.held.clear();
    rec.chart.identity.random_results.clear();
    rec.devices.sources.truncate(1);
    rec.devices.sources[0].profile.bindings.clear();
    rec.status.incomplete = Incomplete::default();
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn event_times_are_deltas_per_source() {
    let events = [
        control(0, 100, 7, 1),
        control(1, 0, 9, 1),
        control(0, 105, 7, 0),
        control(1, 3, 9, 0),
    ];
    let mut w = Writer::default();
    write_events(&mut w, TimeUs(100), &events);
    // Per event: source, zigzag delta, tag 0, control, zigzag value. Source
    // 0 counts from `opened_at` (100): deltas 0 then 5; source 1 starts at
    // 0, so -100 then 3.
    assert_eq!(
        w.0,
        [
            4, //
            0, 0, 0, 7, 2, //
            1, 0xc7, 0x01, 0, 9, 2, //
            0, 10, 0, 7, 0, //
            1, 6, 0, 9, 0,
        ]
    );
}

#[test]
fn zero_sources_is_refused() {
    let mut rec = sample(0);
    rec.devices.sources.clear();
    rec.start.held.clear();
    assert_eq!(
        decode(&encode(&rec), &limits()),
        Err(DecodeError::NoSources)
    );
}

#[test]
fn events_of_unknown_sources_are_refused() {
    let mut rec = sample(9);
    rec.events.push(control(3, 0, 1, 1));
    assert_eq!(
        decode(&encode(&rec), &limits()),
        Err(DecodeError::UnknownSource(3))
    );
}

#[test]
fn newer_version_is_refused() {
    let file = encode(&sample(10));
    assert!(decode(&file, &limits()).is_ok());
    for v in [FORMAT_VERSION + 1, u16::MAX, 0] {
        let mut newer = file.clone();
        newer[4..6].copy_from_slice(&v.to_le_bytes());
        assert_eq!(
            decode(&newer, &limits()),
            Err(DecodeError::UnsupportedVersion(v))
        );
    }
}

#[test]
fn truncated_body_is_an_error_not_a_panic() {
    let rec = sample(60);
    let body = body_bytes(&rec);
    assert!(body.len() > 200, "{}", body.len());
    assert_eq!(decode(&wrap(&body, 3), &limits()), Ok(rec.clone()));
    for len in 0..body.len() {
        let res = decode(&wrap(&body[..len], 3), &limits());
        assert!(res.is_err(), "body prefix of {len} bytes decoded");
    }
    let file = encode(&rec);
    for len in 0..file.len() {
        let res = decode(&file[..len], &limits());
        assert!(res.is_err(), "file prefix of {len} bytes decoded");
    }
}

#[test]
fn oversized_declared_length_is_refused_before_decompressing() {
    let max = DecodeLimits::STANDARD.max_uncompressed;
    // The body is not zstd data: only a check made before decompressing
    // can report the length.
    let mut file = MAGIC.to_vec();
    file.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    file.push(1);
    file.extend_from_slice(&(max + 1).to_le_bytes());
    file.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(
        decode(&file, &limits()),
        Err(DecodeError::DeclaredLengthTooLarge {
            declared: max + 1,
            max,
        })
    );
    file[7..11].copy_from_slice(&max.to_le_bytes());
    assert_eq!(decode(&file, &limits()), Err(DecodeError::Decompress));

    // A body that decompresses beyond its declared length stops there.
    let body = body_bytes(&sample(40));
    let declared = u32::try_from(body.len()).unwrap();
    let mut short = wrap(&body, 3);
    short[7..11].copy_from_slice(&(declared - 1).to_le_bytes());
    assert_eq!(decode(&short, &limits()), Err(DecodeError::Decompress));
    let mut long = wrap(&body, 3);
    long[7..11].copy_from_slice(&(declared + 1).to_le_bytes());
    assert_eq!(
        decode(&long, &limits()),
        Err(DecodeError::LengthMismatch {
            declared: declared + 1,
            actual: body.len(),
        })
    );
}

#[test]
fn section_length_beyond_remaining_bytes_is_an_error() {
    let body = body_bytes(&sample(20));
    // The first section's length.
    let mut first = body.clone();
    first[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode(&wrap(&first, 3), &limits()),
        Err(DecodeError::SectionLength {
            section: Section::Versions,
            len: u32::MAX,
            remaining: body.len() - 4,
        })
    );
    // The last section's length, one past the end of the body.
    let mut offset = 0;
    let mut last = 0;
    while offset < body.len() {
        last = offset;
        let len = u32::from_le_bytes(body[offset..offset + 4].try_into().unwrap());
        offset += 4 + len as usize;
    }
    assert_eq!(offset, body.len());
    let remaining = body.len() - last - 4;
    let mut over = body.clone();
    over[last..last + 4].copy_from_slice(&u32::try_from(remaining + 1).unwrap().to_le_bytes());
    assert_eq!(
        decode(&wrap(&over, 3), &limits()),
        Err(DecodeError::SectionLength {
            section: Section::Status,
            len: u32::try_from(remaining + 1).unwrap(),
            remaining,
        })
    );
}

/// A small fixed record whose body is pinned by hash.
fn golden() -> PlayRecord {
    PlayRecord {
        versions: Versions {
            evaluator: 1,
            app: "0.1.0".to_owned(),
            provenance: BuildProvenance::Production,
        },
        chart: ChartRecord {
            identity: ChartIdentity {
                sha256: [0xa1; 32],
                md5: [0xb2; 16],
                resolved_hash: [0xc3; 32],
                loader_version: 1,
                random_results: vec![3, 1],
            },
            random_seed: 42,
        },
        preset: preset_record(b"canonical preset bytes"),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![
                source(DeviceKind::Keyboard, 2_500, &[(0x1e, 0), (0x2c, 1)]),
                SourceRecord {
                    receive: ReceiveMethod::Scripted,
                    ..source(DeviceKind::Controller, -300, &[(5, 7)])
                },
            ],
            audio_offset: TimeUs(-10_000),
            display_offset: TimeUs(16_000),
        },
        play_type: PlayType::Full,
        start: InputStart {
            opened_at: TimeUs(1_000_000),
            held: vec![(SourceId(0), ControlId(0x2c), 1)],
        },
        events: vec![
            control(0, 1_000_000, 0x1e, 1),
            control(1, 1_000_000, 5, 1),
            control(0, 1_020_000, 0x1e, 0),
            control(0, 1_020_000, 0x2c, 0),
            control(1, 1_300_000, 5, 0),
            PhysicalInputEvent {
                received: TimeUs(1_500_000),
                source: SourceId(1),
                kind: InputKind::Disconnected,
            },
        ],
        arrivals: vec![Arrival {
            at: TimeUs(1_400_000),
            kind: DeviceKind::Keyboard,
        }],
        status: Status {
            end: EndStatus::EndedMidway,
            finalized: TimeUs(1_450_000),
            incomplete: Incomplete {
                overflow: true,
                source_limit: false,
                suspend: true,
            },
        },
    }
}

/// SHA-256 of `body_bytes(&golden())`, computed by a separate encoder
/// written from the layout in the crate documentation.
const GOLDEN_BODY_SHA256: &str = "aa9fcd9fe8d4d682b282e1ca985699ab2fe38866b340780182594e8b0e422e29";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn v1_golden_body() {
    let rec = golden();
    let body = body_bytes(&rec);
    assert_eq!(hex(&Sha256::digest(&body)), GOLDEN_BODY_SHA256);
    // Another zstd encoding of the same body decodes to the same record.
    for level in [1, 3, 19] {
        let decoded = decode(&wrap(&body, level), &limits()).unwrap();
        assert_eq!(
            hex(&Sha256::digest(body_bytes(&decoded))),
            GOLDEN_BODY_SHA256
        );
        assert_eq!(decoded, rec);
    }
}

#[test]
fn two_thousand_notes_fit_in_tens_of_kb() {
    // 2000 taps over 2 minutes, cycling the 8 lanes.
    let mut builder = ChartBuilder::new(150_000);
    for i in 0..2000_i64 {
        builder = builder.tap(lane((i % 8) as u8), TimeUs(1_000_000 + i * 60_000));
    }
    let chart = builder.build().unwrap();
    let mut notes: Vec<(TimeUs, Lane)> = Lane::all()
        .flat_map(|l| chart.lanes[l.index()].iter().map(move |n| (n.time, l)))
        .collect();
    notes.sort();
    assert_eq!(notes.len(), 2000);

    // Autoplay: one scripted source presses each note's control at the note
    // and releases it 40 ms later.
    let scan = |l: Lane| 0x1e + l.index() as u32;
    let mut events = Vec::with_capacity(4000);
    for &(t, l) in &notes {
        events.push(control(0, t.0, scan(l), 1));
        events.push(control(0, t.0 + 40_000, scan(l), 0));
    }
    events.sort_by_key(|e| e.received);
    let mut rec = sample(0);
    rec.devices.sources = vec![SourceRecord {
        receive: ReceiveMethod::Scripted,
        ..source(
            DeviceKind::Keyboard,
            0,
            &Lane::all()
                .map(|l| (scan(l), l.index() as u8))
                .collect::<Vec<_>>(),
        )
    }];
    rec.start = InputStart {
        opened_at: TimeUs(0),
        held: Vec::new(),
    };
    rec.events = events;
    rec.arrivals.clear();
    assert_eq!(rec.events.len(), 4000);

    let body = body_bytes(&rec);
    assert!(body.len() < 64 * 1024, "body is {} bytes", body.len());
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
}

#[test]
fn preset_hash_is_sha256_of_canonical_bytes() {
    let preset = rules::custom();
    let hash = preset_hash(&preset);
    assert_eq!(
        hash,
        <[u8; 32]>::from(Sha256::digest(preset.canonical_bytes()))
    );
    let rec = PresetRecord::of(&preset);
    assert_eq!(rec.sha256, hash);
    assert_eq!(rec.id, preset.id);

    let mut bad = sample(4);
    bad.preset.sha256[0] ^= 1;
    assert_eq!(
        decode(&encode(&bad), &limits()),
        Err(DecodeError::PresetHashMismatch)
    );
}
