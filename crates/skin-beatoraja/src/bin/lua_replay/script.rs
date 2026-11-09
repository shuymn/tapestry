//! The frames a replay plays: a self-made chart, stepped by the evaluator
//! with a scripted input log, each frame built as the player's view gives
//! it (`game::render`'s fields: tally, gauges, note facts, input,
//! completion).

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, Evaluator, InputKind, InputStart, JudgeEvent,
    PhysicalInputEvent, SourceId, SourceProfile,
};

/// One script: a chart of `notes` notes and, per note, the offset of its
/// press from the note (µs), `None` for no press; plus presses with no
/// note near (empty POORs).
#[derive(Clone, Debug)]
pub struct Script {
    pub name: &'static str,
    pub presses: Vec<Option<i64>>,
    pub ghosts: Vec<i64>,
    /// Further frame times (chart µs).
    pub extra: Vec<i64>,
}

/// The first note's time and the gap between notes.
const FIRST: i64 = 1_000_000;
const GAP: i64 = 400_000;
/// Frames start this far before chart time 0 (READY) and run this long
/// past the last note (the audio tail).
const LEAD: i64 = 1_000_000;
const TAIL: i64 = 2_000_000;
/// The state clock at chart time 0: the play state began this long
/// before (its loading).
pub const STATE_AT_ZERO: i64 = 1_500_000;
/// The frame step: 240 Hz.
const STEP: i64 = 4_167;

impl Script {
    /// `full`: 7 notes, every one pressed on time.
    pub fn full() -> Self {
        Self {
            name: "full",
            presses: vec![Some(0); 7],
            ghosts: Vec::new(),
            extra: Vec::new(),
        }
    }

    /// `mixed`: 13 notes, every judgment, misses and empty POORs.
    pub fn mixed() -> Self {
        Self {
            name: "mixed",
            presses: vec![
                Some(0),
                Some(30_000),
                Some(-60_000),
                Some(0),
                None,
                Some(110_000),
                Some(0),
                Some(-25_000),
                None,
                Some(0),
                Some(70_000),
                Some(0),
                Some(-140_000),
            ],
            ghosts: vec![FIRST + GAP * 4 + GAP / 2, FIRST + GAP * 9 + GAP / 2],
            extra: Vec::new(),
        }
    }

    /// The script named `name`.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "full" => Some(Self::full()),
            "mixed" => Some(Self::mixed()),
            _ => None,
        }
    }

    pub fn note_time(i: usize) -> i64 {
        FIRST + GAP * i64::try_from(i).unwrap_or(i64::MAX / GAP)
    }

    /// The chart: one note on key 1 per press entry.
    pub fn chart(&self) -> Chart {
        (0..self.presses.len())
            .fold(ChartBuilder::new(150_000), |b, i| {
                b.tap(lane(), TimeUs(Self::note_time(i)))
            })
            .build()
            .expect("a self-made chart builds")
    }

    /// The last note's time.
    pub fn last_note(&self) -> TimeUs {
        TimeUs(Self::note_time(self.presses.len().saturating_sub(1)))
    }

    /// The input log, in time order: a press and its release 50 ms later.
    fn inputs(&self) -> Vec<PhysicalInputEvent> {
        let mut at: Vec<i64> = self
            .presses
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.map(|d| Self::note_time(i) + d))
            .chain(self.ghosts.iter().copied())
            .collect();
        at.sort_unstable();
        let mut out = Vec::new();
        for t in at {
            out.push(set(t, 1));
            out.push(set(t + 50_000, 0));
        }
        out.sort_by_key(|e| e.received.0);
        out
    }

    /// The frame times: every step from READY through the tail, and each
    /// input's time.
    fn times(&self) -> Vec<i64> {
        let end = self.last_note().0 + TAIL;
        let mut t: Vec<i64> = (0..)
            .map(|k: i64| -LEAD + k * STEP)
            .take_while(|&t| t <= end)
            .collect();
        t.extend(self.inputs().iter().map(|e| e.received.0));
        t.extend(self.extra.iter().copied());
        t.sort_unstable();
        t.dedup();
        t
    }

    /// Plays the script, calling `frame` once per frame with what the
    /// evaluator gives at that time: the frame's chart time, its events
    /// (the effects), and the evaluator.
    pub fn play(
        &self,
        chart: &Chart,
        preset: &rules::RulePreset,
        mut frame: impl FnMut(TimeUs, &[JudgeEvent], &Evaluator<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let device = device();
        let start = InputStart {
            opened_at: TimeUs(-LEAD),
            held: Vec::new(),
        };
        let inputs = self.inputs();
        let mut ev = Evaluator::new(chart, preset, &device, &start, None, inputs.len() + 1)
            .map_err(|e| format!("evaluator: {e:?}"))?;
        let mut next = 0;
        let mut out = Vec::new();
        for now in self.times() {
            while let Some(i) = inputs.get(next).filter(|i| i.received.0 <= now) {
                ev.push_input(*i).map_err(|e| format!("input: {e:?}"))?;
                next += 1;
            }
            out.clear();
            ev.advance_to(TimeUs(now), &mut out)
                .map_err(|e| format!("advance: {e:?}"))?;
            frame(TimeUs(now), &out, &ev)?;
        }
        Ok(())
    }
}

fn lane() -> Lane {
    Lane::new(1).expect("key 1")
}

fn set(at: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(1),
            value,
        },
    }
}

/// One source binding control `n` to lane `n`, with no offset.
fn device() -> DeviceProfile {
    DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: ControlId(u32::try_from(l.index()).unwrap_or(u32::MAX)),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}
