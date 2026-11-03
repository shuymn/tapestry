//! Lane input in the play view through the logic step (design.md "Rendering
//! and Skins", lane input): every lane's held state and latest edges as
//! the step's conversion left them, frozen where an abort stops the play
//! (focus loss or a lost last source; after completion the audio tail's
//! feedback carries them on, `tail_feedback.rs`), and started over by a
//! rearm from its fresh opening.

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::{MS, control, device, lane, press};
use eval::{InputKind, InputStart, LaneInput, PhysicalInputEvent, SourceId};
use game::{AbortCause, Logic, LogicConfig};
use input::{SourceReader, source_queue};

const CONFIG: LogicConfig = LogicConfig {
    inputs: 64,
    effects: 64,
};

fn release(received: i64, source: u16, l: Lane) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Control {
            control: control(l),
            value: 0,
        },
    }
}

fn input(held: bool, pressed: Option<i64>, released: Option<i64>) -> LaneInput {
    LaneInput {
        held,
        pressed: pressed.map(|t| TimeUs(t * MS)),
        released: released.map(|t| TimeUs(t * MS)),
    }
}

/// A focus loss aborts the play: the view keeps the lanes as the last
/// step left them, whatever input arrives after; losing the last source
/// does the same, its disconnect never converted.
#[test]
fn an_abort_freezes_the_lanes_where_the_last_step_left_them() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    for cause in [AbortCause::FocusLoss, AbortCause::LastSourceRemoved] {
        let (mut writer, reader) = source_queue(8);
        let (mut logic, _side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &dev,
            &InputStart::default(),
            vec![reader],
            CONFIG,
            Vec::new(),
        )
        .unwrap();
        writer.push(press(100 * MS, 0, lane(3))).unwrap();
        writer.publish(TimeUs(200 * MS));
        logic.advance_to(TimeUs(200 * MS));
        let frozen = logic.view().input;
        assert_eq!(frozen.lane(lane(3)), input(true, Some(100), None));
        if cause == AbortCause::FocusLoss {
            writer.push(release(300 * MS, 0, lane(3))).unwrap();
            writer.publish(TimeUs(400 * MS));
            logic.abort(cause);
        } else {
            writer
                .push(PhysicalInputEvent {
                    received: TimeUs(300 * MS),
                    source: SourceId(0),
                    kind: InputKind::Disconnected,
                })
                .unwrap();
            writer.close(TimeUs(300 * MS));
        }
        let a = logic.advance_to(TimeUs(400 * MS));
        assert_eq!(a.aborted, Some(cause));
        logic.stop_playing();
        assert_eq!(logic.view().input, frozen, "{cause:?}");
    }
}

/// A rearm starts the lane input over from the next opening, in place: a
/// lane held there is held without a press, the cancelled candidate's
/// edges are gone, and nothing is converted yet.
#[test]
fn a_rearm_starts_the_lanes_over_from_its_opening() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(5_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(8);
    let (mut logic, _side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        vec![reader],
        CONFIG,
        Vec::new(),
    )
    .unwrap();
    writer.push(press(100 * MS, 0, lane(3))).unwrap();
    writer.push(release(150 * MS, 0, lane(3))).unwrap();
    writer.publish(TimeUs(200 * MS));
    logic.advance_to(TimeUs(200 * MS));
    let readers = logic.lend_readers();
    logic.restore_readers(readers);
    logic.rearm(
        &InputStart {
            opened_at: TimeUs(0),
            held: vec![(SourceId(0), control(lane(6)), 1)],
        },
        TimeUs(10_000 * MS),
    );
    let view = logic.view();
    assert_eq!(view.generation, 1);
    assert_eq!(view.input.lane(lane(3)), LaneInput::default());
    assert_eq!(view.input.lane(lane(6)), input(true, None, None));
    assert_eq!(view.input.converted(), None);
}
