//! A decision frame's build allocates nothing (design.md "Scope of No
//! Allocation"; "Play Lifecycle", the decision screen): the decision
//! installation's skin builds its frames into its slot's staging, its
//! lines borrowed from a prepared label slot, its first frame and the
//! later ones, with rasterized lines (a TrueType skin) and with bitmap
//! lines (a bitmap-font skin); a refused build (no decision installation,
//! after its removal) allocates nothing either, and its error is looked at
//! only after counting stops. The labels' preparation, the worker's and
//! the boundary's, runs outside the count.
//!
//! The counting allocator counts only on the thread that opted in, so the
//! worker and the harness's threads do not disturb it. This file holds a
//! single test for the same reason.

mod common;
mod font_support;
mod outline_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use bms_model::TimeUs;
use common::offscreen;
use outline_support::{
    Value, bitmap, bitmap_files, decision_bundle_in, decision_folder, default_font, text, ttf,
};
use render::{
    FillStep, LabelBook, LabelJob, LabelKey, LabelStore, LabelWorker, Renderer, Settled, SkinBundle,
};
use skin_model::{DecisionFrame, DecisionStrings, DecisionTimers, ResultLabels, ScreenKind};

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

/// A counted span of this thread's allocations, from a positive control;
/// it stops counting when ended or dropped, so the assertions and their
/// messages are never counted.
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

/// Runs `work` counted; its result and its allocations.
fn counted<R>(work: impl FnOnce() -> R) -> (R, u64) {
    let span = Span::start();
    let out = work();
    let (control, allocs) = span.end();
    assert_eq!(control, 1, "the positive control was not counted");
    (out, allocs)
}

const TARGET: [u32; 2] = [1_920, 1_080];
const TITLE: [i32; 4] = [100, 100, 1_200, 18];
const ARTIST: [i32; 4] = [100, 300, 1_200, 24];

/// The renderer with `dir`'s decision skin committed, and its labels for
/// "AB" by "BA" prepared through the worker into a slot: the slot and its
/// generation, with the store holding them.
fn prepared(dir: &tempfile::TempDir) -> (Renderer, Arc<SkinBundle>, LabelStore, usize) {
    let mut r = offscreen(TARGET[0], TARGET[1]);
    let b = decision_bundle_in(&r, dir.path(), &default_font());
    let token = r.install(ScreenKind::Decision, &b, 1).unwrap();
    r.commit(token).unwrap();
    let (tx, rx) = mpsc::channel();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(());
    });
    let worker = LabelWorker::for_bundle(&b, Some(wake)).unwrap();
    worker.post(LabelJob {
        generation: 1,
        key: LabelKey {
            chart: PathBuf::from("a"),
            revision: 1,
            target: TARGET,
        },
        strings: DecisionStrings::new(&ResultLabels {
            title: "AB".to_owned(),
            artist: "BA".to_owned(),
            ..ResultLabels::default()
        }),
        plan: Arc::new(r.decision_rows().unwrap().unwrap()),
    });
    rx.recv_timeout(Duration::from_secs(60)).unwrap();
    let mut store = LabelStore::new(LabelBook::shared(TARGET));
    store.offer(worker.take().unwrap().unwrap());
    let slot = store.claim().unwrap();
    while r.write_decision_labels(&mut store) == FillStep::Wrote {}
    let _ = r.gpu().device().poll(wgpu::PollType::wait_indefinitely());
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 1
        }
    );
    (r, b, store, slot)
}

/// Builds frames of `b`'s decision screen from `store`'s slot at 0, 1 000
/// and 2 000 ms, each counted: the allocations of each.
fn frames(r: &mut Renderer, b: &SkinBundle, store: &LabelStore, slot: usize) -> Vec<u64> {
    let mut dec = b.skin().prepare_decision();
    let labels = store.content(slot, 1).unwrap();
    (0..3)
        .map(|n| {
            let frame = DecisionFrame {
                now: TimeUs(n * 1_000_000),
                target: TARGET,
                layout: &skin_model::Layout::AUTHOR,
                timers: DecisionTimers {
                    screen: Some(TimeUs(0)),
                    fadeout: (n > 1).then_some(TimeUs(2_000_000)),
                },
            };
            let (built, allocs) = counted(|| r.build_decision(&mut dec, labels, &frame).is_ok());
            assert!(built);
            r.submit_decision(&[]).unwrap();
            allocs
        })
        .collect()
}

#[test]
fn decision_build_alloc() {
    let ttf_dir = decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[
            text("f", 18, Value::Ref(10), TITLE),
            text("f", 24, Value::Ref(16), ARTIST),
        ],
    );
    let (mut r, b, store, slot) = prepared(&ttf_dir);
    assert!(store.content(slot, 1).unwrap().text.get(0).is_some());
    assert_eq!(frames(&mut r, &b, &store, slot), [0, 0, 0]);
    // A refused build: no decision installation.
    r.remove(ScreenKind::Decision);
    let mut dec = b.skin().prepare_decision();
    let labels = store.content(slot, 1).unwrap();
    let frame = DecisionFrame {
        now: TimeUs(0),
        target: TARGET,
        layout: &skin_model::Layout::AUTHOR,
        timers: DecisionTimers::default(),
    };
    let (refused, allocs) = counted(|| r.build_decision(&mut dec, labels, &frame));
    assert_eq!(allocs, 0);
    assert!(refused.is_err());

    let fnt_dir = decision_folder(
        &bitmap_files(),
        &[bitmap()],
        &[
            text("g", 8, Value::Ref(10), TITLE),
            text("g", 8, Value::Ref(16), ARTIST),
        ],
    );
    let (mut r, b, store, slot) = prepared(&fnt_dir);
    assert!(store.content(slot, 1).unwrap().bitmap.heap_bytes() > 0);
    assert_eq!(frames(&mut r, &b, &store, slot), [0, 0, 0]);
}
