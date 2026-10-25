//! The logic step against source queues: D8's consumer order, staging,
//! retirement, the input budget, abort, and the guards that keep the
//! record honest.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, judgments, lane, press};
use eval::{
    DeviceProfile, EvalError, InputKind, InputStart, PhysicalInputEvent, Published, SourceId,
};
use game::{AbortCause, Logic, LogicConfig, LogicError, SourceRead};
use input::{SourceReader, SourceWriter, source_queue};
use rules::{Judgment, RulePreset};

const CONFIG: LogicConfig = LogicConfig {
    inputs: 64,
    effects: 64,
};

/// A budget of `inputs` control events, plus one disconnect per source.
fn budget(inputs: usize) -> LogicConfig {
    LogicConfig {
        inputs,
        effects: 64,
    }
}

fn taps(times_ms: &[(u8, i64)]) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for &(l, t) in times_ms {
        b = b.tap(lane(l), TimeUs(t * MS));
    }
    b.build().unwrap()
}

fn logic<'a, R: SourceRead>(
    chart: &'a Chart,
    rule: &'a RulePreset,
    dev: &'a DeviceProfile,
    readers: Vec<R>,
    config: LogicConfig,
) -> Logic<'a, R> {
    Logic::new(
        chart,
        rule,
        dev,
        &InputStart::default(),
        readers,
        config,
        Vec::new(),
    )
    .unwrap()
    .0
}

fn disconnected(at: i64, source: u16) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(source),
        kind: InputKind::Disconnected,
    }
}

/// A reader whose producer pushes and publishes a press right after the
/// consumer's first empty pop: between that pop and the next watermark
/// read.
struct Racing {
    reader: SourceReader,
    writer: SourceWriter,
    late: Option<PhysicalInputEvent>,
}

impl SourceRead for Racing {
    fn published(&self) -> Published {
        self.reader.published()
    }

    fn pop(&mut self) -> Option<PhysicalInputEvent> {
        let ev = self.reader.pop();
        if ev.is_none()
            && let Some(p) = self.late.take()
        {
            self.writer.push(p).unwrap();
            self.writer.publish(p.received);
        }
        ev
    }
}

#[test]
fn boundary_press_published_between_pop_and_watermark_read_is_judged() {
    // A tap at 1 s; the custom preset's late BAD edge is 180 ms, so its
    // miss deadline, and the press, are at 1 180 000 µs.
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let late_edge = 1_180_000;
    let (writer, reader) = source_queue(16);
    writer.publish(TimeUs(late_edge - 1));
    let racing = Racing {
        reader,
        writer,
        late: Some(press(late_edge, 0, lane(1))),
    };
    let (mut logic, mut render) = Logic::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        vec![racing],
        CONFIG,
        Vec::new(),
    )
    .unwrap();

    // The watermark was read before the empty pop, so the miss at the late
    // edge is not final yet.
    let first = logic.advance_to(TimeUs(late_edge + 500));
    assert_eq!(first.watermark, Some(TimeUs(late_edge - 1)));
    assert_eq!(first.emitted, 0);
    let second = logic.advance_to(TimeUs(late_edge + 1_500));
    assert_eq!(second.watermark, Some(TimeUs(late_edge)));
    assert_eq!(
        judgments(logic.events()),
        vec![(1, 0, Judgment::Bad, Some(180_000))]
    );
    assert!(!logic.overflowed());
    assert_eq!(render.view().tally.counts.bad, 1);
    assert_eq!(render.view().tally.counts.poor, 0);
}

#[test]
fn event_beyond_published_watermark_stays_staged() {
    let chart = taps(&[(1, 1_000), (2, 1_100)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    // Pushed, but the publication is still behind the first of them.
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.push(press(1_100_000, 0, lane(2))).unwrap();
    writer.publish(TimeUs(999_999));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], CONFIG);

    let a = logic.advance_to(TimeUs(1_000_500));
    assert_eq!(a.watermark, Some(TimeUs(999_999)));
    assert_eq!(a.emitted, 0);
    // Popping stopped after the first event beyond the watermark.
    assert_eq!(logic.record().events, vec![press(1_000_000, 0, lane(1))]);

    writer.publish(TimeUs(1_000_000));
    logic.advance_to(TimeUs(1_001_000));
    assert_eq!(
        judgments(logic.events()),
        vec![(1, 0, Judgment::Perfect, Some(0))]
    );
    assert_eq!(logic.record().events.len(), 2);
}

#[test]
fn unmoved_watermark_step_emits_nothing() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.publish(TimeUs(1_000_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], CONFIG);

    let a = logic.advance_to(TimeUs(1_000_700));
    assert_eq!(judgments(logic.events()).len(), 1);
    let b = logic.advance_to(TimeUs(1_001_900));
    assert_eq!(a.watermark, Some(TimeUs(1_000_000)));
    assert_eq!(b.watermark, Some(TimeUs(1_000_000)));
    assert_eq!(b.emitted, 0);
    assert!(logic.events().is_empty());
    assert_eq!(b.finalization_delay, Some(TimeUs(1_900)));
    assert_eq!(logic.finalized(), Some(TimeUs(1_000_000)));
    assert!(!logic.overflowed());
}

#[test]
fn removed_source_drains_then_retires_while_another_remains() {
    let chart = taps(&[(1, 400), (2, 900)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut gone, r0) = source_queue(4);
    let (mut kept, r1) = source_queue(4);
    gone.push(press(400_000, 0, lane(1))).unwrap();
    gone.push(disconnected(500_000, 0)).unwrap();
    gone.close(TimeUs(500_000));
    kept.publish(TimeUs(450_000));
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], CONFIG);

    // The closed source still bounds the watermark while it drains.
    let a = logic.advance_to(TimeUs(451_000));
    assert_eq!(a.watermark, Some(TimeUs(450_000)));
    assert_eq!(
        judgments(logic.events()),
        vec![(1, 0, Judgment::Perfect, Some(0))]
    );
    kept.publish(TimeUs(700_000));
    let b = logic.advance_to(TimeUs(701_000));
    assert_eq!(b.watermark, Some(TimeUs(500_000)));
    // Drained and retired: the open source alone sets the watermark.
    kept.push(press(900_000, 1, lane(2))).unwrap();
    kept.publish(TimeUs(950_000));
    let c = logic.advance_to(TimeUs(951_000));
    assert_eq!(c.watermark, Some(TimeUs(950_000)));
    assert_eq!(c.aborted, None);
    assert_eq!(
        judgments(logic.events()),
        vec![(2, 0, Judgment::Perfect, Some(0))]
    );
}

#[test]
fn losing_every_source_aborts_before_advancing() {
    let chart = taps(&[(1, 300)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (w0, r0) = source_queue(4);
    let (w1, r1) = source_queue(4);
    w0.publish(TimeUs(200_000));
    w1.publish(TimeUs(200_000));
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], CONFIG);
    assert_eq!(
        logic.advance_to(TimeUs(201_000)).watermark,
        Some(TimeUs(200_000))
    );

    // Both devices go in the same batch, after the note's miss deadline
    // (480 000 µs): their final watermarks would finalize the miss.
    let mut writers = [w0, w1];
    for (s, w) in (0..).zip(&mut writers) {
        w.push(disconnected(500_000, s)).unwrap();
        w.close(TimeUs(500_000));
    }
    let a = logic.advance_to(TimeUs(501_000));
    assert_eq!(a.watermark, None);
    assert_eq!(a.emitted, 0);
    assert_eq!(a.aborted, Some(AbortCause::LastSourceRemoved));
    assert_eq!(logic.finalized(), Some(TimeUs(200_000)));
    let record = logic.record();
    // The tails are recorded, not evaluated.
    assert_eq!(
        record.events,
        vec![disconnected(500_000, 0), disconnected(500_000, 1)]
    );
    let batch = eval::evaluate(&chart, &record.eval_input().unwrap(), &rule).unwrap();
    assert!(judgments(&batch.events).is_empty());
}

#[test]
fn capacity_one_takes_one_input() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(4);
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.publish(TimeUs(1_200_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], budget(0));
    let a = logic.advance_to(TimeUs(1_200_500));

    assert_eq!(
        judgments(logic.events()),
        vec![(1, 0, Judgment::Perfect, Some(0))]
    );
    assert!(a.complete);
    assert!(!logic.overflowed());
}

#[test]
fn equal_time_group_beyond_the_budget_aborts_unevaluated() {
    let chart = taps(&[(1, 1_000), (2, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(4);
    // One batch of two presses, and a budget of one input (the one source's disconnect allowance).
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.push(press(1_000_000, 0, lane(2))).unwrap();
    writer.publish(TimeUs(1_000_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], budget(0));
    let a = logic.advance_to(TimeUs(1_000_500));

    assert_eq!(a.aborted, Some(AbortCause::Overflow));
    assert_eq!(a.emitted, 0);
    assert!(logic.overflowed());
    let record = logic.record();
    assert_eq!(record.events, vec![press(1_000_000, 0, lane(1))]);
    // Nothing was finalized, so there is nothing to re-evaluate.
    assert_eq!(record.finalized, None);
    assert_eq!(record.eval_input(), None);
    assert_eq!(logic.advance_to(TimeUs(2_000_000)).watermark, None);
}

#[test]
fn input_beyond_the_budget_aborts_at_the_finalized_watermark() {
    let chart = taps(&[(1, 1_000), (2, 1_100), (3, 1_200), (4, 1_300)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut w0, r0) = source_queue(16);
    let (mut w1, r1) = source_queue(16);
    w0.publish(TimeUs(999_999));
    w1.publish(TimeUs(999_999));
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], budget(1));
    logic.advance_to(TimeUs(1_000_000));
    // Four inputs over two sources against a budget of three (one control
    // event and two disconnect allowances), which is
    // shared: no single source exceeds it.
    w0.push(press(1_000_000, 0, lane(1))).unwrap();
    w0.push(press(1_100_000, 0, lane(2))).unwrap();
    w1.push(press(1_200_000, 1, lane(3))).unwrap();
    w1.push(press(1_300_000, 1, lane(4))).unwrap();
    w0.publish(TimeUs(2_000_000));
    w1.publish(TimeUs(2_000_000));
    let a = logic.advance_to(TimeUs(2_000_500));

    assert_eq!(a.aborted, Some(AbortCause::Overflow));
    assert_eq!(a.emitted, 0);
    assert!(logic.overflowed());
    let record = logic.record();
    assert_eq!(record.finalized, Some(TimeUs(999_999)));
    assert_eq!(
        record.events,
        vec![
            press(1_000_000, 0, lane(1)),
            press(1_100_000, 0, lane(2)),
            press(1_200_000, 1, lane(3)),
        ]
    );
}

#[test]
fn input_at_or_before_the_finalized_watermark_aborts() {
    let chart = taps(&[(1, 1_050)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    writer.publish(TimeUs(1_000_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], CONFIG);
    logic.advance_to(TimeUs(1_000_500));
    // A producer breaking the protocol: an event below its publication.
    writer.push(press(900_000, 0, lane(1))).unwrap();
    writer.publish(TimeUs(1_100_000));
    let a = logic.advance_to(TimeUs(1_100_500));

    assert_eq!(a.aborted, Some(AbortCause::Overflow));
    assert_eq!(a.watermark, None);
    assert!(logic.events().is_empty());
    assert!(logic.overflowed());
    assert_eq!(logic.finalized(), Some(TimeUs(1_000_000)));
    assert_eq!(logic.record().events, vec![press(900_000, 0, lane(1))]);
}

#[test]
fn event_of_another_source_aborts() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut writer, reader) = source_queue(16);
    let (quiet, other) = source_queue(16);
    writer.push(press(1_000_000, 1, lane(1))).unwrap();
    writer.publish(TimeUs(1_000_000));
    quiet.publish(TimeUs(1_000_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader, other], CONFIG);
    let a = logic.advance_to(TimeUs(1_000_500));

    assert_eq!(a.aborted, Some(AbortCause::Overflow));
    assert!(logic.events().is_empty());
    assert!(logic.overflowed());
    assert_eq!(logic.record().events, vec![press(1_000_000, 1, lane(1))]);
}

#[test]
fn abort_keeps_queued_events_and_stops_advancing() {
    let chart = taps(&[(1, 1_000), (2, 1_500)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.publish(TimeUs(1_200_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], CONFIG);
    logic.advance_to(TimeUs(1_200_500));
    assert_eq!(judgments(logic.events()).len(), 1);
    // Queued and published, then the play aborts before the next step.
    writer.push(press(1_500_000, 0, lane(2))).unwrap();
    writer.publish(TimeUs(1_600_000));
    logic.abort(AbortCause::FocusLoss);
    logic.abort(AbortCause::Overflow);
    let a = logic.advance_to(TimeUs(1_600_500));

    assert_eq!(a.watermark, None);
    assert_eq!(a.emitted, 0);
    // The first cause stays.
    assert_eq!(a.aborted, Some(AbortCause::FocusLoss));
    assert!(!logic.overflowed());
    let record = logic.record();
    assert_eq!(
        record.events,
        vec![press(1_000_000, 0, lane(1)), press(1_500_000, 0, lane(2))]
    );
    assert_eq!(record.finalized, Some(TimeUs(1_200_000)));
}

#[test]
fn abort_tail_beyond_the_budget_marks_overflow() {
    let chart = taps(&[(1, 1_000), (2, 1_500)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    writer.push(press(1_000_000, 0, lane(1))).unwrap();
    writer.publish(TimeUs(1_200_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader], budget(0));
    logic.advance_to(TimeUs(1_200_500));
    assert!(!logic.overflowed());
    writer.push(press(1_500_000, 0, lane(2))).unwrap();
    logic.abort(AbortCause::FocusLoss);

    assert!(logic.overflowed());
    assert_eq!(logic.record().events, vec![press(1_000_000, 0, lane(1))]);
}

#[test]
fn disconnect_after_a_full_d8_window_fits_the_budget() {
    // A tap at 3 s on lane 1; the inputs below play lane 7, which has none.
    let chart = taps(&[(1, 3_000)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    // D8: one bound button per source, pressed and released 30 times a
    // second over a 2 s window: 2 × 60 × 2 = 240 control events.
    let inputs = common::window_capacity(TimeUs(0), TimeUs(2_000_000), 2);
    assert_eq!(inputs, 240);
    let (mut w0, r0) = source_queue(256);
    let (mut w1, r1) = source_queue(256);
    for k in 0..60 {
        for (s, w) in [(0, &mut w0), (1, &mut w1)] {
            let t = k * 33_000 + i64::from(s) * 1_000;
            w.push(press(t, s, lane(7))).unwrap();
            w.push(PhysicalInputEvent {
                kind: InputKind::Control {
                    control: common::control(lane(7)),
                    value: 0,
                },
                ..press(t + 10_000, s, lane(7))
            })
            .unwrap();
        }
    }
    w0.publish(TimeUs(1_999_999));
    w1.publish(TimeUs(1_999_999));
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], budget(inputs));
    logic.advance_to(TimeUs(2_000_000));
    assert_eq!(logic.record().events.len(), 240);
    // The window is spent; source 0 is then removed while source 1 stays.
    w0.push(disconnected(2_000_000, 0)).unwrap();
    w0.close(TimeUs(2_000_000));
    w1.publish(TimeUs(2_100_000));
    let a = logic.advance_to(TimeUs(2_100_500));

    assert_eq!(a.aborted, None);
    assert_eq!(a.watermark, Some(TimeUs(2_000_000)));
    assert!(!logic.overflowed());
    assert_eq!(logic.record().events.len(), 241);
}

#[test]
fn source_beyond_its_share_of_the_window_aborts() {
    let chart = taps(&[(1, 1_000), (2, 1_100)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut w0, r0) = source_queue(4);
    let (w1, r1) = source_queue(4);
    // No control events in the window: each source may stage only its
    // disconnect, though the shared budget (two) has room for both.
    w0.push(press(1_000_000, 0, lane(1))).unwrap();
    w0.push(press(1_100_000, 0, lane(2))).unwrap();
    w0.publish(TimeUs(1_200_000));
    w1.publish(TimeUs(1_200_000));
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], budget(0));
    let a = logic.advance_to(TimeUs(1_200_500));

    assert_eq!(a.aborted, Some(AbortCause::Overflow));
    assert!(logic.events().is_empty());
    assert_eq!(logic.record().events.len(), 2);
}

#[test]
fn overflow_abort_from_the_caller_marks_overflow() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (_writer, reader) = source_queue(4);
    let mut logic = logic(&chart, &rule, &dev, vec![reader], CONFIG);
    logic.abort(AbortCause::Overflow);

    assert_eq!(logic.aborted(), Some(AbortCause::Overflow));
    assert!(logic.overflowed());
}

/// A source that never publishes anything.
struct Silent;

impl SourceRead for Silent {
    fn published(&self) -> Published {
        Published::open(TimeUs(i64::MIN))
    }

    fn pop(&mut self) -> Option<PhysicalInputEvent> {
        None
    }
}

#[test]
fn readers_beyond_the_source_id_range_are_refused() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let config = LogicConfig {
        inputs: 1,
        effects: 1,
    };
    let start = InputStart::default();
    let limit = 1 << 16;
    let readers = || Vec::new();
    let at_limit: Vec<Silent> = std::iter::repeat_with(|| Silent).take(limit).collect();
    assert!(Logic::new(&chart, &rule, &dev, &start, at_limit, config, readers()).is_ok());
    let beyond: Vec<Silent> = std::iter::repeat_with(|| Silent).take(limit + 1).collect();
    assert_eq!(
        Logic::new(&chart, &rule, &dev, &start, beyond, config, readers()).err(),
        Some(LogicError::TooManySources(limit + 1))
    );
}

#[test]
fn evaluator_refusal_is_reported() {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let rule = rules::custom();
    let dev = device(&[0]);
    let readers: Vec<Silent> = vec![Silent];
    let result = Logic::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        readers,
        CONFIG,
        Vec::new(),
    );
    assert_eq!(
        result.err(),
        Some(LogicError::Eval(EvalError::NoJudgeableNotes))
    );
}
