//! Seed inputs, generated from the public types each time the fuzzer runs
//! (`examples/write_seeds.rs`), so they follow format changes and no seed
//! bytes are committed. The `bms_load` seeds are the self-made charts in
//! `crates/bms-parser/tests/charts/`, which the runner passes as they are.

use bms_model::{ChartIdentity, Lane, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, InputStart, PhysicalInputEvent,
    ScratchSettings, SourceId, SourceProfile,
};
use input::raw;
use replay::{
    Arrival, BuildProvenance, ChartRecord, DecodeLimits, DeviceKind, DeviceRecord, EndStatus,
    FORMAT_VERSION, Incomplete, MAGIC, PlayRecord, PlayType, PresetRecord, ReceiveMethod,
    SourceRecord, Status, TransformRecord, Versions,
};

/// A named seed.
pub type Seed = (&'static str, Vec<u8>);

/// The seeds of each generated target, by target name.
#[must_use]
pub fn all() -> Vec<(&'static str, Vec<Seed>)> {
    vec![
        ("replay_file", replay_files()),
        ("replay_body", replay_bodies()),
        ("raw_input", raw_inputs()),
        ("preset_canonical", preset_canonicals()),
    ]
}

fn lane(i: u8) -> Lane {
    Lane::new(i).expect("a lane below the lane count")
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

fn axis(control: u32, l: u8, mode: AxisMode, press: u32) -> AxisBinding {
    AxisBinding {
        control: ControlId(control),
        lane: lane(l),
        scratch: ScratchSettings {
            mode,
            press_threshold: press,
            hold_threshold: press / 2,
            release_after_us: 120_000,
        },
    }
}

/// A keyboard and a controller with an absolute and a relative axis, a
/// held key, key and axis events, a disconnect and an arrival.
fn two_sources() -> PlayRecord {
    let mut controller = SourceRecord {
        receive: ReceiveMethod::Scripted,
        ..source(DeviceKind::Controller, -300, &[(5, 7)])
    };
    controller.profile.axes = vec![
        axis(0x1_0030, 0, AxisMode::Absolute { wrap: 256 }, 2),
        axis(0x1_0035, 6, AxisMode::Relative, 3),
    ];
    controller.axis_out_of_range = 4;
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
                loader_version: 5,
                random_results: vec![3, 1],
            },
            random_seed: 42,
            transform: TransformRecord {
                id: "passthrough".to_owned(),
                version: 4,
            },
        },
        // Its hash matches its bytes, so a mutated body gets past the preset
        // check while those bytes stay intact.
        preset: PresetRecord::of(&rules::custom()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![
                source(DeviceKind::Keyboard, 2_500, &[(0x1e, 1), (0x2c, 2)]),
                controller,
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
            control(1, 1_200_000, 0x1_0030, 131),
            control(1, 1_250_000, 0x1_0035, -4),
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

/// One keyboard, no events, no held keys, played to the end.
fn no_events() -> PlayRecord {
    let mut rec = two_sources();
    rec.devices.sources.truncate(1);
    rec.start.held.clear();
    rec.events.clear();
    rec.arrivals.clear();
    rec.status = Status {
        end: EndStatus::PlayedToEnd,
        finalized: TimeUs(0),
        incomplete: Incomplete::default(),
    };
    rec
}

/// The widest values each field takes: extreme times, offsets and
/// controls, and a delta from `i64::MAX` to `i64::MIN`.
fn extremes() -> PlayRecord {
    let mut rec = two_sources();
    rec.versions.evaluator = u32::MAX;
    rec.chart.random_seed = u64::MAX;
    rec.chart.identity.random_results = vec![u32::MAX, 0];
    rec.devices.sources[0].profile.input_offset = TimeUs(i64::MIN);
    rec.devices.sources[1].axis_out_of_range = u64::MAX;
    rec.devices.audio_offset = TimeUs(i64::MAX);
    rec.start.opened_at = TimeUs(i64::MAX);
    rec.start.held = vec![(SourceId(1), ControlId(u32::MAX), i32::MIN)];
    rec.events = vec![
        control(0, i64::MAX, u32::MAX, i32::MAX),
        control(0, i64::MIN, 0, i32::MIN),
        control(1, 0, 0x1_0030, -1),
    ];
    rec.status.finalized = TimeUs(i64::MIN);
    rec
}

fn valid_records() -> [(&'static str, PlayRecord); 3] {
    [
        ("two_sources", two_sources()),
        ("no_events", no_events()),
        ("extremes", extremes()),
    ]
}

/// The payloads of a body's sections, in order.
fn split_sections(body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some((len, tail)) = rest.split_first_chunk::<4>() {
        let (payload, tail) = tail.split_at(u32::from_le_bytes(*len) as usize);
        out.push(payload.to_vec());
        rest = tail;
    }
    out
}

fn join_sections(sections: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for s in sections {
        body.extend_from_slice(
            &u32::try_from(s.len())
                .expect("a section fits in u32")
                .to_le_bytes(),
        );
        body.extend_from_slice(s);
    }
    body
}

/// Section indexes in the body (`replay::Section` order).
const DEVICES: usize = 4;
const EVENTS: usize = 6;

/// `replay_body` seeds: valid bodies, then one malformed body per error
/// the smoke tests expect.
#[must_use]
pub fn replay_bodies() -> Vec<Seed> {
    let mut seeds: Vec<Seed> = valid_records()
        .into_iter()
        .map(|(name, rec)| (name, replay::body_bytes(&rec)))
        .collect();
    let body = replay::body_bytes(&two_sources());
    let sections = split_sections(&body);
    let with_section = |index: usize, payload: Vec<u8>| {
        let mut forged = sections.clone();
        forged[index] = payload;
        join_sections(&forged)
    };

    // The last section's length is one past the end of the body.
    let mut past_end = body.clone();
    let status_at = body.len() - sections[8].len() - 4;
    let len = u32::try_from(sections[8].len() + 1).expect("a short section");
    past_end[status_at..status_at + 4].copy_from_slice(&len.to_le_bytes());
    seeds.push(("section_past_end", past_end));

    // Two events, within the event limit, with five payload bytes left:
    // two events take at least six.
    seeds.push((
        "count_beyond_data",
        with_section(EVENTS, vec![2, 0, 0, 1, 0, 0]),
    ));

    // More events than `DecodeLimits::STANDARD` allows (4 Mi): 2^23 as a
    // varint.
    seeds.push((
        "too_many_events",
        with_section(EVENTS, vec![0x80, 0x80, 0x80, 0x04]),
    ));

    // A devices section with no source.
    seeds.push(("zero_sources", with_section(DEVICES, vec![0, 0, 0])));

    // A byte after the last section.
    let mut trailing = body.clone();
    trailing.push(0);
    seeds.push(("trailing_bytes", trailing));

    // The first section, then half of the next section's length.
    seeds.push(("truncated", body[..4 + sections[0].len() + 2].to_vec()));

    seeds
}

fn header(version: u16, encoding: u8, declared: u32) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&version.to_le_bytes());
    out.push(encoding);
    out.extend_from_slice(&declared.to_le_bytes());
    out
}

/// `replay_file` seeds: files as `replay::encode` writes them, then one
/// file per header and compression error, and an empty body.
#[must_use]
pub fn replay_files() -> Vec<Seed> {
    let mut seeds: Vec<Seed> = valid_records()
        .into_iter()
        .map(|(name, rec)| (name, replay::encode(&rec)))
        .collect();
    let file = replay::encode(&two_sources());
    let body_len = replay::body_bytes(&two_sources()).len();
    let declared = u32::try_from(body_len).expect("a short body");
    let compressed = &file[11..];
    let with_header = |head: Vec<u8>| [head.as_slice(), compressed].concat();

    let mut bad_magic = file.clone();
    bad_magic[0] ^= 0x20;
    seeds.push(("bad_magic", bad_magic));
    seeds.push((
        "unsupported_version",
        with_header(header(FORMAT_VERSION.wrapping_add(1), 1, declared)),
    ));
    seeds.push((
        "unsupported_encoding",
        with_header(header(FORMAT_VERSION, 2, declared)),
    ));
    seeds.push((
        "declared_too_large",
        with_header(header(
            FORMAT_VERSION,
            1,
            DecodeLimits::STANDARD.max_uncompressed + 1,
        )),
    ));
    // Declared one byte short, so the body does not fit.
    seeds.push((
        "decompress",
        with_header(header(FORMAT_VERSION, 1, declared - 1)),
    ));
    seeds.push((
        "length_mismatch",
        with_header(header(FORMAT_VERSION, 1, declared + 1)),
    ));
    // No compressed bytes and an empty body: the body decoder is reached.
    seeds.push(("header_only", header(FORMAT_VERSION, 1, 0)));
    seeds
}

/// `RIM_TYPEKEYBOARD` block with `RAWKEYBOARD` after the header, in the
/// build target's layout ([`raw::HEADER_SIZE`]).
fn keyboard(device: usize, wparam: usize, make: u16, flags: u16, vkey: u16) -> Vec<u8> {
    let size = raw::HEADER_SIZE + raw::KEYBOARD_SIZE;
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&raw::RIM_TYPEKEYBOARD.to_ne_bytes());
    b.extend_from_slice(&u32::try_from(size).expect("a small block").to_ne_bytes());
    b.extend_from_slice(&device.to_ne_bytes());
    b.extend_from_slice(&wparam.to_ne_bytes());
    b.extend_from_slice(&make.to_ne_bytes());
    b.extend_from_slice(&flags.to_ne_bytes());
    b.extend_from_slice(&0u16.to_ne_bytes());
    b.extend_from_slice(&vkey.to_ne_bytes());
    let message: u32 = if flags & raw::RI_KEY_BREAK == 0 {
        0x100
    } else {
        0x101
    };
    b.extend_from_slice(&message.to_ne_bytes());
    b.extend_from_slice(&0x5EED_u32.to_ne_bytes());
    b
}

/// `RIM_TYPEHID` block with `count` reports of `size` bytes.
fn hid(device: usize, wparam: usize, size: u32, count: u32) -> Vec<u8> {
    let reports = (size * count) as usize;
    let len = raw::HEADER_SIZE + raw::HID_HEADER_SIZE + reports;
    let mut b = Vec::with_capacity(len);
    b.extend_from_slice(&raw::RIM_TYPEHID.to_ne_bytes());
    b.extend_from_slice(&u32::try_from(len).expect("a small block").to_ne_bytes());
    b.extend_from_slice(&device.to_ne_bytes());
    b.extend_from_slice(&wparam.to_ne_bytes());
    b.extend_from_slice(&size.to_ne_bytes());
    b.extend_from_slice(&count.to_ne_bytes());
    b.extend((0..reports).map(|i| (i * 37) as u8));
    b
}

/// A count byte, then `blocks` each padded to a pointer-size boundary as
/// `NEXTRAWINPUTBLOCK` expects.
fn buffer(count: u8, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![count];
    for b in blocks {
        out.extend_from_slice(b);
        out.resize(1 + (out.len() - 1).next_multiple_of(size_of::<usize>()), 0);
    }
    out
}

/// `raw_input` seeds.
#[must_use]
pub fn raw_inputs() -> Vec<Seed> {
    let press = keyboard(0x11A0, 0, 0x1E, 0, 0x41);
    let release_e0 = keyboard(
        0x11A0,
        raw::RIM_INPUTSINK,
        0x1D,
        raw::RI_KEY_BREAK | raw::RI_KEY_E0,
        0x11,
    );
    // Three 3-byte reports: the block is not a multiple of the pointer size.
    let reports = hid(0x22B0, 0, 3, 3);
    let mut cut = keyboard(0x33C0, 0, 0x2C, 0, 0x5A);
    cut.truncate(raw::HEADER_SIZE + 4);
    vec![
        ("keyboard_press", buffer(1, std::slice::from_ref(&press))),
        ("hid_reports", buffer(1, std::slice::from_ref(&reports))),
        (
            "mixed_padded",
            buffer(3, &[press.clone(), reports, release_e0]),
        ),
        // The second block's size runs past the buffer.
        ("truncated", buffer(2, &[press.clone(), cut])),
        // Five blocks asked for, two present.
        ("count_beyond", buffer(5, &[press.clone(), press])),
    ]
}

/// Where `bytes`, [`rules::custom`]'s, hold the gauge count: the first byte
/// that differs from the same preset's without gauges.
fn gauge_count_at(bytes: &[u8]) -> usize {
    let mut empty = rules::custom();
    empty.gauges.clear();
    bytes
        .iter()
        .zip(empty.canonical_bytes())
        .position(|(&a, b)| a != b)
        .expect("the gauges change the bytes")
}

/// `preset_canonical` seeds: valid presets, then one malformed input per
/// refusal the smoke tests expect.
#[must_use]
pub fn preset_canonicals() -> Vec<Seed> {
    let custom = rules::custom();
    let bytes = custom.canonical_bytes();
    // Strings and a gauge list whose lengths take two varint bytes.
    let mut long = custom.clone();
    long.id = "p".repeat(200);
    let gauge = long.gauges[1].clone();
    long.gauges = (0..130)
        .map(|i| rules::GaugeRule {
            id: format!("{}{i}", "g".repeat(130)),
            ..gauge.clone()
        })
        .collect();
    let mut seeds: Vec<Seed> = vec![("custom", bytes.clone()), ("long", long.canonical_bytes())];

    let edited = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut b = bytes.clone();
        edit(&mut b);
        b
    };
    seeds.push(("other_version", edited(&|b| b[0] += 1)));
    // The version with a redundant zero byte.
    seeds.push((
        "non_minimal_varint",
        edited(&|b| {
            b[0] |= 0x80;
            b.insert(1, 0);
        }),
    ));
    // 16383 gauges, more than the bytes left.
    let count_at = gauge_count_at(&bytes);
    assert!(
        bytes.len() - count_at < 16_383,
        "the custom preset is short"
    );
    seeds.push((
        "count_beyond_data",
        edited(&|b| {
            b.splice(count_at..=count_at, [0xff, 0x7f]);
        }),
    ));
    // The first gauge's kind, after the count and its id, is tag 2.
    let id = &custom.gauges[0].id;
    assert!(
        id.len() < 128 && custom.gauges.len() < 128,
        "one-byte lengths"
    );
    seeds.push(("unknown_tag", edited(&|b| b[count_at + 2 + id.len()] = 2)));
    // The version as eleven varint bytes.
    seeds.push((
        "varint_overflow",
        edited(&|b| {
            b.splice(
                0..1,
                [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
            );
        }),
    ));
    // The id's first byte, then the preset version, after the id, as 2^32.
    let id = &custom.id;
    assert!(id.len() < 128 && custom.version < 128, "one-byte varints");
    seeds.push(("invalid_utf8", edited(&|b| b[2] = 0xff)));
    let version_at = 2 + id.len();
    seeds.push((
        "value_too_large",
        edited(&|b| {
            b.splice(version_at..=version_at, [0x80, 0x80, 0x80, 0x80, 0x10]);
        }),
    ));
    // The implementation's name, after the version and its length, starts
    // with a capital.
    seeds.push((
        "unknown_implementation",
        edited(&|b| b[version_at + 2] = b'S'),
    ));
    // Five rank scales: the count precedes the first scale, whose first
    // byte is the first to change with it.
    let mut scaled = custom.clone();
    scaled.rank_scale.rank_milli[0] += 1;
    let rank_at = bytes
        .iter()
        .zip(scaled.canonical_bytes())
        .position(|(&a, b)| a != b)
        .expect("a rank scale changes the bytes")
        - 1;
    assert_eq!(bytes[rank_at], 4, "the rank scale count");
    seeds.push(("wrong_length", edited(&|b| b[rank_at] = 5)));
    seeds.push(("trailing_bytes", edited(&|b| b.push(0))));
    seeds.push((
        "truncated",
        edited(&|b| {
            b.pop();
        }),
    ));
    let mut no_gauges = custom;
    no_gauges.gauges.clear();
    seeds.push(("invalid", no_gauges.canonical_bytes()));
    seeds
}
