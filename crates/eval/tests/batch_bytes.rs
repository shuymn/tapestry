//! `Evaluator::batch_bytes` bounds what a batch evaluation holds at its
//! peak (design.md "Recording Capacity"): measured over the record's copy
//! and `evaluate`, on charts of taps, long notes and mines, with one gauge
//! and with many, at the sizes where the gauge histories grow, and with
//! several sources.
//!
//! A global allocator keeps the bytes live on threads that opted in and
//! their peak; this file holds a single test for that reason.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, device, lane, press, release};
use eval::{
    ControlId, DeviceProfile, EvalInput, Evaluator, InputStart, PhysicalInputEvent, SourceId,
};
use rules::RulePreset;

struct Peak;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn add(n: isize) {
    if TRACK.try_with(Cell::get).unwrap_or(false) {
        let now = LIVE.fetch_add(n, Ordering::Relaxed) + n;
        PEAK.fetch_max(now, Ordering::Relaxed);
    }
}

fn size(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

// SAFETY: delegates to `System`; only adds to counters, which never
// allocate.
unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(size(layout.size()));
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(-size(layout.size()));
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(size(new_size) - size(layout.size()));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Peak = Peak;

/// The peak bytes `f` holds beyond what was live when it started.
fn peak_of(f: &mut dyn FnMut()) -> usize {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    f();
    TRACK.with(|t| t.set(false));
    usize::try_from(PEAK.load(Ordering::Relaxed)).unwrap()
}

/// `notes` notes every 40 ms on lanes 1-6, every third a 100 ms long note,
/// with a mine on lane 7 every fifth.
fn chart(notes: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..notes {
        let t = 1_000 * MS + i * 40 * MS;
        let l = lane(u8::try_from(i % 6).unwrap() + 1);
        b = if i % 3 == 0 {
            b.long(l, TimeUs(t), TimeUs(t + 100 * MS))
        } else {
            b.tap(l, TimeUs(t))
        };
        if i % 5 == 0 {
            b = b.mine(lane(7), TimeUs(t + 7 * MS), 2);
        }
    }
    b.build().unwrap()
}

/// Each note pressed and released by `sources` in turn, with an empty
/// POOR press before every fourth, and lane 7 held over each mine.
fn inputs(chart: &Chart, sources: u16) -> Vec<PhysicalInputEvent> {
    let mut out = Vec::new();
    for (k, l) in chart.lanes.iter().enumerate() {
        let n = u8::try_from(k).unwrap();
        for (i, note) in l.iter().enumerate() {
            let s = u16::try_from(i).unwrap() % sources;
            let t = note.time.0;
            match note.kind {
                bms_model::NoteKind::Mine { .. } => {
                    out.push(press(t - MS, s, n));
                    out.push(release(t + MS, s, n));
                }
                _ => {
                    if i % 4 == 0 {
                        out.push(press(t - 300 * MS, s, n));
                        out.push(release(t - 290 * MS, s, n));
                    }
                    out.push(press(t, s, n));
                    out.push(release(note.end().0 + 5 * MS, s, n));
                }
            }
        }
    }
    out.sort_by_key(|e| e.received);
    out
}

/// A preset of `gauges` gauges, copies of the tapestry preset's first,
/// with distinct IDs and lamp ranks.
fn preset(gauges: u16) -> RulePreset {
    let mut p = rules::tapestry();
    let first = p.gauges[0].clone();
    p.gauges = (0..gauges)
        .map(|i| {
            let mut g = first.clone();
            g.id = format!("g{i}");
            g.lamp = i;
            g
        })
        .collect();
    p
}

/// The peak of copying `events` and evaluating them, against the
/// estimate.
fn check(
    chart: &Chart,
    preset: &RulePreset,
    device: &DeviceProfile,
    events: &[PhysicalInputEvent],
) {
    check_held(chart, preset, device, events, 0);
}

/// [`check`], with a held-input snapshot of `held` controls of source 0,
/// none of them bound, in the record and copied with it.
fn check_held(
    chart: &Chart,
    preset: &RulePreset,
    device: &DeviceProfile,
    events: &[PhysicalInputEvent],
    held: u32,
) {
    let start = InputStart {
        opened_at: TimeUs(-1_000 * MS),
        held: (0..held)
            .map(|c| (SourceId(0), ControlId(10_000 + c), 1))
            .collect(),
    };
    let estimate = Evaluator::new(chart, preset, device, &start, None, 0)
        .unwrap()
        .batch_bytes(events.len(), start.held.len());
    let snapshot = start.held.capacity() * eval::HELD_BYTES;
    let record = events.to_vec();
    let peak = peak_of(&mut || {
        let input = EvalInput {
            events: record.clone(),
            device: device.clone(),
            start: start.clone(),
            finalized: None,
            section: None,
        };
        let evaluated = eval::evaluate(chart, &input, preset).unwrap();
        drop(std::hint::black_box(evaluated));
    }) + record.capacity() * size_of::<PhysicalInputEvent>()
        + snapshot;
    assert!(
        peak <= estimate,
        "{peak} > {estimate}: {} gauges, {} inputs, {} sources",
        preset.gauges.len(),
        events.len(),
        device.sources.len()
    );
}

#[test]
fn batch_bytes_covers_evaluate() {
    // The counter sees allocations: a positive control.
    let control = peak_of(&mut || drop(std::hint::black_box(Vec::<u8>::with_capacity(4_096))));
    assert!(control >= 4_096, "{control}");
    let one = device(&[0]);
    // A mixed chart under one gauge, two, and many.
    let mixed = chart(300);
    let played = inputs(&mixed, 1);
    for gauges in [1, 2, 64] {
        check(&mixed, &preset(gauges), &one, &played);
    }
    // Many gauges and one judgment: each history holds one point in a
    // vector of the least capacity.
    let single = chart(1);
    check(&single, &preset(1_000), &one, &[]);
    check(&single, &preset(1_000), &one, &inputs(&single, 1));
    // At and past the sizes where a history's vector doubles.
    for notes in [3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33] {
        let c = chart(notes);
        check(&c, &preset(3), &one, &inputs(&c, 1));
        check(&c, &preset(3), &one, &[]);
    }
    // Several sources, each with its own staging.
    for sources in [2_u16, 4, 16] {
        let offsets: Vec<i64> = (0..sources).map(|s| i64::from(s) * 1_000).collect();
        check(
            &mixed,
            &preset(2),
            &device(&offsets),
            &inputs(&mixed, sources),
        );
    }
    // A held-input snapshot, one note and no input: the snapshot's copies
    // are most of the peak.
    for held in [1, 1_000, 100_000] {
        check_held(&single, &preset(1), &one, &[], held);
    }
}
