//! Generators for the property tests: charts, presets, input logs and
//! delivery schedules, valid by construction.
//!
//! Each strategy draws independent raw values and maps them to a valid
//! value (`% n`, cumulative gaps), so shrinking a raw value always gives a
//! valid case and nothing is filtered out. Times sit on a 10 ms grid with
//! occasional jitter, and offsets are often grid multiples, so equal times
//! across sources and between inputs and deadlines are common.

use bms_model::{
    Chart, ChartBuilder, HeaderValue, JudgmentHeader, JudgmentHeaderName, LANE_COUNT, Lane, LnMode,
    Note, NoteKind, SoundId, TimeUs,
};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, DeviceProfile, EvalInput, Evaluator, InputKind,
    InputStart, JudgeEventKind, PhysicalInputEvent, ScratchSettings, Section, SourceId,
    SourceProfile, evaluate, evaluation_time,
};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use rules::{
    Assignment, DefaultTotal, EmptyPoorRange, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    HeaderPolicy, JudgeRank, JudgeWindows, LaneTable, LnEndWindows, LowDamage, MineRule,
    PerJudgment, PerLaneClass, RankScale, RulePreset, ScaledRank, TotalCorrection, Window,
    WindowPolicy,
};

use super::live::{Delivery, Publish, SourceStep};

const MS: i64 = 1_000;
/// The time grid.
const TICK: i64 = 10 * MS;
const FULL: i64 = GaugeValue::FULL.0;
const PERCENT: i64 = GaugeValue::PERCENT;
/// The axis control a source may bind to the scratch.
const AXIS: ControlId = ControlId(200);
/// The extra button control a source may bind.
const EXTRA: ControlId = ControlId(100);
/// The control the joint cases bind and press.
const JOINT: ControlId = ControlId(400);
/// A control no source binds.
const UNBOUND: ControlId = ControlId(300);

fn lane(i: usize) -> Lane {
    Lane::new(u8::try_from(i).unwrap()).unwrap()
}

/// `raw` mapped onto `0..len` (`len` at least 1).
fn pick(raw: impl Into<u64>, len: usize) -> usize {
    usize::try_from(raw.into() % u64::try_from(len.max(1)).unwrap()).unwrap()
}

/// `raw` mapped onto `0..n`.
fn below(raw: impl Into<u64>, n: i64) -> i64 {
    i64::try_from(raw.into() % u64::try_from(n).unwrap()).unwrap()
}

// ---------------------------------------------------------------- chart

#[derive(Clone, Debug)]
struct RawNote {
    gap: u8,
    jitter: u8,
    kind: u8,
    len: u8,
    damage: u8,
    sound: Option<u8>,
}

fn raw_note() -> impl Strategy<Value = RawNote> {
    (
        1_u8..=20,
        any::<u8>(),
        any::<u8>(),
        1_u8..=30,
        0_u8..=60,
        option::of(any::<u8>()),
    )
        .prop_map(|(gap, jitter, kind, len, damage, sound)| RawNote {
            gap,
            jitter,
            kind,
            len,
            damage,
            sound,
        })
}

#[derive(Clone, Debug)]
struct RawChart {
    lanes: Vec<Vec<RawNote>>,
    sounds: u8,
    rank: (u8, u32),
    total: Option<u32>,
    ln: bool,
}

fn raw_chart() -> impl Strategy<Value = RawChart> {
    (
        vec(vec(raw_note(), 0..=5), LANE_COUNT),
        0_u8..=4,
        (any::<u8>(), any::<u32>()),
        option::of(any::<u32>()),
        any::<bool>(),
    )
        .prop_map(|(lanes, sounds, rank, total, ln)| RawChart {
            lanes,
            sounds,
            rank,
            total,
            ln,
        })
}

/// 0, a few µs either way, or 20 ms either way.
fn jitter(raw: u8) -> i64 {
    let k = i64::from(raw / 8 % 3) + 1;
    match raw % 8 {
        4 => k,
        5 => -k,
        6 => 20 * MS,
        7 => -20 * MS,
        _ => 0,
    }
}

/// The rank declarations: none, `#RANK -1` to `#RANK 5` (beatoraja-style
/// reads 0-4, tapestry any, and the rank scale takes 0-3), a `#DEFEXRANK` up
/// to 200 % as an integer token or a decimal, one that is not a number, or
/// a `#RANK` followed by one of those, which the header policies may read
/// differently.
fn rank(raw: (u8, u32)) -> Vec<JudgmentHeader> {
    let declare = |name, value| JudgmentHeader { name, value };
    let r = declare(
        JudgmentHeaderName::Rank,
        HeaderValue::integer(below(raw.1, 7) - 1),
    );
    let defexrank = match raw.0 / 8 % 3 {
        0 => HeaderValue::integer(below(raw.1 / 7, 200) + 1),
        1 => HeaderValue::decimal_milli(below(raw.1 / 7, 200_000) + 1),
        _ => HeaderValue::NotANumber,
    };
    let d = declare(JudgmentHeaderName::DefExRank, defexrank);
    match raw.0 % 4 {
        0 => Vec::new(),
        1 => vec![r],
        2 => vec![d],
        _ => vec![r, d],
    }
}

/// The `#TOTAL` declarations: none, or one from -0.001 to 400 as a decimal,
/// after a `#TOTAL 200` half the time, so a value not positive keeps the
/// earlier one under beatoraja-style and the default under tapestry.
fn total(raw: Option<u32>) -> Vec<JudgmentHeader> {
    let declare = |value| JudgmentHeader {
        name: JudgmentHeaderName::Total,
        value,
    };
    let Some(t) = raw else {
        return Vec::new();
    };
    let last = declare(HeaderValue::decimal_milli(below(t / 2, 400_002) - 1));
    if t % 2 == 0 {
        vec![last]
    } else {
        vec![declare(HeaderValue::integer(200)), last]
    }
}

/// An `opened_at` before every receive time of `logs` (they can be
/// negative), else 0.
fn opened_before<'a>(logs: impl IntoIterator<Item = &'a PhysicalInputEvent>) -> TimeUs {
    TimeUs(
        logs.into_iter()
            .map(|e| e.received.0 - 1)
            .min()
            .unwrap_or(0)
            .min(0),
    )
}

fn build_chart(raw: &RawChart) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..raw.sounds {
        b = b.sound(&format!("s{i}"));
    }
    // The sound of each note in creation order, which is time order.
    let mut sounds: Vec<Vec<Option<u8>>> = vec![Vec::new(); LANE_COUNT];
    let mut last_end = [0_i64; LANE_COUNT];
    let mut judgeable = false;
    for (l, notes) in raw.lanes.iter().enumerate() {
        let mut grid = 100 * MS;
        let mut prev_end: Option<i64> = None;
        for n in notes {
            grid += i64::from(n.gap) * TICK;
            let mut t = grid + jitter(n.jitter);
            if let Some(e) = prev_end
                && t <= e
            {
                t = e + 1;
            }
            let end = match n.kind % 6 {
                0..=2 => {
                    b = b.tap(lane(l), TimeUs(t));
                    t
                }
                3 | 4 => {
                    let end = t + i64::from(n.len) * TICK;
                    grid += i64::from(n.len) * TICK;
                    b = b.long(lane(l), TimeUs(t), TimeUs(end));
                    end
                }
                _ => {
                    b = b.mine(lane(l), TimeUs(t), i32::from(n.damage));
                    t
                }
            };
            judgeable |= n.kind % 6 <= 4;
            sounds[l].push(n.sound);
            prev_end = Some(end);
            last_end[l] = end;
        }
    }
    if !judgeable {
        // A fix-up, not a filter: every chart has a note to judge.
        b = b.tap(lane(1), TimeUs(last_end[1] + 500 * MS));
        sounds[1].push(None);
    }
    let mut chart = b.build().unwrap();
    // The builder has no setters for note sounds or the meta.
    for (notes, sounds) in chart.lanes.iter_mut().zip(&sounds) {
        for (note, sound) in notes.iter_mut().zip(sounds) {
            if raw.sounds > 0
                && let Some(s) = sound
                && !matches!(note.kind, NoteKind::Mine { .. })
            {
                note.sound = Some(SoundId(u16::from(s % raw.sounds)));
            }
        }
    }
    chart.meta.judgment_headers = rank(raw.rank);
    chart.meta.judgment_headers.extend(total(raw.total));
    chart.meta.ln_mode = raw.ln.then_some(LnMode::Ln);
    chart.validate().unwrap();
    chart
}

// --------------------------------------------------------------- preset

#[derive(Clone, Debug)]
struct RawGauge {
    survival: bool,
    bounds: [u32; 4],
    deltas: [(u8, u32); 6],
    /// The correction, and a survival gauge's failure threshold.
    policy: (u8, u32),
    low: Vec<(u8, u8, u32)>,
}

fn raw_gauge() -> impl Strategy<Value = RawGauge> {
    (
        any::<bool>(),
        any::<[u32; 4]>(),
        any::<[(u8, u32); 6]>(),
        any::<(u8, u32)>(),
        vec(any::<(u8, u8, u32)>(), 0..=3),
    )
        .prop_map(|(survival, bounds, deltas, policy, low)| RawGauge {
            survival,
            bounds,
            deltas,
            policy,
            low,
        })
}

/// One lane class's table, raw.
#[derive(Clone, Debug)]
struct RawTable {
    early: [u32; 4],
    late: [u32; 4],
    symmetric: bool,
    empty_poor: (u32, u32),
    ln_end: [u32; 3],
}

fn raw_table() -> impl Strategy<Value = RawTable> {
    (
        (any::<[u32; 4]>(), any::<[u32; 4]>(), any::<bool>()),
        (any::<(u32, u32)>(), any::<[u32; 3]>()),
    )
        .prop_map(
            |((early, late, symmetric), (empty_poor, ln_end))| RawTable {
                early,
                late,
                symmetric,
                empty_poor,
                ln_end,
            },
        )
}

#[derive(Clone, Debug)]
struct RawPreset {
    keys: RawTable,
    /// The scratch's own table, used when flag 16 is set; else the keys'.
    scratch: RawTable,
    flags: u32,
    rank: [u32; 6],
    defexrank: u32,
    total: u32,
    gauges: Vec<RawGauge>,
    ex: [u8; 5],
    per_unit: u32,
}

fn raw_preset() -> impl Strategy<Value = RawPreset> {
    (
        (raw_table(), raw_table(), any::<u32>()),
        (any::<[u32; 6]>(), any::<u32>(), any::<u32>()),
        vec(raw_gauge(), 1..=3),
        (any::<[u8; 5]>(), any::<u32>()),
    )
        .prop_map(
            |((keys, scratch, flags), (rank, defexrank, total), gauges, (ex, per_unit))| {
                RawPreset {
                    keys,
                    scratch,
                    flags,
                    rank,
                    defexrank,
                    total,
                    gauges,
                    ex,
                    per_unit,
                }
            },
        )
}

/// A window edge increment: a grid multiple (so deadlines meet inputs),
/// or any µs count; the first edge may be 0.
fn edge_step(raw: u32, first: bool) -> i64 {
    let v = u64::from(raw / 4);
    match raw % 4 {
        0 if first => 0,
        0 | 1 => (below(v, 10) + 1) * TICK,
        2 => (below(v, 4) + 1) * 5 * MS,
        _ => below(v, 100_000) + 1,
    }
}

/// Four strictly ascending edges from cumulative positive steps.
fn edges(raw: [u32; 4]) -> [i64; 4] {
    let mut e = [0; 4];
    let mut at = 0;
    for (k, r) in raw.iter().enumerate() {
        at += edge_step(*r, k == 0);
        e[k] = at;
    }
    e
}

fn windows_of(early: [i64; 4], late: [i64; 4]) -> JudgeWindows {
    let w = |k: usize| Window {
        early: TimeUs(early[k]),
        late: TimeUs(late[k]),
    };
    JudgeWindows {
        perfect: w(0),
        great: w(1),
        good: w(2),
        bad: w(3),
    }
}

/// A gauge change: large fixed amounts (to reach the bounds), small fixed
/// amounts, shares of the increment, or anything in range.
fn delta(sel: u8, v: u32) -> GaugeDelta {
    let v64 = u64::from(v);
    match sel % 5 {
        0 => GaugeDelta::Fixed(GaugeValue(-FULL + below(v64, 10 * PERCENT))),
        1 => GaugeDelta::Fixed(GaugeValue(FULL - below(v64, 10 * PERCENT))),
        2 => GaugeDelta::Fixed(GaugeValue(below(v64, 20 * PERCENT + 1) - 10 * PERCENT)),
        3 => GaugeDelta::TotalShare {
            num: v % 5,
            den: v / 5 % 4 + 1,
        },
        _ => GaugeDelta::Fixed(GaugeValue(below(v64, 2 * FULL + 1) - FULL)),
    }
}

fn build_gauge(raw: &RawGauge, id: usize) -> GaugeRule {
    let [a, b, c, d] = raw.bounds.map(u64::from);
    let (sel, v) = raw.policy;
    let (kind, min, start, max) = if raw.survival {
        // Often a low start, so failures happen.
        let max = FULL - below(a, 2) * below(a / 2, FULL / 2);
        let start = if b % 2 == 0 {
            1 + below(b / 2, 5 * PERCENT)
        } else {
            1 + below(b / 2, max)
        };
        let start = start.min(max);
        // Failing at 0, below 2 % (the LR2-style threshold), or below any
        // value up to the start.
        let fails_below = match (sel / 3) % 3 {
            0 => 0,
            1 => (2 * PERCENT).min(start),
            _ => below(v, start + 1),
        };
        (
            GaugeKind::Survival {
                fails_below: GaugeValue(fails_below),
            },
            0,
            start,
            max,
        )
    } else {
        let min = below(a % 3, 2) * below(a / 3, FULL / 4);
        let max = min + below(b, (FULL - min) + 1);
        let start = min + below(c, (max - min) + 1);
        let clear = min + below(d, (max - min) + 1);
        (
            GaugeKind::EndThreshold {
                clear: GaugeValue(clear),
            },
            min,
            start,
            max,
        )
    };
    let correction = [
        TotalCorrection::None,
        TotalCorrection::LimitIncrease,
        TotalCorrection::ScaleDamage,
    ][usize::from(sel % 3)];
    // A corrected gauge has fixed changes only: a share becomes a small
    // fixed increase.
    let fixed = |d: GaugeDelta| match (correction, d) {
        (TotalCorrection::None, d) | (_, d @ GaugeDelta::Fixed(_)) => d,
        (_, GaugeDelta::TotalShare { num, den }) => {
            GaugeDelta::Fixed(GaugeValue(i64::from(num * 4_000 / den)))
        }
    };
    let [p, g, gd, bd, pr, ep] = raw.deltas.map(|(s, v)| fixed(delta(s, v)));
    // Bands in strictly ascending order; equal thresholds collapse to one.
    let mut low_damage: Vec<LowDamage> = raw
        .low
        .iter()
        .map(|&(num, den, below_raw)| {
            let den = u32::from(den % 4) + 1;
            LowDamage {
                below: GaugeValue(min + below(below_raw, (max - min) + 1)),
                num: u32::from(num) % (den + 1),
                den,
            }
        })
        .collect();
    low_damage.sort_unstable_by_key(|b| b.below);
    low_damage.dedup_by_key(|b| b.below);
    GaugeRule {
        id: format!("g{id}"),
        kind,
        start: GaugeValue(start),
        min: GaugeValue(min),
        max: GaugeValue(max),
        change: PerJudgment {
            perfect: p,
            great: g,
            good: gd,
            bad: bd,
            poor: pr,
        },
        empty_poor: ep,
        correction,
        low_damage,
    }
}

/// An empty POOR distance: 0, a grid multiple, or any µs count.
fn distance(raw: u32) -> TimeUs {
    let v = u64::from(raw / 3);
    TimeUs(match raw % 3 {
        0 => 0,
        1 => below(v, 51) * TICK,
        _ => below(v, 500_001),
    })
}

/// A rank scale: the usual ones, or anything up to 200 %.
fn scale(raw: u32) -> i64 {
    match raw % 3 {
        0 => [50_000, 75_000, 100_000, 125_000][pick(raw / 3, 4)],
        1 => 100_000,
        _ => below(raw / 3, 200_000) + 1,
    }
}

/// A whole-percent rank scale: the usual ones, or anything up to 200 %.
fn percent(raw: u32) -> u32 {
    let p = match raw % 3 {
        0 => [25, 50, 75, 100, 125][pick(raw / 3, 5)],
        1 => 100,
        _ => below(raw / 3, 200) + 1,
    };
    u32::try_from(p).unwrap()
}

/// The factor `p`'s window policy scales edges by for the rank its header
/// policy reads from `headers`, as `(num, den)`, by hand.
fn scale_of(p: &RulePreset, headers: &[JudgmentHeader]) -> (i64, i64) {
    let rank = p.headers.read(headers).rank;
    let percent = |s: ScaledRank| match rank {
        Some(JudgeRank::Rank(r @ 0..=4)) => (i64::from(s.rank_percent[r as usize]), 100),
        Some(JudgeRank::DefExRankMilli(v)) if v > 0 => {
            (v * i64::from(s.defexrank_percent), 10_000_000)
        }
        _ => (i64::from(s.absent_percent), 100),
    };
    match p.window_policy {
        WindowPolicy::Thousandths(s) => (s.scale_milli(rank).unwrap(), 100_000),
        WindowPolicy::Scaled(s) | WindowPolicy::Interpolated(s) => percent(s),
    }
}

/// `edge` at the factor `num / den` by the LR2 rule, by hand, after the
/// reference's `lr2JudgeScaling`: linear from 100 %; below, the row of
/// the table the edge reaches, then the two columns around the percentage.
fn lr2_edge(edge: i64, num: i128, den: i128) -> i128 {
    const TABLE: [[i128; 5]; 4] = [
        [0, 0, 0, 0, 0],
        [0, 8_000, 15_000, 18_000, 21_000],
        [0, 24_000, 30_000, 40_000, 60_000],
        [0, 40_000, 60_000, 100_000, 120_000],
    ];
    let base = i128::from(edge);
    if num >= den {
        return base * num / den;
    }
    let column = usize::try_from(4 * num / den).unwrap();
    let mut s = 0;
    while s < TABLE.len() && base >= TABLE[s][4] {
        s += 1;
    }
    let (d, x1, x2) = if s < TABLE.len() {
        let (lo, hi) = (TABLE[s - 1], TABLE[s]);
        let (n, d) = (base - lo[4], hi[4] - lo[4]);
        let x = |c: usize| d * lo[c] + n * (hi[c] - lo[c]);
        (d, x(column), x(column + 1))
    } else {
        let last = TABLE[s - 1];
        (last[4], base * last[column], base * last[column + 1])
    };
    // The percentage `100 num / den` lies `(4 num − column den) / den` of
    // the way to the next column.
    (x1 * den + (4 * num - i128::try_from(column).unwrap() * den) * (x2 - x1)) / (den * d)
}

/// `p`'s windows of a lane class whose table holds `windows` and
/// `ln_end`, at the rank its header policy reads from `headers`, by hand:
/// every edge scaled and floored, or under the interpolated policy
/// PERFECT, GREAT and GOOD by [`lr2_edge`], each clamped to the next wider
/// on its side, BAD as it is.
pub fn windows_at(
    p: &RulePreset,
    headers: &[JudgmentHeader],
    windows: JudgeWindows,
) -> JudgeWindows {
    let (num, den) = scale_of(p, headers);
    let w = windows.table().map(|(_, w)| w);
    let scaled = |e: TimeUs| TimeUs(e.0 * num / den);
    match p.window_policy {
        WindowPolicy::Thousandths(_) | WindowPolicy::Scaled(_) => {
            windows_of(w.map(|w| scaled(w.early).0), w.map(|w| scaled(w.late).0))
        }
        WindowPolicy::Interpolated(_) => {
            let side = |edge: fn(&Window) -> TimeUs| {
                let mut e = w.map(|w| edge(&w).0);
                for k in (0..3).rev() {
                    let v = lr2_edge(e[k], i128::from(num), i128::from(den));
                    e[k] = i64::try_from(v.min(i128::from(e[k + 1]))).unwrap();
                }
                e
            };
            windows_of(side(|w| w.early), side(|w| w.late))
        }
    }
}

fn table_of(raw: &RawTable) -> LaneTable {
    let late = edges(raw.late);
    let early = if raw.symmetric {
        late
    } else {
        edges(raw.early)
    };
    LaneTable {
        windows: windows_of(early, late),
        ln_end: ln_end_of(raw.ln_end, early),
        empty_poor: EmptyPoorRange {
            ahead: distance(raw.empty_poor.0),
            behind: distance(raw.empty_poor.1),
        },
    }
}

/// Valid LN-end edges for the note windows' `early` edges: each at least
/// its judgment's early edge and the better one, by 0, a grid multiple, or
/// any µs count more; or, with the first's top bit set, all three at
/// GOOD's (LR2-style's tie).
fn ln_end_of(raw: [u32; 3], early: [i64; 4]) -> LnEndWindows {
    let mut e = [0; 3];
    let mut floor = 0;
    for (k, r) in raw.iter().enumerate() {
        let v = u64::from((r & 0x7fff_ffff) / 3);
        let extra = match r % 3 {
            0 => 0,
            1 => below(v, 10) * TICK,
            _ => below(v, 200_000),
        };
        e[k] = floor.max(early[k]) + extra;
        floor = e[k];
    }
    if raw[0] >> 31 == 1 {
        e = [e[2]; 3];
    }
    LnEndWindows {
        perfect: TimeUs(e[0]),
        great: TimeUs(e[1]),
        good: TimeUs(e[2]),
    }
}

fn build_preset(raw: &RawPreset) -> RulePreset {
    let mut p = rules::tapestry();
    let flag = |bit: u32| raw.flags & (1 << bit) != 0;
    let keys = table_of(&raw.keys);
    p.tables = PerLaneClass {
        keys,
        scratch: if flag(16) {
            table_of(&raw.scratch)
        } else {
            keys
        },
    };
    p.empty_poor.breaks_combo = flag(0);
    p.empty_poor.counts_bp = flag(1);
    p.empty_poor.counted = flag(2);
    // Both assignment policies: nearest first (tapestry) and combo priority
    // (beatoraja-style).
    p.assignment = if flag(18) {
        Assignment::ComboPriority
    } else {
        Assignment::NearestTiesEarlier
    };
    // Both LN-head eligibilities: every head inside BAD (tapestry,
    // beatoraja-style) and no late BAD head (LR2-style), with either
    // assignment.
    p.ln_mode.head_eligibility = if flag(19) {
        rules::HeadEligibility::NotLateBad
    } else {
        rules::HeadEligibility::InsideBad
    };
    p.headers = if flag(3) {
        HeaderPolicy::DefExRankFirst
    } else {
        HeaderPolicy::Decimals
    };
    // The three window policies: thousandths (tapestry), scaled
    // (beatoraja-style) and interpolated (LR2-style).
    p.window_policy = if flag(17) {
        let scaled = ScaledRank {
            rank_percent: [0, 1, 2, 3, 4].map(|k| percent(raw.rank[k])),
            absent_percent: percent(raw.rank[5]),
            defexrank_percent: percent(raw.defexrank),
        };
        if flag(20) {
            WindowPolicy::Interpolated(scaled)
        } else {
            WindowPolicy::Scaled(scaled)
        }
    } else {
        WindowPolicy::Thousandths(RankScale {
            rank_milli: [0, 1, 2, 3].map(|k| scale(raw.rank[k])),
            absent_milli: scale(raw.rank[4]),
        })
    };
    p.default_total = match raw.total % 4 {
        0 | 1 => DefaultTotal::Fixed {
            milli: below(raw.total / 4, 400_000) + 1,
        },
        2 => DefaultTotal::NotesCurve,
        _ => DefaultTotal::NotesLinear,
    };
    p.gauges = raw
        .gauges
        .iter()
        .enumerate()
        .map(|(i, g)| build_gauge(g, i))
        .collect();
    let ex = raw.ex.map(|e| u32::from(e % 4));
    p.score.ex = PerJudgment {
        perfect: ex[0],
        great: ex[1],
        good: ex[2],
        bad: ex[3],
        poor: ex[4],
    };
    p.score.breaks_combo = PerJudgment {
        perfect: flag(5),
        great: flag(6),
        good: flag(7),
        bad: !flag(8),
        poor: !flag(9),
    };
    p.score.counts_bp = PerJudgment {
        perfect: flag(10),
        great: flag(11),
        good: flag(12),
        bad: !flag(13),
        poor: !flag(14),
    };
    let per_unit = match raw.per_unit % 3 {
        0 => 0,
        1 => PERCENT,
        _ => below(raw.per_unit / 3, FULL + 1),
    };
    p.mine = MineRule::WhileHeld {
        per_unit: GaugeValue(per_unit),
    };
    p
}

// ------------------------------------------------------------------ log

#[derive(Clone, Debug)]
struct RawEvent {
    gap: u8,
    jitter: u8,
    control: u8,
    value: u8,
}

fn raw_event() -> impl Strategy<Value = RawEvent> {
    (0_u8..=12, any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(gap, jitter, control, value)| {
        RawEvent {
            gap,
            jitter,
            control,
            value,
        }
    })
}

#[derive(Clone, Debug)]
struct RawSource {
    offset: (u8, u16),
    extra: Option<u8>,
    axis: Option<(u8, u8, u8, u32)>,
    held: u16,
    events: Vec<RawEvent>,
    disconnect: Option<u8>,
    capacity: u8,
}

fn raw_source() -> impl Strategy<Value = RawSource> {
    (
        any::<(u8, u16)>(),
        option::weighted(0.3, any::<u8>()),
        option::weighted(0.4, any::<(u8, u8, u8, u32)>()),
        any::<u16>(),
        vec(raw_event(), 0..=24),
        option::weighted(0.4, 0_u8..=20),
        any::<u8>(),
    )
        .prop_map(
            |(offset, extra, axis, held, events, disconnect, capacity)| RawSource {
                offset,
                extra,
                axis,
                held,
                events,
                disconnect,
                capacity,
            },
        )
}

#[derive(Clone, Debug)]
struct RawLog {
    sources: Vec<RawSource>,
    interleave: Vec<u8>,
}

fn raw_log() -> impl Strategy<Value = RawLog> {
    (vec(raw_source(), 1..=3), vec(any::<u8>(), 0..=80)).prop_map(|(sources, interleave)| RawLog {
        sources,
        interleave,
    })
}

fn offset(raw: (u8, u16)) -> i64 {
    match raw.0 % 6 {
        0 => 0,
        1 => TICK,
        2 => -TICK,
        3 => 2 * TICK,
        4 => -2 * TICK,
        _ => below(raw.1, 10_001) - 5 * MS,
    }
}

fn scratch(raw: (u8, u8, u8, u32)) -> ScratchSettings {
    let (mode, press, hold, release) = raw;
    ScratchSettings {
        mode: match mode % 3 {
            0 => AxisMode::Absolute { wrap: 0 },
            1 => AxisMode::Absolute {
                wrap: 16 + u32::from(mode / 3 % 49),
            },
            _ => AxisMode::Relative,
        },
        press_threshold: u32::from(press % 5),
        hold_threshold: u32::from(hold % 4),
        release_after_us: if release % 2 == 0 {
            (release / 2 % 20 + 1) * 10_000
        } else {
            release / 2 % 200_000 + 1
        },
    }
}

/// The device, the snapshot and each source's log in receive order.
fn build_log(raw: &RawLog) -> (DeviceProfile, InputStart, Vec<Vec<PhysicalInputEvent>>) {
    let mut device = DeviceProfile::default();
    let mut start = InputStart::default();
    let mut logs = Vec::new();
    for (i, s) in raw.sources.iter().enumerate() {
        let source = SourceId(u16::try_from(i).unwrap());
        let off = offset(s.offset);
        let mut bindings: Vec<Binding> = (0..LANE_COUNT)
            .map(|l| Binding {
                control: ControlId(u32::try_from(l).unwrap()),
                lane: lane(l),
            })
            .collect();
        if let Some(l) = s.extra {
            bindings.push(Binding {
                control: EXTRA,
                lane: lane(pick(l, LANE_COUNT)),
            });
        }
        let axes: Vec<AxisBinding> = s
            .axis
            .map(|a| AxisBinding {
                control: AXIS,
                lane: Lane::SCRATCH,
                scratch: scratch(a),
            })
            .into_iter()
            .collect();
        for l in 0..LANE_COUNT {
            if s.held & (1 << l) != 0 {
                start
                    .held
                    .push((source, ControlId(u32::try_from(l).unwrap()), 1));
            }
        }
        if !axes.is_empty() && s.held & (1 << 8) != 0 {
            start.held.push((source, AXIS, i32::from(s.held >> 9 & 63)));
        }
        let mut controls: Vec<ControlId> = bindings.iter().map(|b| b.control).collect();
        let relative = axes
            .first()
            .is_some_and(|a| a.scratch.mode == AxisMode::Relative);
        if !axes.is_empty() {
            // As likely as all buttons together.
            controls.extend([AXIS; 4]);
        }
        let mut log = Vec::new();
        let mut grid = 0;
        let mut received = i64::MIN;
        for e in &s.events {
            grid += i64::from(e.gap) * TICK;
            let t = if e.gap == 0 && received != i64::MIN {
                // One batch: the same receive time.
                received
            } else {
                (grid + off + small_jitter(e.jitter)).max(received)
            };
            received = t;
            let control = if e.control % 16 == 15 {
                UNBOUND
            } else {
                controls[pick(e.control, controls.len())]
            };
            let value = if control == AXIS {
                if relative {
                    i32::from(e.value % 7) - 3
                } else {
                    i32::from(e.value % 64)
                }
            } else {
                [0, 0, 0, 1, 2, -1][pick(e.value, 6)]
            };
            log.push(PhysicalInputEvent {
                received: TimeUs(t),
                source,
                kind: InputKind::Control { control, value },
            });
        }
        // Source 0 stays: removing the last open source aborts the play.
        if i > 0
            && let Some(gap) = s.disconnect
        {
            let t = (grid + off + i64::from(gap) * TICK).max(received);
            log.push(PhysicalInputEvent {
                received: TimeUs(t),
                source,
                kind: InputKind::Disconnected,
            });
        }
        device.sources.push(SourceProfile {
            input_offset: TimeUs(off),
            bindings,
            axes,
        });
        logs.push(log);
    }
    (device, start, logs)
}

/// Mostly none; sometimes 1 to 3 µs late.
fn small_jitter(raw: u8) -> i64 {
    if raw % 8 == 4 {
        i64::from(raw / 8 % 3) + 1
    } else {
        0
    }
}

/// One recording order of `logs` that keeps each source's order: `choices`
/// picks the source of each next event.
pub fn interleave(logs: &[Vec<PhysicalInputEvent>], choices: &[u8]) -> Vec<PhysicalInputEvent> {
    let mut next = vec![0; logs.len()];
    let total: usize = logs.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for k in 0..total {
        let open: Vec<usize> = (0..logs.len())
            .filter(|&i| next[i] < logs[i].len())
            .collect();
        let i = open[pick(choices.get(k).copied().unwrap_or(0), open.len())];
        out.push(logs[i][next[i]]);
        next[i] += 1;
    }
    out
}

// -------------------------------------------------------------- section

#[derive(Clone, Debug)]
pub struct RawSection {
    start: (u8, u16),
    end: (u8, u16),
    lead_in: (u8, u32),
    gauges: [(u8, u32); 3],
}

pub fn raw_section() -> impl Strategy<Value = RawSection> {
    (
        any::<(u8, u16)>(),
        any::<(u8, u16)>(),
        any::<(u8, u32)>(),
        any::<[(u8, u32); 3]>(),
    )
        .prop_map(|(start, end, lead_in, gauges)| RawSection {
            start,
            end,
            lead_in,
            gauges,
        })
}

/// Whether `n` is a target of `section` (design.md "Section Evaluation"),
/// restated here: a tap or mine in `[start, end)`, a long note whose head
/// is at or after the start and whose end is before the end.
pub fn is_target(section: Option<&Section>, n: &Note) -> bool {
    section.is_none_or(|s| s.start <= n.time && n.time < s.end && n.end() < s.end)
}

/// A section whose edges sit on note times and long-note ends, 1 µs either
/// side of them, or anywhere on the grid, with a lead-in of 0, grid
/// multiples, any µs count or 2 s, and each gauge starting at its preset
/// start, its limits or anywhere between.
fn build_section(chart: &Chart, preset: &RulePreset, raw: &RawSection) -> Section {
    let mut edges: Vec<i64> = chart
        .lanes
        .iter()
        .flatten()
        .flat_map(|n| [n.time.0, n.end().0])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let at = |(sel, v): (u8, u16)| match sel % 5 {
        0 | 1 => edges[pick(v, edges.len())],
        2 => edges[pick(v, edges.len())] - 1,
        3 => edges[pick(v, edges.len())] + 1,
        _ => below(v, 300) * TICK,
    };
    let (a, b) = (at(raw.start), at(raw.end));
    let (sel, v) = raw.lead_in;
    let lead_in = match sel % 4 {
        0 => 0,
        1 => below(v, 200) * TICK,
        2 => below(v, 2_000_001),
        _ => 2_000 * MS,
    };
    let initial_gauges = preset
        .gauges
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let (sel, v) = raw.gauges[i % 3];
            // A survival gauge never starts at 0 or below its threshold.
            let low = match g.kind {
                GaugeKind::Survival { fails_below } => fails_below.0.max(1),
                GaugeKind::EndThreshold { .. } => g.min.0,
            };
            GaugeValue(match sel % 4 {
                0 => g.start.0,
                1 => low,
                2 => g.max.0,
                _ => low + below(v, g.max.0 - low + 1),
            })
        })
        .collect();
    Section {
        start: TimeUs(a.min(b)),
        end: TimeUs(a.max(b)),
        lead_in: TimeUs(lead_in),
        initial_gauges,
    }
}

/// The section's earliest target: (lane, time).
fn first_target(chart: &Chart, section: &Section) -> Option<(usize, TimeUs)> {
    judgeable(chart, Some(section)).first().copied()
}

// ----------------------------------------------------------------- play

/// A generated play with what the properties and the coverage test need.
#[derive(Clone, Debug)]
pub struct Play {
    pub chart: Chart,
    pub preset: RulePreset,
    pub input: EvalInput,
    /// Each source's events in receive order.
    pub logs: Vec<Vec<PhysicalInputEvent>>,
    /// Each source's producer capacity: its largest group of equal
    /// receive times, one more, and a generated extra.
    pub capacity: Vec<usize>,
    /// The deadlines of the chart, or of the section, sorted: misses, LN
    /// ends and mines.
    pub deadlines: Vec<TimeUs>,
    pub final_deadline: TimeUs,
    /// Times a publication may aim at: input evaluation times, deadlines
    /// and the final deadline (never empty, even for a section without
    /// targets or input).
    pub times: Vec<TimeUs>,
    /// A watermark the schedules step on deliberately (the joint case of an
    /// input on a deadline at an intermediate W).
    pub sync_at: Option<TimeUs>,
}

#[derive(Clone, Debug)]
pub struct RawPlay {
    chart: RawChart,
    preset: RawPreset,
    log: RawLog,
    joint: (u8, u16),
    section: Option<RawSection>,
}

pub fn raw_play() -> impl Strategy<Value = RawPlay> {
    (
        raw_chart(),
        raw_preset(),
        raw_log(),
        any::<(u8, u16)>(),
        option::weighted(0.4, raw_section()),
    )
        .prop_map(|(chart, preset, log, joint, section)| RawPlay {
            chart,
            preset,
            log,
            joint,
            section,
        })
}

/// The judgeable notes of `section` (the whole chart for `None`): (lane,
/// time), in time order.
fn judgeable(chart: &Chart, section: Option<&Section>) -> Vec<(usize, TimeUs)> {
    let mut v: Vec<(usize, TimeUs)> = chart
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(l, notes)| {
            notes
                .iter()
                .filter(move |n| !matches!(n.kind, NoteKind::Mine { .. }) && is_target(section, n))
                .map(move |n| (l, n.time))
        })
        .collect();
    v.sort_by_key(|&(l, t)| (t, l));
    v
}

/// Each lane class's windows after the window policy.
fn lane_windows(ev: &Evaluator<'_>) -> PerLaneClass<JudgeWindows> {
    PerLaneClass {
        keys: ev.table(lane(1)).windows,
        scratch: ev.table(Lane::SCRATCH).windows,
    }
}

fn deadlines(
    chart: &Chart,
    tables: &PerLaneClass<JudgeWindows>,
    section: Option<&Section>,
) -> Vec<TimeUs> {
    let mut v: Vec<TimeUs> = Vec::new();
    for (l, notes) in chart.lanes.iter().enumerate() {
        let windows = tables.of_lane(lane(l));
        for note in notes.iter().filter(|n| is_target(section, n)) {
            deadline_of(note, windows, &mut v);
        }
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// A note's deadlines: its miss at its lane's late BAD edge, a long note's
/// end, a mine's time.
fn deadline_of(note: &Note, windows: &JudgeWindows, v: &mut Vec<TimeUs>) {
    match note.kind {
        NoteKind::Tap => v.push(TimeUs(note.time.0 + windows.bad.late.0)),
        NoteKind::Long { end, .. } => {
            v.push(TimeUs(note.time.0 + windows.bad.late.0));
            v.push(end);
        }
        NoteKind::Mine { .. } => v.push(note.time),
    }
}

/// Inserts `e` into a source's log in receive order, before its
/// disconnect, which moves later if needed.
fn insert(log: &mut Vec<PhysicalInputEvent>, e: PhysicalInputEvent) {
    let disconnect = log
        .last()
        .copied()
        .filter(|d| d.kind == InputKind::Disconnected);
    if disconnect.is_some() {
        log.pop();
    }
    let at = log.partition_point(|x| x.received <= e.received);
    log.insert(at, e);
    if let Some(mut d) = disconnect {
        d.received = d
            .received
            .max(log.last().map_or(d.received, |x| x.received));
        log.push(d);
    }
}

/// A change of the button that plays lane `l`.
fn button(received: i64, source: usize, l: usize, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(u16::try_from(source).unwrap()),
        kind: InputKind::Control {
            control: ControlId(u32::try_from(l).unwrap()),
            value,
        },
    }
}

/// A press (`value` 1) or release (0) of the joint cases' own control,
/// which no random event touches.
fn joint(received: i64, source: usize, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(u16::try_from(source).unwrap()),
        kind: InputKind::Control {
            control: JOINT,
            value,
        },
    }
}

/// Binds the joint control of source `s` to lane `l`; returns the
/// source's offset.
fn bind(device: &mut DeviceProfile, s: usize, l: usize) -> i64 {
    let source = &mut device.sources[s];
    source.bindings.push(Binding {
        control: JOINT,
        lane: lane(l),
    });
    source.input_offset.0
}

/// Releases every button of every source that plays lane `l`, except the
/// joint control, at evaluation time `at`, so a joint press there is a
/// press edge.
fn clear_lane(logs: &mut [Vec<PhysicalInputEvent>], device: &DeviceProfile, l: usize, at: i64) {
    for (s, (log, source)) in logs.iter_mut().zip(&device.sources).enumerate() {
        for b in &source.bindings {
            if b.lane == lane(l) && b.control != JOINT {
                let mut e = joint(at + source.input_offset.0, s, 0);
                e.kind = InputKind::Control {
                    control: b.control,
                    value: 0,
                };
                insert(log, e);
            }
        }
    }
}

/// A note the log misses, so that a joint press on it shows, else any
/// judgeable note: (lane, time).
fn target(
    chart: &Chart,
    preset: &RulePreset,
    device: &DeviceProfile,
    start: &InputStart,
    logs: &[Vec<PhysicalInputEvent>],
    section: Option<&Section>,
    raw: u64,
) -> (usize, TimeUs) {
    let base = EvalInput {
        events: interleave(logs, &[]),
        device: device.clone(),
        start: start.clone(),
        finalized: None,
        section: section.cloned(),
    };
    let e = evaluate(chart, &base, preset).unwrap();
    let mut notes: Vec<(usize, TimeUs)> = e
        .events
        .iter()
        .filter_map(|ev| match ev.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                deviation: None,
                ..
            } if ev.origin.is_none() => Some((lane.index(), chart.lanes[lane.index()][note].time)),
            _ => None,
        })
        .collect();
    if notes.is_empty() {
        notes = judgeable(chart, section);
    }
    if notes.is_empty() {
        notes = judgeable(chart, None);
    }
    notes[pick(raw, notes.len())]
}

pub fn build_play(raw: &RawPlay) -> Play {
    let chart = build_chart(&raw.chart);
    let mut preset = build_preset(&raw.preset);
    let (mut device, mut start, mut logs) = build_log(&raw.log);
    let n = logs.len();
    let mut section = raw
        .section
        .as_ref()
        .map(|r| build_section(&chart, &preset, r));
    let (windows, final_deadline) = {
        let ev = Evaluator::new(&chart, &preset, &device, &start, section.as_ref(), 0).unwrap();
        (lane_windows(&ev), ev.final_deadline())
    };
    let (sel, param) = raw.joint;
    let param = u64::from(param);
    let mut sync_at = None;
    // Without a section, the section-only case 3 plays case 1, which only
    // a whole-chart play reaches: a section takes input after its final
    // deadline for conversion only.
    let case = match sel % 8 {
        3 if section.is_none() => 1,
        c => c,
    };
    match case {
        // A press on a missed note's miss deadline, which the schedules make
        // an intermediate W: inputs go first at equal times, across steps
        // too, so the press judges the note.
        0 => {
            let (l, t) = target(
                &chart,
                &preset,
                &device,
                &start,
                &logs,
                section.as_ref(),
                param,
            );
            let s = pick(param / 8, n);
            let x = t.0 + windows.of_lane(lane(l)).bad.late.0;
            let off = bind(&mut device, s, l);
            clear_lane(&mut logs, &device, l, x - 1);
            insert(&mut logs[s], joint(x + off, s, 1));
            insert(&mut logs[s], joint(x + off + TICK, s, 0));
            sync_at = Some(TimeUs(x));
        }
        // An effective input after the final deadline: an empty POOR on
        // the lane of the last note, which batch evaluation must reach.
        1 => {
            let &(l, t) = judgeable(&chart, section.as_ref())
                .last()
                .or(judgeable(&chart, None).last())
                .unwrap();
            let at = final_deadline.0 + if param % 2 == 0 { 1 } else { TICK };
            // The range of the lane's class, never scaled.
            let [keys, scratch] = preset.tables.each_mut();
            let table = if lane(l) == Lane::SCRATCH {
                scratch
            } else {
                keys
            };
            let behind = &mut table.empty_poor.behind;
            *behind = TimeUs(behind.0.max(at - t.0));
            let off = bind(&mut device, 0, l);
            clear_lane(&mut logs, &device, l, at - 1);
            insert(&mut logs[0], joint(at + off, 0, 1));
            insert(&mut logs[0], joint(at + off + TICK, 0, 0));
        }
        // Two sources at one evaluation time on one lane, in an order that
        // shows: one releases the lane it held since before a missed note's
        // window as the other presses it. Releasing first frees the lane,
        // so the press judges the note; pressing first leaves the lane
        // held, with no press edge.
        2 if n >= 2 => {
            let (l, t) = target(
                &chart,
                &preset,
                &device,
                &start,
                &logs,
                section.as_ref(),
                param,
            );
            let s = pick(param / 8, n);
            let r = (s + 1 + pick(param / 64, n - 1)) % n;
            let early = t.0 - windows.of_lane(lane(l)).bad.early.0 - TICK;
            let off_s = bind(&mut device, s, l);
            let off_r = bind(&mut device, r, l);
            insert(&mut logs[s], joint(early + off_s, s, 1));
            insert(&mut logs[s], joint(t.0 + off_s, s, 0));
            clear_lane(&mut logs, &device, l, t.0 - 1);
            insert(&mut logs[r], joint(t.0 + off_r, r, 1));
            insert(&mut logs[r], joint(t.0 + off_r + TICK, r, 0));
        }
        // A press on the early BAD edge of the section's first target, or
        // 1 µs before it, with the lead-in long enough to hold it: judged,
        // or ignored with no empty POOR.
        3 if let Some(s) = section.as_mut()
            && let Some((l, t)) = first_target(&chart, s) =>
        {
            let at = t.0 - windows.of_lane(lane(l)).bad.early.0 - i64::from(param % 2 == 1);
            s.lead_in = TimeUs(s.lead_in.0.max(s.start.0 - at));
            let src = pick(param / 8, n);
            let off = bind(&mut device, src, l);
            clear_lane(&mut logs, &device, l, at - 1);
            insert(&mut logs[src], joint(at + off, src, 1));
            insert(&mut logs[src], joint(at + off + TICK, src, 0));
        }
        _ => {}
    }
    // Recording opened before every event, joint ones included.
    start.opened_at = opened_before(logs.iter().flatten());
    let capacity = logs
        .iter()
        .zip(&raw.log.sources)
        .map(|(log, s)| max_group(log) + 1 + pick(s.capacity, 6))
        .collect();
    let events = interleave(&logs, &raw.log.interleave);
    let input = EvalInput {
        events,
        device,
        start,
        finalized: None,
        section,
    };
    let deadlines = deadlines(&chart, &windows, input.section.as_ref());
    let mut times: Vec<TimeUs> = input
        .events
        .iter()
        .map(|e| evaluation_time(e, &input.device))
        .chain(deadlines.iter().copied())
        .chain([final_deadline])
        .collect();
    times.sort_unstable();
    times.dedup();
    Play {
        chart,
        preset,
        input,
        logs,
        capacity,
        deadlines,
        final_deadline,
        times,
        sync_at,
    }
}

/// The largest number of events of `log` with one receive time.
fn max_group(log: &[PhysicalInputEvent]) -> usize {
    log.chunk_by(|a, b| a.received == b.received)
        .map(<[_]>::len)
        .max()
        .unwrap_or(0)
}

// -------------------------------------------------------------- excerpt

/// A section excerpted from a full log, and the mid-chart start it must
/// match: the same section, device, prefix `C` and later input `R`, with
/// the full log's earlier input `E` replaced by a snapshot of what it left
/// held.
#[derive(Clone, Debug)]
pub struct Excerpt {
    pub chart: Chart,
    pub preset: RulePreset,
    /// The full log: `E`, a quiet gap, `C`, then `R`.
    pub full: EvalInput,
    /// The mid-chart start: the snapshot after `E`, then `C` and `R`.
    pub mid: EvalInput,
    /// The section's lead-in start, `start − lead_in`.
    pub lead_in_start: TimeUs,
    /// How many events `E` holds.
    pub earlier: usize,
}

#[derive(Clone, Debug)]
pub struct RawExcerpt {
    play: RawPlay,
    section: RawSection,
    axis: (u8, u8, u8, u32),
    prefix: (u8, u8),
}

pub fn raw_excerpt() -> impl Strategy<Value = RawExcerpt> {
    (
        raw_play(),
        raw_section(),
        any::<(u8, u8, u8, u32)>(),
        any::<(u8, u8)>(),
    )
        .prop_map(|(play, section, axis, prefix)| RawExcerpt {
            play,
            section,
            axis,
            prefix,
        })
}

/// The longest scratch release time [`scratch`] gives, µs.
const MAX_RELEASE: i64 = 200_000;

/// The snapshot a play's conversion has after `start` and each source's
/// `earlier` events, once every scratch release is due and every axis has
/// paused: the buttons pressed and each absolute axis's position.
fn snapshot(start: &InputStart, earlier: &[Vec<PhysicalInputEvent>]) -> InputStart {
    let mut held: std::collections::BTreeMap<(SourceId, ControlId), i32> =
        start.held.iter().map(|&(s, c, v)| ((s, c), v)).collect();
    for e in earlier.iter().flatten() {
        match e.kind {
            InputKind::Control { control, value } => {
                held.insert((e.source, control), value);
            }
            InputKind::Disconnected => held.retain(|&(s, _), _| s != e.source),
        }
    }
    InputStart {
        opened_at: TimeUs(0),
        held: held
            .into_iter()
            // An axis keeps its position; a button is held when not 0.
            .filter(|&((_, c), v)| c == AXIS || v != 0)
            .map(|((s, c), v)| (s, c, v))
            .collect(),
    }
}

/// A play with a section and an axis on source 0, its log split at the
/// section's lead-in start `L` into `E` (before `L − c − G`) and `R` (from
/// `L`), the events between dropped, and a prefix `C` on source 0 in
/// `[L − c, L)`: a button pressed and held, and the axis pressed `c` before
/// `L` and turned back 1 µs before it, so `L` sees a held control, a
/// pending scratch release and, when one step back does not press,
/// accumulated movement. `G` is longer than any release time, so every
/// pending release of `E` is due and every axis has paused before `C`.
pub fn build_excerpt(raw: &RawExcerpt) -> Excerpt {
    let mut rp = raw.play.clone();
    rp.section = Some(raw.section.clone());
    rp.log.sources[0].axis.get_or_insert(raw.axis);
    let play = build_play(&rp);
    let section = play.input.section.clone().unwrap();
    let device = &play.input.device;
    let lead_in_start = section.start.0 - section.lead_in.0;
    let axis = device.sources[0].axes[0];
    let release = i64::from(axis.scratch.release_after_us);
    let half = (release / 2).max(1);
    let c_from = lead_in_start - half - 2;
    let quiet_from = c_from - MAX_RELEASE - 1;
    let mut earlier = vec![Vec::new(); play.logs.len()];
    let mut rest = vec![Vec::new(); play.logs.len()];
    for (i, log) in play.logs.iter().enumerate() {
        for e in log {
            let t = evaluation_time(e, device).0;
            if t < quiet_from {
                earlier[i].push(*e);
            } else if t >= lead_in_start {
                rest[i].push(*e);
            }
        }
    }
    let off = device.sources[0].input_offset.0;
    let turn = |at: i64, value: i32| PhysicalInputEvent {
        received: TimeUs(at + off),
        source: SourceId(0),
        kind: InputKind::Control {
            control: AXIS,
            value,
        },
    };
    let press = i32::try_from(axis.scratch.press_threshold.max(1)).unwrap();
    let mut prefix = vec![button(c_from + off, 0, pick(raw.prefix.0, LANE_COUNT), 1)];
    match axis.scratch.mode {
        AxisMode::Relative => prefix.extend([
            turn(lead_in_start - half, press),
            turn(lead_in_start - 1, -1),
        ]),
        AxisMode::Absolute { .. } => {
            let base = i32::from(raw.prefix.1 % 64);
            prefix.extend([
                turn(c_from, base),
                turn(lead_in_start - half, base + press),
                turn(lead_in_start - 1, base + press - 1),
            ]);
        }
    }
    let join = |first: &[Vec<PhysicalInputEvent>]| -> Vec<Vec<PhysicalInputEvent>> {
        (0..play.logs.len())
            .map(|i| {
                let c: &[PhysicalInputEvent] = if i == 0 { &prefix } else { &[] };
                [first[i].as_slice(), c, rest[i].as_slice()].concat()
            })
            .collect()
    };
    let full_logs = join(&earlier);
    let mid_logs = join(&vec![Vec::new(); play.logs.len()]);
    let order = &raw.play.log.interleave;
    let full_events = interleave(&full_logs, order);
    let mut full_start = play.input.start.clone();
    full_start.opened_at = opened_before(&full_events);
    let mut mid_start = snapshot(&play.input.start, &earlier);
    let mid_events = interleave(&mid_logs, order);
    mid_start.opened_at = opened_before(&mid_events);
    let input = |events, start| EvalInput {
        events,
        device: device.clone(),
        start,
        finalized: None,
        section: Some(section.clone()),
    };
    Excerpt {
        full: input(full_events, full_start),
        mid: input(mid_events, mid_start),
        lead_in_start: TimeUs(lead_in_start),
        earlier: earlier.iter().map(Vec::len).sum(),
        chart: play.chart,
        preset: play.preset,
    }
}

// ------------------------------------------------------------- delivery

#[derive(Clone, Debug)]
struct RawStep {
    push: u8,
    late: u8,
    publish: (u8, u16),
    close: bool,
}

fn raw_step() -> impl Strategy<Value = RawStep> {
    (0_u8..=6, any::<u8>(), any::<(u8, u16)>(), any::<bool>()).prop_map(
        |(push, late, publish, close)| RawStep {
            push,
            late,
            publish,
            close,
        },
    )
}

#[derive(Clone, Debug)]
pub struct RawDelivery {
    steps: Vec<Vec<RawStep>>,
    order: [u8; 3],
    terminal: (u8, u16),
    sync: u8,
}

pub fn raw_delivery() -> impl Strategy<Value = RawDelivery> {
    (
        vec(vec(raw_step(), 0..=3), 0..=12),
        any::<[u8; 3]>(),
        any::<(u8, u16)>(),
        any::<u8>(),
    )
        .prop_map(|(steps, order, terminal, sync)| RawDelivery {
            steps,
            order,
            terminal,
            sync,
        })
}

/// The latest time the play's evaluation needs: the final deadline or the
/// last input, whichever is later.
pub fn end_of(play: &Play) -> TimeUs {
    let last = play
        .input
        .events
        .iter()
        .map(|e| evaluation_time(e, &play.input.device))
        .max();
    last.map_or(play.final_deadline, |t| t.max(play.final_deadline))
}

fn fraction(lo: i64, hi: i64, f: u16) -> i64 {
    let span = i128::from(hi) - i128::from(lo);
    i64::try_from(i128::from(lo) + span * i128::from(f) / i128::from(u16::MAX)).unwrap()
}

pub fn build_delivery(play: &Play, raw: &RawDelivery) -> Delivery {
    let n = play.logs.len();
    let end = end_of(play).0;
    let first = play
        .input
        .events
        .iter()
        .map(|e| evaluation_time(e, &play.input.device).0)
        .min()
        .unwrap_or(end)
        .min(end);
    let (sel, v) = raw.terminal;
    let terminal = TimeUs(match sel % 4 {
        0 => end,
        1 => end + below(v, 50 * MS + 1),
        // An aborted prefix.
        _ => fraction(first - 1, end, v),
    });
    let mut steps: Vec<Vec<SourceStep>> = raw
        .steps
        .iter()
        .map(|step| {
            step.iter()
                .map(|s| SourceStep {
                    push: usize::from(s.push),
                    late_push: if s.late % 4 == 3 {
                        usize::from(s.late / 4 % 2) + 1
                    } else {
                        0
                    },
                    publish: match s.publish.0 % 6 {
                        0 => Publish::Hold,
                        1 => Publish::LastPushed,
                        2 => Publish::Highest,
                        3 | 4 => Publish::Fraction(s.publish.1),
                        _ => Publish::Eval(play.times[pick(s.publish.1, play.times.len())]),
                    },
                    close: s.close,
                })
                .collect()
        })
        .collect();
    if let Some(x) = play.sync_at {
        let sync = SourceStep {
            push: usize::MAX,
            late_push: 0,
            publish: Publish::Eval(x),
            close: false,
        };
        let at = pick(raw.sync, steps.len() + 1);
        steps.splice(at..at, std::iter::repeat_n(vec![sync], 4));
    }
    let mut staged_order: Vec<usize> = (0..n).collect();
    staged_order.sort_by_key(|&i| (raw.order[i], i));
    Delivery {
        capacity: play.capacity.clone(),
        steps,
        staged_order,
        terminal,
    }
}

// ------------------------------------------------------------ widening

/// A play for the widening property: each of `k` lanes holds one
/// judgeable note and gets at most one press (and a release after it).
#[derive(Clone, Debug)]
pub struct Widening {
    pub chart: Chart,
    pub narrow: RulePreset,
    pub wide: RulePreset,
    pub input: EvalInput,
}

/// One widening note: lane, gap, kind, and its press, if any (how the
/// deviation is chosen, a value, and how long it is held).
type RawWideNote = (u8, u8, u8, Option<(u8, u32, u8)>);

#[derive(Clone, Debug)]
pub struct RawWidening {
    notes: Vec<RawWideNote>,
    rank: (u8, u32),
    preset: RawPreset,
    widen: ([u32; 4], [u32; 4], [u32; 3]),
    offset: (u8, u16),
}

pub fn raw_widening() -> impl Strategy<Value = RawWidening> {
    (
        vec(
            (
                any::<u8>(),
                1_u8..=50,
                any::<u8>(),
                option::weighted(0.85, any::<(u8, u32, u8)>()),
            ),
            1..=8,
        ),
        (any::<u8>(), any::<u32>()),
        raw_preset(),
        any::<([u32; 4], [u32; 4], [u32; 3])>(),
        any::<(u8, u16)>(),
    )
        .prop_map(|(notes, rank, preset, widen, offset)| RawWidening {
            notes,
            rank,
            preset,
            widen,
            offset,
        })
}

/// A non-negative widening step: often none.
fn widen_step(raw: u32) -> i64 {
    match raw % 4 {
        0 => 0,
        1 => (below(raw / 4, 5) + 1) * TICK,
        2 => 1,
        _ => below(raw / 4, 50_000),
    }
}

pub fn build_widening(raw: &RawWidening) -> Widening {
    let narrow = build_preset(&raw.preset);
    let mut wide = narrow.clone();
    // Cumulative non-negative increments keep every side ascending.
    let widen = |w: [i64; 4], steps: [u32; 4]| {
        let mut out = w;
        let mut add = 0;
        for k in 0..4 {
            add += widen_step(steps[k]);
            out[k] += add;
        }
        out
    };
    let side = |w: &JudgeWindows, early: bool| {
        w.table()
            .map(|(_, w)| if early { w.early.0 } else { w.late.0 })
    };
    // Each lane class's table widens by the same steps.
    for table in wide.tables.each_mut() {
        let narrow = *table;
        table.windows = windows_of(
            widen(side(&narrow.windows, true), raw.widen.0),
            widen(side(&narrow.windows, false), raw.widen.1),
        );
        // The LN-end edges widen too, by their own steps, and at least as
        // far as the widened early edges they must cover; each stays wider
        // than the better one.
        let early = side(&table.windows, true);
        let mut floor = 0;
        let mut add = 0;
        let mut end = [0; 3];
        for (k, (_, edge)) in narrow.ln_end.table().into_iter().enumerate() {
            add += widen_step(raw.widen.2[k]);
            end[k] = (edge.0 + add).max(early[k]).max(floor);
            floor = end[k] + 1;
        }
        table.ln_end = LnEndWindows {
            perfect: TimeUs(end[0]),
            great: TimeUs(end[1]),
            good: TimeUs(end[2]),
        };
    }
    // Distinct lanes, one judgeable note each.
    let mut used = [false; LANE_COUNT];
    let mut b = ChartBuilder::new(150_000);
    let mut planned = Vec::new();
    for &(l, gap, kind, press) in &raw.notes {
        let mut l = pick(l, LANE_COUNT);
        while used[l] {
            l = (l + 1) % LANE_COUNT;
        }
        used[l] = true;
        let t = 500 * MS + i64::from(gap) * TICK;
        let long = kind % 3 == 0;
        b = if long {
            b.long(
                lane(l),
                TimeUs(t),
                TimeUs(t + (i64::from(kind / 3 % 30) + 1) * TICK),
            )
        } else {
            b.tap(lane(l), TimeUs(t))
        };
        planned.push((l, t, press));
    }
    let mut chart = b.build().unwrap();
    chart.meta.judgment_headers = rank(raw.rank);
    let off = offset(raw.offset);
    let device = crate::common::device(&[off]);
    let scaled = |p: &RulePreset| {
        lane_windows(&Evaluator::new(&chart, p, &device, &InputStart::default(), None, 0).unwrap())
    };
    let (wn, ww) = (scaled(&narrow), scaled(&wide));
    let mut log = Vec::new();
    for &(l, t, press) in &planned {
        let Some((sel, v, hold)) = press else {
            continue;
        };
        // Uniform within ±300 ms, or on a window edge of either preset, or
        // 1 µs either side of it.
        let deviation = if sel % 2 == 0 {
            below(v, 600_001) - 300_000
        } else {
            let w = if sel / 2 % 2 == 0 { &wn } else { &ww };
            let (_, window) = w.of_lane(lane(l)).table()[pick(v, 4)];
            let edge = if v / 4 % 2 == 0 {
                -window.early.0
            } else {
                window.late.0
            };
            edge + [0, 1, -1][pick(v / 8, 3)]
        };
        let at = t + deviation + off;
        log.push(button(at, 0, l, 1));
        log.push(button(at + (i64::from(hold % 40) + 1) * TICK, 0, l, 0));
    }
    log.sort_by_key(|e| e.received);
    let start = InputStart {
        opened_at: opened_before(&log),
        held: Vec::new(),
    };
    Widening {
        chart,
        narrow,
        wide,
        input: EvalInput {
            events: log,
            device,
            start,
            finalized: None,
            section: None,
        },
    }
}

// -------------------------------------------------------- held through

/// A play for the held-duration property: one long note on key 1, pressed
/// inside the scaled BAD window and held at least as long as the long note.
#[derive(Clone, Debug)]
pub struct HeldThrough {
    pub chart: Chart,
    pub preset: RulePreset,
    pub input: EvalInput,
    /// The press's deviation from the head.
    pub deviation: i64,
    /// The key windows at the chart's rank, by hand.
    pub windows: JudgeWindows,
}

#[derive(Clone, Debug)]
pub struct RawHeldThrough {
    preset: RawPreset,
    rank: (u8, u32),
    length: u32,
    deviation: u32,
    extra: u32,
}

pub fn raw_held_through() -> impl Strategy<Value = RawHeldThrough> {
    (
        raw_preset(),
        (any::<u8>(), any::<u32>()),
        any::<(u32, u32, u32)>(),
    )
        .prop_map(
            |(preset, rank, (length, deviation, extra))| RawHeldThrough {
                preset,
                rank,
                length,
                deviation,
                extra,
            },
        )
}

pub fn build_held_through(raw: &RawHeldThrough) -> HeldThrough {
    let preset = build_preset(&raw.preset);
    // A zero-length, a 1 µs, a grid or any long note up to 2 s.
    let length = match raw.length % 4 {
        0 => 0,
        1 => 1,
        2 => (below(raw.length / 4, 200) + 1) * TICK,
        _ => below(raw.length / 4, 2_000_001),
    };
    let head = 1_000 * MS;
    let mut chart = ChartBuilder::new(150_000)
        .long(lane(1), TimeUs(head), TimeUs(head + length))
        .build()
        .unwrap();
    chart.meta.judgment_headers = rank(raw.rank);
    // The BAD window at the chart's rank, by hand.
    let windows = windows_at(
        &preset,
        &chart.meta.judgment_headers,
        preset.tables.keys.windows,
    );
    let (early, late) = (windows.bad.early.0, windows.bad.late.0);
    let deviation = below(raw.deviation, early + late + 1) - early;
    // Held exactly as long as the long note, a little or much longer.
    let extra = match raw.extra % 3 {
        0 => 0,
        1 => below(raw.extra / 3, 3),
        _ => below(raw.extra / 3, 1_000_000),
    };
    let press = head + deviation;
    let events = vec![
        button(press, 0, 1, 1),
        button(press + length + extra, 0, 1, 0),
    ];
    HeldThrough {
        chart,
        preset,
        input: EvalInput {
            start: InputStart {
                opened_at: opened_before(&events),
                held: Vec::new(),
            },
            events,
            device: crate::common::device(&[0]),
            finalized: None,
            section: None,
        },
        deviation,
        windows,
    }
}
