use bms_model::{ChartIdentity, Lane, Speed, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, InputStart, PhysicalInputEvent,
    ScratchSettings, SourceId, SourceProfile,
};
use sha2::{Digest, Sha256};

use crate::wire::Writer;
use crate::{
    Arrival, BuildProvenance, ChartRecord, DecodeError, DecodeLimits, DeviceKind, DeviceRecord,
    EndStatus, FORMAT_VERSION, Incomplete, InitialGauge, MAGIC, PlayRecord, PlayType, Practice,
    PresetRecord, ReceiveMethod, Section, SourceRecord, Status, TransformRecord, Versions,
    WakePolicy, body_bytes, decode, encode, preset_hash, write_events, write_status,
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
            axes: Vec::new(),
        },
        axis_out_of_range: 0,
    }
}

/// `s` with an axis per `(control, lane, mode)`; the other settings are
/// distinct per axis and from every other field.
fn with_axes(mut s: SourceRecord, axes: &[(u32, u8, AxisMode)]) -> SourceRecord {
    s.axis_out_of_range = 3;
    s.profile.axes = axes
        .iter()
        .zip(0..)
        .map(|(&(c, l, mode), i)| AxisBinding {
            control: ControlId(c),
            lane: lane(l),
            scratch: ScratchSettings {
                mode,
                press_threshold: 6 + i,
                hold_threshold: 9 + i,
                release_after_us: 120_000 + i,
            },
        })
        .collect();
    s
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
                played_hash: [0x34; 32],
                loader_version: 1,
                random_results: vec![2, 1, 7],
            },
            random_seed: 0x0123_4567_89ab_cdef,
            transform: TransformRecord {
                id: "mirror".to_owned(),
                version: 2,
            },
        },
        preset: PresetRecord::of(&rules::tapestry()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![
                source(DeviceKind::Keyboard, 3_000, &[(0x1e, 0), (0x1f, 1)]),
                source(DeviceKind::Keyboard, -1_500, &[(0x20, 2), (0x21, 3)]),
                with_axes(
                    SourceRecord {
                        receive: ReceiveMethod::Scripted,
                        ..source(DeviceKind::Controller, 0, &[(1, 4), (2, 5), (3, 6), (4, 7)])
                    },
                    &[(0x1_0030, 0, AxisMode::Absolute { wrap: 1024 })],
                ),
            ],
            audio_offset: TimeUs(-12_000),
            display_offset: TimeUs(8_000),
            wake: crate::WakePolicy::Input {
                heartbeat_us: 1_000,
                input_gap_us: 125,
                drain_cut: 1_024,
            },
        },
        play_type: PlayType::Full,
        speed: Speed::NORMAL,
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
fn an_event_takes_at_most_max_event_bytes() {
    // Source 63, a delta from 0 to i64::MIN (zigzag u64::MAX: 10
    // bytes), the control tag, control u32::MAX (5) and value i32::MIN
    // (zigzag 0xFFFF_FFFF: 5).
    let worst = |kind| PhysicalInputEvent {
        received: TimeUs(i64::MIN),
        source: SourceId(63),
        kind,
    };
    let control = InputKind::Control {
        control: ControlId(u32::MAX),
        value: i32::MIN,
    };
    let len = |kind| {
        let mut w = Writer::default();
        write_events(&mut w, TimeUs(0), &[worst(kind)]);
        // Less the event count.
        w.0.len() - 1
    };
    assert_eq!(len(control), crate::MAX_EVENT_BYTES);
    assert_eq!(len(InputKind::Disconnected), 12);
    // The standard limits leave room for 3 Mi worst-case events.
    assert_eq!(
        crate::max_record_events(&DecodeLimits::STANDARD),
        ((64 << 20) - (1 << 20)) / 22
    );
}

#[test]
fn round_trip_preserves_every_field() {
    let rec = sample(5000);
    assert_eq!(rec.events.len(), 5000);
    assert_eq!(rec.devices.sources.len(), 3);
    assert!(rec.events.iter().any(|e| e.kind == InputKind::Disconnected));
    assert_eq!(rec.chart.transform.version, 2);
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
    rec.chart.transform.version = u32::MAX;
    assert_eq!(decode(&encode(&rec), &limits()), Ok(rec.clone()));

    rec.events.clear();
    rec.arrivals.clear();
    rec.start.held.clear();
    rec.chart.identity.random_results.clear();
    rec.chart.transform.id.clear();
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
    // Before the first release, older versions are refused too.
    for v in [FORMAT_VERSION + 1, u16::MAX, 0, FORMAT_VERSION - 1] {
        let mut newer = file.clone();
        newer[4..6].copy_from_slice(&v.to_le_bytes());
        assert_eq!(
            decode(&newer, &limits()),
            Err(DecodeError::UnsupportedVersion(v))
        );
    }
}

#[test]
fn a_record_from_before_the_out_of_range_counts_is_refused() {
    // Format 2 had no out-of-range count per source. Its files are refused
    // by their version, before the body is read, with no conversion.
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&2_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(2))
    );
}

#[test]
fn a_record_from_before_the_input_failure_reason_is_refused() {
    // Format 3 had no "input failed" reason; its files are refused by
    // their version, with no conversion.
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&3_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(3))
    );
}

#[test]
fn a_record_from_before_the_wake_policy_is_refused() {
    // Format 4 had no wake policy; its files are refused by their version,
    // with no conversion.
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&4_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(4))
    );
}

#[test]
fn a_record_from_before_the_speed_is_refused() {
    // Format 5 had no speed; its files are refused by their version, with
    // no conversion to 1×.
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&5_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(5))
    );
}

#[test]
fn a_record_from_before_the_practice_fields_is_refused() {
    // Format 6 had no practice conditions; its files are refused by their
    // version, with no conversion to a full play.
    let mut old = wrap(&body_bytes(&golden()), 3);
    old[4..6].copy_from_slice(&6_u16.to_le_bytes());
    assert_eq!(
        decode(&old, &limits()),
        Err(DecodeError::UnsupportedVersion(6))
    );
}

/// A round of `[10 s, 20 s)` with a 2 s lead-in, its third attempt, with
/// `normal` at 50 % and `survival` at 70 %.
fn practice() -> Practice {
    Practice {
        start: TimeUs(10_000_000),
        end: TimeUs(20_000_000),
        lead_in: TimeUs(2_000_000),
        attempt: 3,
        initial_gauges: vec![
            InitialGauge {
                id: "normal".to_owned(),
                value: rules::GaugeValue(5_000_000),
            },
            InitialGauge {
                id: "survival".to_owned(),
                value: rules::GaugeValue(7_000_000),
            },
        ],
    }
}

#[test]
fn practice_rounds_round_trip_and_a_repeated_gauge_id_is_refused() {
    for play_type in [
        PlayType::MidChartStart(practice()),
        PlayType::SectionRepeat(practice()),
        PlayType::SectionRepeat(Practice {
            initial_gauges: Vec::new(),
            ..practice()
        }),
    ] {
        let mut rec = sample(5);
        rec.play_type = play_type;
        assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
    }
    let mut rec = sample(5);
    let mut twice = practice();
    twice.initial_gauges[1].id = "normal".to_owned();
    rec.play_type = PlayType::SectionRepeat(twice);
    assert_eq!(
        decode(&encode(&rec), &limits()),
        Err(DecodeError::InvalidValue("initial gauge ID"))
    );
    // Tag 3 names no play type.
    let sections = split_sections(&body_bytes(&golden()));
    let mut other = sections.clone();
    let session = &mut other[Section::Session as usize];
    let n = session.len();
    assert_eq!(session[n - 2..], [0, 13]);
    session[n - 2] = 3;
    assert_eq!(
        decode(&wrap(&join_sections(&other), 3), &limits()),
        Err(DecodeError::InvalidValue("play type"))
    );
}

#[test]
fn initial_gauges_beyond_the_data_are_refused_before_allocating() {
    // A round's session section that claims u64::MAX gauges in a few
    // bytes: refused by the bytes left, before anything is reserved for
    // them.
    let sections = split_sections(&body_bytes(&golden()));
    let mut other = sections.clone();
    let session = &mut other[Section::Session as usize];
    let n = session.len();
    session.truncate(n - 2);
    let mut w = Writer::default();
    w.uint(2);
    for v in [10_000_000, 20_000_000, 2_000_000] {
        w.int(v);
    }
    w.uint(1);
    w.uint(u64::MAX);
    w.uint(13);
    session.extend_from_slice(&w.0);
    assert_eq!(
        decode(&wrap(&join_sections(&other), 3), &limits()),
        Err(DecodeError::CountBeyondData(u64::MAX))
    );
}

#[test]
fn every_speed_round_trips_and_one_off_the_ladder_is_refused() {
    for speed in Speed::ladder() {
        let mut rec = sample(5);
        rec.speed = speed;
        assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
    }
    // The session section ends with the speed's k; 9, 31 and a k past u32
    // name no speed.
    let rec = golden();
    let sections = split_sections(&body_bytes(&rec));
    let session = &sections[Section::Session as usize];
    assert_eq!(session.last(), Some(&13));
    for k in [&[9][..], &[31], &[0x80, 0x80, 0x80, 0x80, 0x10]] {
        let mut sections = sections.clone();
        let session = &mut sections[Section::Session as usize];
        session.pop();
        session.extend_from_slice(k);
        assert_eq!(
            decode(&wrap(&join_sections(&sections), 3), &limits()),
            Err(DecodeError::InvalidValue("speed")),
            "{k:?}"
        );
    }
}

#[test]
fn both_wake_policies_round_trip_and_another_is_refused() {
    let policies = [
        WakePolicy::Input {
            heartbeat_us: 1_000,
            input_gap_us: 125,
            drain_cut: 1_024,
        },
        WakePolicy::Input {
            heartbeat_us: u32::MAX,
            input_gap_us: 0,
            drain_cut: u32::MAX,
        },
        WakePolicy::Timer {
            heartbeat_us: 1_000,
        },
        WakePolicy::Timer { heartbeat_us: 0 },
    ];
    for wake in policies {
        let mut rec = sample(5);
        rec.devices.wake = wake;
        assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
    }
    // A timer policy ends the devices section with its tag and period;
    // tag 2 names no policy.
    let mut rec = sample(5);
    rec.devices.wake = WakePolicy::Timer { heartbeat_us: 7 };
    let mut sections = split_sections(&body_bytes(&rec));
    let devices = &mut sections[Section::Devices as usize];
    let n = devices.len();
    assert_eq!(devices[n - 2..], [1, 7]);
    devices[n - 2] = 2;
    assert_eq!(
        decode(&wrap(&join_sections(&sections), 3), &limits()),
        Err(DecodeError::InvalidValue("wake policy"))
    );
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

/// The fixed record of [`golden`], before its axes.
fn golden_base() -> PlayRecord {
    PlayRecord {
        versions: Versions {
            evaluator: 3,
            app: "0.1.0".to_owned(),
            provenance: BuildProvenance::Production,
        },
        chart: ChartRecord {
            identity: ChartIdentity {
                sha256: [0xa1; 32],
                md5: [0xb2; 16],
                resolved_hash: [0xc3; 32],
                played_hash: [0xc4; 32],
                loader_version: 5,
                random_results: vec![3, 1],
            },
            random_seed: 42,
            transform: TransformRecord {
                id: "passthrough".to_owned(),
                version: 4,
            },
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
            wake: crate::WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: Speed::from_twentieths(13).unwrap(),
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
                input_failed: false,
            },
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A small fixed record whose body is pinned by hash: two sources, the
/// controller with two axes, one absolute and one relative, and a report
/// of each.
fn golden() -> PlayRecord {
    let mut rec = golden_base();
    let axis = |control, l, mode, press, hold, release| AxisBinding {
        control: ControlId(control),
        lane: lane(l),
        scratch: ScratchSettings {
            mode,
            press_threshold: press,
            hold_threshold: hold,
            release_after_us: release,
        },
    };
    rec.devices.sources[1].profile.axes = vec![
        axis(0x1_0030, 0, AxisMode::Absolute { wrap: 256 }, 2, 1, 100_000),
        axis(0x1_0035, 6, AxisMode::Relative, 3, 2, 150_000),
    ];
    rec.events.insert(4, control(1, 1_200_000, 0x1_0030, 131));
    rec.events.insert(5, control(1, 1_250_000, 0x1_0035, -4));
    // The absolute axis reported 4 values outside its declared range.
    rec.devices.sources[1].axis_out_of_range = 4;
    rec
}

/// SHA-256 of `body_bytes(&golden())`, computed by a separate encoder
/// written from the layout in the crate documentation.
const GOLDEN_BODY_SHA256: &str = "55bd78549dc36bbed5e7494f4eeac79972ae7ae8f2036d46393627d4b66049cf";

#[test]
fn golden_body() {
    let rec = golden();
    let file = encode(&rec);
    assert_eq!(file[4..6], FORMAT_VERSION.to_le_bytes());
    let body = body_bytes(&rec);
    assert_eq!(hex(&Sha256::digest(&body)), GOLDEN_BODY_SHA256);
    // Another zstd encoding of the same body decodes to the same record.
    for level in [1, 3, 19] {
        assert_eq!(decode(&wrap(&body, level), &limits()), Ok(rec.clone()));
    }
}

#[test]
fn malformed_axes_are_refused() {
    // The devices section of a single controller with one axis, each
    // field forgeable: (axis count, lane, mode).
    let devices = |count: u64, l: u64, mode: u64| {
        let mut w = Writer::default();
        // One source: controller, raw input, offset 0, no bindings.
        w.raw(&[1, 1, 0, 0, 0]);
        w.uint(count);
        w.uint(0x1_0030);
        w.uint(l);
        w.uint(mode);
        if mode == 0 {
            w.uint(256);
        }
        w.raw(&[2, 1, 7]);
        // No value outside the range.
        w.uint(0);
        // Audio and display offsets.
        w.raw(&[0, 0]);
        // A timer wake policy of 1 ms.
        w.uint(1);
        w.uint(1_000);
        w.0
    };
    let mut sections = split_sections(&body_bytes(&golden()));
    let decode_with = |sections: &mut Vec<Vec<u8>>, payload: Vec<u8>| {
        sections[4] = payload;
        decode(&wrap(&join_sections(sections), 3), &limits())
    };
    // Well formed, the section reads, and the error comes from the events
    // of source 1, which this single source does not have.
    assert_eq!(
        decode_with(&mut sections, devices(1, 0, 1)),
        Err(DecodeError::UnknownSource(1))
    );
    assert_eq!(
        decode_with(&mut sections, devices(1, 0, 2)),
        Err(DecodeError::InvalidValue("axis mode"))
    );
    assert_eq!(
        decode_with(&mut sections, devices(1, 8, 1)),
        Err(DecodeError::InvalidValue("lane"))
    );
    assert_eq!(
        decode_with(&mut sections, devices(3, 0, 1)),
        Err(DecodeError::CountBeyondData(3))
    );
}

#[test]
fn records_binding_more_than_the_limits_are_refused() {
    // Well-formed records whose bytes hold every binding they declare.
    let axis = |i: u32| AxisBinding {
        control: ControlId(0x1_0000 + i),
        lane: lane(0),
        scratch: ScratchSettings::DEFAULT,
    };
    let key = |i: u32| Binding {
        control: ControlId(i),
        lane: lane(1),
    };
    let record = |sources: usize, keys: u32, axes: u32| {
        let mut rec = golden();
        rec.events.clear();
        rec.start.held.clear();
        let mut s = rec.devices.sources[1].clone();
        s.profile.bindings = (0..keys).map(key).collect();
        s.profile.axes = (0..axes).map(axis).collect();
        rec.devices.sources = vec![s; sources];
        rec
    };
    let decoded = |rec: &PlayRecord| decode(&encode(rec), &limits()).map(|_| ());
    let refused = Err(DecodeError::TooManyBindings);
    // Per source: 8 axes and 256 keys fit, one more of either does not.
    assert_eq!(decoded(&record(1, 256, 8)), Ok(()));
    assert_eq!(decoded(&record(1, 256, 9)), refused);
    assert_eq!(decoded(&record(1, 257, 8)), refused);
    // In all: 16 axes and 1024 keys fit, more do not.
    assert_eq!(decoded(&record(2, 0, 8)), Ok(()));
    assert_eq!(decoded(&record(3, 0, 6)), refused);
    assert_eq!(decoded(&record(4, 256, 1)), Ok(()));
    assert_eq!(decoded(&record(5, 205, 1)), refused);
    // A hundred thousand axes in about a megabyte is refused at its count.
    let huge = record(1, 0, 100_000);
    assert!(body_bytes(&huge).len() < 2 << 20);
    assert_eq!(decoded(&huge), refused);
}

/// The held-input snapshot names each control once, at most as many per
/// source and in all as a profile can bind; a hundred thousand entries in
/// a small record are refused at their count, before anything is
/// allocated for them.
#[test]
fn an_oversized_held_snapshot_is_refused() {
    let with = |sources: u16, per_source: u32| {
        let mut rec = golden();
        rec.devices.sources = vec![rec.devices.sources[1].clone(); usize::from(sources)];
        rec.events.clear();
        rec.start.held = (0..sources)
            .flat_map(|s| (0..per_source).map(move |c| (SourceId(s), ControlId(c), 1)))
            .collect();
        rec
    };
    let decoded = |rec: &PlayRecord| decode(&encode(rec), &limits()).map(|_| ());
    let refused = Err(DecodeError::TooManyHeld);
    let per_source =
        u32::try_from(eval::MAX_BINDINGS_PER_SOURCE + eval::MAX_AXES_PER_SOURCE).unwrap();
    assert_eq!(decoded(&with(1, per_source)), Ok(()));
    assert_eq!(decoded(&with(1, per_source + 1)), refused);
    // In all: four full sources fit, a fifth does not.
    assert_eq!(decoded(&with(3, per_source)), Ok(()));
    assert_eq!(decoded(&with(5, per_source)), refused);
    // One control named twice.
    let mut twice = with(1, 2);
    twice.start.held.push((SourceId(0), ControlId(1), 0));
    assert_eq!(decoded(&twice), refused);
    let huge = with(1, 100_000);
    assert!(body_bytes(&huge).len() < 2 << 20);
    assert_eq!(decoded(&huge), refused);
}

#[test]
fn two_thousand_notes_fit_in_tens_of_kb() {
    // Autoplay of 2000 taps 60 ms apart over 2 minutes, cycling the 8
    // lanes: one scripted source presses each note's control at the note
    // and releases it 40 ms later, so the events come out in time order.
    let scan = |l: Lane| 0x1e + l.index() as u32;
    let mut events = Vec::with_capacity(4000);
    for i in 0..2000_i64 {
        let (t, l) = (1_000_000 + i * 60_000, lane((i % 8) as u8));
        events.push(control(0, t, scan(l), 1));
        events.push(control(0, t + 40_000, scan(l), 0));
    }
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
    let preset = rules::tapestry();
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

#[test]
fn status_reasons_and_end_statuses_are_pinned() {
    let status = |end, [overflow, source_limit, suspend, input_failed]: [bool; 4]| Status {
        end,
        finalized: TimeUs(-2),
        incomplete: Incomplete {
            overflow,
            source_limit,
            suspend,
            input_failed,
        },
    };
    let none = [false; 4];
    // End status tag, zigzag finalized watermark (-2 is 3), reason bits.
    let cases = [
        (status(EndStatus::PlayedToEnd, none), [0, 3, 0]),
        (status(EndStatus::EndedMidway, none), [1, 3, 0]),
        (
            status(EndStatus::EndedMidway, [true, false, false, false]),
            [1, 3, 1],
        ),
        (
            status(EndStatus::EndedMidway, [false, true, false, false]),
            [1, 3, 2],
        ),
        (
            status(EndStatus::EndedMidway, [false, false, true, false]),
            [1, 3, 4],
        ),
        (
            status(EndStatus::EndedMidway, [false, false, false, true]),
            [1, 3, 8],
        ),
    ];
    for (s, bytes) in cases {
        let mut w = Writer::default();
        write_status(&mut w, &s);
        assert_eq!(w.0, bytes, "{s:?}");
        let mut rec = sample(9);
        rec.status = s;
        assert_eq!(decode(&encode(&rec), &limits()), Ok(rec));
    }
}

/// A play whose input thread failed is incomplete for that reason alone
/// (its round trip is in `status_reasons_and_end_statuses_are_pinned`).
#[test]
fn an_input_failure_alone_marks_the_record_incomplete() {
    let input_failed = Incomplete {
        input_failed: true,
        ..Incomplete::default()
    };
    assert!(input_failed.any());
    assert!(!Incomplete::default().any());
}

#[test]
fn lowered_limits_refuse_files_sources_and_events() {
    let rec = sample(30);
    let file = encode(&rec);
    let exact = DecodeLimits {
        max_compressed: file.len(),
        max_uncompressed: DecodeLimits::STANDARD.max_uncompressed,
        max_sources: 3,
        max_events: 30,
    };
    assert_eq!(decode(&file, &exact), Ok(rec));
    let below = |change: fn(&mut DecodeLimits)| {
        let mut l = exact;
        change(&mut l);
        decode(&file, &l)
    };
    assert_eq!(
        below(|l| l.max_compressed -= 1),
        Err(DecodeError::FileTooLarge {
            len: file.len(),
            max: file.len() - 1,
        })
    );
    assert_eq!(
        below(|l| l.max_sources = 2),
        Err(DecodeError::TooManySources(3))
    );
    assert_eq!(
        below(|l| l.max_events = 29),
        Err(DecodeError::TooManyEvents(30))
    );
}

/// The payloads of `body`'s sections, in order.
fn split_sections(body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        out.push(rest[4..4 + len].to_vec());
        rest = &rest[4 + len..];
    }
    assert_eq!(out.len(), 9);
    out
}

fn join_sections(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for s in sections {
        body.extend_from_slice(&u32::try_from(s.len()).unwrap().to_le_bytes());
        body.extend_from_slice(s);
    }
    body
}

#[test]
fn forged_counts_are_refused_before_allocating() {
    let sections = split_sections(&body_bytes(&sample(9)));
    let huge = u64::MAX >> 4;
    let no_event_limit = DecodeLimits {
        max_events: u64::MAX,
        ..limits()
    };
    let payload = |prefix: &[u8], n: u64, suffix: &[u8]| {
        let mut w = Writer::default();
        w.raw(prefix);
        w.uint(n);
        w.raw(suffix);
        w.0
    };
    let beyond = DecodeError::CountBeyondData;
    // (section index, forged payload, expected error, limits)
    let cases = [
        // The app version string's length, after two versions.
        (0, payload(&[3, 5], huge, &[]), beyond(huge), limits()),
        // `#RANDOM` results, after the four hashes and the seed.
        (1, payload(&[0; 113], huge, &[]), beyond(huge), limits()),
        // Five sources take at least 20 bytes; none are left.
        (4, payload(&[], 5, &[]), beyond(5), limits()),
        // Held controls, after `opened_at`.
        (5, payload(&[0], huge, &[]), beyond(huge), limits()),
        // Events, with the event limit out of the way.
        (6, payload(&[], huge, &[]), beyond(huge), no_event_limit),
        // Two events take at least six bytes; five are left.
        (6, payload(&[], 2, &[0, 0, 1, 0, 0]), beyond(2), limits()),
        // Arrivals.
        (7, payload(&[], huge, &[]), beyond(huge), limits()),
    ];
    for (index, forged_payload, expected, lim) in cases {
        let mut forged = sections.clone();
        forged[index] = forged_payload;
        assert_eq!(
            decode(&wrap(&join_sections(&forged), 3), &lim),
            Err(expected),
            "section {index}"
        );
    }
}
