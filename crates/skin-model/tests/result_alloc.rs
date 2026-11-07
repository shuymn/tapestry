//! A result skin's per-frame build allocates nothing (docs/stage3.md
//! S10-09g, S10-09h): numbers over shown and hidden values and their
//! guards, an image choice over the clear type, chart text, the fade-out
//! and a `when` timer, both judge graphs with their background and reveal,
//! the gauge graph with its reveal and no-input mark, and the timing
//! distribution with its bars, lines and areas, frame after frame as the timers move, over views that change between
//! frames, unavailable ones among them.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

mod result_support;
mod selection_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use result_support::{RFx, best, number, ready, relement, rint};
use rules::GaugeValue;
use skin_model::{
    BestView, DEVIATION_SLOTS, ElementDef, FontRef, FrameUv, GRAPH_COLUMNS, GaugeColumn,
    GaugeGraphColors, GaugeGraphDef, GaugeHistory, GaugeLook, ImageChoiceDef, ImageChoicePart,
    ImageSpan, JudgeGraphDef, JudgeGraphKind, PastTable, ResultDefs, ResultNumber, ResultStatus,
    ResultView, Rgba8, Skin, Source, TextDef, TextField, TextImage, TextTable, TextValue, TimerRef,
    TimingGraphDef, Unavailable, WidgetDefs, result_timer,
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

/// The allocations `f` makes on this thread.
fn counted(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    f();
    TRACK.with(|t| t.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

fn skin() -> Skin {
    let text = |field| TextDef {
        value: TextValue::Field(field),
        font: FontRef::Default,
        align: skin_model::Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    let choice = ImageChoiceDef {
        value: rint("result.clear"),
        choices: (0..9)
            .map(|image| {
                Some(ImageChoicePart {
                    span: ImageSpan {
                        image,
                        first: 0,
                        frames: 1,
                    },
                    cycle: None,
                    track: None,
                    native: None,
                })
            })
            .collect(),
        past_table: PastTable::Hide,
    };
    let mut def = selection_support::skin_def(
        vec![
            relement(Source::Number(0), [0, 0, 10, 10], Some("result.valid")),
            relement(Source::Number(1), [0, 20, 10, 10], Some("best.known")),
            relement(Source::Number(2), [0, 40, 10, 10], Some("timing.valid")),
            relement(
                Source::Number(3),
                [0, 60, 10, 10],
                Some("chart.level_valid"),
            ),
            relement(
                Source::Image(3),
                [0, 80, 10, 10],
                Some("best.known && best.bp_diff != 0"),
            ),
            relement(Source::ImageChoice(0), [0, 100, 10, 10], None),
            relement(Source::Text(0), [0, 120, 200, 20], None),
            ElementDef {
                timer: TimerRef::Builtin(result_timer::FADEOUT),
                ..relement(Source::Image(4), [0, 140, 10, 10], None)
            },
            ElementDef {
                timer: TimerRef::When(0),
                ..relement(Source::Image(5), [0, 160, 10, 10], None)
            },
            relement(Source::JudgeGraph(0), [0, 200, 520, 82], None),
            relement(Source::JudgeGraph(1), [0, 300, 520, 82], None),
            relement(Source::GaugeGraph(0), [0, 400, 519, 259], None),
            relement(Source::TimingGraph(0), [600, 0, 520, 82], None),
        ],
        WidgetDefs {
            numbers: vec![
                number("ex_score", 6),
                number("best.bp_diff", 4),
                number("timing.mean_x100", 5),
                number("chart.level", 2),
            ],
            texts: vec![text(TextField::FullTitle)],
            image_choices: vec![choice],
            ..WidgetDefs::default()
        },
    );
    def.whens = vec![result_support::rbool("result.cleared")];
    let graph = |kind, back, reverse| JudgeGraphDef {
        kind,
        delay: TimeUs(500_000),
        reverse,
        back,
        chips: 0,
        solid: 1,
        fit: skin_model::GraphFit::Outer { trim: true },
    };
    let judge_graphs = vec![
        graph(JudgeGraphKind::Judgment, true, false),
        graph(JudgeGraphKind::EarlyLate, true, true),
    ];
    let translucent = |rgb| Rgba8::rgb(rgb, 0xaa);
    let look = GaugeGraphColors {
        border_line: Rgba8::rgb(0xff_0000, 0x80),
        border_back: translucent(0x44_0000),
        line: Rgba8::rgb(0x00_00ff, 0x80),
        back: translucent(0x00_4400),
    };
    let gauge_graphs = vec![GaugeGraphDef {
        colors: [look; GaugeLook::COUNT],
        solid: 1,
    }];
    let timing_graphs = vec![TimingGraphDef {
        gx: 301,
        colors: [Rgba8::rgb(0xf5_f5f5, 0xff); 8],
        average: true,
        dev: true,
        solid: 1,
    }];
    Skin::new_result(
        def,
        ResultDefs {
            judge_graphs,
            gauge_graphs,
            timing_graphs,
        },
    )
    .unwrap()
}

/// `v` with a gauge history crossing the border every few columns and a
/// what-if's mark, deviations in every slot, and judge areas.
fn with_graphs(mut v: ResultView) -> ResultView {
    let g = |p: i64| GaugeValue(p * GaugeValue::PERCENT);
    let mut columns = [GaugeColumn {
        low: g(0),
        high: g(0),
        last: g(0),
    }; GRAPH_COLUMNS];
    for (i, c) in (0_i64..).zip(columns.iter_mut()) {
        let (a, b) = (i * 37 % 101, i * 53 % 101);
        *c = GaugeColumn {
            low: g(a.min(b)),
            high: g(a.max(b)),
            last: g(b),
        };
    }
    v.gauge = Some(Box::new(GaugeHistory {
        columns,
        max: g(100),
        border: g(80),
        look: GaugeLook::Normal,
        reached: 288,
        no_input_from: Some(200),
    }));
    let mut deviations = [0_u32; DEVIATION_SLOTS];
    for (i, d) in (0_u32..).zip(deviations.iter_mut()) {
        *d = i % 23 + 1;
    }
    v.deviations = Some(Box::new(deviations));
    v.timing_areas = Some([[-20, 20], [-60, 60], [-150, 150], [-220, 220], [-150, 500]]);
    v
}

#[test]
fn a_result_build_allocates_nothing() {
    let mut fx = RFx::new(skin());
    let line = TextImage::new(
        FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 0,
        },
        [0.0, 0.0, 1.0, 1.0],
        1.0,
    );
    let mut unavailable = ready(10, BestView::Unavailable, 512);
    unavailable.status = ResultStatus::Unavailable(Unavailable::Failed);
    let views = [
        with_graphs(ready(1_000, best(ResultNumber::Shown(-3)), 512)),
        ready(1_200, best(ResultNumber::Hidden), 300),
        unavailable,
        with_graphs(ready(1_400, BestView::Unavailable, 1)),
    ];
    let mut total = 0;
    // The first build after preparation, before any view, on the batch
    // reserved at the skin's capacity.
    total += counted(|| fx.frame(0));
    for v in &views {
        fx.view(v);
        fx.skin
            .attach_result_text(&mut fx.res, TextTable::new(vec![line; 1]))
            .unwrap();
        // The first build after each view change counts too.
        total += counted(|| fx.frame(0));
        assert!(!fx.batch.quads().is_empty());
        // Past the reveal, a ready view's graphs draw their backgrounds
        // and chips, the gauge's line and the distribution's bars.
        total += counted(|| fx.frame(1_600));
        if v.status == ResultStatus::Ready {
            assert!(fx.batch.quads().len() > 2 * 6, "{}", fx.batch.quads().len());
        }
        if v.gauge.is_some() {
            assert!(
                fx.batch.quads().len() > 288 + 301,
                "{}",
                fx.batch.quads().len()
            );
        }
        assert_eq!(fx.batch.dropped(), 0);
        for ms in (16..2_000).step_by(16) {
            fx.timers.fadeout = (ms > 1_000).then_some(TimeUs(1_000_000));
            total += counted(|| fx.frame(ms));
        }
    }
    assert_eq!(total, 0);
    // The positive control: the counter sees an allocation on this thread
    // (design.md "Scope of No Allocation").
    assert!(
        counted(|| {
            std::hint::black_box(vec![0_u8; 1]);
        }) > 0
    );
}
