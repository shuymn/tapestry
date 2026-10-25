//! The headless runner over whole plays.

mod common;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{
    MS, autoplay_script, chart, control, count_judged, device, judgments, lane, window_capacity,
};
use eval::{ControlId, DeviceProfile, InputKind, PhysicalInputEvent, SourceId};
use game::{LogicConfig, RunOutcome, Runner};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};
use rules::{Judgment, RulePreset};

/// Recording opens 1 s before chart time 0.
const START: TimeUs = TimeUs(-1_000_000);
const NOTES: i64 = 500;

/// 500 notes 97 ms apart (every 5th a 60 ms long note) and one keyboard
/// with a 3 ms input offset.
fn play() -> (Chart, RulePreset, DeviceProfile) {
    (
        chart(NOTES, 97 * MS, 60 * MS),
        rules::custom(),
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
    )
    .unwrap();
    (events, runner)
}

#[test]
fn runner_autoplay_is_all_perfect() {
    let (chart, rule, dev) = play();
    let (_, mut runner) = autoplay_runner(&chart, &rule, &dev, 4_096);
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);

    assert_eq!(judgments(runner.stream()).len(), 500);
    assert_eq!(count_judged(runner.stream(), Judgment::Perfect), 500);
    assert!(!runner.incomplete());
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
    let batch = eval::evaluate(&chart, &record, &rule).unwrap();
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
    let rule = rules::custom();
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
    let mut runner = Runner::new(&chart, &rule, &dev, sources, script, config, START).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Aborted);

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
    let batch = eval::evaluate(&chart, &record, &rule).unwrap();
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
