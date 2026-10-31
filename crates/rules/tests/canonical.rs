//! Property: every valid preset survives `canonical_bytes` then
//! `from_canonical_bytes` unchanged.
//!
//! The presets are valid, since the decoder refuses the others, and reach
//! every enum variant, both booleans, one-byte to ten-byte varints, strings
//! and gauge lists whose length takes one or two varint bytes, and lamp
//! ranks drawn as a permutation of the gauges, spread apart or contiguous.

use bms_model::TimeUs;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rules::{
    Assignment, DefaultTotal, Denominator, EmptyPoorRange, EmptyPoorRule, GaugeDelta, GaugeKind,
    GaugeRule, GaugeValue, HeadEligibility, HeaderPolicy, Implementation, JudgeWindows, LaneTable,
    LateHeadRule, LnEndWindows, LnMode, LnModeRule, LowDamage, MineRule, MissRule, PerJudgment,
    PerLaneClass, RankScale, RuleError, RulePreset, SameTimeOrder, ScaledRank, ScoreWeights,
    TotalCorrection, TotalMode, Window, WindowPolicy,
};

const FULL: i64 = GaugeValue::FULL.0;

fn u32s() -> impl Strategy<Value = u32> {
    prop_oneof![0_u32..128, any::<u32>(), Just(u32::MAX)]
}

/// Not negative: small, any, and the largest.
fn non_negative() -> impl Strategy<Value = i64> {
    prop_oneof![0_i64..64, 0_i64..=i64::MAX, Just(i64::MAX)]
}

/// A non-empty string of one char, a few, or at least 128 bytes.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        vec(any::<char>(), 1),
        vec(any::<char>(), 2..8),
        vec(any::<char>(), 128..136),
    ]
    .prop_map(|c| c.into_iter().collect())
}

/// Edge steps: each window and LN-end edge stays below 2^31 µs, so a rank
/// scale up to 200 % keeps it within `MAX_WINDOW_EDGE` (2^32 µs).
fn step() -> BoxedStrategy<i64> {
    prop_oneof![1_i64..64, 1_i64..=1 << 28].boxed()
}

/// The best judgment's edge, which may be 0.
fn first() -> BoxedStrategy<i64> {
    prop_oneof![0_i64..64, 0_i64..=1 << 28].boxed()
}

fn windows_and_ln_end() -> impl Strategy<Value = (JudgeWindows, LnEndWindows)> {
    (
        [first(), step(), step(), step()],
        [first(), step(), step(), step()],
        ([first(), first(), first()], any::<bool>()),
    )
        .prop_map(|(early, late, (ln, tied))| {
            let ascending = |steps: [i64; 4]| {
                let mut edge = -1;
                steps.map(|s| {
                    edge += s.max(1);
                    TimeUs(edge)
                })
            };
            let [e0, e1, e2, e3] = ascending(early);
            let [l0, l1, l2, l3] = ascending(late);
            let windows = JudgeWindows {
                perfect: Window {
                    early: e0,
                    late: l0,
                },
                great: Window {
                    early: e1,
                    late: l1,
                },
                good: Window {
                    early: e2,
                    late: l2,
                },
                bad: Window {
                    early: e3,
                    late: l3,
                },
            };
            // Each LN-end edge is at least its window's early edge and the
            // better one's; they may tie, by a zero step or all at one width.
            let perfect = e0.0 + ln[0];
            let great = perfect.max(e1.0) + ln[1];
            let good = great.max(e2.0) + ln[2];
            let (perfect, great, good) = if tied {
                (good, good, good)
            } else {
                (perfect, great, good)
            };
            let ln_end = LnEndWindows {
                perfect: TimeUs(perfect),
                great: TimeUs(great),
                good: TimeUs(good),
            };
            (windows, ln_end)
        })
}

/// A scale up to 200 %.
fn scale() -> impl Strategy<Value = i64> {
    prop_oneof![1_i64..64, 1_i64..=200_000]
}

fn percent() -> impl Strategy<Value = u32> {
    prop_oneof![1_u32..8, 1_u32..=200]
}

fn window_policy() -> impl Strategy<Value = WindowPolicy> {
    prop_oneof![
        ([scale(), scale(), scale(), scale()], scale()).prop_map(|(rank_milli, absent_milli)| {
            WindowPolicy::Thousandths(RankScale {
                rank_milli,
                absent_milli,
            })
        }),
        (
            [percent(), percent(), percent(), percent(), percent()],
            percent(),
            u32s(),
            any::<bool>(),
        )
            .prop_map(
                |(rank_percent, absent_percent, defexrank_percent, interpolated)| {
                    let scaled = ScaledRank {
                        rank_percent,
                        absent_percent,
                        defexrank_percent,
                    };
                    if interpolated {
                        WindowPolicy::Interpolated(scaled)
                    } else {
                        WindowPolicy::Scaled(scaled)
                    }
                }
            ),
    ]
}

/// A lane class's table.
fn table() -> impl Strategy<Value = LaneTable> {
    (windows_and_ln_end(), non_negative(), non_negative()).prop_map(
        |((windows, ln_end), ahead, behind)| LaneTable {
            windows,
            ln_end,
            empty_poor: EmptyPoorRange {
                ahead: TimeUs(ahead),
                behind: TimeUs(behind),
            },
        },
    )
}

fn bools() -> impl Strategy<Value = PerJudgment<bool>> {
    any::<[bool; 5]>().prop_map(per_judgment)
}

fn per_judgment<T>([perfect, great, good, bad, poor]: [T; 5]) -> PerJudgment<T> {
    PerJudgment {
        perfect,
        great,
        good,
        bad,
        poor,
    }
}

fn delta() -> impl Strategy<Value = GaugeDelta> {
    prop_oneof![
        prop_oneof![-64_i64..64, -FULL..=FULL, Just(-FULL), Just(FULL)]
            .prop_map(|v| GaugeDelta::Fixed(GaugeValue(v))),
        (u32s(), prop_oneof![1_u32..128, 1..=u32::MAX])
            .prop_map(|(num, den)| GaugeDelta::TotalShare { num, den }),
    ]
}

/// A gauge without its id, its values ordered as `validate` requires.
fn gauge() -> impl Strategy<Value = GaugeRule> {
    let values = [0..=FULL, 0..=FULL, 0..=FULL, 0..=FULL];
    (
        values,
        (any::<bool>(), 0..=FULL),
        [delta(), delta(), delta(), delta(), delta()],
        delta(),
        prop_oneof![
            Just(TotalCorrection::None),
            Just(TotalCorrection::LimitIncrease),
            Just(TotalCorrection::ScaleDamage),
        ],
        prop_oneof![
            vec((0..=FULL, 1..=u32::MAX, 0_u32..=u32::MAX), 0..=1),
            vec((0..=FULL, 1..=u32::MAX, 0_u32..=u32::MAX), 2..6),
        ],
    )
        .prop_map(
            |(mut v, (survival, threshold), change, empty_poor, correction, low)| {
                v.sort_unstable();
                let [min, start, clear, max] = v.map(GaugeValue);
                let (kind, min, start, max) = if survival {
                    let max = max.max(GaugeValue(1));
                    let start = start.clamp(GaugeValue(1), max);
                    let fails_below = GaugeValue(threshold.min(start.0));
                    (
                        GaugeKind::Survival { fails_below },
                        GaugeValue::EMPTY,
                        start,
                        max,
                    )
                } else {
                    (GaugeKind::EndThreshold { clear }, min, start, max)
                };
                // Strictly ascending bands within the bounds; equal ones
                // collapse to one.
                let mut low_damage: Vec<LowDamage> = low
                    .into_iter()
                    .map(|(below, den, num)| LowDamage {
                        below: GaugeValue(below.clamp(min.0, max.0)),
                        // At most `den`, `den` included.
                        num: u32::try_from(u64::from(num) % (u64::from(den) + 1)).unwrap(),
                        den,
                    })
                    .collect();
                low_damage.sort_unstable_by_key(|b| b.below);
                low_damage.dedup_by_key(|b| b.below);
                // A corrected gauge has fixed changes only.
                let fixed = |d: GaugeDelta| match (correction, d) {
                    (TotalCorrection::None, d) | (_, d @ GaugeDelta::Fixed(_)) => d,
                    (_, GaugeDelta::TotalShare { num, .. }) => {
                        GaugeDelta::Fixed(GaugeValue(i64::from(num % 1_000)))
                    }
                };
                GaugeRule {
                    id: String::new(),
                    lamp: 0,
                    kind,
                    start,
                    min,
                    max,
                    change: per_judgment(change.map(fixed)),
                    empty_poor: fixed(empty_poor),
                    correction,
                    low_damage,
                }
            },
        )
}

/// One, a few, or at least 128 gauges, with distinct ids, and lamp ranks
/// a shuffled permutation of the list's positions: contiguous or spread by
/// a stride, counted up from 0 or down from `u16::MAX`.
fn gauges() -> impl Strategy<Value = Vec<GaugeRule>> {
    (
        text(),
        prop_oneof![vec(gauge(), 1), vec(gauge(), 2..5), vec(gauge(), 128..131)],
    )
        .prop_flat_map(|(base, gauges)| {
            let positions: Vec<u16> = (0..u16::try_from(gauges.len()).unwrap()).collect();
            (
                Just((base, gauges)),
                Just(positions).prop_shuffle(),
                prop_oneof![Just(1_u16), 1_u16..=500],
                any::<bool>(),
            )
        })
        .prop_map(|((base, mut gauges), order, stride, down)| {
            for (i, (g, position)) in gauges.iter_mut().zip(order).enumerate() {
                g.id = format!("{base}{i}");
                // At most 130 × 500, within a u16.
                let rank = position * stride;
                g.lamp = if down { u16::MAX - rank } else { rank };
            }
            gauges
        })
}

fn preset() -> impl Strategy<Value = RulePreset> {
    (
        (text(), u32s(), table(), table()),
        any::<[bool; 3]>(),
        (
            prop_oneof![
                Just(HeaderPolicy::Decimals),
                Just(HeaderPolicy::DefExRankFirst)
            ],
            window_policy(),
            prop_oneof![
                Just(Assignment::NearestTiesEarlier),
                Just(Assignment::ComboPriority)
            ],
            prop_oneof![
                Just(HeadEligibility::InsideBad),
                Just(HeadEligibility::NotLateBad)
            ],
        ),
        prop_oneof![
            prop_oneof![1_i64..64, 1_i64..=i64::MAX]
                .prop_map(|milli| DefaultTotal::Fixed { milli }),
            Just(DefaultTotal::NotesCurve),
            Just(DefaultTotal::NotesLinear),
        ],
        gauges(),
        ([u32s(), u32s(), u32s(), u32s(), u32s()], bools(), bools()),
        0..=FULL,
    )
        .prop_map(
            |(
                (id, version, keys, scratch),
                [breaks_combo, counts_bp, counted],
                (headers, window_policy, assignment, head_eligibility),
                default_total,
                gauges,
                (ex, breaks_combo_by, counts_bp_by),
                per_unit,
            )| {
                RulePreset {
                    id,
                    version,
                    implementation: Implementation::Standard {
                        version: Implementation::STANDARD_VERSION,
                    },
                    tables: PerLaneClass { keys, scratch },
                    miss: MissRule::PoorAfterLateBadOrOvertaken,
                    empty_poor: EmptyPoorRule {
                        breaks_combo,
                        counts_bp,
                        counted,
                    },
                    headers,
                    window_policy,
                    default_total,
                    denominator: Denominator::TapsAndLnHeads,
                    gauges,
                    score: ScoreWeights {
                        ex: per_judgment(ex),
                        breaks_combo: breaks_combo_by,
                        counts_bp: counts_bp_by,
                    },
                    same_time_order: SameTimeOrder::InputsFirst,
                    assignment,
                    ln_mode: LnModeRule {
                        default: LnMode::Ln,
                        late_head: LateHeadRule::CompletesAtPress,
                        head_eligibility,
                    },
                    total_mode: TotalMode::ScalesIncrease,
                    mine: MineRule::WhileHeld {
                        per_unit: GaugeValue(per_unit),
                    },
                }
            },
        )
}

proptest! {
    /// Decoding a valid preset's canonical bytes gives the preset back.
    #[test]
    fn canonical_bytes_round_trip(p in preset()) {
        prop_assert_eq!(p.validate(), Ok(()));
        prop_assert_eq!(RulePreset::from_canonical_bytes(&p.canonical_bytes()), Ok(p));
    }
}

/// Over a fixed number of deterministic samples, the generator reaches every
/// variant of each enum with more than one, both values of every boolean,
/// ten-byte varints, and each length class of the strings and gauge lists.
#[test]
fn generator_reaches_every_variant_and_length_class() {
    let mut runner = TestRunner::deterministic();
    let presets = preset();
    let mut counts = [0_usize; 36];
    // Per boolean field, whether it was seen false and true.
    let mut bools_seen = [[false; 2]; 13];
    for _ in 0..256 {
        let p = presets.new_tree(&mut runner).unwrap().current();
        let deltas = p.gauges.iter().flat_map(|g| {
            let c = g.change;
            [c.perfect, c.great, c.good, c.bad, c.poor, g.empty_poor]
        });
        let fixed = deltas
            .clone()
            .any(|d| matches!(d, GaugeDelta::Fixed(v) if v.0 < 0));
        let share = deltas
            .clone()
            .any(|d| matches!(d, GaugeDelta::TotalShare { .. }));
        let reached = [
            fixed,
            share,
            p.gauges
                .iter()
                .any(|g| matches!(g.kind, GaugeKind::EndThreshold { .. })),
            p.gauges
                .iter()
                .any(|g| matches!(g.kind, GaugeKind::Survival { .. })),
            p.gauges.iter().any(
                |g| matches!(g.kind, GaugeKind::Survival { fails_below } if fails_below.0 > 0),
            ),
            p.gauges.iter().any(|g| g.low_damage.is_empty()),
            p.gauges.iter().any(|g| g.low_damage.len() == 1),
            p.gauges.iter().any(|g| g.low_damage.len() >= 2),
            p.gauges
                .iter()
                .any(|g| g.correction == TotalCorrection::None),
            p.gauges
                .iter()
                .any(|g| g.correction == TotalCorrection::LimitIncrease),
            p.gauges
                .iter()
                .any(|g| g.correction == TotalCorrection::ScaleDamage),
            matches!(p.default_total, DefaultTotal::Fixed { .. }),
            p.default_total == DefaultTotal::NotesCurve,
            p.default_total == DefaultTotal::NotesLinear,
            p.tables.keys.empty_poor.ahead.0 >= 1 << 62,
            p.tables.scratch.empty_poor.behind.0 >= 1 << 62,
            p.tables.keys != p.tables.scratch,
            matches!(p.window_policy, WindowPolicy::Thousandths(_)),
            matches!(p.window_policy, WindowPolicy::Scaled(_)),
            matches!(p.window_policy, WindowPolicy::Interpolated(_)),
            p.tables.keys.ln_end.great == p.tables.keys.ln_end.good,
            p.id.chars().count() == 1,
            p.id.len() >= 128,
            p.gauges.len() == 1,
            (2..128).contains(&p.gauges.len()),
            p.gauges.len() >= 128,
            p.gauges.iter().any(|g| g.id.len() >= 128),
            p.headers == HeaderPolicy::Decimals,
            p.headers == HeaderPolicy::DefExRankFirst,
            p.assignment == Assignment::NearestTiesEarlier,
            p.assignment == Assignment::ComboPriority,
            p.ln_mode.head_eligibility == HeadEligibility::InsideBad,
            p.ln_mode.head_eligibility == HeadEligibility::NotLateBad,
            // Ranks out of list order, spread apart, and of two or three
            // varint bytes.
            p.gauges.windows(2).any(|w| w[0].lamp > w[1].lamp),
            p.gauges.len() >= 2 && {
                let mut ranks: Vec<u16> = p.gauges.iter().map(|g| g.lamp).collect();
                ranks.sort_unstable();
                ranks.windows(2).all(|w| w[1] - w[0] >= 2)
            },
            p.gauges.iter().any(|g| g.lamp >= 1 << 14),
        ];
        for (count, hit) in counts.iter_mut().zip(reached) {
            *count += usize::from(hit);
        }
        let e = p.empty_poor;
        let all = |w: PerJudgment<bool>| [w.perfect, w.great, w.good, w.bad, w.poor];
        let [b0, b1, b2, b3, b4] = all(p.score.breaks_combo);
        let [c0, c1, c2, c3, c4] = all(p.score.counts_bp);
        let bools = [
            e.breaks_combo,
            e.counts_bp,
            e.counted,
            b0,
            b1,
            b2,
            b3,
            b4,
            c0,
            c1,
            c2,
            c3,
            c4,
        ];
        for (seen, b) in bools_seen.iter_mut().zip(bools) {
            seen[usize::from(b)] = true;
        }
    }
    assert!(
        counts.iter().all(|&c| c > 0),
        "unreached classes: {counts:?}"
    );
    assert!(
        bools_seen.iter().all(|&[f, t]| f && t),
        "a boolean field missing a value: {bools_seen:?}"
    );
}

/// Each built-in, and one with a rank changed: encoding then decoding gives
/// the preset back, and a changed rank changes the bytes and so the preset's
/// hash, which is the SHA-256 of these bytes.
#[test]
fn canonical_round_trips_lamp_ranks() {
    for preset in [
        rules::tapestry(),
        rules::beatoraja_style(),
        rules::lr2_style(),
    ] {
        let bytes = preset.canonical_bytes();
        assert_eq!(bytes.first(), Some(&11));
        assert_eq!(rules::CANONICAL_VERSION, 11);
        assert_eq!(RulePreset::from_canonical_bytes(&bytes), Ok(preset.clone()));
        let mut changed = preset.clone();
        let top = changed.gauges.iter().map(|g| g.lamp).max().unwrap();
        changed.gauges[0].lamp = top + 1;
        let changed_bytes = changed.canonical_bytes();
        assert_ne!(changed_bytes, bytes);
        assert_eq!(
            RulePreset::from_canonical_bytes(&changed_bytes),
            Ok(changed)
        );
    }
}

/// Bytes whose two gauges share a rank: tapestry's encoding with the
/// survival gauge's rank (the byte after its id) set to normal's, 0.
#[test]
fn canonical_decoder_refuses_duplicate_lamp_ranks() {
    let mut bytes = rules::tapestry().canonical_bytes();
    let id = b"survival";
    let at = bytes
        .windows(id.len())
        .position(|w| w == id)
        .expect("the survival gauge's id")
        + id.len();
    assert_eq!(bytes[at], 1);
    bytes[at] = 0;
    assert_eq!(
        RulePreset::from_canonical_bytes(&bytes),
        Err(rules::CanonicalError::Invalid(RuleError::DuplicateLamp {
            rank: 0,
            first: "normal".to_owned(),
            second: "survival".to_owned(),
        }))
    );
}
