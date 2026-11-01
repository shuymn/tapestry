//! After warmup, a frame's app work allocates nothing (design.md "Scope of
//! No Allocation"): [`render::build_frame`], the skin's build into the
//! preallocated [`SpriteBatch`], with effects, and the staging of the batch
//! into instances. `Renderer::build` runs the same function before
//! `Renderer::submit` calls into wgpu, whose own allocations are out of
//! scope, so the test drives it without a GPU. The result screen's page,
//! drawn every frame while it shows, allocates nothing either.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! harness's own threads do not disturb it. This file holds a single test
//! for the same reason. Profiles and the positive control: design.md
//! "Scope of No Allocation".

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{ChartBuilder, TimeUs};
use common::{MS, input, lane, view};
use eval::{JudgeEvent, JudgeEventKind};
use render::{Instance, MAX_SPRITES, SimpleSkin, SpriteBatch};
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

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
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
fn frame_does_not_allocate_after_warmup() {
    const WARMUP: i64 = 10;
    const FRAMES: i64 = 450;
    const FRAME_US: i64 = 16_667;
    // Taps 25 ms apart over every lane, then a long note per key lane, with
    // BPM changes and a STOP, so frames draw notes, long note bodies,
    // text and the effects of the judgments fed to them.
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(2_000 * MS), 180_000)
        .stop(TimeUs(3_000 * MS), TimeUs(250 * MS));
    let mut taps = Vec::new();
    for i in 0..240 {
        let l = u8::try_from(i % 8).unwrap();
        let t = TimeUs(500 * MS + i64::from(i) * 25 * MS);
        b = b.tap(lane(l), t);
        taps.push((l, t));
    }
    for l in 1..8 {
        let start = 6_500 + i64::from(l) * 50;
        b = b.long(lane(l), TimeUs(start * MS), TimeUs((start + 400) * MS));
    }
    let chart = b.build().unwrap();
    let mut skin = SimpleSkin::new();
    skin.load(&chart, None);
    let mut batch = SpriteBatch::with_capacity(MAX_SPRITES);
    let mut staging: Vec<Instance> = Vec::with_capacity(MAX_SPRITES);
    let v = view(42, 1_234);
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(64);
    let (mut next, mut seq, mut drawn, mut held, mut ended) = (0, 0, 0, 0, 0);

    TRACK.with(|t| t.set(true));
    // The count works: an allocation here is counted.
    drop(std::hint::black_box(vec![1u8; 8]));
    assert_eq!(ALLOCS.swap(0, Ordering::Relaxed), 1);
    for frame in 0..WARMUP + FRAMES {
        if frame == WARMUP {
            ALLOCS.store(0, Ordering::Relaxed);
        }
        let now = TimeUs(frame * FRAME_US);
        effects.clear();
        // Judge every tap that has reached the line, and hold each long
        // note from its head, finishing it at its end.
        while next < taps.len() && taps[next].1 <= now && effects.len() < 32 {
            let (l, t) = taps[next];
            let note = chart.lanes[usize::from(l)]
                .iter()
                .position(|n| n.time == t)
                .unwrap();
            effects.push(JudgeEvent {
                seq,
                time: t,
                origin: Some(t),
                kind: JudgeEventKind::Judged {
                    lane: lane(l),
                    note,
                    judgment: Judgment::ALL[next % 5],
                    deviation: Some(TimeUs(0)),
                    release: None,
                    combo: u32::try_from(next).unwrap(),
                },
            });
            next += 1;
            seq += 1;
        }
        for l in 1..8u8 {
            let notes = &chart.lanes[usize::from(l)];
            let (index, ln) = (notes.len() - 1, notes[notes.len() - 1]);
            let end = ln.end();
            // Odd lanes are released 100 ms early, even lanes at the end.
            let release = if l % 2 == 1 {
                TimeUs(end.0 - 100 * MS)
            } else {
                end
            };
            let kinds: &[JudgeEventKind] = if ln.time.0 > now.0 - FRAME_US && ln.time <= now {
                held += 1;
                &[JudgeEventKind::LnHeld {
                    lane: lane(l),
                    note: index,
                    judgment: Judgment::Great,
                    deviation: TimeUs(0),
                }]
            } else if release.0 > now.0 - FRAME_US && release <= now {
                ended += 1;
                &[
                    JudgeEventKind::LnHoldEnded {
                        lane: lane(l),
                        note: index,
                    },
                    JudgeEventKind::Judged {
                        lane: lane(l),
                        note: index,
                        judgment: Judgment::Great,
                        deviation: Some(TimeUs(0)),
                        release: Some(TimeUs(release.0 - end.0)),
                        combo: 1,
                    },
                ]
            } else {
                &[]
            };
            // A release at the end does not end the visual hold early.
            let skip = usize::from(release == end && kinds.len() == 2);
            for &kind in &kinds[skip..] {
                effects.push(JudgeEvent {
                    seq,
                    time: now,
                    origin: None,
                    kind,
                });
                seq += 1;
            }
        }
        // What `Renderer::build` does, before `submit` calls into wgpu.
        let f = input(&v, now, TimeUs(8 * MS), &effects);
        render::build_frame(&mut skin, &f, &mut batch, &mut staging);
        if frame >= WARMUP {
            drawn += staging.len();
        }
    }
    let allocs = ALLOCS.swap(0, Ordering::Relaxed);
    // The result screen's page, its graphs at their fullest, allocates
    // nothing either: built from lines already made and graphs held.
    let mut graphs = game::ResultGraphs::EMPTY;
    graphs.traces.fill(Some(game::GaugeTrace {
        gauge: 0,
        columns: [game::GaugeColumn {
            low: rules::GaugeValue(0),
            high: rules::GaugeValue::FULL,
            last: rules::GaugeValue::FULL,
        }; game::GRAPH_COLUMNS],
        mark: rules::GaugeKind::Survival {
            fails_below: rules::GaugeValue(1),
        },
        failed: Some(100),
    }));
    graphs.emphasized = Some(2);
    graphs.no_input_from = Some(200);
    graphs.timing.bins = [3; game::TIMING_BINS];
    let lines: Vec<String> = TRACK.with(|t| {
        t.set(false);
        let lines = vec!["RESULT".repeat(12); 27];
        t.set(true);
        lines
    });
    for _ in 0..3 {
        batch.clear();
        render::result_screen(&lines, Some(&graphs), &mut batch);
        batch.stage(&mut staging);
        assert!(staging.len() > 27 * 60);
    }
    let result_allocs = ALLOCS.load(Ordering::Relaxed);
    TRACK.with(|t| t.set(false));
    assert_eq!(result_allocs, 0, "the result page allocated");
    assert_eq!(next, taps.len(), "every tap judged");

    assert!(drawn > 0);
    assert_eq!((held, ended), (7, 7), "every long note held and finished");
    assert_eq!(allocs, 0);
}
