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
    FORMAT_VERSION, Incomplete, InitialGauge, MAGIC, PlayRecord, PlayType, Practice, PresetRecord,
    ReceiveMethod, SourceRecord, Status, TransformRecord, Versions, WakePolicy,
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
        ("skin_json", skin_jsons()),
        ("skin_fnt", skin_fnts()),
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
        // Its hash matches its bytes, so a mutated body gets past the preset
        // check while those bytes stay intact.
        preset: PresetRecord::of(&rules::tapestry()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![
                source(DeviceKind::Keyboard, 2_500, &[(0x1e, 1), (0x2c, 2)]),
                controller,
            ],
            audio_offset: TimeUs(-10_000),
            display_offset: TimeUs(16_000),
            wake: WakePolicy::Input {
                heartbeat_us: 1_000,
                input_gap_us: 125,
                drain_cut: 1_024,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
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

/// A round of section repeat with both of the preset's gauges by ID.
fn practice_round() -> PlayRecord {
    let mut rec = no_events();
    rec.play_type = PlayType::SectionRepeat(Practice {
        start: TimeUs(10_000_000),
        end: TimeUs(20_000_000),
        lead_in: TimeUs(2_000_000),
        attempt: 2,
        initial_gauges: [("normal", 5_000_000), ("survival", 7_000_000)]
            .map(|(id, value)| InitialGauge {
                id: id.to_owned(),
                value: rules::GaugeValue(value),
            })
            .to_vec(),
    });
    rec
}

fn valid_records() -> [(&'static str, PlayRecord); 4] {
    [
        ("two_sources", two_sources()),
        ("no_events", no_events()),
        ("extremes", extremes()),
        ("practice_round", practice_round()),
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

/// Where `bytes`, [`rules::tapestry`]'s, hold the gauge count: the first byte
/// that differs from the same preset's without gauges.
fn gauge_count_at(bytes: &[u8]) -> usize {
    let mut empty = rules::tapestry();
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
    let tapestry = rules::tapestry();
    let bytes = tapestry.canonical_bytes();
    // Strings and a gauge list whose lengths take two varint bytes, and
    // lamp ranks of three.
    let mut long = tapestry.clone();
    long.id = "p".repeat(200);
    let gauge = long.gauges[1].clone();
    long.gauges = (0..130)
        .map(|i| rules::GaugeRule {
            id: format!("{}{i}", "g".repeat(130)),
            lamp: u16::MAX - i,
            ..gauge.clone()
        })
        .collect();
    // beatoraja-style carries the other header, window and assignment
    // policies, a scratch table of its own, damage bands, the increase
    // limit and the default #TOTAL from the note count; the gauge policies
    // seed the other correction and default and a failure threshold.
    let mut policies = rules::beatoraja_style();
    policies.default_total = rules::DefaultTotal::NotesLinear;
    policies.gauges[4].kind = rules::GaugeKind::Survival {
        fails_below: rules::GaugeValue(2 * rules::GaugeValue::PERCENT),
    };
    policies.gauges[4].correction = rules::TotalCorrection::ScaleDamage;
    let mut seeds: Vec<Seed> = vec![
        ("tapestry", bytes.clone()),
        (
            "beatoraja_style",
            rules::beatoraja_style().canonical_bytes(),
        ),
        // The interpolated window policy and tied LN-end edges.
        ("lr2_style", rules::lr2_style().canonical_bytes()),
        ("gauge_policies", policies.canonical_bytes()),
        ("long", long.canonical_bytes()),
    ];

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
        "the tapestry preset is short"
    );
    seeds.push((
        "count_beyond_data",
        edited(&|b| {
            b.splice(count_at..=count_at, [0xff, 0x7f]);
        }),
    ));
    // The first gauge's kind, after the count, its id and its lamp rank, is
    // tag 2.
    let id = &tapestry.gauges[0].id;
    assert!(
        id.len() < 128 && tapestry.gauges.len() < 128,
        "one-byte lengths"
    );
    let lamp_at = count_at + 2 + id.len();
    assert_eq!(bytes[lamp_at], 0, "the first gauge's lamp rank");
    seeds.push(("unknown_tag", edited(&|b| b[lamp_at + 1] = 2)));
    // Both gauges ranked 0.
    let mut same_lamp = tapestry.clone();
    same_lamp.gauges[1].lamp = 0;
    seeds.push(("duplicate_lamp", same_lamp.canonical_bytes()));
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
    let id = &tapestry.id;
    assert!(id.len() < 128 && tapestry.version < 128, "one-byte varints");
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
    let first_change = |base: &[u8], other: &rules::RulePreset| {
        base.iter()
            .zip(other.canonical_bytes())
            .position(|(&a, b)| a != b)
            .expect("the edit changes the bytes")
    };
    let mut scaled = tapestry.clone();
    let rules::WindowPolicy::Thousandths(scale) = &mut scaled.window_policy else {
        panic!("tapestry scales in thousandths");
    };
    scale.rank_milli[0] += 1;
    let rank_at = first_change(&bytes, &scaled) - 1;
    assert_eq!(bytes[rank_at], 4, "the rank scale count");
    seeds.push(("wrong_length", edited(&|b| b[rank_at] = 5)));
    // Six rank percentages of beatoraja-style's window policy.
    let style = rules::beatoraja_style();
    let style_bytes = style.canonical_bytes();
    let mut percent = style.clone();
    let rules::WindowPolicy::Scaled(s) = &mut percent.window_policy else {
        panic!("beatoraja-style scales by whole percentages");
    };
    s.rank_percent[0] += 1;
    let percent_at = first_change(&style_bytes, &percent) - 1;
    assert_eq!(style_bytes[percent_at], 5, "the rank percentage count");
    let mut six = style_bytes;
    six[percent_at] = 6;
    seeds.push(("wrong_percent_length", six));
    seeds.push(("older_version", edited(&|b| b[0] -= 1)));
    seeds.push(("trailing_bytes", edited(&|b| b.push(0))));
    seeds.push((
        "truncated",
        edited(&|b| {
            b.pop();
        }),
    ));
    let mut no_gauges = tapestry;
    no_gauges.gauges.clear();
    seeds.push(("invalid", no_gauges.canonical_bytes()));
    seeds
}

/// One `skin_json` input: the files joined by NUL bytes, entry file first.
fn skin_input(files: &[&str]) -> Vec<u8> {
    files.join("\0").into_bytes()
}

/// `skin_json` seeds: headers whose wrapper-enabled fields (options, file
/// choices, offsets) hold includes, choices and conditional items, the
/// lenient syntax, one refused input per kind, then skins that import
/// ([`skin_imports`]).
#[must_use]
pub fn skin_jsons() -> Vec<Seed> {
    let options = r#"{"type": 0, "name": "seed", "author": "a", "w": 1920, "h": 1080,
        "property": [
            {"name": "mode", "item": [{"name": "on", "op": 901}, {"name": "off", "op": 902}], "def": "on"},
            {"include": "i1.json"},
            {"if": 901, "values": {"include": "i2.json"}},
            {"if": [[901, 902], -903], "value": {"name": "either"}}
        ],
        "filepath": [{"name": "defs", "path": "*.json", "def": "s"}],
        "offset": [{"name": "o", "id": 40, "x": true, "y": true, "a": "true"}]}"#;
    let choices = r#"{"type": 0,
        "property": [{"name": "c", "item": [{"name": "x", "op": 1}]}],
        "filepath": [[{"if": 1, "value": {"name": "one", "path": "*"}}, {"value": {"include": "i1.json"}}]],
        "offset": {"include": "i2.json"}}"#;
    let lenient = "\u{feff}// a comment\n{type: 0, name: unquoted name  ,\n\
        property: [\n  {name: a /* block */}\n  {name: b},\n],\n\
        offset: [{id: 10.7, x: TRUE}]}";
    let configured_fails = configured_fails_entry();
    vec![
        (
            "options",
            skin_input(&[
                options,
                r#"[{"name": "included", "item": [{"name": "i", "op": 903}]}]"#,
                r#"[{"name": "when on"}, {"include": "i3.json"}]"#,
                r#"{"name": "nested"}"#,
            ]),
        ),
        (
            "choices",
            skin_input(&[
                choices,
                r#"{"name": "fallback", "path": "*.json"}"#,
                r#"[{"if": 1, "values": [{"name": "o1", "id": 1}]}, {"name": "o2", "id": 2}]"#,
            ]),
        ),
        ("lenient", skin_input(&[lenient])),
        (
            "cycle",
            skin_input(&[
                r#"{"type": 0, "property": {"include": "i1.json"}}"#,
                r#"{"include": "s.json"}"#,
            ]),
        ),
        ("not_play", skin_input(&[r#"{"type": 1}"#])),
        ("size", skin_input(&[r#"{"type": 0, "w": 1366, "h": 768}"#])),
        (
            "syntax",
            skin_input(&[r#"{"type": 0, "property": [{} {}]}"#]),
        ),
        (
            "missing",
            skin_input(&[r#"{"type": 0, "offset": [{"include": "none.json"}]}"#]),
        ),
        ("configured_fails", skin_input(&[&configured_fails, "[]"])),
        ("bar_group_limit", play_skin(&bar_group_limit_body())),
    ]
    .into_iter()
    .chain(skin_imports())
    .collect()
}

/// The bitmap font `font.fnt` of the `skin_json` folder: `A`, `B` and `l`
/// side by side on its 16×8 page `font_0.png`, a missing glyph, and a
/// kerning pair.
pub const FONT_FNT: &str = "info face=\"seed\" size=8 bold=0 italic=0 padding=0,0,0,0 spacing=1,1
common lineHeight=8 base=7 scaleW=16 scaleH=8 pages=1 packed=0
page id=0 file=\"font_0.png\"
chars count=4
char id=65   x=0 y=0 width=4 height=6 xoffset=0 yoffset=1 xadvance=5 page=0 chnl=15
char id=66   x=4 y=0 width=4 height=6 xoffset=0 yoffset=1 xadvance=5 page=0 chnl=15
char id=108  x=8 y=0 width=2 height=7 xoffset=1 yoffset=0 xadvance=3 page=0 chnl=15
char id=-1   x=10 y=0 width=4 height=6 xoffset=0 yoffset=1 xadvance=5 page=0 chnl=15
kernings count=1
kerning first=65 second=66 amount=-1
";

/// `skin_fnt` seeds: [`FONT_FNT`] and its variants that parse, then one
/// refused file per error the smoke tests expect.
#[must_use]
pub fn skin_fnts() -> Vec<Seed> {
    let edit = |from: &str, to: &str| FONT_FNT.replacen(from, to, 1).into_bytes();
    vec![
        ("font", FONT_FNT.as_bytes().to_vec()),
        ("crlf", FONT_FNT.replace('\n', "\r\n").into_bytes()),
        (
            "two_pages",
            edit(
                "pages=1 packed=0\npage id=0 file=\"font_0.png\"",
                "pages=2 packed=0\npage id=0 file=\"font_0.png\"\npage id=1 file=\"sub/font_1.png\"",
            ),
        ),
        ("supplementary", edit("id=108", "id=128512")),
        ("binary", b"BMF\x03\x01\x00\x00\x00".to_vec()),
        ("no_size", edit(" size=8", "")),
        ("page_id", edit("page id=0", "page id=1")),
        ("glyph_page", edit("xadvance=3 page=0", "xadvance=3 page=1")),
        ("duplicate_id", edit("id=66", "id=65")),
        ("count", edit("chars count=4", "chars count=5")),
        ("trailing", format!("{FONT_FNT}char id=67\n").into_bytes()),
        ("lone_cr", edit("page=0 chnl=15\n", "page=0\rchnl=15\n")),
        (
            "no_glyphs",
            b"info face=\"\" size=8 padding=0,0,0,0\ncommon lineHeight=8 base=7 scaleW=16 \
              scaleH=8 pages=1\npage id=0 file=\"font_0.png\"\nchar id=65 x=0 y=0 width=0 \
              height=6 xoffset=0 yoffset=1 xadvance=5 page=0\n"
                .to_vec(),
        ),
    ]
}

/// A 1920×1080 play skin of `body` with the resources of the skin folder
/// as sources `strip`, `grid` and font `f`.
fn play_skin(body: &str) -> Vec<u8> {
    skin_input(&[&format!(
        r#"{{"type": 0, "w": 1920, "h": 1080,
            "source": [{{"id": "strip", "path": "strip.png"}}, {{"id": "grid", "path": "grid.png"}}],
            "font": [{{"id": "f", "path": "font.ttf"}}], {body}}}"#
    )])
}

/// `skin_json` seeds that import: images, numbers, text, bitmap text,
/// tracks, ops,
/// offsets, a default file choice and the play objects.
#[must_use]
pub fn skin_imports() -> Vec<Seed> {
    vec![
        (
            "images",
            play_skin(
                r#""image": [
                    {"id": "s", "src": "strip", "w": 100, "h": 10, "divx": 10, "timer": 41, "cycle": 100},
                    {"id": "g", "src": "grid", "w": 8, "h": 8, "divx": 2, "divy": 2}],
                "destination": [
                    {"id": "s", "timer": 41, "dst": [{"x": 10, "y": 10, "w": 20, "h": 20}]},
                    {"id": "g", "blend": 2, "center": 7, "dst": [{"x": 50, "y": 10, "w": 8, "h": 8, "angle": 30}]}]"#,
            ),
        ),
        (
            "numbers",
            play_skin(
                r#""value": [
                    {"id": "ex", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "ref": 71},
                    {"id": "pg", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 13, "ref": 110, "align": 2}],
                "destination": [
                    {"id": "ex", "timer": 41, "dst": [{"x": 10, "y": 500, "w": 10, "h": 10}]},
                    {"id": "pg", "timer": 41, "dst": [{"x": 10, "y": 600, "w": 10, "h": 10}]}]"#,
            ),
        ),
        (
            "text",
            play_skin(
                r#""text": [
                    {"id": "t", "font": "f", "size": 20, "ref": 10},
                    {"id": "c", "font": "f", "size": 20, "constantText": "seed", "align": 1, "overflow": 2}],
                "destination": [
                    {"id": "t", "timer": 41, "dst": [{"x": 400, "y": 900, "w": 300, "h": 20}]},
                    {"id": "c", "timer": 41, "dst": [{"x": 400, "y": 800, "w": 300, "h": 20}]}]"#,
            ),
        ),
        (
            "bitmap_text",
            skin_input(&[r#"{"type": 0, "w": 1920, "h": 1080,
                "font": [{"id": "b", "path": "font.fnt"}],
                "text": [
                    {"id": "c", "font": "b", "size": 16, "constantText": "ABl?", "align": 1},
                    {"id": "t", "font": "b", "size": 8, "ref": 10}],
                "destination": [
                    {"id": "c", "timer": 41, "dst": [{"x": 400, "y": 800, "w": 300, "h": 16}]},
                    {"id": "t", "timer": 41, "dst": [{"x": 400, "y": 900, "w": 300, "h": 8}]}]}"#]),
        ),
        (
            "tracks",
            play_skin(
                r#""image": [{"id": "g", "src": "grid", "w": 4, "h": 4}],
                "destination": [{"id": "g", "timer": 41, "loop": 100, "dst": [
                    {"time": 0, "x": 0, "y": 0, "w": 4, "h": 4, "acc": 2},
                    {"time": 100, "x": 100, "a": 128},
                    {"time": 200, "x": 0, "a": 255}]}]"#,
            ),
        ),
        (
            "ops",
            play_skin(
                r#""property": [{"name": "p", "item": [{"name": "x", "op": 900}]}],
                "image": [{"id": "g", "src": "grid", "w": 4, "h": 4}],
                "destination": [
                    {"id": "g", "op": [-233], "dst": [{"w": 4, "h": 4}]},
                    {"id": "g", "op": [232, 900], "dst": [{"w": 4, "h": 4}]},
                    {"id": "g", "op": [7777], "dst": [{"w": 4, "h": 4}]}]"#,
            ),
        ),
        (
            "offsets",
            play_skin(
                r#""offset": [{"name": "o", "id": 40, "x": true}],
                "image": [{"id": "g", "src": "grid", "w": 4, "h": 4}],
                "destination": [
                    {"id": "g", "offset": 40, "dst": [{"w": 4, "h": 4}]},
                    {"id": "g", "offsets": [4], "dst": [{"w": 4, "h": 4}]}]"#,
            ),
        ),
        (
            "choices_default",
            play_skin(
                r#""filepath": [{"name": "Look", "path": "*.png", "def": "GRID"}],
                "source": [{"id": "any", "path": "*.png"}],
                "image": [{"id": "i", "src": "any", "w": -1, "h": -1}],
                "destination": [{"id": "i", "stretch": 9, "dst": [{"w": 100, "h": 100}]}]"#,
            ),
        ),
        ("lanes", play_skin(&lanes_body())),
        ("bar_groups", play_skin(&bar_groups_body())),
        ("paints", play_skin(&paints_body())),
        ("followers", play_skin(&followers_body())),
        (
            "judge",
            play_skin(
                r#""image": [
                    {"id": "g0", "src": "grid", "w": 4, "h": 4}, {"id": "g1", "src": "grid", "x": 4, "w": 4, "h": 4},
                    {"id": "g2", "src": "grid", "y": 4, "w": 4, "h": 4}, {"id": "g3", "src": "grid", "x": 4, "y": 4, "w": 4, "h": 4}],
                "value": [
                    {"id": "ten", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4},
                    {"id": "eleven", "src": "strip", "w": 99, "h": 10, "divx": 11, "digit": 3}],
                "judge": [{"id": "j", "index": 0, "shift": true,
                    "images": [
                        {"id": "g0", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g1", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g2", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g3", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g3", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g3", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]},
                        {"id": "g0", "timer": 46, "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]}],
                    "numbers": [
                        {"id": "ten", "timer": 46, "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "eleven", "timer": 46, "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "ten", "timer": 46, "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "ten", "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "ten", "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "ten", "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]},
                        {"id": "eleven", "timer": 46, "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]}]}],
                "destination": [{"id": "j", "dst": [{"w": 0, "h": 0}]}]"#,
            ),
        ),
        (
            "gauge",
            play_skin(
                r#""image": [
                    {"id": "n0", "src": "grid", "w": 4, "h": 4}, {"id": "n1", "src": "grid", "x": 4, "w": 4, "h": 4},
                    {"id": "n2", "src": "grid", "y": 4, "w": 4, "h": 4}, {"id": "n3", "src": "grid", "x": 4, "y": 4, "w": 4, "h": 4}],
                "gauge": {"id": "gg", "nodes": ["n0", "n1", "n2", "n3"], "parts": 50, "type": 3, "cycle": 100},
                "destination": [{"id": "gg", "dst": [{"x": 600, "y": 100, "w": 500, "h": 10}]}]"#,
            ),
        ),
        (
            "rates",
            play_skin(
                r#""image": [{"id": "g", "src": "grid", "w": 4, "h": 4}],
                "imageset": [
                    {"id": "kind", "images": ["g"], "ref": 40},
                    {"id": "first", "images": ["g"], "value": 40},
                    {"id": "byvalue", "images": ["g"], "value": 71, "ref": 40}],
                "value": [
                    {"id": "r102", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "ref": 102},
                    {"id": "r103", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "padding": 1, "value": "score_rate_afterdot"},
                    {"id": "r115", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "value": 115},
                    {"id": "r116", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "padding": 1, "value": 40, "ref": 116},
                    {"id": "r155", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "value": "score_rate2"},
                    {"id": "r156", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "padding": 1, "ref": 156}],
                "destination": [
                    {"id": "r102", "op": [42], "dst": [{"x": 10, "y": 100, "w": 10, "h": 10}]},
                    {"id": "r103", "op": [-42], "dst": [{"x": 10, "y": 120, "w": 10, "h": 10}]},
                    {"id": "r115", "op": [43, 43], "dst": [{"x": 10, "y": 140, "w": 10, "h": 10}]},
                    {"id": "r116", "draw": "!gauge_hard", "dst": [{"x": 10, "y": 160, "w": 10, "h": 10}]},
                    {"id": "r155", "op": ["gauge_ex"], "dst": [{"x": 10, "y": 180, "w": 10, "h": 10}]},
                    {"id": "r156", "op": [-1046], "dst": [{"x": 10, "y": 200, "w": 10, "h": 10}]},
                    {"id": "kind", "dst": [{"x": 100, "y": 100, "w": 4, "h": 4}]},
                    {"id": "first", "dst": [{"x": 100, "y": 120, "w": 4, "h": 4}]},
                    {"id": "byvalue", "dst": [{"x": 100, "y": 140, "w": 4, "h": 4}]}]"#,
            ),
        ),
        (
            "readouts",
            play_skin(
                r#""image": [{"id": "g", "src": "grid", "w": 4, "h": 4}],
                "value": [
                    {"id": "b90", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "ref": 90},
                    {"id": "b91", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "value": "minbpm"},
                    {"id": "b160", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "value": 160},
                    {"id": "t161", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "value": "playtime_minute"},
                    {"id": "t162", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "padding": 1, "ref": 162},
                    {"id": "t163", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "value": 163},
                    {"id": "t164", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "value": "timeleft_second"},
                    {"id": "c14", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": "lanecover1"},
                    {"id": "c314", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "ref": 314},
                    {"id": "c315", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": "hidden1"},
                    {"id": "c316", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": 316},
                    {"id": "h10", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": "hispeed_lr2"},
                    {"id": "h310", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "ref": 310},
                    {"id": "h311", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "padding": 1, "value": "hispeed_afterdot"},
                    {"id": "d312", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": 312},
                    {"id": "d313", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4, "value": "duration_green"},
                    {"id": "v15", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "value": 15, "ref": 71},
                    {"id": "v131", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "ref": 131},
                    {"id": "v92", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 3, "ref": 92}],
                "destination": [
                    {"id": "b90", "dst": [{"x": 10, "y": 100, "w": 10, "h": 10}]},
                    {"id": "b91", "dst": [{"x": 10, "y": 120, "w": 10, "h": 10}]},
                    {"id": "b160", "op": [271], "dst": [{"x": 10, "y": 140, "w": 10, "h": 10}]},
                    {"id": "t161", "dst": [{"x": 10, "y": 160, "w": 10, "h": 10}]},
                    {"id": "t162", "dst": [{"x": 10, "y": 180, "w": 10, "h": 10}]},
                    {"id": "t163", "dst": [{"x": 10, "y": 200, "w": 10, "h": 10}]},
                    {"id": "t164", "dst": [{"x": 10, "y": 220, "w": 10, "h": 10}]},
                    {"id": "c14", "dst": [{"x": 10, "y": 240, "w": 10, "h": 10}]},
                    {"id": "c314", "dst": [{"x": 10, "y": 260, "w": 10, "h": 10}]},
                    {"id": "c315", "dst": [{"x": 10, "y": 280, "w": 10, "h": 10}]},
                    {"id": "c316", "dst": [{"x": 10, "y": 300, "w": 10, "h": 10}]},
                    {"id": "h10", "dst": [{"x": 10, "y": 320, "w": 10, "h": 10}]},
                    {"id": "h310", "dst": [{"x": 10, "y": 340, "w": 10, "h": 10}]},
                    {"id": "h311", "dst": [{"x": 10, "y": 360, "w": 10, "h": 10}]},
                    {"id": "d312", "op": ["lift1_on"], "dst": [{"x": 10, "y": 380, "w": 10, "h": 10}]},
                    {"id": "d313", "dst": [{"x": 10, "y": 400, "w": 10, "h": 10}]},
                    {"id": "v15", "dst": [{"x": 10, "y": 420, "w": 10, "h": 10}]},
                    {"id": "v131", "dst": [{"x": 10, "y": 440, "w": 10, "h": 10}]},
                    {"id": "v92", "dst": [{"x": 10, "y": 460, "w": 10, "h": 10}]},
                    {"id": "g", "op": [270], "dst": [{"x": 100, "y": 100, "w": 4, "h": 4}]},
                    {"id": "g", "op": ["!lanecover1_changing"], "dst": [{"x": 100, "y": 120, "w": 4, "h": 4}]},
                    {"id": "g", "op": [271, "lanecover1_on"], "dst": [{"x": 100, "y": 140, "w": 4, "h": 4}]},
                    {"id": "g", "op": ["lift1_on"], "dst": [{"x": 100, "y": 160, "w": 4, "h": 4}]},
                    {"id": "g", "op": [-273], "dst": [{"x": 100, "y": 180, "w": 4, "h": 4}]}]"#,
            ),
        ),
        ("selectors", play_skin(&selectors_body())),
    ]
}

/// Image selection, generated: groups of the grid's four frames by the
/// gauge's type, the cover's switch, no property and past their frames;
/// image sets with a hole, an undefined id, a wider image and a repeat,
/// by each supported selector; and sets by image indices 500-507, which
/// the model cannot read.
fn selectors_body() -> String {
    let group = |id: &str, fields: &str| {
        format!(
            r#"{{"id": "{id}", "src": "grid", "w": 8, "h": 8, "divx": 2, "divy": 2,
                "timer": 41, "cycle": 100{fields}}}"#
        )
    };
    let groups = [
        ("q40", r#", "len": 2, "ref": 40"#),
        ("q330", r#", "len": 2, "ref": 330"#),
        ("qnull", r#", "len": 4"#),
        ("qnever", r#", "len": 5, "ref": 40"#),
    ];
    let sets: Vec<(String, String)> = [
        ("v71", r#""value": 71"#),
        ("v110", r#""value": 110"#),
        ("r40", r#""ref": 40"#),
        ("r331", r#""ref": 331"#),
        ("r332", r#""value": 40, "ref": 332"#),
    ]
    .into_iter()
    .map(|(id, f)| (id.to_owned(), f.to_owned()))
    .chain((500..=507).map(|r| (format!("r{r}"), format!(r#""ref": {r}"#))))
    .collect();
    let images: Vec<String> = groups
        .iter()
        .map(|(id, f)| group(id, f))
        .chain([
            r#"{"id": "a", "src": "grid", "w": 4, "h": 4}"#.to_owned(),
            r#"{"id": "b", "src": "grid", "x": 4, "w": 4, "h": 4}"#.to_owned(),
            r#"{"id": "wide", "src": "grid", "y": 4, "w": 8, "h": 4}"#.to_owned(),
        ])
        .collect();
    let imagesets: Vec<String> = sets
        .iter()
        .map(|(id, f)| {
            format!(r#"{{"id": "{id}", "images": ["a", "b", null, "none", "wide", "a"], {f}}}"#)
        })
        .collect();
    let dsts: Vec<String> = groups
        .iter()
        .map(|(id, _)| (*id).to_owned())
        .chain(sets.iter().map(|(id, _)| id.clone()))
        .enumerate()
        .map(|(i, id)| {
            format!(
                r#"{{"id": "{id}", "stretch": 1, "dst": [{{"x": {}, "y": 700, "w": 8, "h": 4}}]}}"#,
                10 + 20 * i
            )
        })
        .collect();
    format!(
        r#""image": [{}], "imageset": [{}], "destination": [{}]"#,
        images.join(", "),
        imagesets.join(", "),
        dsts.join(", ")
    )
}

/// The bar line group of [`lanes_body`].
const LANES_GROUP: &str =
    r#""group": [{"id": "bar", "dst": [{"x": 20, "y": 200, "w": 430, "h": 2}]}]"#;

/// [`lanes_body`] with four bar line groups: the plain one; an added,
/// translucent one following LIFT on the play timer over an image of two
/// frames whose `len` and `ref` (500) are unused; a turned one while
/// SUDDEN+ is on; and one of an undefined image, dropped.
fn bar_groups_body() -> String {
    lanes_body()
        .replace(
            r#"{"id": "cv", "src": "grid", "x": 4, "w": 4, "h": 4}],"#,
            r#"{"id": "cv", "src": "grid", "x": 4, "w": 4, "h": 4},
               {"id": "anim", "src": "grid", "w": 4, "h": 4, "divx": 2, "cycle": 100, "len": 2, "ref": 500}],"#,
        )
        .replace(
            LANES_GROUP,
            r#""group": [
                {"id": "bar", "dst": [{"x": 20, "y": 200, "w": 430, "h": 2}]},
                {"id": "anim", "blend": 2, "offset": 3, "timer": 41, "dst": [{"x": 20, "y": 199, "w": 430, "h": 4, "a": 128}]},
                {"id": "bar", "op": [271], "dst": [{"x": 100, "y": 198, "w": 50, "h": 6, "angle": 10}]},
                {"id": "nope", "dst": [{"x": 0, "y": 0, "w": 1, "h": 1}]}]"#,
        )
}

/// [`lanes_body`] with one bar line group past the cap.
fn bar_group_limit_body() -> String {
    // Empty records: the count is refused before any record is read.
    let groups = vec!["{}"; skin_model::MAX_BAR_GROUPS + 1].join(",");
    lanes_body().replace(LANES_GROUP, &format!(r#""group": [{groups}]"#))
}

/// [`lanes_body`] with what follows the covers, each id repeated: an image
/// beside the lanes following SUDDEN+, a number following LIFT, and a
/// judge whose grade follows LIFT, its relative combo taking 3 and 4
/// without motion of its own; HIDDEN+ keeps 3 and 5 as its own.
fn followers_body() -> String {
    lanes_body()
        .replace(
            r#"{"id": "cv", "src": "grid", "x": 4, "w": 4, "h": 4}],"#,
            r#"{"id": "cv", "src": "grid", "x": 4, "w": 4, "h": 4},
               {"id": "f", "src": "grid", "w": 4, "h": 4}],
            "value": [
                {"id": "num", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 2, "ref": 71},
                {"id": "ten", "src": "strip", "w": 100, "h": 10, "divx": 10, "digit": 4}],
            "judge": [{"id": "j", "index": 0,
                "images": [{"id": "f", "timer": 46, "offsets": [3, 3], "dst": [{"x": 600, "y": 500, "w": 80, "h": 20}]}],
                "numbers": [{"id": "ten", "timer": 46, "offset": 3, "offsets": [4, 3, 4], "dst": [{"x": 10, "y": 0, "w": 10, "h": 20}]}]}],"#,
        )
        .replace(
            r#"{"id": "hid", "dst""#,
            r#"{"id": "f", "offsets": [4, 4], "dst": [{"x": 500, "y": 300, "w": 4, "h": 4}]},
                {"id": "num", "offset": 3, "offsets": [3], "dst": [{"x": 500, "y": 400, "w": 10, "h": 10}]},
                {"id": "j", "dst": [{"w": 0, "h": 0}]},
                {"id": "hid", "offsets": [3, 5, 3], "dst""#,
        )
}

/// [`lanes_body`] with SUDDEN+ painted four ways between LIFT and HIDDEN+:
/// a split image, an additive one overhanging the lanes, a type-4 slider
/// and an image set chosen by the gauge's percent.
fn paints_body() -> String {
    let body = lanes_body();
    let at = body.find(r#""destination""#).unwrap_or(body.len());
    format!(
        r#"{}"slider": [{{"id": "sl", "src": "grid", "x": 4, "w": 4, "h": 4, "type": 4, "angle": 2, "range": 600}}],
            "imageset": [{{"id": "cs", "images": ["cv", "p"], "value": 110}}],
            "destination": [
                {{"id": "notes", "dst": [{{"w": 0, "h": 0}}]}},
                {{"id": "lc", "dst": [{{"x": 20, "y": 0, "w": 430, "h": 200}}]}},
                {{"id": "cv", "offset": 4, "dst": [{{"x": 20, "y": 800, "w": 215, "h": 600}}]}},
                {{"id": "cv", "offset": 4, "blend": 2, "dst": [{{"x": 0, "y": 700, "w": 500, "h": 300}}]}},
                {{"id": "sl", "dst": [{{"x": 235, "y": 800, "w": 215, "h": 600}}]}},
                {{"id": "cs", "offset": 4, "dst": [{{"x": 20, "y": 760, "w": 430, "h": 40}}]}},
                {{"id": "hid", "dst": [{{"x": 20, "y": -400, "w": 430, "h": 600}}]}}]"#,
        body.get(..at).unwrap_or_default()
    )
}

/// Lanes of `grid` parts over x 20..450, y 200..800, with a bar line
/// group on their bottom edge, SUDDEN+'s paint, LIFT and HIDDEN+.
fn lanes_body() -> String {
    let eight = [r#""p""#; 8].join(", ");
    let lists = [
        "note",
        "lnstart",
        "lnend",
        "lnbody",
        "lnactive",
        "mine",
        "hcnstart",
        "hcnend",
        "hcnbody",
        "hcnactive",
        "hcndamage",
        "hcnreactive",
    ]
    .map(|f| format!(r#""{f}": [{eight}]"#))
    .join(", ");
    let mut lanes: Vec<String> = (0..7)
        .map(|i| format!(r#"{{"x": {}, "y": 200, "w": 50, "h": 600}}"#, 100 + 50 * i))
        .collect();
    lanes.push(r#"{"x": 20, "y": 200, "w": 80, "h": 600}"#.to_owned());
    format!(
        r#""image": [{{"id": "p", "src": "grid", "w": 4, "h": 4}},
                     {{"id": "bar", "src": "grid", "y": 4, "w": 4, "h": 1}},
                     {{"id": "cv", "src": "grid", "x": 4, "w": 4, "h": 4}}],
            "note": {{"id": "notes", {lists}, "size": [20, 20, 20, 20, 20, 20, 20, 20], "dst": [{}],
                     {LANES_GROUP}}},
            "liftCover": [{{"id": "lc", "src": "grid", "x": 4, "y": 4, "w": 4, "h": 4, "disapearLine": 200}}],
            "hiddenCover": [{{"id": "hid", "src": "grid", "y": 4, "w": 4, "h": 4, "disapearLine": 200}}],
            "destination": [
                {{"id": "notes", "dst": [{{"w": 0, "h": 0}}]}},
                {{"id": "cv", "offset": 4, "dst": [{{"x": 20, "y": 800, "w": 430, "h": 600}}]}},
                {{"id": "lc", "dst": [{{"x": 20, "y": 0, "w": 430, "h": 200}}]}},
                {{"id": "hid", "dst": [{{"x": 20, "y": -400, "w": 430, "h": 600}}]}}]"#,
        lanes.join(", ")
    )
}

/// A header pass of 600 includes; the configured pass makes them too, then
/// fails on a missing file, so the pass after it starts from a partly
/// filled arena and counts that are only right if they start again.
fn configured_fails_entry() -> String {
    let includes = vec![r#"{"include": "i1.json"}"#; 600].join(",");
    format!(
        r#"{{"type": 0, "property": [{{"name": "m", "item": [{{"op": 901}}]}}, {includes},
        {{"if": 901, "values": {{"include": "none.json"}}}}]}}"#
    )
}
