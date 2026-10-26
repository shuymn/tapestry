//! After construction, a whole play allocates nothing: scripted input,
//! publication, the logic step, the audio step and draining the render side,
//! including a play whose output is mostly input-driven and an overflow over
//! several sources.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! Profiles and the positive control: design.md "Scope of No Allocation".

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{ChartBuilder, SoundId, TimeUs};
use common::{MS, count_judged, device, lane, press, window_capacity};
use eval::{InputKind, InputStart, JudgeEventKind, PhysicalInputEvent, SourceId};
use game::{AbortCause, Logic, LogicConfig, RunOutcome, Runner};
use input::{DeviceKind, ScriptedInput, SourceSet, autoplay, source_queue};
use rules::Judgment;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn note() {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: delegates to `System`; only adds a counter, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn logic_does_not_allocate_during_play() {
    const START: TimeUs = TimeUs(-1_000_000);
    const MASH: i64 = 2_000;
    // 100 taps 61 ms apart over the keys, and a sounded scratch note at
    // 10 s. Before it, the scratch is pressed 2 000 times between 299 and
    // 181 ms ahead, just outside its BAD window: each press is a sound, an
    // empty POOR and two gauge events, so the output far exceeds what the
    // notes alone can emit.
    let mut b = ChartBuilder::new(150_000).sound("k.wav");
    for i in 0..100 {
        let l = lane(u8::try_from(i % 7).unwrap() + 1);
        b = b.tap(l, TimeUs(1_000 * MS + i * 61 * MS));
    }
    let mut chart = b.tap(lane(0), TimeUs(10_000 * MS)).build().unwrap();
    for note in chart.lanes.iter_mut().flatten() {
        note.sound = Some(SoundId(0));
    }
    let rule = rules::custom();
    let dev = device(&[2 * MS]);
    let mut events = autoplay(&chart, &dev.sources[0]).unwrap();
    for k in 0..MASH {
        // Received times include the 2 ms offset.
        let t = 9_701_000 + 2 * MS + k * 59;
        events.push(press(t, 0, lane(0)));
        events.push(PhysicalInputEvent {
            kind: InputKind::Control {
                control: common::control(lane(0)),
                value: 0,
            },
            ..press(t + 29, 0, lane(0))
        });
    }
    events.sort_by_key(|e| e.received);
    let script = ScriptedInput::from_log(&InputStart::default(), &events, &[DeviceKind::Keyboard]);
    let config = LogicConfig {
        inputs: window_capacity(START, TimeUs(11_000 * MS), 8),
        effects: 4_096,
    };
    let sources = SourceSet::new(common::sources()).unwrap();
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

    // Three sources each with two inputs inside the watermark, against a
    // shared budget of four (one control event and three disconnect
    // allowances); each source's own share (two) holds both of its.
    let taps = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let three = device(&[0, 0, 0]);
    let mut writers = Vec::new();
    let mut readers = Vec::new();
    for s in 0..3 {
        let (mut w, r) = source_queue(4);
        for value in [1, 0] {
            w.push(PhysicalInputEvent {
                source: SourceId(s),
                kind: InputKind::Control {
                    control: common::control(lane(1)),
                    value,
                },
                ..press(1_000_000, 0, lane(1))
            })
            .unwrap();
        }
        w.publish(TimeUs(1_000_000));
        writers.push(w);
        readers.push(r);
    }
    let budget = LogicConfig {
        inputs: 1,
        effects: 16,
    };
    let (mut overflowing, _side) = Logic::new(
        &taps,
        &rule,
        &three,
        &InputStart::default(),
        readers,
        budget,
        Vec::new(),
    )
    .unwrap();

    TRACK.with(|t| t.set(true));
    // The count works: an allocation that escapes is counted in any profile.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    let outcome = runner.run_to_end(TimeUs(MS));
    let overflow = overflowing.advance_to(TimeUs(1_000_500));
    TRACK.with(|t| t.set(false));

    assert_eq!(outcome, RunOutcome::Complete);
    assert_eq!(runner.record().events.len(), 202 + 2 * 2_000);
    let stream = runner.stream();
    assert_eq!(count_judged(stream, Judgment::Perfect), 101);
    let empty = stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::EmptyPoor { .. }))
        .count();
    assert_eq!(empty, 2_000);
    assert_eq!(runner.effects().len(), stream.len());
    assert_eq!(overflow.aborted, Some(AbortCause::Overflow));
    assert_eq!(overflowing.record().events.len(), 4);
    // The audio step ran inside the tracked region: every key sound became
    // a start (the schedule log keeps none, so each start counts as
    // dropped from it), and the device was fed through the whole play.
    let sounds = stream
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::PlaySound { .. }))
        .count();
    assert_eq!(sounds, 101 + 2_000);
    let engine = runner.audio().engine();
    assert_eq!(engine.scheduled_dropped(), sounds as u64);
    assert_eq!(runner.dropped_sounds(), 0);
    assert!(engine.written() >= 48 * 11_000, "{}", engine.written());
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
