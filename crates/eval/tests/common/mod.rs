//! Helpers shared by the evaluator's integration tests.

#![allow(dead_code)]

use bms_model::{Chart, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, Evaluator, InputKind, InputStart, JudgeEvent,
    JudgeEventKind, PhysicalInputEvent, SourceId, SourceProfile, Tally,
};
use rules::{Judgment, RulePreset};

pub const MS: i64 = 1_000;

/// Key lane `n` (1-7); 0 is the scratch.
pub fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

/// The control each source binds to lane `n`: control `n`.
pub fn control(n: u8) -> ControlId {
    ControlId(u32::from(n))
}

/// One source per offset (µs), each binding control `n` to lane `n`.
pub fn device(offsets: &[i64]) -> DeviceProfile {
    DeviceProfile {
        sources: offsets
            .iter()
            .map(|&o| SourceProfile {
                input_offset: TimeUs(o),
                bindings: Lane::all()
                    .map(|l| Binding {
                        control: control(u8::try_from(l.index()).unwrap()),
                        lane: l,
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub fn set(received: i64, source: u16, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Control {
            control: control(n),
            value,
        },
    }
}

pub fn press(received: i64, source: u16, n: u8) -> PhysicalInputEvent {
    set(received, source, n, 1)
}

pub fn release(received: i64, source: u16, n: u8) -> PhysicalInputEvent {
    set(received, source, n, 0)
}

pub fn disconnect(received: i64, source: u16) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Disconnected,
    }
}

/// Evaluates `inputs` (already in merge order) with the custom preset up to
/// `watermark`, in one step.
pub fn run(
    chart: &Chart,
    device: &DeviceProfile,
    start: &InputStart,
    inputs: &[PhysicalInputEvent],
    watermark: i64,
) -> (Vec<JudgeEvent>, Tally) {
    let preset = rules::custom();
    run_with(chart, &preset, device, start, inputs, watermark)
}

/// Version 1 of the custom preset: empty POOR only up to 300 ms before the
/// next unjudged note, without BP. Its tests keep that range covered.
pub fn custom_v1() -> RulePreset {
    let mut p = rules::custom();
    p.version = 1;
    p.empty_poor.ahead = TimeUs(300 * MS);
    p.empty_poor.behind = None;
    p.empty_poor.counts_bp = false;
    p
}

pub fn run_with(
    chart: &Chart,
    preset: &RulePreset,
    device: &DeviceProfile,
    start: &InputStart,
    inputs: &[PhysicalInputEvent],
    watermark: i64,
) -> (Vec<JudgeEvent>, Tally) {
    let mut ev = Evaluator::new(chart, preset, device, start, 64).unwrap();
    for &i in inputs {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(watermark), &mut out).unwrap();
    (out, *ev.tally())
}

/// The judgments in `events`: (lane index, note index, judgment, deviation
/// in µs).
pub fn judgments(events: &[JudgeEvent]) -> Vec<(usize, usize, Judgment, Option<i64>)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                ..
            } => Some((lane.index(), note, judgment, deviation.map(|d| d.0))),
            _ => None,
        })
        .collect()
}

pub fn empty_poors(events: &[JudgeEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
        .count()
}
