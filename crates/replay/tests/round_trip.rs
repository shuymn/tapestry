//! Property: every play record survives `encode` then `decode` unchanged
//! (design.md "Testing Strategy").
//!
//! Every integer mixes small values (one-byte varints) with the full range
//! (ten-byte varints and wrapping time deltas).

use bms_model::{ChartIdentity, Lane, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, InputStart, PhysicalInputEvent,
    ScratchSettings, SourceId, SourceProfile,
};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use replay::{
    Arrival, BuildProvenance, ChartRecord, DecodeLimits, DeviceKind, DeviceRecord, EndStatus,
    Incomplete, PlayRecord, PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status,
    TransformRecord, Versions, WakePolicy, decode, encode,
};
use sha2::{Digest, Sha256};

fn u64s() -> impl Strategy<Value = u64> {
    prop_oneof![0_u64..128, any::<u64>(), (1_u64 << 63).., Just(u64::MAX)]
}

fn u32s() -> impl Strategy<Value = u32> {
    prop_oneof![0_u32..128, any::<u32>(), Just(u32::MAX)]
}

fn i64s() -> impl Strategy<Value = i64> {
    prop_oneof![-64_i64..64, any::<i64>(), Just(i64::MIN), Just(i64::MAX)]
}

fn i32s() -> impl Strategy<Value = i32> {
    prop_oneof![-64_i32..64, any::<i32>(), Just(i32::MIN), Just(i32::MAX)]
}

fn text() -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..8).prop_map(|c| c.into_iter().collect())
}

fn kind() -> impl Strategy<Value = DeviceKind> {
    prop_oneof![Just(DeviceKind::Keyboard), Just(DeviceKind::Controller)]
}

fn lane() -> impl Strategy<Value = Lane> {
    (0_u8..8).prop_map(|l| Lane::new(l).unwrap())
}

fn versions() -> impl Strategy<Value = Versions> {
    (
        u32s(),
        text(),
        prop_oneof![
            Just(BuildProvenance::Production),
            Just(BuildProvenance::Development)
        ],
    )
        .prop_map(|(evaluator, app, provenance)| Versions {
            evaluator,
            app,
            provenance,
        })
}

fn chart() -> impl Strategy<Value = ChartRecord> {
    (
        (
            any::<[u8; 32]>(),
            any::<[u8; 16]>(),
            any::<[u8; 32]>(),
            u32s(),
            vec(u32s(), 0..8),
        ),
        u64s(),
        (text(), u32s()),
    )
        .prop_map(
            |(
                (sha256, md5, resolved_hash, loader_version, random_results),
                random_seed,
                (id, version),
            )| {
                ChartRecord {
                    identity: ChartIdentity {
                        sha256,
                        md5,
                        resolved_hash,
                        loader_version,
                        random_results,
                    },
                    random_seed,
                    transform: TransformRecord { id, version },
                }
            },
        )
}

/// A real preset's record under another id, or any bytes with their hash.
fn preset() -> impl Strategy<Value = PresetRecord> {
    prop_oneof![
        text().prop_map(|id| {
            let mut p = rules::custom();
            p.id = id;
            PresetRecord::of(&p)
        }),
        (text(), vec(any::<u8>(), 0..64)).prop_map(|(id, canonical_bytes)| PresetRecord {
            id,
            sha256: Sha256::digest(&canonical_bytes).into(),
            canonical_bytes,
        }),
    ]
}

fn axis() -> impl Strategy<Value = AxisBinding> {
    (
        u32s(),
        lane(),
        prop_oneof![
            u32s().prop_map(|wrap| AxisMode::Absolute { wrap }),
            Just(AxisMode::Relative)
        ],
        (u32s(), u32s(), u32s()),
    )
        .prop_map(
            |(control, lane, mode, (press_threshold, hold_threshold, release_after_us))| {
                AxisBinding {
                    control: ControlId(control),
                    lane,
                    scratch: ScratchSettings {
                        mode,
                        press_threshold,
                        hold_threshold,
                        release_after_us,
                    },
                }
            },
        )
}

fn source() -> impl Strategy<Value = SourceRecord> {
    (
        kind(),
        prop_oneof![Just(ReceiveMethod::RawInput), Just(ReceiveMethod::Scripted)],
        i64s(),
        vec(
            (u32s(), lane()).prop_map(|(c, lane)| Binding {
                control: ControlId(c),
                lane,
            }),
            0..=8,
        ),
        // At most 4 sources of 4 axes stay within `eval::MAX_AXES`.
        vec(axis(), 0..=4),
        u64s(),
    )
        .prop_map(
            |(kind, receive, offset, bindings, axes, axis_out_of_range)| SourceRecord {
                kind,
                receive,
                profile: SourceProfile {
                    input_offset: TimeUs(offset),
                    bindings,
                    axes,
                },
                axis_out_of_range,
            },
        )
}

fn wake() -> impl Strategy<Value = WakePolicy> {
    prop_oneof![
        (u32s(), u32s(), u32s()).prop_map(|(heartbeat_us, input_gap_us, drain_cut)| {
            WakePolicy::Input {
                heartbeat_us,
                input_gap_us,
                drain_cut,
            }
        }),
        u32s().prop_map(|heartbeat_us| WakePolicy::Timer { heartbeat_us }),
    ]
}

fn record() -> impl Strategy<Value = PlayRecord> {
    (
        (versions(), chart(), preset(), text()),
        (vec(source(), 1..=4), i64s(), i64s(), wake()),
        (i64s(), vec((any::<u16>(), u32s(), i32s()), 0..=6)),
        vec(
            (
                any::<u16>(),
                prop_oneof![i64s(), -1_000_i64..1_000],
                prop_oneof![
                    3 => (u32s(), i32s()).prop_map(|(c, value)| InputKind::Control {
                        control: ControlId(c),
                        value,
                    }),
                    1 => Just(InputKind::Disconnected),
                ],
            ),
            0..=64,
        ),
        vec((i64s(), kind()), 0..4),
        (
            prop_oneof![Just(EndStatus::PlayedToEnd), Just(EndStatus::EndedMidway)],
            i64s(),
            any::<[bool; 4]>(),
        ),
    )
        .prop_map(
            |(
                (versions, chart, preset, session_policy),
                (sources, audio_offset, display_offset, wake),
                (opened_at, held),
                events,
                arrivals,
                (end, finalized, [overflow, source_limit, suspend, input_failed]),
            )| {
                let n = u16::try_from(sources.len()).unwrap();
                let source = |raw: u16| SourceId(raw % n);
                PlayRecord {
                    versions,
                    chart,
                    preset,
                    session_policy,
                    devices: DeviceRecord {
                        sources,
                        audio_offset: TimeUs(audio_offset),
                        display_offset: TimeUs(display_offset),
                        wake,
                    },
                    play_type: PlayType::Full,
                    start: InputStart {
                        opened_at: TimeUs(opened_at),
                        held: held
                            .into_iter()
                            .map(|(s, c, v)| (source(s), ControlId(c), v))
                            .collect(),
                    },
                    // Small times sit near `opened_at`, so deltas are small
                    // too; full-range ones wrap.
                    events: events
                        .into_iter()
                        .map(|(s, t, kind)| PhysicalInputEvent {
                            received: TimeUs(if (-1_000..1_000).contains(&t) {
                                opened_at.wrapping_add(t)
                            } else {
                                t
                            }),
                            source: source(s),
                            kind,
                        })
                        .collect(),
                    arrivals: arrivals
                        .into_iter()
                        .map(|(at, kind)| Arrival {
                            at: TimeUs(at),
                            kind,
                        })
                        .collect(),
                    status: Status {
                        end,
                        finalized: TimeUs(finalized),
                        incomplete: Incomplete {
                            overflow,
                            source_limit,
                            suspend,
                            input_failed,
                        },
                    },
                }
            },
        )
}

proptest! {
    /// P6: decoding an encoded record gives the record back.
    #[test]
    fn encode_then_decode_gives_the_record_back(rec in record()) {
        prop_assert_eq!(decode(&encode(&rec), &DecodeLimits::STANDARD), Ok(rec));
    }
}

/// Whether `v` takes a ten-byte varint.
fn ten_bytes(v: u64) -> bool {
    v >= 1 << 63
}

/// Whether `v` takes a ten-byte zigzag varint.
fn ten_bytes_signed(v: i64) -> bool {
    ten_bytes(((v << 1) ^ (v >> 63)).cast_unsigned())
}

/// The generator reaches ten-byte varints, wrapping time deltas, and each
/// list empty, over a fixed number of deterministic samples. Lists stay
/// below 128 entries, so their counts are one-byte varints and the decode
/// limits are not reached here (the unit tests cover those).
#[test]
fn generator_reaches_ten_byte_varints_and_empty_lists() {
    let mut runner = TestRunner::deterministic();
    let records = record();
    let mut counts = [0_usize; 10];
    for _ in 0..256 {
        let r = records.new_tree(&mut runner).unwrap().current();
        let sources = &r.devices.sources;
        // A delta that wraps around the i64 range.
        let mut prev = vec![r.start.opened_at.0; sources.len()];
        let wraps = r.events.iter().any(|e| {
            let p = std::mem::replace(&mut prev[e.source.index()], e.received.0);
            e.received.0.checked_sub(p).is_none()
        });
        let reached = [
            ten_bytes(r.chart.random_seed)
                || sources.iter().any(|s| ten_bytes(s.axis_out_of_range)),
            ten_bytes_signed(r.start.opened_at.0)
                || ten_bytes_signed(r.status.finalized.0)
                || sources
                    .iter()
                    .any(|s| ten_bytes_signed(s.profile.input_offset.0)),
            wraps,
            r.events.is_empty(),
            r.arrivals.is_empty(),
            r.start.held.is_empty(),
            r.chart.identity.random_results.is_empty(),
            sources.iter().any(|s| s.profile.bindings.is_empty()),
            sources.iter().any(|s| s.profile.axes.is_empty()),
            r.events.len() > 32,
        ];
        for (n, hit) in counts.iter_mut().zip(reached) {
            *n += usize::from(hit);
        }
    }
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
}
