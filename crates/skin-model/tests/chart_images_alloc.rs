//! A play skin's frames drawing the chart's own images allocate nothing
//! (S10-12 design §4.5): play frames with both images installed, with one
//! absent and with none, under every stretch, the native-size ones
//! included, and loading frames while the images are unavailable,
//! published and absent, with the fields and the negated conditions that
//! read them; setting the images between frames allocates nothing either.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

mod lanes_support;
mod play_support;
mod widget_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use skin_model::chart_image::{ChartImage, ChartKey, ChartStretch, ChartTable, ChartUv};
use skin_model::{
    ElementDef, Filter, FrameUv, LoadState, LoadingFrame, Place, QuadBatch, Source, TimerRef, timer,
};

use lanes_support::rect;
use play_support::{Harness, VIEW, load, readouts_for, uvs};
use widget_support::{boolean, chart};

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

/// The allocations `f` makes on this thread.
fn counted(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    f();
    TRACK.with(|t| t.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

const STRETCHES: [ChartStretch; 7] = [
    ChartStretch::Fill,
    ChartStretch::Inner,
    ChartStretch::Outer,
    ChartStretch::FitWidth,
    ChartStretch::FitHeight,
    ChartStretch::NoExpanding,
    ChartStretch::NoResize,
];

fn element(which: ChartImage, stretch: ChartStretch, x: i32) -> ElementDef {
    ElementDef {
        source: Source::ChartImage {
            which,
            filter: Filter::Linear,
            stretch,
        },
        track: rect([x, 100, 300, 100]),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: skin_model::Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

fn uv(native: [u32; 2]) -> ChartUv {
    ChartUv {
        uv: FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 3,
        },
        native,
    }
}

/// The live key of the tests' frames.
const KEY: ChartKey = ChartKey {
    renderer: 1,
    installation: 2,
    atlas: 3,
    generation: 4,
};

#[test]
fn chart_image_frames_do_not_allocate() {
    // The positive control: the counter sees an allocation on this thread.
    let n = counted(|| {
        std::hint::black_box(Vec::<u8>::with_capacity(64));
    });
    assert!(n > 0, "the counter missed an allocation");
    let mut elements = Vec::new();
    for (i, s) in (0..).zip(STRETCHES) {
        elements.push(element(ChartImage::Stage, s, 10 * i));
        elements.push(element(ChartImage::Back, s, 10 * i + 5));
    }
    for (i, src) in (0..).zip(["chart.stagefile", "!chart.stagefile", "!chart.backbmp"]) {
        elements.push(ElementDef {
            source: Source::Image(0),
            visible: Some(boolean(src)),
            ..element(ChartImage::Stage, ChartStretch::Fill, 1_000 + 100 * i)
        });
    }
    let skin = load(Vec::new(), elements).unwrap();
    let other = ChartKey {
        generation: 5,
        ..KEY
    };
    let tables = [
        ChartTable::new(Some(uv([640, 480])), Some(uv([200, 600]))).keyed(KEY),
        ChartTable::new(None, Some(uv([1, 1]))).keyed(KEY),
        ChartTable::ABSENT.keyed(KEY),
        // Another array's: unavailable while loading, absent in a play.
        ChartTable::new(Some(uv([640, 480])), None).keyed(other),
    ];

    // Play frames, the images set between them.
    let mut h = Harness::new(skin.clone(), &chart(), &play_support::full(0));
    h.play.set_chart_live(Some(KEY));
    h.frame(0, &[]);
    let mut drawn = 0;
    for (k, table) in (1..).zip(tables) {
        let n = counted(|| {
            h.play.set_chart_images(table);
            h.frame(k * 10, &[]);
        });
        assert_eq!(n, 0, "play frame {k}");
        drawn += h.batch.quads().len();
    }
    assert!(drawn > 0);

    // Loading frames: unavailable, then each table, then unavailable.
    let uvs = uvs(&skin);
    let mut loading = skin.prepare_loading(skin_model::Side::Left);
    loading.set_chart_live(Some(KEY));
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = skin_model::Layout::AUTHOR;
    let covers = readouts_for(VIEW).covers;
    let build = |loading: &mut skin_model::Loading, batch: &mut QuadBatch, now: i64| {
        batch.clear();
        let frame = LoadingFrame {
            display_offset: TimeUs(0),
            fix_hispeed: 0,
            difficulty: None,
            bga: None,
            autoplay: false,
            now: TimeUs(now),
            load: LoadState {
                since: TimeUs(0),
                percent: 0,
            },
            target: [1_920, 1_080],
            layout: &layout,
            turntable: 0,
            lanes: VIEW,
            covers,
            gauge: None,
            play_level: None,
        };
        skin.build_loading(loading, &frame, &uvs, batch).unwrap();
    };
    build(&mut loading, &mut batch, 0);
    let states = [
        None,
        Some(tables[0]),
        Some(tables[1]),
        Some(tables[2]),
        Some(tables[3]),
        None,
    ];
    for (k, table) in (1..).zip(states) {
        let n = counted(|| {
            loading.set_chart_images(table);
            build(&mut loading, &mut batch, k * 1_000);
        });
        assert_eq!(n, 0, "loading frame {k}");
    }
}
