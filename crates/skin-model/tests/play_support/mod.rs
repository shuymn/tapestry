//! Helpers shared by the play tests: effects, snapshots and a harness that
//! builds frames.

#![allow(dead_code)]

use bms_model::{Chart, Lane, TimeUs};
use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
use rules::{GaugeValue, Judgment, PerJudgment};
use skin_model::{
    Attempt, Blend, Ease, ElementDef, End, GaugeDesc, KeyDef, PLAY, Play, PlayContext, Program,
    QuadBatch, Scope, Skin, SkinFrame, Source, TimerRef, Ty, compile,
};

pub const MS: i64 = 1_000;

pub fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

pub fn ev(time_ms: i64, kind: JudgeEventKind) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(time_ms * MS),
        origin: None,
        kind,
    }
}

pub fn judged(
    l: u8,
    note: usize,
    judgment: Judgment,
    deviation_ms: Option<i64>,
    release_ms: Option<i64>,
    combo: u32,
) -> JudgeEventKind {
    JudgeEventKind::Judged {
        lane: lane(l),
        note,
        judgment,
        deviation: deviation_ms.map(|d| TimeUs(d * MS)),
        release: release_ms.map(|r| TimeUs(r * MS)),
        combo,
    }
}

pub fn ln_held(l: u8, note: usize, judgment: Judgment) -> JudgeEventKind {
    JudgeEventKind::LnHeld {
        lane: lane(l),
        note,
        judgment,
        deviation: TimeUs(0),
    }
}

pub fn hold_ended(l: u8, note: usize) -> JudgeEventKind {
    JudgeEventKind::LnHoldEnded {
        lane: lane(l),
        note,
    }
}

pub fn mine(l: u8, note: usize) -> JudgeEventKind {
    JudgeEventKind::MineHit {
        lane: lane(l),
        note,
    }
}

pub fn empty_poor(l: u8) -> JudgeEventKind {
    JudgeEventKind::EmptyPoor { lane: lane(l) }
}

pub fn gauge(g: usize, value: GaugeValue) -> JudgeEventKind {
    JudgeEventKind::Gauge {
        gauge: g,
        value,
        reference: value,
    }
}

pub fn tally(combo: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 0,
        bp: 0,
        combo,
        max_combo: combo,
        empty_poor: 0,
    }
}

pub fn state(value: GaugeValue) -> GaugeState {
    GaugeState {
        value,
        reference: value,
        failed_at: None,
    }
}

pub fn pct(p: i64) -> GaugeValue {
    GaugeValue(p * GaugeValue::PERCENT)
}

/// Two gauges, both starting at 20 % with a 100 % maximum.
pub const GAUGES: [GaugeDesc; 2] = [
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
    },
    GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
    },
];

pub fn full(lead_in_ms: i64) -> PlayContext<'static> {
    PlayContext {
        gauges: &GAUGES,
        attempt: Attempt::Full {
            lead_in: TimeUs(lead_in_ms * MS),
        },
        empty_poor_breaks_combo: false,
    }
}

pub fn bool_program(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap()
}

/// An element at x = `x` with a hold track from 0, on `timer`.
pub fn element(timer: TimerRef, x: i32) -> ElementDef {
    let mut props = [None; 9];
    props[0] = Some(x);
    ElementDef {
        source: Source::Uv([0.0, 0.0, 1.0, 1.0]),
        track: skin_model::Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer,
        visible: None,
        blend: Blend::Alpha,
        clip: None,
    }
}

/// A skin, its play, the snapshot it reads and a batch.
pub struct Harness {
    pub skin: Skin,
    pub play: Play,
    pub tally: Tally,
    pub gauges: Vec<GaugeState>,
    pub gauge: usize,
    pub offset: TimeUs,
    pub batch: QuadBatch,
}

impl Harness {
    pub fn new(skin: Skin, chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        let play = skin.prepare(chart, ctx).unwrap();
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            skin,
            play,
            tally: tally(0),
            gauges: vec![state(GAUGES[0].start), state(GAUGES[1].start)],
            gauge: 0,
            offset: TimeUs(0),
            batch,
        }
    }

    /// An empty skin over `chart`.
    pub fn empty(chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        Self::new(Skin::new(Vec::new(), Vec::new()).unwrap(), chart, ctx)
    }

    /// Builds the frame at `now_us` with `effects`.
    pub fn frame_us(&mut self, now_us: i64, effects: &[JudgeEvent]) {
        self.batch.clear();
        let frame = SkinFrame {
            now: TimeUs(now_us),
            display_offset: self.offset,
            tally: &self.tally,
            gauges: &self.gauges,
            gauge: self.gauge,
            effects,
        };
        self.skin.build(&mut self.play, &frame, &mut self.batch);
    }

    /// Builds the frame at `now_ms` with `effects`.
    pub fn frame(&mut self, now_ms: i64, effects: &[JudgeEvent]) {
        self.frame_us(now_ms * MS, effects);
    }

    /// Timer `slot`'s start in µs.
    pub fn start(&self, slot: u16) -> Option<i64> {
        self.play.timer(slot).map(|t| t.0)
    }

    /// Timer `slot`'s start in ms (exact ms expected).
    pub fn start_ms(&self, slot: u16) -> Option<i64> {
        self.start(slot).map(|t| {
            assert_eq!(t % MS, 0, "slot {slot} starts off the ms");
            t / MS
        })
    }

    /// Timer `slot`'s reading in ms.
    pub fn elapsed(&self, slot: u16) -> i32 {
        self.play.state().timers[usize::from(slot)]
    }

    pub fn field(&self, slot: u16) -> i32 {
        self.play.state().fields[usize::from(slot)]
    }
}
