//! Generators for the property tests: charts, presets, input logs and
//! delivery schedules, valid by construction.
//!
//! Each strategy draws independent raw values and maps them to a valid
//! value (`% n`, cumulative gaps), so shrinking a raw value always gives a
//! valid case and nothing is filtered out. Times sit on a 10 ms grid with
//! occasional jitter, and offsets are often grid multiples, so equal times
//! across sources and between inputs and deadlines are common.

use bms_model::{
    Chart, ChartBuilder, JudgeRank, LANE_COUNT, Lane, LnMode, NoteKind, SoundId, TimeUs,
};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, DeviceProfile, EvalInput, Evaluator, InputKind,
    InputStart, JudgeEventKind, PhysicalInputEvent, ScratchSettings, SourceId, SourceProfile,
    evaluate, evaluation_time,
};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;
use rules::{
    GaugeDelta, GaugeKind, GaugeRule, GaugeValue, JudgeWindows, LowDamage, MineRule, PerJudgment,
    RulePreset, Window,
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
    chart.meta.rank = match raw.rank.0 % 3 {
        0 => None,
        1 => Some(JudgeRank::Rank(
            i32::try_from(below(raw.rank.1, 6)).unwrap() - 1,
        )),
        _ => Some(JudgeRank::DefExRankMilli(below(raw.rank.1, 200_000) + 1)),
    };
    chart.meta.total_milli = raw.total.map(|t| below(t, 400_002) - 1);
    chart.meta.ln_mode = raw.ln.then_some(LnMode::Ln);
    chart.validate().unwrap();
    chart
}

// --------------------------------------------------------------- preset

#[derive(Clone, Debug)]
struct RawGauge {
    survival: bool,
    bounds: [u32; 4],
    deltas: [(u8, u32); 7],
    low: Option<(u8, u8, u32)>,
}

fn raw_gauge() -> impl Strategy<Value = RawGauge> {
    (
        any::<bool>(),
        any::<[u32; 4]>(),
        any::<[(u8, u32); 7]>(),
        option::of(any::<(u8, u8, u32)>()),
    )
        .prop_map(|(survival, bounds, deltas, low)| RawGauge {
            survival,
            bounds,
            deltas,
            low,
        })
}

#[derive(Clone, Debug)]
struct RawPreset {
    early: [u32; 4],
    late: [u32; 4],
    symmetric: bool,
    empty_poor: (u32, u32),
    margin: u32,
    flags: u16,
    rank: [u32; 5],
    total: u32,
    gauges: Vec<RawGauge>,
    ex: [u8; 5],
    per_unit: u32,
}

fn raw_preset() -> impl Strategy<Value = RawPreset> {
    (
        (any::<[u32; 4]>(), any::<[u32; 4]>(), any::<bool>()),
        (any::<(u32, u32)>(), any::<u32>(), any::<u16>()),
        (any::<[u32; 5]>(), any::<u32>()),
        vec(raw_gauge(), 1..=3),
        (any::<[u8; 5]>(), any::<u32>()),
    )
        .prop_map(
            |(
                (early, late, symmetric),
                (empty_poor, margin, flags),
                (rank, total),
                gauges,
                (ex, per_unit),
            )| RawPreset {
                early,
                late,
                symmetric,
                empty_poor,
                margin,
                flags,
                rank,
                total,
                gauges,
                ex,
                per_unit,
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
    let (kind, min, start, max) = if raw.survival {
        // Often a low start, so failures happen.
        let max = FULL - below(a, 2) * below(a / 2, FULL / 2);
        let start = if b % 2 == 0 {
            1 + below(b / 2, 5 * PERCENT)
        } else {
            1 + below(b / 2, max)
        };
        (GaugeKind::Survival, 0, start.min(max), max)
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
    let [p, g, gd, bd, pr, ep, lb] = raw.deltas.map(|(s, v)| delta(s, v));
    let low_damage = raw.low.map(|(num, den, below_raw)| {
        let den = u32::from(den % 4) + 1;
        LowDamage {
            below: GaugeValue(min + below(below_raw, (max - min) + 1)),
            num: u32::from(num) % (den + 1),
            den,
        }
    });
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
        ln_break: lb,
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

fn build_preset(raw: &RawPreset) -> RulePreset {
    let mut p = rules::custom();
    let late = edges(raw.late);
    let early = if raw.symmetric {
        late
    } else {
        edges(raw.early)
    };
    p.windows = windows_of(early, late);
    let flag = |bit: u16| raw.flags & (1 << bit) != 0;
    p.empty_poor.ahead = distance(raw.empty_poor.0);
    p.empty_poor.behind = distance(raw.empty_poor.1);
    p.empty_poor.breaks_combo = flag(0);
    p.empty_poor.counts_bp = flag(1);
    p.empty_poor.counted = flag(2);
    p.ln_break.release_margin = TimeUs(below(raw.margin, 21) * TICK / 2);
    p.ln_break.breaks_combo = flag(3);
    p.ln_break.counts_bp = flag(4);
    p.rank_scale.rank_milli = [0, 1, 2, 3].map(|k| scale(raw.rank[k]));
    p.rank_scale.absent_milli = scale(raw.rank[4]);
    p.default_total_milli = below(raw.total, 400_000) + 1;
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
    /// The chart's deadlines, sorted: misses, LN ends and mines.
    pub deadlines: Vec<TimeUs>,
    pub final_deadline: TimeUs,
    /// Times a publication may aim at: input evaluation times and
    /// deadlines.
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
}

pub fn raw_play() -> impl Strategy<Value = RawPlay> {
    (raw_chart(), raw_preset(), raw_log(), any::<(u8, u16)>()).prop_map(
        |(chart, preset, log, joint)| RawPlay {
            chart,
            preset,
            log,
            joint,
        },
    )
}

/// The judgeable notes: (lane, time), in time order.
fn judgeable(chart: &Chart) -> Vec<(usize, TimeUs)> {
    let mut v: Vec<(usize, TimeUs)> = chart
        .lanes
        .iter()
        .enumerate()
        .flat_map(|(l, notes)| {
            notes
                .iter()
                .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }))
                .map(move |n| (l, n.time))
        })
        .collect();
    v.sort_by_key(|&(l, t)| (t, l));
    v
}

fn deadlines(chart: &Chart, windows: &JudgeWindows) -> Vec<TimeUs> {
    let mut v: Vec<TimeUs> = Vec::new();
    for note in chart.lanes.iter().flatten() {
        match note.kind {
            NoteKind::Tap => v.push(TimeUs(note.time.0 + windows.bad.late.0)),
            NoteKind::Long { end, .. } => {
                v.push(TimeUs(note.time.0 + windows.bad.late.0));
                v.push(end);
            }
            NoteKind::Mine { .. } => v.push(note.time),
        }
    }
    v.sort_unstable();
    v.dedup();
    v
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
    raw: u64,
) -> (usize, TimeUs) {
    let base = EvalInput {
        events: interleave(logs, &[]),
        device: device.clone(),
        start: start.clone(),
        finalized: None,
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
        notes = judgeable(chart);
    }
    notes[pick(raw, notes.len())]
}

pub fn build_play(raw: &RawPlay) -> Play {
    let chart = build_chart(&raw.chart);
    let mut preset = build_preset(&raw.preset);
    let (mut device, start, mut logs) = build_log(&raw.log);
    let n = logs.len();
    let (windows, final_deadline) = {
        let ev = Evaluator::new(&chart, &preset, &device, &start, 0).unwrap();
        (*ev.windows(), ev.final_deadline())
    };
    let (sel, param) = raw.joint;
    let param = u64::from(param);
    let mut sync_at = None;
    match sel % 8 {
        // A press on a missed note's miss deadline, which the schedules make
        // an intermediate W: inputs go first at equal times, across steps
        // too, so the press judges the note.
        0 => {
            let (l, t) = target(&chart, &preset, &device, &start, &logs, param);
            let s = pick(param / 8, n);
            let x = t.0 + windows.bad.late.0;
            let off = bind(&mut device, s, l);
            clear_lane(&mut logs, &device, l, x - 1);
            insert(&mut logs[s], joint(x + off, s, 1));
            insert(&mut logs[s], joint(x + off + TICK, s, 0));
            sync_at = Some(TimeUs(x));
        }
        // An effective input after the final deadline: an empty POOR on
        // the lane of the last note, which batch evaluation must reach.
        1 => {
            let &(l, t) = judgeable(&chart).last().unwrap();
            let at = final_deadline.0 + if param % 2 == 0 { 1 } else { TICK };
            let behind = &mut preset.empty_poor.behind;
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
            let (l, t) = target(&chart, &preset, &device, &start, &logs, param);
            let s = pick(param / 8, n);
            let r = (s + 1 + pick(param / 64, n - 1)) % n;
            let early = t.0 - windows.bad.early.0 - TICK;
            let off_s = bind(&mut device, s, l);
            let off_r = bind(&mut device, r, l);
            insert(&mut logs[s], joint(early + off_s, s, 1));
            insert(&mut logs[s], joint(t.0 + off_s, s, 0));
            clear_lane(&mut logs, &device, l, t.0 - 1);
            insert(&mut logs[r], joint(t.0 + off_r, r, 1));
            insert(&mut logs[r], joint(t.0 + off_r + TICK, r, 0));
        }
        _ => {}
    }
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
    };
    let deadlines = deadlines(&chart, &windows);
    let mut times: Vec<TimeUs> = input
        .events
        .iter()
        .map(|e| evaluation_time(e, &input.device))
        .chain(deadlines.iter().copied())
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
    widen: ([u32; 4], [u32; 4]),
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
        any::<([u32; 4], [u32; 4])>(),
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
    let side = |p: &RulePreset, early: bool| {
        p.windows
            .table()
            .map(|(_, w)| if early { w.early.0 } else { w.late.0 })
    };
    wide.windows = windows_of(
        widen(side(&narrow, true), raw.widen.0),
        widen(side(&narrow, false), raw.widen.1),
    );
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
    chart.meta.rank = match raw.rank.0 % 3 {
        0 => None,
        1 => Some(JudgeRank::Rank(
            i32::try_from(below(raw.rank.1, 6)).unwrap() - 1,
        )),
        _ => Some(JudgeRank::DefExRankMilli(below(raw.rank.1, 200_000) + 1)),
    };
    let off = offset(raw.offset);
    let device = DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(off),
            bindings: (0..LANE_COUNT)
                .map(|l| Binding {
                    control: ControlId(u32::try_from(l).unwrap()),
                    lane: lane(l),
                })
                .collect(),
            axes: Vec::new(),
        }],
    };
    let scaled = |p: &RulePreset| {
        *Evaluator::new(&chart, p, &device, &InputStart::default(), 0)
            .unwrap()
            .windows()
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
            let (_, window) = w.table()[pick(v, 4)];
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
    Widening {
        chart,
        narrow,
        wide,
        input: EvalInput {
            events: log,
            device,
            start: InputStart::default(),
            finalized: None,
        },
    }
}
