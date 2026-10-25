//! Completion, aborts, and live evaluation against batch evaluation.

mod common;

use std::collections::VecDeque;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, disconnect, lane, press, release};
use eval::{
    ControlId, EvalInput, Evaluator, InputKind, InputStart, JudgeEvent, JudgeEventKind, Published,
    SourceId, SourceState, Staged, evaluate, merge, watermark,
};
use rules::RulePreset;

/// Runs a play live, the way the logic thread does: at each step every open
/// source has published everything received up to `now`; the consumer
/// stages it, takes the watermark, merges up to it, and advances. A
/// disconnect closes its source at its receive time; a closed source is
/// retired once drained. Returns the events and the last watermark.
fn live(
    chart: &Chart,
    rule: &RulePreset,
    input: &EvalInput,
    steps: impl IntoIterator<Item = i64>,
) -> (Vec<JudgeEvent>, TimeUs) {
    let n = input.device.sources.len();
    let mut queues: Vec<VecDeque<_>> = vec![VecDeque::new(); n];
    for e in &input.events {
        queues[e.source.index()].push_back(*e);
    }
    let mut staged: Vec<Staged> = (0..n)
        .map(|i| Staged::with_capacity(SourceId(u16::try_from(i).unwrap()), 64))
        .collect();
    let mut published = vec![Published::open(TimeUs(i64::MIN)); n];
    let mut ev = Evaluator::new(
        chart,
        rule,
        &input.device,
        &input.start,
        input.events.len().max(1),
    )
    .unwrap();
    let mut out = Vec::new();
    let mut merged = Vec::new();
    let mut last = TimeUs(i64::MIN);
    for now in steps {
        for i in 0..n {
            if published[i].state != SourceState::Open {
                continue;
            }
            published[i] = Published::open(TimeUs(now));
            while let Some(e) = queues[i].front().copied().filter(|e| e.received.0 <= now) {
                queues[i].pop_front();
                staged[i].push(e).unwrap();
                if e.kind == InputKind::Disconnected {
                    published[i] = Published::closed(e.received);
                    break;
                }
            }
        }
        let Some(w) = watermark(&published, &input.device) else {
            break;
        };
        merge(&mut staged, &input.device, w, &mut merged);
        for e in merged.drain(..) {
            ev.push_input(e).unwrap();
        }
        ev.advance_to(w, &mut out).unwrap();
        last = w;
        for i in 0..n {
            let nothing_queued = queues[i].is_empty();
            published[i].retire_if_drained(nothing_queued, &staged[i]);
        }
    }
    (out, last)
}

fn judged(events: &[JudgeEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }))
        .count()
}

/// A chart and a log across 3 sources with different offsets: same-time
/// events from two sources, a control held at start, a long note, a mine,
/// and a disconnect while a lane is held.
fn scenario() -> (Chart, EvalInput) {
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(200 * MS))
        .tap(lane(1), TimeUs(400 * MS))
        .tap(lane(1), TimeUs(600 * MS))
        .long(lane(2), TimeUs(300 * MS), TimeUs(700 * MS))
        .tap(lane(3), TimeUs(500 * MS))
        .mine(lane(4), TimeUs(650 * MS), 10)
        .tap(lane(5), TimeUs(800 * MS))
        .tap(lane(5), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    // Offsets: source 0 none, source 1 is 3 ms late, source 2 2 ms early.
    let dev = device(&[0, 3 * MS, -2 * MS]);
    let events = vec![
        // Source 0 holds lane 3 at the start.
        release(100 * MS, 0, 3),
        press(200 * MS, 0, 1),
        release(210 * MS, 0, 1),
        press(303 * MS, 1, 2), // evaluation 300 ms
        press(403 * MS, 1, 1), // evaluation 400 ms
        release(413 * MS, 1, 1),
        press(500 * MS, 0, 3),
        release(520 * MS, 0, 3),
        // Two sources at evaluation time 600 ms.
        press(598 * MS, 2, 4),
        press(600 * MS, 0, 1),
        release(610 * MS, 0, 1),
        release(553 * MS, 1, 2), // evaluation 550 ms, before 700 - 100: LN break
        disconnect(678 * MS, 2), // releases lane 4 after the mine at 650 ms
        press(800 * MS, 0, 5),
        release(810 * MS, 0, 5),
        // Both press lane 5 at evaluation time 1000 ms; source 0 first.
        press(1_000 * MS, 0, 5),
        press(1_003 * MS, 1, 5),
        release(1_010 * MS, 0, 5),
        release(1_013 * MS, 1, 5),
    ];
    let start = InputStart {
        opened_at: TimeUs(0),
        held: vec![(SourceId(0), ControlId(3), 1)],
    };
    (
        chart,
        EvalInput {
            events,
            device: dev,
            start,
            finalized: None,
        },
    )
}

/// SplitMix64, for seeded step sizes.
fn split_mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

const END: i64 = 1_300 * MS;

#[test]
fn incremental_matches_batch_for_every_watermark_step() {
    let (chart, input) = scenario();
    let rule = rules::custom();
    let batch = evaluate(&chart, &input, &rule).unwrap();
    // Counted by hand: lane 1's three taps, the LN head, lane 3's tap, and
    // lane 5's two taps.
    assert_eq!(judged(&batch.events), 7);
    assert_eq!(batch.tally.ln_breaks, 1);
    assert!(
        batch
            .events
            .iter()
            .any(|e| matches!(e.kind, JudgeEventKind::MineHit { .. }))
    );
    assert!(batch.completed);

    let mut runs: Vec<(&str, Vec<i64>)> = vec![
        ("1 us", (0..=END).collect()),
        ("1 ms", (0..=END).step_by(1_000).collect()),
        ("7 ms", (0..=END + 7 * MS).step_by(7_000).collect()),
        // One jump to the end; the second wake at the same time follows
        // the removed source's retirement.
        ("jump", vec![END, END]),
    ];
    for seed in [1_u64, 42, 2024] {
        let mut state = seed;
        let mut steps = Vec::new();
        let mut now = 0;
        while now < END {
            now += i64::try_from(split_mix(&mut state) % 20_000).unwrap() + 1;
            steps.push(now);
        }
        runs.push(("random", steps));
    }
    for (name, steps) in runs {
        let (events, _) = live(&chart, &rule, &input, steps);
        assert_eq!(judged(&events), 7, "{name}");
        assert_eq!(events, batch.events, "{name}");
    }
}

#[test]
fn aborted_prefix_matches_live() {
    let (chart, input) = scenario();
    let rule = rules::custom();
    // Abort after the step at 650 ms; later events stay in the record.
    let (events, finalized) = live(&chart, &rule, &input, (0..=650 * MS).step_by(1_000));
    assert_eq!(finalized, TimeUs(647 * MS));
    assert_eq!(judged(&events), 5);
    let aborted = EvalInput {
        finalized: Some(finalized),
        ..input.clone()
    };
    let prefix = evaluate(&chart, &aborted, &rule).unwrap();
    assert_eq!(prefix.events, events);
    assert!(!prefix.completed);
    assert!(prefix.gauges.iter().all(|g| !g.cleared));
    // The whole chart goes on past the abort.
    let full = evaluate(&chart, &input, &rule).unwrap();
    assert_eq!(full.events[..events.len()], events[..]);
    assert_eq!(judged(&full.events) - judged(&events), 2);
}

#[test]
fn completion_waits_for_watermark_past_final_deadline() {
    // One tap at 1000 ms misses at 1180 ms, the final deadline. The only
    // source is 5 ms late, so its receive time must reach 1185 ms.
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let rule = rules::custom();
    let dev = device(&[5 * MS]);
    let mut ev = Evaluator::new(&chart, &rule, &dev, &InputStart::default(), 4).unwrap();
    assert_eq!(ev.final_deadline(), TimeUs(1_180 * MS));
    let mut out = Vec::new();
    for received in [1_180 * MS, 1_185 * MS - 1] {
        let w = watermark(&[Published::open(TimeUs(received))], &dev).unwrap();
        ev.advance_to(w, &mut out).unwrap();
        assert!(
            out.is_empty(),
            "nothing before the watermark passes 1180 ms"
        );
        assert!(!ev.is_complete());
    }
    let w = watermark(&[Published::open(TimeUs(1_185 * MS))], &dev).unwrap();
    ev.advance_to(w, &mut out).unwrap();
    let ends: Vec<_> = out
        .iter()
        .filter(|e| e.kind == JudgeEventKind::SongEnd)
        .map(|e| e.time)
        .collect();
    assert_eq!(ends, [TimeUs(1_180 * MS)]);
    assert_eq!(out.last().map(|e| e.kind), Some(JudgeEventKind::SongEnd));
    assert!(ev.is_complete());
    // Once only.
    let before = out.len();
    ev.advance_to(TimeUs(5_000 * MS), &mut out).unwrap();
    assert_eq!(out.len(), before);
}

#[test]
fn same_input_same_result_twice() {
    let (chart, input) = scenario();
    let rule = rules::custom();
    let a = evaluate(&chart, &input, &rule).unwrap();
    let b = evaluate(&chart, &input, &rule).unwrap();
    assert!(!a.events.is_empty());
    assert_eq!(a, b);
}
