//! An evaluator reset in place (design.md "Play Lifecycle", the start
//! gate) is the evaluator a fresh construction makes from the new start:
//! the same events from the same input, whatever the run before it judged,
//! held or failed, and the reset and the run after it allocate nothing.
//!
//! A counting global allocator counts allocations made on threads that opted
//! in (a thread-local flag); this file holds a single test for that reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, control, device, lane, press, release};
use eval::{
    AxisBinding, ControlId, Evaluator, InputKind, InputStart, JudgeEvent, PhysicalInputEvent,
    ScratchSettings, Section, SourceId,
};

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

/// Taps on lanes 1 to 3 every 100 ms from 1 s, a long note on lane 4 at
/// 1.5 s, a mine on lane 5, and scratch taps at 1.55 and 2.05 s.
fn chart() -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..20 {
        b = b.tap(lane(1 + (i % 3) as u8), TimeUs((1_000 + i * 100) * MS));
    }
    b.tap(lane(0), TimeUs(1_550 * MS))
        .tap(lane(0), TimeUs(2_050 * MS))
        .long(lane(4), TimeUs(1_500 * MS), TimeUs(1_900 * MS))
        .mine(lane(5), TimeUs(1_200 * MS), 10)
        .build()
        .unwrap()
}

/// The scratch's absolute axis, bound to lane 0.
const AXIS: ControlId = ControlId(0x1_0030);

fn axis(received: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(0),
        kind: InputKind::Control {
            control: AXIS,
            value,
        },
    }
}

fn run(ev: &mut Evaluator<'_>, inputs: &[PhysicalInputEvent], to: i64, out: &mut Vec<JudgeEvent>) {
    let mut next = 0;
    let mut w = -1_000 * MS;
    while w <= to {
        while next < inputs.len() && inputs[next].received.0 <= w {
            ev.push_input(inputs[next]).unwrap();
            next += 1;
        }
        ev.advance_to(TimeUs(w), out).unwrap();
        w += MS;
    }
}

#[test]
fn a_reset_evaluator_is_a_fresh_one_and_allocates_nothing() {
    let chart = chart();
    let preset = rules::beatoraja_style();
    let mut dev = device(&[0]);
    dev.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: lane(0),
        scratch: ScratchSettings {
            mode: eval::AxisMode::Absolute { wrap: 256 },
            ..ScratchSettings::DEFAULT
        },
    });
    let section = Section {
        start: TimeUs(1_300 * MS),
        end: TimeUs(2_500 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
    };
    // The cancelled run: presses on the first notes, the long note held,
    // lane 5 held over the mine and early presses that only empty-POOR.
    let mut first: Vec<PhysicalInputEvent> = (0..8)
        .flat_map(|i| {
            let t = (1_000 + i * 100) * MS;
            let n = 1 + (i % 3) as u8;
            [press(t, 0, n), release(t + 20 * MS, 0, n)]
        })
        .collect();
    first.extend([
        press(1_150 * MS, 0, 5),
        press(1_500 * MS, 0, 4),
        press(900 * MS, 0, 6),
    ]);
    // The scratch turned for the 1.55 s note: still pressing, its release
    // pending, when the run is cancelled.
    first.extend((0..4).map(|k| axis((1_540 + k) * MS, 10 + i32::try_from(k).unwrap())));
    first.sort_by_key(|e| e.received);
    // Input queued, not yet reached by the watermark, at the reset.
    let queued = [press(1_700 * MS, 0, 1), axis(1_710 * MS, 40)];
    // The next run starts with lane 2 held and the axis at 100, and plays
    // every note late.
    let start = InputStart {
        opened_at: TimeUs(-500 * MS),
        held: vec![(SourceId(0), control(2), 1), (SourceId(0), AXIS, 100)],
    };
    let mut second: Vec<PhysicalInputEvent> = (0..20)
        .flat_map(|i| {
            let t = (1_030 + i * 100) * MS;
            let n = 1 + (i % 3) as u8;
            [press(t, 0, n), release(t + 20 * MS, 0, n)]
        })
        .collect();
    second.push(press(1_520 * MS, 0, 4));
    second.push(release(1_900 * MS, 0, 4));
    second.extend((0..4).map(|k| axis((2_060 + k) * MS, 101 + i32::try_from(k).unwrap())));
    second.sort_by_key(|e| e.received);
    let end = 5_000;

    for section in [None, Some(&section)] {
        let mut fresh = Evaluator::new(&chart, &preset, &dev, &start, section, 64).unwrap();
        let mut expected = Vec::with_capacity(fresh.max_events(second.len()));
        run(&mut fresh, &second, end * MS, &mut expected);

        let mut ev =
            Evaluator::new(&chart, &preset, &dev, &InputStart::default(), section, 64).unwrap();
        let mut out = Vec::with_capacity(ev.max_events(first.len() + second.len()));
        run(&mut ev, &first, 1_600 * MS, &mut out);
        for e in queued {
            ev.push_input(e).unwrap();
        }
        assert!(!out.is_empty(), "the cancelled run judged");
        TRACK.with(|t| t.set(true));
        // The count works: an allocation that escapes is counted.
        drop(std::hint::black_box(vec![1u8; 8]));
        assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
        out.clear();
        ev.reset(&start);
        assert_eq!((ev.tally().ex_score, ev.tally().max_combo), (0, 0));
        run(&mut ev, &second, end * MS, &mut out);
        TRACK.with(|t| t.set(false));
        assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 0);
        assert_eq!(out, expected);
        assert_eq!(ev.tally(), fresh.tally());
        assert_eq!(ev.gauges(), fresh.gauges());
        assert!(ev.is_complete());
    }
}
