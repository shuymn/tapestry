//! Helpers shared by the game's integration tests.

#![allow(dead_code)]

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, InputKind, JudgeEvent, JudgeEventKind, PhysicalInputEvent,
    SourceId, SourceProfile,
};
use input::{DeviceKind, ScriptedInput, SourceConfig, autoplay};
use rules::Judgment;

pub const MS: i64 = 1_000;

/// The control a source binds to `lane`: `0x100 + lane index`, so control
/// ids never equal lane numbers.
pub fn control(lane: Lane) -> ControlId {
    ControlId(0x100 + u32::try_from(lane.index()).unwrap())
}

/// One source per offset (µs), each binding every lane.
pub fn device(offsets: &[i64]) -> DeviceProfile {
    DeviceProfile {
        sources: offsets
            .iter()
            .map(|&o| SourceProfile {
                input_offset: TimeUs(o),
                bindings: Lane::all()
                    .map(|l| Binding {
                        control: control(l),
                        lane: l,
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

/// `count` notes from 1 s on, `gap` apart, cycling over the 8 lanes from
/// the scratch; every 5th is a long note `long` long.
pub fn chart(count: i64, gap: i64, long: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..count {
        let l = lane(u8::try_from(i % 8).unwrap());
        let t = 1_000 * MS + i * gap;
        b = if i % 5 == 4 {
            b.long(l, TimeUs(t), TimeUs(t + long))
        } else {
            b.tap(l, TimeUs(t))
        };
    }
    b.build().unwrap()
}

/// The recording window's input capacity (D8): every bound button pressed
/// and released 30 times a second from `start` to `end`, whole seconds up.
pub fn window_capacity(start: TimeUs, end: TimeUs, buttons: usize) -> usize {
    let secs = u64::try_from(end.0 - start.0).unwrap().div_ceil(1_000_000);
    let secs = usize::try_from(secs).unwrap();
    buttons * 60 * secs
}

pub fn sources() -> SourceConfig {
    SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 16,
        queue_capacity: 1024,
        max_arrivals: 2,
    }
}

/// Autoplay of `chart` for source 0 of `device`, and a script feeding it
/// from one keyboard.
pub fn autoplay_script(
    chart: &Chart,
    device: &DeviceProfile,
) -> (Vec<PhysicalInputEvent>, ScriptedInput) {
    let events = autoplay(chart, &device.sources[0]).unwrap();
    let script = ScriptedInput::from_log(&Default::default(), &events, &[DeviceKind::Keyboard]);
    (events, script)
}

pub fn press(received: i64, source: u16, lane: Lane) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Control {
            control: control(lane),
            value: 1,
        },
    }
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

pub fn count_judged(events: &[JudgeEvent], j: Judgment) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { judgment, .. } if judgment == j))
        .count()
}
