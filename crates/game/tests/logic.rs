//! The logic step against source queues: D8's consumer order, staging,
//! retirement, abort, and the guards that keep the record honest.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, judgments, lane, press};
use eval::{
    DeviceProfile, EvalError, InputKind, InputStart, PhysicalInputEvent, Published, SourceId,
};
use game::{Logic, LogicConfig, LogicError, SourceRead};
use input::{SourceReader, SourceWriter, source_queue};
use rules::{Judgment, RulePreset};

const CONFIG: LogicConfig = LogicConfig {
    inputs: 64,
    effects: 64,
};

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
    assert!(!logic.incomplete());
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
    assert!(!logic.incomplete());
}

#[test]
fn every_source_retired_at_once_stops_advancing() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut w0, r0) = source_queue(4);
    let (mut w1, r1) = source_queue(4);
    for (w, s) in [(&mut w0, 0), (&mut w1, 1)] {
        w.push(PhysicalInputEvent {
            received: TimeUs(500_000),
            source: SourceId(s),
            kind: InputKind::Disconnected,
        })
        .unwrap();
        w.close(TimeUs(500_000));
    }
    let mut logic = logic(&chart, &rule, &dev, vec![r0, r1], CONFIG);

    // Both are closed at their final watermark and drained in this step.
    let a = logic.advance_to(TimeUs(501_000));
    assert_eq!(a.watermark, Some(TimeUs(500_000)));
    // Then both are retired: there is no watermark and nothing advances.
    let b = logic.advance_to(TimeUs(502_000));
    assert_eq!(b.watermark, None);
    assert_eq!(b.emitted, 0);
    assert_eq!(logic.finalized(), Some(TimeUs(500_000)));
    assert_eq!(logic.record().events.len(), 2);
}

#[test]
fn staging_full_holds_back_the_watermark_and_loses_nothing() {
    let chart = taps(&[(1, 1_000), (2, 1_100), (3, 1_200), (4, 1_300)]);
    let rule = rules::custom();
    let dev = device(&[0]);
    let (mut writer, reader) = source_queue(16);
    for (l, t) in [
        (1, 1_000_000),
        (2, 1_100_000),
        (3, 1_200_000),
        (4, 1_300_000),
    ] {
        writer.push(press(t, 0, lane(l))).unwrap();
    }
    writer.publish(TimeUs(2_000_000));
    // Room for two inputs: staging and the record hold two.
    let config = LogicConfig {
        inputs: 2,
        effects: 64,
    };
    let mut logic = logic(&chart, &rule, &dev, vec![reader], config);

    let mut watermarks = Vec::new();
    let mut judged = Vec::new();
    for step in 1..=4 {
        let a = logic.advance_to(TimeUs(2_000_000 + step));
        watermarks.push(a.watermark.unwrap().0);
        judged.extend(judgments(logic.events()));
    }
    // A full staging buffer bounds its source just below the last popped
    // time, since events of that time may still be queued.
    assert_eq!(watermarks, vec![1_099_999, 1_199_999, 1_299_999, 2_000_000]);
    assert_eq!(
        judged,
        vec![
            (1, 0, Judgment::Perfect, Some(0)),
            (2, 0, Judgment::Perfect, Some(0)),
            (3, 0, Judgment::Perfect, Some(0)),
            (4, 0, Judgment::Perfect, Some(0)),
        ]
    );
    assert!(logic.is_complete());
    // The record had room for two of the four.
    assert_eq!(logic.record().events.len(), 2);
    assert!(logic.incomplete());
}

#[test]
fn input_at_or_before_the_finalized_watermark_marks_incomplete() {
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

    assert_eq!(a.watermark, Some(TimeUs(1_100_000)));
    assert!(judgments(logic.events()).is_empty());
    assert!(logic.incomplete());
    assert_eq!(logic.record().events, vec![press(900_000, 0, lane(1))]);
}

#[test]
fn event_of_another_source_is_recorded_but_not_staged() {
    let chart = taps(&[(1, 1_000)]);
    let rule = rules::custom();
    let dev = device(&[0, 0]);
    let (mut writer, reader) = source_queue(16);
    let (quiet, other) = source_queue(16);
    writer.push(press(1_000_000, 1, lane(1))).unwrap();
    writer.publish(TimeUs(1_000_000));
    quiet.publish(TimeUs(1_000_000));
    let mut logic = logic(&chart, &rule, &dev, vec![reader, other], CONFIG);
    logic.advance_to(TimeUs(1_000_500));

    assert!(judgments(logic.events()).is_empty());
    assert!(logic.incomplete());
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
    logic.abort();
    let a = logic.advance_to(TimeUs(1_600_500));

    assert_eq!(a.watermark, None);
    assert_eq!(a.emitted, 0);
    let record = logic.record();
    assert_eq!(
        record.events,
        vec![press(1_000_000, 0, lane(1)), press(1_500_000, 0, lane(2))]
    );
    assert_eq!(record.finalized, Some(TimeUs(1_200_000)));
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
