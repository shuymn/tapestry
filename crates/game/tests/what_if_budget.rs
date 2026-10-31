//! The result screen's evaluation budget (design.md "Extension
//! Boundaries"): what a what-if holds at its peak stays within
//! `what_if_bytes`, the reservation a play under the viewed preset is
//! admitted with, on a chart of taps, long notes and mines, with a preset
//! of more gauges than the played one.
//!
//! A global allocator keeps the bytes live on threads that opted in and
//! their peak; this file holds a single test.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicIsize, Ordering};

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, device, lane};
use game::{ReEvaluate, ResultContext, what_if_bytes};
use replay::{
    BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete, PlayRecord,
    PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions, WakePolicy,
};
use rules::Implementation;

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

#[test]
fn what_if_peak_stays_within_its_budget() {
    // 400 taps, 60 long notes and 40 mines.
    let mut b = ChartBuilder::new(150_000);
    for i in 0..400 {
        let l = lane(u8::try_from(i % 6).unwrap() + 1);
        b = b.tap(l, TimeUs(1_000 * MS + i * 37 * MS));
    }
    for i in 0..60 {
        let t = 2_000 * MS + i * 250 * MS;
        b = b.long(lane(0), TimeUs(t), TimeUs(t + 100 * MS));
    }
    for i in 0..40 {
        b = b.mine(lane(7), TimeUs(1_017 * MS + i * 371 * MS), 3);
    }
    let chart = b.build().unwrap();
    let played = rules::tapestry();
    let dev = device(&[0]);
    let source = &dev.sources[0];
    let events = input::autoplay(&chart, source).unwrap();
    let finalized =
        eval::Evaluator::new(&chart, &played, &dev, &eval::InputStart::default(), None, 0)
            .unwrap()
            .final_deadline();
    let record = PlayRecord {
        versions: Versions {
            evaluator: Implementation::STANDARD_VERSION,
            app: "0.0.0-test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord::default(),
        preset: PresetRecord::of(&played),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: source.clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: bms_model::Speed::NORMAL,
        start: eval::InputStart {
            opened_at: TimeUs(-1_000 * MS),
            held: Vec::new(),
        },
        events,
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized,
            incomplete: Incomplete::default(),
        },
    };
    // The counter sees allocations: a positive control.
    let peak_of = |f: &mut dyn FnMut()| {
        LIVE.store(0, Ordering::Relaxed);
        PEAK.store(0, Ordering::Relaxed);
        TRACK.with(|t| t.set(true));
        f();
        TRACK.with(|t| t.set(false));
        usize::try_from(PEAK.load(Ordering::Relaxed)).unwrap()
    };
    let control = peak_of(&mut || drop(std::hint::black_box(Vec::<u8>::with_capacity(4_096))));
    assert!(control >= 4_096, "{control}");
    // 1, 8 and 64 gauges where the play had two: every event carries one
    // change per gauge, which the play's own admission never sized; each
    // with the input at and past the steps where the gauge histories and
    // the event stream double, and in full.
    let all = record.events.len();
    let normal = played.gauges[0].clone();
    for gauges in [1, 8, 64] {
        let mut what_if = rules::tapestry();
        what_if.id = "many".to_owned();
        what_if.gauges = (0..gauges)
            .map(|i| rules::GaugeRule {
                id: format!("g{i}"),
                ..normal.clone()
            })
            .collect();
        for inputs in [64, 65, 128, 129, 256, 257, all] {
            let mut record = record.clone();
            record.events.truncate(inputs);
            let budget = what_if_bytes(&chart, &what_if, &record).unwrap();
            let ctx = ResultContext {
                record: &record,
                chart: &chart,
                preset: &played,
                live: &[],
            };
            let mut evaluated = None;
            let peak = peak_of(&mut || evaluated = Some(ReEvaluate::under(&ctx, &what_if)));
            let evaluated = evaluated.unwrap().unwrap();
            assert!(!evaluated.as_played);
            assert_eq!(evaluated.evaluation.gauges.len(), gauges);
            // What the evaluation still holds was counted.
            let stream = evaluated.evaluation.events.capacity() * size_of::<eval::JudgeEvent>();
            assert!(
                peak >= stream,
                "{gauges} gauges, {inputs} inputs: {peak} < {stream}"
            );
            assert!(
                peak <= budget,
                "{gauges} gauges, {inputs} inputs: peak {peak} over the budget {budget}"
            );
            if inputs == all {
                println!("indicator: {gauges} gauges, peak {peak} bytes, budget {budget} bytes");
            }
        }
        // The played preset's own budget would not have covered more gauges.
        if gauges > played.gauges.len() {
            let own = what_if_bytes(&chart, &played, &record).unwrap();
            assert!(own < what_if_bytes(&chart, &what_if, &record).unwrap());
        }
    }
}
