//! The headless runner over whole plays.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{
    MS, autoplay_script, chart, control, count_judged, device, judgments, lane, window_capacity,
};
use eval::{
    ControlId, DeviceProfile, EvalInput, InputKind, InputStart, PhysicalInputEvent, SourceId,
};
use game::{AbortCause, LogicConfig, LogicError, RunOutcome, Runner};
use input::{
    Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceConfig, SourceSet,
};
use rules::{Judgment, RulePreset};

/// Recording opens 1 s before chart time 0.
const START: TimeUs = TimeUs(-1_000_000);
const NOTES: i64 = 500;

/// 500 notes 97 ms apart (every 5th a 60 ms long note) and one keyboard
/// with a 3 ms input offset.
fn play() -> (Chart, RulePreset, DeviceProfile) {
    (
        chart(NOTES, 97 * MS, 60 * MS),
        rules::tapestry(),
        device(&[3 * MS]),
    )
}

fn config(chart: &Chart, effects: usize) -> LogicConfig {
    let last = chart.lanes.iter().flatten().map(|n| n.end()).max().unwrap();
    LogicConfig {
        inputs: window_capacity(START, TimeUs(last.0 + 1_000 * MS), 8),
        effects,
    }
}

fn autoplay_runner<'a>(
    chart: &'a Chart,
    rule: &'a RulePreset,
    dev: &'a DeviceProfile,
    effects: usize,
) -> (Vec<PhysicalInputEvent>, Runner<'a>) {
    let (events, script) = autoplay_script(chart, dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let runner = Runner::new(
        chart,
        rule,
        dev,
        sources,
        script,
        config(chart, effects),
        START,
        common::audio(chart, START),
    )
    .unwrap();
    (events, runner)
}

#[test]
fn autoplay_key_sounds_land_at_their_frames() {
    // 40 taps 50 ms apart from 1.002 s, sounds 0, 1, 2 in turn; presses
    // arrive 3 ms after their notes. The device wakes every 10 ms from
    // START (-1 s), so a press at r lies 5 ms after a wake. It is judged
    // within 2 ms (the next publication covers it), so the wake at r + 5 ms
    // takes it; wake j writes from frame (j + 1) × 480, where the sound must
    // start with its first sample.
    const N: i64 = 40;
    let mut b = ChartBuilder::new(150_000).sound("a").sound("b").sound("c");
    for i in 0..N {
        let l = lane(u8::try_from(i % 7).unwrap() + 1);
        b = b.tap(l, TimeUs(1_002 * MS + i * 50 * MS));
    }
    let mut chart = b.build().unwrap();
    for l in &mut chart.lanes {
        for n in l.iter_mut() {
            let i = (n.time.0 - 1_002 * MS) / (50 * MS);
            n.sound = Some(bms_model::SoundId(u16::try_from(i % 3).unwrap()));
        }
    }
    let rule = rules::tapestry();
    let dev = device(&[3 * MS]);
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = common::audio_with(&chart, START, 6 * 48_000, 0);
    let mut runner = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config(&chart, 256),
        START,
        audio,
    )
    .unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    assert_eq!(count_judged(runner.stream(), Judgment::Perfect), N as usize);

    let left = common::left(&runner);
    let expected: Vec<usize> = (0..N)
        .map(|i| {
            let r = 1_002 * MS + i * 50 * MS + 3 * MS;
            let j = (r + 5 * MS - START.0) / (10 * MS);
            usize::try_from((j + 1) * 480).unwrap()
        })
        .collect();
    assert_eq!(common::onsets(&left), expected);
    for (i, &f) in expected.iter().enumerate() {
        // Sound k is the constant 1000 + 7 k (common::bank), halved by
        // the mixer's headroom.
        let first = f32::from(1_000 + 7 * i16::try_from(i % 3).unwrap()) / 65_536.0;
        assert_eq!(left[f], first, "note {i}");
    }
}

#[test]
fn runner_autoplay_is_all_perfect() {
    let (chart, rule, dev) = play();
    let (_, mut runner) = autoplay_runner(&chart, &rule, &dev, 4_096);
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    assert_eq!(judgments(runner.stream()).len(), 500);
    assert_eq!(count_judged(runner.stream(), Judgment::Perfect), 500);
    assert!(!runner.incomplete().any());
    let view = runner.view();
    assert!(view.complete);
    assert_eq!(view.tally.counts.perfect, 500);
    assert_eq!(view.tally.max_combo, 500);
}

#[test]
fn live_events_equal_batch_evaluation() {
    let (chart, rule, dev) = play();
    let (events, mut runner) = autoplay_runner(&chart, &rule, &dev, 4_096);
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    assert_eq!(judgments(runner.stream()).len(), 500);

    let record = runner.record();
    // One press and one release per note.
    assert_eq!(record.events.len(), 1_000);
    assert_eq!(record.events, events);
    let batch = eval::evaluate(&chart, &record.eval_input().unwrap(), &rule).unwrap();
    assert!(batch.completed);
    assert_eq!(batch.events, runner.stream());
}

#[test]
fn render_side_receives_every_effect_once() {
    let (chart, rule, dev) = play();
    let (_, mut runner) = autoplay_runner(&chart, &rule, &dev, 4_096);
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    let n = runner.stream().len();
    assert!(n > 0);
    let seqs: Vec<u64> = runner.effects().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (0..n as u64).collect::<Vec<_>>());
    assert_eq!(runner.effects(), runner.stream());
    assert_eq!(runner.dropped_effects(), 0);
    assert_eq!(runner.view().events, n as u64);
}

#[test]
fn full_effect_queue_drops_and_counts() {
    let (chart, rule, dev) = play();
    // A judgment brings three events (the judgment and two gauges), so a
    // queue of two overflows at each one.
    let (_, mut runner) = autoplay_runner(&chart, &rule, &dev, 2);
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    let n = runner.stream().len();
    let dropped = usize::try_from(runner.dropped_effects()).unwrap();
    assert!(dropped > 0);
    assert_eq!(runner.effects().len() + dropped, n);
    // What arrives is the stream's own events, in order.
    for e in runner.effects() {
        assert_eq!(runner.stream()[usize::try_from(e.seq).unwrap()], *e);
    }
    assert!(runner.effects().windows(2).all(|w| w[0].seq < w[1].seq));
    // The view still carries every judgment.
    assert_eq!(runner.view().tally.counts.perfect, 500);
}

#[test]
fn aborted_play_records_its_tail_unevaluated() {
    // Taps on lanes 1, 2 and 3 at 1.0, 1.5 and 2.0 s.
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(1_500 * MS))
        .tap(lane(3), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    // Keyboard 1 lags 40 ms, so the global watermark trails keyboard 0's.
    let dev = device(&[0, 40 * MS]);
    let at = |t: i64, action| ScriptItem {
        at: TimeUs(t),
        action,
    };
    let key = |d: u64, l: u8, value: i32| {
        Action::Packet(Packet {
            device: DeviceId(d),
            control: control(lane(l)),
            value,
            background: false,
        })
    };
    let script = ScriptedInput::new(vec![
        at(i64::MIN, Action::Connect(DeviceId(0), DeviceKind::Keyboard)),
        at(i64::MIN, Action::Connect(DeviceId(1), DeviceKind::Keyboard)),
        at(1_000_000, key(0, 1, 1)),
        at(1_050_000, key(0, 1, 0)),
        at(1_540_000, key(1, 2, 1)),
        at(1_590_000, key(1, 2, 0)),
        // Staged at the abort: popped, but beyond the global watermark.
        at(2_000_000, key(0, 3, 1)),
        // Queued at the abort: pushed in the batch before the focus loss,
        // which publishes nothing.
        at(2_019_500, key(0, 3, 0)),
        at(2_020_000, Action::FocusLoss),
    ]);
    let bound: Vec<ControlId> = (1..=3).map(|l| control(lane(l))).collect();
    let script = script
        .with_recorded(DeviceId(0), bound.clone())
        .with_recorded(DeviceId(1), bound);
    let sources = SourceSet::new(common::sources()).unwrap();
    let config = LogicConfig {
        inputs: 64,
        effects: 64,
    };
    let mut runner = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config,
        START,
        common::audio(&chart, START),
    )
    .unwrap();
    assert_eq!(
        runner.run_to_end(TimeUs(MS)),
        RunOutcome::Aborted(AbortCause::FocusLoss)
    );

    // The last step before the abort ran at 2 019 000: both keyboards
    // published 2 018 999, and keyboard 1's offset gives 1 978 999.
    let record = runner.record();
    assert_eq!(record.finalized, Some(TimeUs(1_978_999)));
    let ev = |t: i64, s: u16, l: u8, value: i32| PhysicalInputEvent {
        received: TimeUs(t),
        source: SourceId(s),
        kind: InputKind::Control {
            control: control(lane(l)),
            value,
        },
    };
    let mut recorded = record.events.clone();
    recorded.sort_by_key(|e| (e.source, e.received));
    assert_eq!(
        recorded,
        vec![
            ev(1_000_000, 0, 1, 1),
            ev(1_050_000, 0, 1, 0),
            ev(2_000_000, 0, 3, 1),
            ev(2_019_500, 0, 3, 0),
            ev(1_540_000, 1, 2, 1),
            ev(1_590_000, 1, 2, 0),
        ]
    );
    // Only the two notes before the finalized watermark are judged; the
    // tail's press is not.
    assert_eq!(
        judgments(runner.stream()),
        vec![
            (1, 0, Judgment::Perfect, Some(0)),
            (2, 0, Judgment::Perfect, Some(0)),
        ]
    );
    assert!(
        runner
            .stream()
            .iter()
            .all(|e| e.origin.is_none_or(|o| o.0 < 2_000_000))
    );
    let batch = eval::evaluate(&chart, &record.eval_input().unwrap(), &rule).unwrap();
    assert!(!batch.completed);
    assert_eq!(batch.events, runner.stream());
}

#[test]
#[should_panic(expected = "positive step")]
fn run_to_end_refuses_a_step_that_does_not_move_the_clock() {
    let (chart, rule, dev) = play();
    let (_, mut runner) = autoplay_runner(&chart, &rule, &dev, 16);
    runner.run_to_end(TimeUs(0));
}

#[test]
fn removing_the_last_device_aborts_at_the_finalized_watermark() {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let at = |t: i64, action| ScriptItem {
        at: TimeUs(t),
        action,
    };
    let key = |value| {
        Action::Packet(Packet {
            device: DeviceId(0),
            control: control(lane(1)),
            value,
            background: false,
        })
    };
    let script = ScriptedInput::new(vec![
        at(i64::MIN, Action::Connect(DeviceId(0), DeviceKind::Keyboard)),
        at(1_000_000, key(1)),
        at(1_050_000, key(0)),
        // After lane 2's miss deadline (1 180 000): the device's final
        // watermark would finalize the miss and the song end.
        at(1_200_000, Action::Disconnect(DeviceId(0))),
    ])
    .with_recorded(DeviceId(0), vec![control(lane(1)), control(lane(2))]);
    let sources = SourceSet::new(common::sources()).unwrap();
    let config = LogicConfig {
        inputs: 64,
        effects: 64,
    };
    let mut runner = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config,
        START,
        common::audio(&chart, START),
    )
    .unwrap();
    // Steps of 100 ms: the last one before the removal publishes 1 099 999.
    assert_eq!(
        runner.run_to_end(TimeUs(100 * MS)),
        RunOutcome::Aborted(AbortCause::LastSourceRemoved)
    );

    assert_eq!(
        judgments(runner.stream()),
        vec![(1, 0, Judgment::Perfect, Some(0))]
    );
    let record = runner.record();
    assert_eq!(record.finalized, Some(TimeUs(1_099_999)));
    assert_eq!(record.events.len(), 3);
    assert_eq!(
        record.events.last().map(|e| (e.received, e.kind)),
        Some((TimeUs(1_200_000), InputKind::Disconnected))
    );
}

#[test]
fn event_at_the_opening_belongs_to_the_play() {
    let opened_at = 1_000_000;
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(opened_at))
        .tap(lane(2), TimeUs(1_200 * MS))
        .build()
        .unwrap();
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let ev = |t: i64, l: u8, value: i32| PhysicalInputEvent {
        received: TimeUs(t),
        source: SourceId(0),
        kind: InputKind::Control {
            control: control(lane(l)),
            value,
        },
    };
    // Lane 3 is held from before the opening; lane 1 is pressed in the
    // first batch after it, in the same microsecond.
    let start = InputStart {
        opened_at: TimeUs(opened_at),
        held: vec![(SourceId(0), control(lane(3)), 1)],
    };
    let events = vec![
        ev(opened_at, 1, 1),
        ev(1_040_000, 1, 0),
        ev(1_100_000, 3, 0),
        ev(1_200_000, 2, 1),
        ev(1_250_000, 2, 0),
    ];
    let original = EvalInput {
        events: events.clone(),
        device: dev.clone(),
        start: start.clone(),
        finalized: None,
        section: None,
    };
    let expected = eval::evaluate(&chart, &original, &rule).unwrap().events;
    assert_eq!(count_judged(&expected, Judgment::Perfect), 2);

    let script = ScriptedInput::from_log(&start, &events, &[DeviceKind::Keyboard]);
    let sources = SourceSet::new(common::sources()).unwrap();
    let config = LogicConfig {
        inputs: 64,
        effects: 64,
    };
    let mut runner = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config,
        TimeUs(opened_at),
        common::audio(&chart, TimeUs(opened_at)),
    )
    .unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    assert_eq!(runner.stream(), expected);
    let record = runner.record();
    assert_eq!(record.start, start);
    assert_eq!(record.events, events);
}

#[test]
fn start_at_the_earliest_time_is_refused() {
    let (chart, rule, dev) = play();
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let config = config(&chart, 16);
    let at_min = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config,
        TimeUs(i64::MIN),
        common::audio(&chart, TimeUs(i64::MIN)),
    );
    assert_eq!(at_min.err(), Some(LogicError::StartAtMinimum));
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let next = TimeUs(i64::MIN + 1);
    let audio = common::audio(&chart, next);
    assert!(Runner::new(&chart, &rule, &dev, sources, script, config, next, audio).is_ok());
}

#[test]
fn device_beyond_the_source_limit_marks_the_record_incomplete() {
    let chart = chart(8, 100 * MS, 50 * MS);
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let events = input::autoplay(&chart, &dev.sources[0]).unwrap();
    // Two keyboards, room for one source.
    let script = ScriptedInput::from_log(
        &InputStart::default(),
        &events,
        &[DeviceKind::Keyboard, DeviceKind::Keyboard],
    );
    let sources = SourceSet::new(SourceConfig {
        max_sources: 1,
        ..common::sources()
    })
    .unwrap();
    let mut runner = Runner::new(
        &chart,
        &rule,
        &dev,
        sources,
        script,
        config(&chart, 64),
        START,
        common::audio(&chart, START),
    )
    .unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    assert_eq!(count_judged(runner.stream(), Judgment::Perfect), 8);
    let incomplete = runner.incomplete();
    assert!(incomplete.source_limit);
    assert!(!incomplete.overflow);
}
