//! A frame's app work allocates nothing, from the first frame on (design.md
//! "Scope of No Allocation"): [`render::build_frame`], the skin's build into
//! the preallocated [`SpriteBatch`], with effects, and the staging of the
//! batch into [`Instance`]s. `Renderer::build` runs the same function before
//! `Renderer::submit` calls into wgpu, whose own allocations are out of
//! scope, so the test drives it without a GPU. A batch that fills up counts
//! what it refuses without allocating, and the result screen's page, drawn
//! every frame while it shows, allocates nothing either.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! harness's own threads do not disturb it. This file holds a single test
//! for the same reason. Each counted span is a [`Span`], which starts with
//! the positive control and stops counting on every way out of it, before
//! anything is asserted or reported. Profiles and the positive control:
//! design.md "Scope of No Allocation".

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{Chart, ChartBuilder, TimeUs};
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

/// A counted span of this thread's allocations. Made, it counts from a
/// positive control: an allocation kept through `black_box`, which must
/// be counted. Ended or dropped on any other way out (a panic unwinding
/// through it included), it stops counting, so what follows it, the
/// assertions and their messages, is never counted.
struct Span {
    control: u64,
}

impl Span {
    fn start() -> Self {
        ALLOCS.store(0, Ordering::Relaxed);
        TRACK.with(|t| t.set(true));
        drop(std::hint::black_box(vec![1u8; 8]));
        Self {
            control: ALLOCS.swap(0, Ordering::Relaxed),
        }
    }

    /// Stops counting: the positive control's count, then the span's.
    fn end(self) -> (u64, u64) {
        let control = self.control;
        drop(self);
        (control, ALLOCS.swap(0, Ordering::Relaxed))
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let _ = TRACK.try_with(|t| t.set(false));
    }
}

/// Runs `work` in a [`Span`]; its result and the allocations it made,
/// checked against the positive control once counting has stopped.
fn counted<R>(work: impl FnOnce() -> R) -> (R, u64) {
    let span = Span::start();
    let out = work();
    let (control, allocs) = span.end();
    assert_eq!(control, 1, "the positive control was not counted");
    (out, allocs)
}

const FRAMES: i64 = 460;
const FRAME_US: i64 = 16_667;

/// Taps 25 ms apart over every lane, then a long note per key lane, with
/// BPM changes and a STOP, so frames draw notes, long note bodies, text
/// and the effects of the judgments fed to them.
fn chart() -> (Chart, Vec<(u8, TimeUs)>) {
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
    (b.build().unwrap(), taps)
}

/// What the play has judged so far, fed to the frames as their effects.
struct Feed<'a> {
    chart: &'a Chart,
    taps: &'a [(u8, TimeUs)],
    next: usize,
    seq: u64,
    held: u32,
    ended: u32,
}

impl Feed<'_> {
    /// Fills `effects` with frame `now`'s: every tap that has reached the
    /// line judged, and each long note held from its head and finished at
    /// its end.
    fn fill(&mut self, now: TimeUs, effects: &mut Vec<JudgeEvent>) {
        effects.clear();
        while self.next < self.taps.len() && self.taps[self.next].1 <= now && effects.len() < 32 {
            let (l, t) = self.taps[self.next];
            let note = self.chart.lanes[usize::from(l)]
                .iter()
                .position(|n| n.time == t)
                .unwrap();
            effects.push(JudgeEvent {
                seq: self.seq,
                time: t,
                origin: Some(t),
                kind: JudgeEventKind::Judged {
                    lane: lane(l),
                    note,
                    judgment: Judgment::ALL[self.next % 5],
                    deviation: Some(TimeUs(0)),
                    release: None,
                    combo: u32::try_from(self.next).unwrap(),
                },
            });
            self.next += 1;
            self.seq += 1;
        }
        for l in 1..8u8 {
            let notes = &self.chart.lanes[usize::from(l)];
            let (index, ln) = (notes.len() - 1, notes[notes.len() - 1]);
            let end = ln.end();
            // Odd lanes are released 100 ms early, even lanes at the end.
            let release = if l % 2 == 1 {
                TimeUs(end.0 - 100 * MS)
            } else {
                end
            };
            let kinds: &[JudgeEventKind] = if ln.time.0 > now.0 - FRAME_US && ln.time <= now {
                self.held += 1;
                &[JudgeEventKind::LnHeld {
                    lane: lane(l),
                    note: index,
                    judgment: Judgment::Great,
                    deviation: TimeUs(0),
                }]
            } else if release.0 > now.0 - FRAME_US && release <= now {
                self.ended += 1;
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
                    seq: self.seq,
                    time: now,
                    origin: None,
                    kind,
                });
                self.seq += 1;
            }
        }
    }
}

#[test]
fn frames_do_not_allocate_from_the_first() {
    let (chart, taps) = chart();
    let mut skin = SimpleSkin::new();
    skin.load(&chart, None);
    let mut batch = SpriteBatch::with_capacity(MAX_SPRITES);
    let mut staging: Vec<Instance> = Vec::with_capacity(MAX_SPRITES);
    let v = view(42, 1_234);
    let mut effects: Vec<JudgeEvent> = Vec::with_capacity(64);
    let mut feed = Feed {
        chart: &chart,
        taps: &taps,
        next: 0,
        seq: 0,
        held: 0,
        ended: 0,
    };
    // What `Renderer::build` does, before `submit` calls into wgpu, for
    // frame `k`: the instances it staged.
    let mut frame = |k: i64| {
        let now = TimeUs(k * FRAME_US);
        feed.fill(now, &mut effects);
        let f = input(&v, now, TimeUs(8 * MS), &effects);
        render::build_frame(&mut skin, &f, &mut batch, &mut staging);
        staging.len()
    };

    // The first frame, with the batch and the staging fresh from their
    // reservation.
    let (first, first_allocs) = counted(|| frame(0));
    assert!(first > 0);
    assert_eq!(first_allocs, 0, "the first frame allocated");
    // Every later frame.
    let (drawn, later_allocs) = counted(|| (1..FRAMES).map(&mut frame).sum::<usize>());
    assert!(drawn > 0);
    assert_eq!(later_allocs, 0, "a later frame allocated");
    assert_eq!(feed.next, taps.len(), "every tap judged");
    assert_eq!(
        (feed.held, feed.ended),
        (7, 7),
        "every long note held and finished"
    );

    // A batch and staging too small for a frame of the chart loaded again,
    // its notes not yet judged: the batch counts what it refuses, and
    // nothing allocates.
    skin.load(&chart, None);
    let mut small = SpriteBatch::with_capacity(32);
    let mut small_staging: Vec<Instance> = Vec::with_capacity(32);
    let f = input(&v, TimeUs(3_000 * MS), TimeUs(8 * MS), &[]);
    let (dropped, full_allocs) = counted(|| {
        render::build_frame(&mut skin, &f, &mut small, &mut small_staging);
        small.dropped()
    });
    assert!(dropped > 0, "the full batch counted no refusal");
    assert_eq!(small_staging.len(), 32);
    assert_eq!(full_allocs, 0, "a full batch allocated");

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
    let lines = vec!["RESULT".repeat(12); 27];
    let (staged, result_allocs) = counted(|| {
        let mut staged = 0;
        for _ in 0..3 {
            batch.clear();
            render::result_screen(&lines, Some(&graphs), &mut batch);
            batch.stage(&mut staging);
            staged = staging.len();
        }
        staged
    });
    assert!(staged > 27 * 60);
    assert_eq!(result_allocs, 0, "the result page allocated");
}
