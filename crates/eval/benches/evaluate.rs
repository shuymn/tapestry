//! CPU benchmarks of the evaluator (design.md "Measurement"): batch evaluation of
//! generated 2,000- and 10,000-note charts with dense input, and the same
//! play advanced live in 1 ms steps. Charts and input are built here.

use std::hint::black_box;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use criterion::{Criterion, criterion_group, criterion_main};
use eval::{
    Binding, ControlId, DeviceProfile, EvalInput, Evaluator, InputKind, InputStart,
    PhysicalInputEvent, SourceId, SourceProfile, evaluate,
};
use rules::RulePreset;

const MS: i64 = 1_000;
/// Time between notes: 20 ms over eight lanes is a dense 50 notes a second.
const SPACING: i64 = 20 * MS;
const FIRST: i64 = 1_000 * MS;

fn lane(n: u8) -> Lane {
    Lane::new(n).expect("lane in range")
}

fn control(n: u8) -> ControlId {
    ControlId(u32::from(n))
}

/// One source whose control `n` is bound to lane `n`.
fn device() -> DeviceProfile {
    DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: control(u8::try_from(l.index()).expect("lane index fits")),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}

fn control_event(received: i64, n: u8, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(0),
        kind: InputKind::Control {
            control: control(n),
            value,
        },
    }
}

/// `notes` notes round-robin over the scratch and the seven keys, every
/// fifth a 100 ms long note, and the input that plays each of them: a press
/// 5 ms late and a release 10 ms later (70 ms later on a long note, 30 ms
/// before its end), in receive order.
fn play(notes: i64) -> (Chart, Vec<PhysicalInputEvent>) {
    let mut builder = ChartBuilder::new(150_000).sound("k.wav");
    let mut events = Vec::new();
    for i in 0..notes {
        let n = u8::try_from(i % 8).expect("lane index fits");
        let t = FIRST + i * SPACING;
        let hold = if i % 5 == 0 {
            builder = builder.long(lane(n), TimeUs(t), TimeUs(t + 100 * MS));
            70 * MS
        } else {
            builder = builder.tap(lane(n), TimeUs(t));
            10 * MS
        };
        events.push(control_event(t + 5 * MS, n, 1));
        events.push(control_event(t + 5 * MS + hold, n, 0));
    }
    events.sort_by_key(|e| e.received);
    (builder.build().expect("valid chart"), events)
}

fn bench_batch(c: &mut Criterion) {
    let preset = rules::custom();
    let mut group = c.benchmark_group("batch");
    for notes in [2_000, 10_000] {
        let (chart, events) = play(notes);
        let input = EvalInput {
            events,
            device: device(),
            ..EvalInput::default()
        };
        let played = evaluate(&chart, &input, &preset).expect("evaluates");
        assert!(played.completed);
        assert!(played.tally.max_combo > 1_000, "the input hits the notes");
        group.bench_function(format!("{notes}_notes"), |b| {
            b.iter(|| evaluate(black_box(&chart), black_box(&input), &preset).expect("evaluates"));
        });
    }
    group.finish();
}

/// The logic thread's loop: push what has been received by each 1 ms
/// watermark, then advance to it.
fn live(chart: &Chart, preset: &RulePreset, device: &DeviceProfile, events: &[PhysicalInputEvent]) {
    let mut ev = Evaluator::new(
        chart,
        preset,
        device,
        &InputStart::default(),
        None,
        events.len(),
    )
    .expect("valid evaluator");
    let mut out = Vec::with_capacity(ev.max_events(events.len()));
    let end = ev.final_deadline().0;
    let (mut next, mut w) = (0, 0);
    while w <= end {
        while let Some(e) = events.get(next).filter(|e| e.received.0 <= w) {
            ev.push_input(*e).expect("room in the ring");
            next += 1;
        }
        ev.advance_to(TimeUs(w), &mut out).expect("forward");
        w += MS;
    }
    black_box(&out);
}

fn bench_live(c: &mut Criterion) {
    let preset = rules::custom();
    let device = device();
    let mut group = c.benchmark_group("live_1ms_steps");
    group.sample_size(20);
    for notes in [2_000, 10_000] {
        let (chart, events) = play(notes);
        group.bench_function(format!("{notes}_notes"), |b| {
            b.iter(|| live(&chart, &preset, &device, &events));
        });
    }
    group.finish();
}

/// A dense window (design.md "Evaluation Stages": dense windows are
/// benchmarked): `notes` taps on key 1 every 5 ms, so about 100 notes lie
/// inside one press's BAD window and empty POOR range, each pressed 30 ms
/// late, under each assignment policy (custom's nearest first and
/// beatoraja-style's combo priority).
fn dense(notes: i64) -> (Chart, Vec<PhysicalInputEvent>) {
    let mut builder = ChartBuilder::new(150_000);
    let mut events = Vec::new();
    for i in 0..notes {
        let t = FIRST + i * 5 * MS;
        builder = builder.tap(lane(1), TimeUs(t));
        events.push(control_event(t + 30 * MS, 1, 1));
        events.push(control_event(t + 30 * MS + 2 * MS, 1, 0));
    }
    events.sort_by_key(|e| e.received);
    (builder.build().expect("valid chart"), events)
}

fn bench_dense_window(c: &mut Criterion) {
    let mut group = c.benchmark_group("dense_window");
    let (chart, events) = dense(2_000);
    let input = EvalInput {
        events,
        device: device(),
        ..EvalInput::default()
    };
    for preset in [
        rules::custom(),
        rules::beatoraja_style(),
        rules::lr2_style(),
    ] {
        let played = evaluate(&chart, &input, &preset).expect("evaluates");
        assert!(played.completed);
        group.bench_function(format!("2000_notes_{}", preset.id), |b| {
            b.iter(|| evaluate(black_box(&chart), black_box(&input), &preset).expect("evaluates"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_batch, bench_live, bench_dense_window);
criterion_main!(benches);
