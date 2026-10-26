//! After construction, the evaluator allocates nothing while inputs are
//! pushed and the watermark advances.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.
//! It runs in the dev and release profiles with the same bound, and the
//! tracked stretch starts with a positive control (design.md "Scope of No
//! Allocation"). It also prints the evaluator time per input, an indicator only.

#![expect(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "the test prints an indicator timing; purity applies to the library"
)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use bms_model::{ChartBuilder, NoteKind, SoundId, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{
    AxisBinding, ControlId, Evaluator, InputKind, InputStart, JudgeEventKind, PhysicalInputEvent,
    PushError, ScratchSettings, SourceId,
};
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

const AXIS: ControlId = ControlId(0x1_0030);

#[test]
fn advance_does_not_allocate() {
    const NOTES: i64 = 2_000;
    // Notes every 50 ms over lanes 1-7, so one lane's notes are 350 ms
    // apart. Every 4th is a 200 ms LN, every 8th released early (an LN
    // break). Every 10th has a held mine on the scratch lane, held by its
    // button, and every 10th after the 5th one held by the scratch axis,
    // turned for 3 ms and released by its deadline 100 ms later. Some taps
    // get an empty POOR press 250 ms early. Every note has a sound.
    let mut builder = ChartBuilder::new(150_000).sound("k.wav");
    let mut inputs = Vec::new();
    let mut expected_empty_poor = 0;
    let mut axis_value = 0;
    for i in 0..NOTES {
        let n = u8::try_from(i % 7).unwrap() + 1;
        let t = 1_000 * MS + i * 50 * MS;
        if i % 4 == 0 {
            builder = builder.long(lane(n), TimeUs(t), TimeUs(t + 200 * MS));
            inputs.push(press(t, 0, n));
            let held = if i % 8 == 0 { 50 } else { 120 };
            inputs.push(release(t + held * MS, 0, n));
        } else {
            builder = builder.tap(lane(n), TimeUs(t));
            inputs.push(press(t, 0, n));
            inputs.push(release(t + 20 * MS, 0, n));
        }
        // The previous note on this lane is a tap released long before.
        if i % 6 == 3 && i >= 7 && (i - 7) % 4 != 0 {
            inputs.push(press(t - 250 * MS, 0, n));
            inputs.push(release(t - 240 * MS, 0, n));
            expected_empty_poor += 1;
        }
        if i % 5 == 0 {
            builder = builder.mine(lane(0), TimeUs(t + 10 * MS), 2);
        }
        if i % 10 == 0 {
            inputs.push(press(t, 0, 0));
            inputs.push(release(t + 30 * MS, 0, 0));
        }
        if i % 10 == 5 {
            for k in 0..3 {
                axis_value = (axis_value + 1) % 256;
                inputs.push(PhysicalInputEvent {
                    received: TimeUs(t + k * MS),
                    source: SourceId(0),
                    kind: InputKind::Control {
                        control: AXIS,
                        value: axis_value,
                    },
                });
            }
        }
    }
    inputs.sort_by_key(|e| e.received);
    let mut chart = builder.build().unwrap();
    for note in chart.lanes.iter_mut().flatten() {
        if !matches!(note.kind, NoteKind::Mine { .. }) {
            note.sound = Some(SoundId(0));
        }
    }
    let preset = rules::custom();
    let mut dev = device(&[0]);
    dev.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: lane(0),
        scratch: ScratchSettings {
            mode: eval::AxisMode::Absolute { wrap: 256 },
            ..ScratchSettings::DEFAULT
        },
    });
    let end = 1_000 * MS + NOTES * 50 * MS + 1_000 * MS;
    let mut ev = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), 64).unwrap();
    let bound = ev.max_events(inputs.len());
    let mut out = Vec::with_capacity(bound);
    let capacity = out.capacity();
    // A second evaluator whose ring a burst fills to capacity.
    const BURST: usize = 4;
    let mut burst = Evaluator::new(&chart, &preset, &dev, &InputStart::default(), BURST).unwrap();
    let mut burst_out = Vec::with_capacity(burst.max_events(BURST + 1));

    TRACK.with(|t| t.set(true));
    // The count works: an allocation that escapes is counted in any profile.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    let started = Instant::now();
    let mut next = 0;
    let mut w = 0;
    while w <= end {
        // What the merge would hand over at this watermark.
        while next < inputs.len() && inputs[next].received.0 <= w {
            ev.push_input(inputs[next]).unwrap();
            next += 1;
        }
        ev.advance_to(TimeUs(w), &mut out).unwrap();
        w += MS;
    }
    let elapsed = started.elapsed();
    // Fill the ring to capacity: one more is refused, not allocated.
    for e in &inputs[..BURST] {
        burst.push_input(*e).unwrap();
    }
    let refused = burst.push_input(inputs[BURST]);
    burst
        .advance_to(TimeUs(inputs[BURST].received.0 - 1), &mut burst_out)
        .unwrap();
    let after_drain = burst.push_input(inputs[BURST]);
    TRACK.with(|t| t.set(false));

    let allocations = ALLOCS.load(Ordering::Relaxed);
    let count = |f: fn(&JudgeEventKind) -> bool| out.iter().filter(|e| f(&e.kind)).count();
    assert_eq!(
        count(|k| matches!(
            k,
            JudgeEventKind::Judged {
                judgment: Judgment::Perfect,
                ..
            }
        )),
        2_000
    );
    assert_eq!(ev.tally().ln_breaks, 250);
    assert_eq!(count(|k| matches!(k, JudgeEventKind::MineHit { .. })), 400);
    assert!(expected_empty_poor > 0);
    assert_eq!(
        count(|k| matches!(k, JudgeEventKind::EmptyPoor { .. })),
        expected_empty_poor
    );
    assert_eq!(
        count(|k| matches!(k, JudgeEventKind::PlaySound { .. })),
        2_000 + expected_empty_poor
    );
    assert!(matches!(
        out.last().map(|e| e.kind),
        Some(JudgeEventKind::SongEnd)
    ));
    assert!(out.len() <= bound);
    assert_eq!(out.capacity(), capacity);
    assert_eq!(refused, Err(PushError::Full));
    assert_eq!(after_drain, Ok(()));
    assert!(!burst_out.is_empty());
    assert_eq!(allocations, 0);
    let per_input = elapsed.as_nanos() / u128::try_from(inputs.len()).unwrap();
    println!(
        "indicator: {} inputs and {} watermark steps in {elapsed:?}, {per_input} ns per input (steps included)",
        inputs.len(),
        end / MS + 1
    );
}
