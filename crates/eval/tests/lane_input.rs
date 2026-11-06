//! Each lane's input as conversion left it ([`eval::InputFacts`]): every
//! press and release edge, judged or not, at its evaluation time, the
//! lanes the opening holds, and how far conversion went.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use common::{MS, control, device, disconnect, lane, press, release};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, DeviceProfile, Evaluator, InputKind, InputStart,
    LaneInput, PhysicalInputEvent, ScratchSettings, Section, SourceId,
};

const AXIS: ControlId = ControlId(0x1_0030);

/// One tap on key 1 at 10 s: nothing on the other lanes.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(10_000 * MS))
        .build()
        .unwrap()
}

fn lane_input(held: bool, pressed: Option<i64>, released: Option<i64>) -> LaneInput {
    LaneInput {
        held,
        pressed: pressed.map(TimeUs),
        released: released.map(TimeUs),
        presses: u32::from(pressed.is_some()),
    }
}

fn report(received: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(0),
        kind: InputKind::Control {
            control: AXIS,
            value,
        },
    }
}

/// Two sources 5 ms apart in input offset, each binding control `n` to
/// lane `n`; source 0 also binds control 9 to key 2 and the scratch axis.
fn bound() -> DeviceProfile {
    let mut d = device(&[0, 5 * MS]);
    d.sources[0].bindings.push(Binding {
        control: control(9),
        lane: lane(2),
    });
    d.sources[0].bindings.retain(|b| b.lane != Lane::SCRATCH);
    d.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: Lane::SCRATCH,
        scratch: ScratchSettings {
            mode: AxisMode::Absolute { wrap: 256 },
            ..ScratchSettings::DEFAULT
        },
    });
    d
}

/// Presses on lanes without notes, a key's repeat, two controls on one
/// lane, a second source's offset, a scratch reversal, a scratch hold
/// extending its release deadline and its expiry with no input, and a
/// disconnect: each lane's facts follow every edge at its evaluation
/// time, and the conversion watermark follows each advance.
#[test]
fn every_edge_reaches_the_lane_facts() {
    let chart = chart();
    let dev = bound();
    let rule = rules::tapestry();
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), None, 64).unwrap();
    assert_eq!(ev.input().converted(), None);
    for i in [
        // Key 3, a lane without notes, pressed, repeated, released.
        press(1_000 * MS, 0, 3),
        press(1_030 * MS, 0, 3),
        release(1_100 * MS, 0, 3),
        // Key 2 held by two controls: the first release keeps it held.
        press(1_200 * MS, 0, 2),
        press(1_250 * MS, 0, 9),
        release(1_300 * MS, 0, 2),
        // The scratch: a press, a reversal, a hold moving the deadline.
        report(1_400 * MS, 10),
        report(1_500 * MS, 12),
        report(1_560 * MS, 10),
        report(1_600 * MS, 9),
        // Key 5 on source 1, 5 ms late: at 1 695 ms.
        press(1_700 * MS, 1, 5),
    ] {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_699 * MS), &mut out).unwrap();
    let f = *ev.input();
    assert_eq!(f.converted(), Some(TimeUs(1_699 * MS)));
    assert_eq!(
        f.lane(lane(3)),
        lane_input(false, Some(1_000 * MS), Some(1_100 * MS))
    );
    assert_eq!(f.lane(lane(2)), lane_input(true, Some(1_200 * MS), None));
    // The reversal at 1 560 ms released and pressed again; held wins.
    assert_eq!(
        f.lane(Lane::SCRATCH),
        lane_input(true, Some(1_560 * MS), Some(1_560 * MS))
    );
    assert_eq!(f.lane(lane(5)), lane_input(true, Some(1_695 * MS), None));
    assert_eq!(f.lane(lane(1)), LaneInput::default());
    // The hold at 1 600 ms moved the release to 1 700 ms: still held just
    // before it, released at it with no input.
    ev.advance_to(TimeUs(1_700 * MS - 1), &mut out).unwrap();
    assert!(ev.input().lane(Lane::SCRATCH).held);
    ev.advance_to(TimeUs(1_700 * MS), &mut out).unwrap();
    assert_eq!(
        ev.input().lane(Lane::SCRATCH),
        lane_input(false, Some(1_560 * MS), Some(1_700 * MS))
    );
    // Source 0 removed: key 2's second control lets it go.
    ev.push_input(disconnect(1_800 * MS, 0)).unwrap();
    ev.advance_to(TimeUs(1_800 * MS), &mut out).unwrap();
    assert_eq!(
        ev.input().lane(lane(2)),
        lane_input(false, Some(1_200 * MS), Some(1_800 * MS))
    );
    assert!(ev.input().lane(lane(5)).held);
}

/// The scratch's pending release meets an input at its very deadline:
/// the input comes first, and its hold moves the release later.
#[test]
fn input_at_the_deadline_extends_the_release() {
    let chart = chart();
    let dev = bound();
    let rule = rules::tapestry();
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), None, 8).unwrap();
    for i in [
        report(900 * MS, 10),
        report(1_000 * MS, 12),
        report(1_100 * MS, 13),
    ] {
        ev.push_input(i).unwrap();
    }
    let mut out = Vec::new();
    ev.advance_to(TimeUs(1_199 * MS), &mut out).unwrap();
    assert_eq!(
        ev.input().lane(Lane::SCRATCH),
        lane_input(true, Some(1_000 * MS), None)
    );
    ev.advance_to(TimeUs(1_200 * MS), &mut out).unwrap();
    assert_eq!(
        ev.input().lane(Lane::SCRATCH),
        lane_input(false, Some(1_000 * MS), Some(1_200 * MS))
    );
}

/// A control held at the opening holds its lane without a press, and a
/// reset starts the facts over from the next opening; a section's input
/// before its lead-in, converted but never judged, still counts.
#[test]
fn the_opening_holds_without_a_press_and_a_reset_starts_over() {
    let chart = chart();
    let dev = device(&[0]);
    let rule = rules::tapestry();
    let held = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), control(4), 1)],
    };
    let section = Section {
        start: TimeUs(9_000 * MS),
        end: TimeUs(11_000 * MS),
        lead_in: TimeUs(1_000 * MS),
        initial_gauges: rule.gauges.iter().map(|g| g.start).collect(),
    };
    let mut ev = Evaluator::new(&chart, &rule, &dev, &held, Some(&section), 8).unwrap();
    assert_eq!(ev.input().lane(lane(4)), lane_input(true, None, None));
    ev.push_input(release(500 * MS, 0, 4)).unwrap();
    ev.push_input(press(600 * MS, 0, 6)).unwrap();
    let mut out = Vec::new();
    ev.advance_to(TimeUs(700 * MS), &mut out).unwrap();
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(
        ev.input().lane(lane(4)),
        lane_input(false, None, Some(500 * MS))
    );
    assert_eq!(
        ev.input().lane(lane(6)),
        lane_input(true, Some(600 * MS), None)
    );
    ev.reset(&InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), control(6), 1)],
    });
    assert_eq!(ev.input().converted(), None);
    assert_eq!(ev.input().lane(lane(4)), LaneInput::default());
    assert_eq!(ev.input().lane(lane(6)), lane_input(true, None, None));
}
