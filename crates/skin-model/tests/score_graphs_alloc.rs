//! A play skin's score figures, bars, ranks and pass timers build without
//! allocating (stage 3, S10-10; design.md "Scope of No Allocation"), each
//! widget at its cap and drawing its asserted output: skin A, the six
//! score bars in both orientations, every score number at the most cells
//! a strip takes, the sixteen rank flags and both pass timers, over the
//! frames before the first judgment, after an empty POOR, mid-play, at
//! the end, after a rearm and in a second practice round. Then skins B to
//! D and a one-hour chart's notes graph over the same frames: a BPM graph
//! at 2 048 columns, every column at its four runs; a timing visualizer at
//! jw 1 000 beside a hit-error visualizer 4 096 texels wide over a wrapped
//! ring; and a notes graph, a BPM graph, both visualizers and the score
//! bars together within the budget.
//!
//! A counting global allocator counts allocations made on threads that opt
//! in (a thread-local flag), so the test harness's own threads do not
//! disturb the count. This file holds a single test for the same reason.

mod graph_support;
mod lanes_support;
mod play_support;
mod widget_support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::Tempo;
use bms_model::{Chart, ChartBuilder, TimeUs};
use eval::{Section, Tally};
use rules::PerJudgment;
use skin_model::{
    Align, Atlas, Attempt, BpmColors, BpmGraphDef, ElementDef, EmaMode, GlyphSet, HitErrorDef,
    MAX_CELLS, Pad, PlayContext, PlayDefs, PlayScore, RatioBarDef, RatioForm, RatioSource, Rgba8,
    ScoreBasis, ScoreRate, Skin, Source, TimerRef, TimingVisualizerDef, Variant, WidgetDefs, timer,
};

use play_support::{GAUGES, Harness, MS, def, full, lane};
use widget_support::{TEN, boolean, digits, frame_of, number, sheet};

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

/// The numbers, each over [`MAX_CELLS`] zero-padded cells at y = 400 +
/// 20k: 150, 151, 152 and 153 (the differences offset by 10⁶ to stay at
/// or above 0), 122, 123, 183, 184 and Lua `float_number(113)` in ppm, the
/// best's comparisons shown while `best.valid`.
const NUMBERS: [(&str, Option<&str>); 9] = [
    ("best.ex", None),
    ("target.ex", None),
    ("best.diff + 1000000", Some("best.valid")),
    ("target.diff + 1000000", None),
    ("target.rate_bp / 100", None),
    ("target.rate_bp % 100", None),
    ("best.rate_bp / 100", Some("best.valid")),
    ("best.rate_bp % 100", Some("best.valid")),
    ("best.rate_ppm", Some("best.valid")),
];

/// The bars' length: 168, a multiple of 14 and of 6.
const LONG: f32 = 168.0;

/// Bar `i` of [`ScoreRate::ALL`]: horizontal at y = 100 + 30i, or
/// vertical at x = 400 + 30i.
fn bar_rect(i: usize, vertical: bool) -> [i32; 4] {
    let at = 30 * i32::try_from(i).unwrap();
    if vertical {
        [400 + at, 100, 20, 168]
    } else {
        [100, 100 + at, 168, 20]
    }
}

fn screen(source: Source, r: [i32; 4]) -> ElementDef {
    graph_support::source_element(source, r, 255, timer::SCREEN)
}

/// Skin A.
fn skin() -> Skin {
    let mut elements = Vec::new();
    let mut ratio_bars = Vec::new();
    for vertical in [false, true] {
        for (i, rate) in ScoreRate::ALL.into_iter().enumerate() {
            let n = u32::try_from(ratio_bars.len()).unwrap();
            ratio_bars.push(RatioBarDef {
                image: 0,
                form: RatioForm::Bar { vertical },
                source: RatioSource::Score(rate),
            });
            elements.push(screen(Source::RatioBar(n), bar_rect(i, vertical)));
        }
    }
    let mut widgets = WidgetDefs::default();
    for (k, (value, visible)) in NUMBERS.into_iter().enumerate() {
        let k = u32::try_from(k).unwrap();
        widgets.numbers.push(number(
            digits(TEN, GlyphSet::Ten, MAX_CELLS, Align::Right, Pad::Zero),
            value,
        ));
        let y = 400 + 20 * i32::try_from(k).unwrap();
        elements.push(ElementDef {
            visible: visible.map(boolean),
            ..screen(Source::Number(k), [100, y, 4, 4])
        });
    }
    for (k, name) in (0..8)
        .map(|k| format!("rank.current.{k}"))
        .chain((0..8).map(|k| format!("rank.whole.{k}")))
        .enumerate()
    {
        let x = 700 + 20 * i32::try_from(k).unwrap();
        elements.push(ElementDef {
            visible: Some(boolean(&name)),
            ..screen(Source::Image(0), [x, 100, 10, 10])
        });
    }
    for (w, x) in [(0, 700), (1, 720)] {
        elements.push(ElementDef {
            timer: TimerRef::When(w),
            ..screen(Source::Image(0), [x, 140, 10, 10])
        });
    }
    let whens = vec![boolean("best.reached"), boolean("target.reached")];
    let d = skin_model::SkinDef {
        widgets,
        ..def(whens, sheet(), elements)
    };
    Skin::new_play(
        d,
        PlayDefs {
            ratio_bars,
            ..PlayDefs::default()
        },
    )
    .unwrap()
}

/// Seven taps, 100 ms apart from 1 s.
fn chart() -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..7 {
        b = b.tap(lane(1), TimeUs((1_000 + 100 * i) * MS));
    }
    b.build().unwrap()
}

fn tally(perfect: u32, great: u32, empty_poor: u32) -> Tally {
    Tally {
        counts: PerJudgment {
            perfect,
            great,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 2 * perfect + great,
        empty_poor,
        ..play_support::tally(0)
    }
}

/// What one frame should draw.
struct Want {
    /// The bars' ratios, `None` for none.
    bars: [Option<(u32, u32)>; 6],
    /// The numbers' values, `None` where hidden.
    numbers: [Option<i64>; 9],
    /// The rank flags on, `rank.current.K` at K and `rank.whole.K` at
    /// 8 + K.
    ranks: &'static [usize],
    /// Whether each pass timer's marker draws.
    passes: [bool; 2],
}

/// Checks the frame `h` built against `want`: the quads count, each
/// bar's cut by sampled pixels, each number's glyphs and the markers.
#[track_caller]
fn check(h: &Harness, atlas: &Atlas, want: &Want, at: &str) {
    let quads = h.batch.quads();
    let numbers = want.numbers.iter().flatten().count();
    let total = 2 * want.bars.iter().flatten().count()
        + numbers * usize::from(MAX_CELLS)
        + want.ranks.len()
        + want.passes.iter().filter(|&&p| p).count();
    assert_eq!(quads.len(), total, "{at}: quads");
    let lit = |x: f32, y: f32| graph_support::composite(quads, atlas, x, y, None).1 > 0;
    for (i, r) in want.bars.iter().enumerate() {
        let [hx, hy, ..] = bar_rect(i, false).map(|v| v as f32);
        let [vx, vy, ..] = bar_rect(i, true).map(|v| v as f32);
        let len = r.map_or(0.0, |(n, d)| LONG * n.min(d) as f32 / d as f32);
        let lit_h = |x: f32| lit(hx + x, hy + 10.0);
        let lit_v = |y: f32| lit(vx + 10.0, vy + LONG - y);
        if len > 0.0 {
            assert!(lit_h(len - 0.5) && lit_v(len - 0.5), "{at}: bar {i} short");
        }
        if len < LONG {
            assert!(!lit_h(len + 0.5) && !lit_v(len + 0.5), "{at}: bar {i} long");
        }
    }
    for (k, v) in want.numbers.iter().enumerate() {
        let y = 400.0 + 20.0 * k as f32;
        let mut cells: Vec<(f32, u16)> = quads
            .iter()
            .filter(|q| q.rect[1] == y)
            .map(|q| {
                let (image, frame) = frame_of(&h.uvs, q).expect("a glyph");
                assert_eq!(image, TEN);
                (q.rect[0], frame)
            })
            .collect();
        cells.sort_by(|a, b| a.0.total_cmp(&b.0));
        let shown: String = cells
            .iter()
            .map(|&(_, f)| char::from(b'0' + f as u8))
            .collect();
        let want = v.map_or(String::new(), |v| format!("{v:011}"));
        assert_eq!(shown, want, "{at}: number {}", NUMBERS[k].0);
    }
    for k in 0..16 {
        let x = 705.0 + 20.0 * k as f32;
        assert_eq!(lit(x, 105.0), want.ranks.contains(&k), "{at}: rank {k}");
    }
    for (w, x) in [(0, 705.0), (1, 725.0)] {
        assert_eq!(lit(x, 145.0), want.passes[w], "{at}: pass {w}");
    }
}

/// Builds the frame at `ms` over `t`; the allocations it made.
fn frame(h: &mut Harness, ms: i64, t: Tally) -> u64 {
    h.tally = t;
    counted(|| h.frame(ms, &[]))
}

/// The numbers before the first judgment with B = 11 and T = 13 over 7.
const READY_NUMBERS: [Option<i64>; 9] = [
    Some(11),
    Some(13),
    Some(1_000_000),
    Some(1_000_000),
    Some(92),
    Some(85),
    Some(78),
    Some(57),
    Some(785_714),
];

#[test]
fn score_frames_do_not_allocate() {
    // The positive control: the counter sees an allocation on this thread.
    let n = counted(|| {
        std::hint::black_box(Vec::<u8>::with_capacity(64));
    });
    assert!(n > 0, "the counter missed an allocation");
    let chart = chart();
    let basis = ScoreBasis {
        best: 11,
        best_known: true,
        target: 13,
    };
    let ctx = PlayContext {
        score: PlayScore {
            basis,
            ..PlayScore::NONE
        },
        ..full(0)
    };
    let skin = skin();
    let atlas = skin
        .sheet()
        .atlas(Variant::Lo, graph_support::LIMITS, &|| false)
        .unwrap();
    let mut h = Harness::new(skin, &chart, &ctx);
    h.uvs = atlas.uvs().clone();
    let ready = Want {
        bars: [
            Some((0, 14)),
            Some((0, 1)),
            Some((0, 14)),
            Some((11, 14)),
            Some((0, 14)),
            Some((13, 14)),
        ],
        numbers: READY_NUMBERS,
        ranks: &[],
        passes: [false, false],
    };
    // Before the first judgment.
    assert_eq!(frame(&mut h, 0, tally(0, 0, 0)), 0);
    check(&h, &atlas, &ready, "ready");
    // After an empty POOR: 111 full, AAA by the current rate, F reached.
    assert_eq!(frame(&mut h, 900, tally(0, 0, 1)), 0);
    let mut bars = ready.bars;
    bars[1] = Some((1, 1));
    check(
        &h,
        &atlas,
        &Want {
            bars,
            ranks: &[0, 15],
            ..ready
        },
        "empty POOR",
    );
    // Mid-play: p = 3, ex = 5.
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    let mut numbers = READY_NUMBERS;
    numbers[2] = Some(1_000_001);
    check(
        &h,
        &atlas,
        &Want {
            bars: [
                Some((5, 14)),
                Some((5, 6)),
                Some((4, 14)),
                Some((11, 14)),
                Some((5, 14)),
                Some((13, 14)),
            ],
            numbers,
            ranks: &[1, 13, 14, 15],
            passes: [false, false],
        },
        "mid-play",
    );
    // The end: p = 7, ex = 14, every value at its top.
    assert_eq!(frame(&mut h, 1_700, tally(7, 0, 0)), 0);
    let mut numbers = READY_NUMBERS;
    numbers[2] = Some(1_000_003);
    numbers[3] = Some(1_000_001);
    check(
        &h,
        &atlas,
        &Want {
            bars: [
                Some((14, 14)),
                Some((14, 14)),
                Some((11, 14)),
                Some((11, 14)),
                Some((13, 14)),
                Some((13, 14)),
            ],
            numbers,
            ranks: &[0, 8, 9, 10, 11, 12, 13, 14, 15],
            passes: [true, true],
        },
        "end",
    );
    // After a rearm: as before the first judgment.
    h.play.rearm();
    assert_eq!(frame(&mut h, 0, tally(0, 0, 0)), 0);
    check(&h, &atlas, &ready, "rearm");

    // A second practice round over the same seven targets: no best, no
    // target, its projections fresh at READY and its own mid-play.
    let section = Section {
        start: TimeUs(900 * MS),
        end: TimeUs(2_000 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: GAUGES.iter().map(|g| g.start).collect(),
    };
    let practice = PlayContext {
        attempt: Attempt::Section(&section),
        ..full(0)
    };
    let mut round = h.skin.prepare(&chart, &practice).unwrap();
    std::mem::swap(&mut h.play, &mut round);
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    // The next round, prepared fresh.
    let mut next = h.skin.prepare(&chart, &practice).unwrap();
    std::mem::swap(&mut h.play, &mut next);
    let none = [
        Some(0),
        Some(0),
        Some(1_000_000),
        Some(1_000_000),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
    ];
    assert_eq!(frame(&mut h, 400, tally(0, 0, 0)), 0);
    check(
        &h,
        &atlas,
        &Want {
            bars: [
                Some((0, 14)),
                Some((0, 1)),
                Some((0, 14)),
                Some((0, 14)),
                Some((0, 14)),
                Some((0, 14)),
            ],
            numbers: none,
            ranks: &[],
            passes: [false, false],
        },
        "second round, READY",
    );
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    let mut numbers = none;
    numbers[2] = Some(1_000_005);
    numbers[3] = Some(1_000_005);
    check(
        &h,
        &atlas,
        &Want {
            bars: [
                Some((5, 14)),
                Some((5, 6)),
                Some((0, 14)),
                Some((0, 14)),
                Some((0, 14)),
                Some((0, 14)),
            ],
            numbers,
            ranks: &[1, 13, 14, 15],
            passes: [true, true],
        },
        "second round, mid-play",
    );
    skin_b();
    skin_notes();
    skin_c();
    skin_d();
    one_hour();
}

// ---------------------------------------------------------------------
// Skins B to D and the one-hour notes graph: the play graphs and
// visualizers, each at its cap.

/// An element on `screen` drawing `source` over `r`.
fn screen_el(source: Source, r: [i32; 4]) -> ElementDef {
    graph_support::source_element(source, r, 255, timer::SCREEN)
}

/// A play skin over the widget sheet (image 0 white) of `elements` and
/// `defs`.
fn graph_skin(elements: Vec<ElementDef>, defs: PlayDefs) -> Skin {
    Skin::new_play(def(Vec::new(), sheet(), elements), defs).unwrap()
}

fn tempo(bpm: u128) -> Tempo {
    Tempo::from_units(bpm * Tempo::UNITS_PER_BPM).unwrap()
}

/// What a graph skin's frame shows, checked after every frame.
type Check<'a> = &'a dyn Fn(&Harness, &Atlas, &str);

/// Builds `skin`'s frames over `chart` with `score` at zero allocations,
/// checking each with `check`: during READY, after an empty POOR, mid-play
/// with three offsets recorded, at the end with 130 recorded (the ring
/// wrapped), after a rearm, and in a second practice round's READY and its
/// mid-play.
fn graph_frames(name: &str, skin: Skin, chart: &Chart, score: PlayScore<'_>, check: Check<'_>) {
    let atlas = skin
        .sheet()
        .atlas(Variant::Lo, graph_support::LIMITS, &|| false)
        .unwrap();
    let ctx = PlayContext {
        score,
        ..full(1_000)
    };
    let mut h = Harness::new(skin, chart, &ctx);
    h.uvs = atlas.uvs().clone();
    let label = |stage: &str| format!("{name}: {stage}");
    assert_eq!(frame(&mut h, -500, tally(0, 0, 0)), 0, "{}", label("ready"));
    check(&h, &atlas, &label("ready"));
    assert_eq!(frame(&mut h, 100, tally(0, 0, 1)), 0);
    check(&h, &atlas, &label("empty POOR"));
    for v in [600, -700, 7] {
        h.record(v);
    }
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    check(&h, &atlas, &label("mid-play"));
    for k in 0..127 {
        h.record((k * 17 % 300) - 150);
    }
    assert_eq!(frame(&mut h, 3_000, tally(7, 0, 0)), 0);
    check(&h, &atlas, &label("end"));
    h.play.rearm();
    h.clear_ring();
    assert_eq!(frame(&mut h, -500, tally(0, 0, 0)), 0);
    check(&h, &atlas, &label("rearm"));
    let section = Section {
        start: TimeUs(900 * MS),
        end: TimeUs(2_000 * MS),
        lead_in: TimeUs(500 * MS),
        initial_gauges: GAUGES.iter().map(|g| g.start).collect(),
    };
    let practice = PlayContext {
        attempt: Attempt::Section(&section),
        score,
        ..full(0)
    };
    let mut round = h.skin.prepare(chart, &practice).unwrap();
    std::mem::swap(&mut h.play, &mut round);
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    let mut next = h.skin.prepare(chart, &practice).unwrap();
    std::mem::swap(&mut h.play, &mut next);
    h.clear_ring();
    assert_eq!(frame(&mut h, 400, tally(0, 0, 0)), 0);
    check(&h, &atlas, &label("second round, READY"));
    h.record(20);
    assert_eq!(frame(&mut h, 1_300, tally(2, 1, 0)), 0);
    check(&h, &atlas, &label("second round, mid-play"));
}

/// The quads whose left edge is canvas `x`.
fn at_x(h: &Harness, x: f32) -> usize {
    h.batch.quads().iter().filter(|q| q.rect[0] == x).count()
}

/// Skin B: a BPM graph at 2 048 columns whose every column holds more
/// than four runs, so draws exactly four. The tempo cycles through 75,
/// 150, 300, 600 and 1 200 every 100 µs up to 2 s; the last note is at
/// 1 s, so `last` is `1 000 + 1 000`: about a column a ms, each holding
/// ten rows' strokes, every tempo among them, and the rows reach
/// every column.
fn skin_b() {
    let cycle = [75_000, 150_000, 300_000, 600_000, 1_200_000];
    let mut b = ChartBuilder::new(150_000);
    for k in 1..20_000_i64 {
        let bpm = cycle[usize::try_from(k).unwrap() % cycle.len()];
        b = b.bpm(TimeUs(100 * k), bpm);
    }
    b = b
        .tap(lane(1), TimeUs(100 * MS))
        .tap(lane(1), TimeUs(1_000 * MS));
    let mut chart = b.build().unwrap();
    chart.meta.length = bms_model::ChartSummary::of(&chart).length;
    let profile = bms_model::ChartProfile::of(&chart, &[]);
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(tempo(150)),
        ..PlayScore::NONE
    };
    let skin = graph_skin(
        vec![screen_el(Source::BpmGraph(0), [0, 0, 2_048, 200])],
        PlayDefs {
            bpm_graphs: vec![BpmGraphDef {
                delay: TimeUs(0),
                line_width: 2,
                colors: BpmColors::DEFAULT,
                solid: 0,
            }],
            ..PlayDefs::default()
        },
    );
    let check = |h: &Harness, _: &Atlas, at: &str| {
        let low: Vec<i32> = (0..2_048)
            .filter(|&c| at_x(h, c as f32) != 4)
            .take(20)
            .collect();
        assert!(low.is_empty(), "{at}: {low:?}");
        assert_eq!(h.batch.quads().len(), 4 * 2_048, "{at}");
        for x in [0.0, 1_024.0, 2_047.0] {
            assert_eq!(at_x(h, x), 4, "{at}: column {x}");
        }
    };
    graph_frames("B", skin, &chart, score, &check);
}

const GREEN: u32 = 0x00_ff00;
/// The hit-error bands' colours in skin C: PERFECT red, GREAT blue, GOOD
/// yellow, BAD magenta, beyond cyan; the average green, the centre white.
const HIT_BANDS: [u32; 5] = [0xff_0000, 0x00_00ff, 0xff_ff00, 0xff_00ff, 0x00_ffff];

/// Skin C's hit-error texel of an offset: `(4 096 − 4)/2` less the offset
/// scaled by `4 096 / 2 001`, truncated toward 0.
fn hit_x(v: i64) -> i64 {
    2_046 + (-v.clamp(-1_000, 1_000) * 4_096) / 2_001
}

/// The offsets skin C's average has taken by stage ([`graph_frames`]):
/// none before mid-play; 600, −700 (outside the BAD window, read and
/// skipped) and 7 then; at the end the newest 100 of the
/// 127 recorded after them (the first 27 overwritten unread); after a
/// rearm and in a fresh round none, then 20.
fn ema_inputs(at: &str) -> Vec<i64> {
    let tail: Vec<i64> = (0..127).map(|k| (k * 17 % 300) - 150).collect();
    if at.ends_with(": mid-play") && !at.contains("second round") {
        vec![600, -700, 7]
    } else if at.ends_with(": end") {
        let mut v = vec![600, -700, 7];
        v.extend(&tail[27..]);
        v
    } else if at.ends_with("second round, mid-play") {
        vec![20]
    } else {
        Vec::new()
    }
}

/// The average after `values` at weight 0.1, by the source's arithmetic,
/// over the BAD window (−150, 150).
fn ema_of(values: &[i64]) -> i64 {
    let mut ema = 0_i64;
    for &v in values {
        if -150 < v && v < 150 {
            ema = ema.wrapping_add((0.1_f32 * v.wrapping_sub(ema) as f32) as i64);
        }
    }
    ema
}

/// Skin C's hit-error output: the newest line in its band's colour at the
/// pixmap's bottom row (only the newest reaches it with decay), every
/// offset no longer in the ring gone from the middle row (every line in the
/// ring crosses it), and the average's line and triangle in green.
fn hit_error_output(h: &Harness, atlas: &Atlas, at: &str) {
    let px = |x: f64, row: f64| {
        graph_support::composite(h.batch.quads(), atlas, x as f32, (100.0 + row) as f32, None)
    };
    let green = [0.0, 255.0, 0.0];
    let ema = i64::from(ema_of(&ema_inputs(at)) as i32);
    let ema_x = hit_x(ema);
    let ring: Vec<i64> = (0..h.ring_count.min(100))
        .map(|age| {
            let slot = (h.ring_count - 1 - age) % 100;
            i64::from(h.ring[usize::try_from(slot).unwrap()])
        })
        .collect();
    // The average's line, full height, painted last.
    graph_support::near(px(ema_x as f64 + 2.0, 199.5).0, green, 1.0);
    // Its triangle from the top: half width 40, depth 66 rows.
    let tip = ema_x + 2;
    graph_support::near(px((tip - 39) as f64 + 0.5, 0.5).0, green, 1.0);
    graph_support::near(px((tip + 39) as f64 + 0.5, 0.5).0, green, 1.0);
    if let Some(&newest) = ring.first() {
        let x = hit_x(newest);
        if (x - ema_x).abs() > 4 {
            let band = [[-20, 20], [-60, 60], [-100, 100], [-150, 150]]
                .iter()
                .position(|&[lo, hi]| lo < newest && newest < hi)
                .unwrap_or(4);
            let c = HIT_BANDS[band];
            let want = [(c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff].map(|v| v as f32);
            graph_support::near(px(x as f64 + 2.0, 199.5).0, want, 1.0);
        }
    }
    // Offsets recorded before, no longer in the ring: their texels are
    // empty where nothing in the ring, the centre or the average reaches.
    let gone: Vec<i64> = if at.ends_with(": end") {
        [600, -700, 7]
            .into_iter()
            .chain((0..27).map(|k| (k * 17 % 300) - 150))
            .collect()
    } else if at.ends_with(": rearm") || at.ends_with("second round, READY") {
        (0..127).map(|k| (k * 17 % 300) - 150).collect()
    } else {
        Vec::new()
    };
    let busy = |x: i64| {
        (x - 2_046).abs() < 4
            || (x - ema_x).abs() < 4
            || (x - tip).abs() <= 41
            || ring.iter().any(|&v| (x - hit_x(v)).abs() < 4)
    };
    let mut checked = 0;
    for v in gone {
        let x = hit_x(v);
        if !busy(x) {
            assert_eq!(px(x as f64 + 2.0, 100.5).1, 0, "{at}: {v} ms still drawn");
            checked += 1;
        }
    }
    if at.ends_with(": end") || at.ends_with(": rearm") {
        assert!(checked > 0, "{at}: no expired offset was checked");
    }
}

/// Skin C: a timing visualizer at jw 1 000 and a hit-error visualizer
/// 4 096 texels wide over a 100-line window with 4-texel lines and both
/// average marks.
fn skin_c() {
    let chart = chart();
    let areas = [[-20, 20], [-60, 60], [-100, 100], [-150, 150], [-200, 200]];
    let score = PlayScore {
        areas: Some(areas),
        ..PlayScore::NONE
    };
    let white = Rgba8::rgb(0xff_ffff, 0xff);
    let tv = TimingVisualizerDef {
        judge_width: 1_000,
        width: 2_001,
        line_width: 1,
        line: Rgba8::rgb(0x00_ffff, 0xff),
        center: white,
        bands: [white; 5],
        decay: true,
        solid: 0,
    };
    let he = HitErrorDef {
        width: 4_096,
        judge_width: 1_000,
        line_width: 4,
        window: 100,
        lines: true,
        band_colors: true,
        ema_mode: EmaMode::Both,
        alpha: 0.1,
        line: white,
        center: white,
        ema: Rgba8::rgb(GREEN, 0xff),
        bands: HIT_BANDS.map(|c| Rgba8::rgb(c, 0xff)),
        decay: true,
        solid: 0,
    };
    assert!(tv.quads() + he.quads() <= skin_model::MAX_WIDGET_QUADS);
    let skin = graph_skin(
        vec![
            screen_el(Source::TimingVisualizer(0), [0, 0, 2_001, 20]),
            screen_el(Source::HitErrorVisualizer(0), [0, 100, 4_096, 200]),
        ],
        PlayDefs {
            timing_visualizers: vec![tv],
            hit_errors: vec![he],
            ..PlayDefs::default()
        },
    );
    let check = |h: &Harness, atlas: &Atlas, at: &str| {
        let n = h.ring_count.min(100);
        // The band of the timing visualizer: its first quad starts at 0.
        assert!(at_x(h, 0.0) >= 1, "{at}");
        // A line per offset in range (all are: within ±1 000 ms).
        let lines = h
            .batch
            .quads()
            .iter()
            .filter(|q| {
                q.rect[1] < 20.0 && q.rect[2] == 1.0 && q.color[2] == 1.0 && q.color[0] == 0.0
            })
            .count();
        assert_eq!(lines, usize::try_from(n).unwrap(), "{at}");
        hit_error_output(h, atlas, at);
    };
    graph_frames("C", skin, &chart, score, &check);
}

/// Skin D: a notes graph, a 600-column BPM graph, both visualizers with a
/// skin's usual fields and the six score bars, together within the
/// budget.
fn skin_d() {
    let mut b = ChartBuilder::new(150_000).bpm(TimeUs(1_000 * MS), 300_000);
    for k in 0..40_i64 {
        b = b.tap(
            lane(1 + u8::try_from(k % 7).unwrap()),
            TimeUs((100 + 50 * k) * MS),
        );
    }
    let mut chart = b.build().unwrap();
    chart.meta.length = bms_model::ChartSummary::of(&chart).length;
    let profile = bms_model::ChartProfile::of(&chart, &[]);
    let areas = [[-20, 20], [-60, 60], [-100, 100], [-150, 150], [-200, 200]];
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(tempo(150)),
        areas: Some(areas),
        ..PlayScore::NONE
    };
    let white = Rgba8::rgb(0xff_ffff, 0xff);
    let mut elements = vec![
        screen_el(Source::JudgeGraph(0), [0, 0, 400, 100]),
        screen_el(Source::BpmGraph(0), [0, 120, 600, 100]),
        screen_el(Source::TimingVisualizer(0), [0, 240, 301, 20]),
        screen_el(Source::HitErrorVisualizer(0), [0, 280, 300, 40]),
    ];
    let mut ratio_bars = Vec::new();
    for (i, rate) in ScoreRate::ALL.into_iter().enumerate() {
        ratio_bars.push(RatioBarDef {
            image: 0,
            form: RatioForm::Bar { vertical: false },
            source: RatioSource::Score(rate),
        });
        let y = 400 + 30 * i32::try_from(i).unwrap();
        elements.push(screen_el(
            Source::RatioBar(u32::try_from(i).unwrap()),
            [0, y, 168, 20],
        ));
    }
    let skin = graph_skin(
        elements,
        PlayDefs {
            ratio_bars,
            judge_graphs: vec![skin_model::JudgeGraphDef {
                kind: skin_model::JudgeGraphKind::Notes,
                delay: TimeUs(500 * MS),
                reverse: false,
                back: true,
                chips: 0,
                solid: 0,
                fit: skin_model::GraphFit::Stretch,
            }],
            bpm_graphs: vec![BpmGraphDef {
                delay: TimeUs(0),
                line_width: 2,
                colors: BpmColors::DEFAULT,
                solid: 0,
            }],
            timing_visualizers: vec![TimingVisualizerDef {
                judge_width: 150,
                width: 301,
                line_width: 1,
                line: Rgba8::rgb(0x00_ffff, 0xff),
                center: white,
                bands: [white; 5],
                decay: false,
                solid: 0,
            }],
            hit_errors: vec![HitErrorDef {
                width: 300,
                judge_width: 150,
                line_width: 2,
                window: 50,
                lines: true,
                band_colors: false,
                ema_mode: EmaMode::Both,
                alpha: 0.1,
                line: white,
                center: white,
                ema: Rgba8::rgb(0x00_ff00, 0xff),
                bands: [white; 5],
                decay: false,
                solid: 0,
            }],
        },
    );
    let check = |h: &Harness, atlas: &Atlas, at: &str| {
        let px = |x: f32, y: f32| graph_support::composite(h.batch.quads(), atlas, x, y, None);
        let near = |(rgb, covered): ([f32; 3], u32), want: [f32; 3]| {
            assert!(covered > 0, "{at}: nothing at {want:?}");
            graph_support::near(rgb, want, 1.0);
        };
        // The notes graph: 18 key taps in second 0 of 4 over 400 px, scale
        // 20 cells of 5 px: bin 0's bottom cell in `cccccc` at x 80, clear
        // of every cursor; not yet revealed at a round's READY (its 500 ms
        // reveal starts there).
        if !at.ends_with("second round, READY") {
            near(px(80.0, 97.5), [204.0; 3]);
        }
        // The BPM graph's MAIN row, green, at the left of its rect: 150
        // over 150 is row `⌊98/2⌋ = 49` of 100.
        near(px(0.5, 220.0 - 49.5), [0.0, 255.0, 0.0]);
        // The timing band, all white, its centre texel under the grid,
        // while no line crosses it.
        if h.ring_count == 0 {
            near(px(150.5, 250.0), [191.25; 3]);
            near(px(151.5, 250.0), [255.0; 3]);
        }
        // The hit-error pixmap, 300 texels over jw 150 (`x = 149 − ⌊v ·
        // 300 / 301⌋`): the average (the same offsets and weight as skin
        // C's) green over its texel, its triangle at the top from 4 left of
        // its tip (its first row 0.4 px tall: 100 rows over 40 px), and the
        // newest line white.
        let x_of = |v: i64| 149 + (-v.clamp(-150, 150) * 300) / 301;
        let ema_x = x_of(i64::from(ema_of(&ema_inputs(at)) as i32));
        near(px(ema_x as f32 + 0.5, 319.5), [0.0, 255.0, 0.0]);
        near(px((ema_x + 1 - 4) as f32 + 0.5, 280.2), [0.0, 255.0, 0.0]);
        if h.ring_count > 0 {
            let slot = usize::try_from((h.ring_count - 1) % 100).unwrap();
            let x = x_of(i64::from(h.ring[slot]));
            if (x - ema_x).abs() > 3 {
                near(px(x as f32 + 1.0, 319.5), [255.0; 3]);
            }
        }
        // Each score bar is drawn at its own rect.
        for i in 0..6_u8 {
            let y = 400.0 + 30.0 * f32::from(i);
            assert!(
                h.batch.quads().iter().any(|q| q.rect[1] == y),
                "{at}: bar {i}"
            );
        }
    };
    graph_frames("D", skin, &chart, score, &check);
}

/// The notes graph at its cap: a 1 022 s chart (1 024 seconds, 512 bins
/// of 2 s), every category, scale 100, the background, and the cursors.
/// Bin 2's fullest second, 5, holds one of each category: a scratch long
/// note's head (5.5 s) and the body of one from 4.2 s, a scratch tap, a
/// key long note's head and another's body likewise, a key tap and a mine;
/// second 10 (bin 5) holds 100 key taps, which set the scale.
fn skin_notes() {
    let mut b = ChartBuilder::new(150_000)
        .long(lane(0), TimeUs(4_200 * MS), TimeUs(5_300 * MS))
        .tap(lane(0), TimeUs(5_400 * MS))
        .long(lane(0), TimeUs(5_500 * MS), TimeUs(7_000 * MS))
        .long(lane(1), TimeUs(4_200 * MS), TimeUs(5_300 * MS))
        .long(lane(2), TimeUs(5_500 * MS), TimeUs(7_000 * MS))
        .tap(lane(3), TimeUs(5_400 * MS))
        .mine(lane(4), TimeUs(5_600 * MS), 10)
        .tap(lane(1), TimeUs(1_022_000 * MS));
    for k in 0..100_i64 {
        b = b.tap(
            lane(1 + u8::try_from(k % 7).unwrap()),
            TimeUs((10_000 + 10 * k) * MS),
        );
    }
    let mut chart = b.build().unwrap();
    chart.meta.length = bms_model::ChartSummary::of(&chart).length;
    let profile = bms_model::ChartProfile::of(&chart, &[]);
    assert_eq!((profile.bins().len(), profile.bin_seconds()), (512, 2));
    assert_eq!(profile.bins()[2], [1; 7]);
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let def = skin_model::JudgeGraphDef {
        kind: skin_model::JudgeGraphKind::Notes,
        delay: TimeUs(0),
        reverse: false,
        back: true,
        chips: 0,
        solid: 0,
        fit: skin_model::GraphFit::Stretch,
    };
    let skin = graph_skin(
        vec![screen_el(Source::JudgeGraph(0), [0, 0, 1_024, 500])],
        PlayDefs {
            judge_graphs: vec![def],
            ..PlayDefs::default()
        },
    );
    let colors: [u32; 7] = [
        0x44_ff44, 0x22_8822, 0xff_4444, 0x44_44ff, 0x22_2288, 0xcc_cccc, 0x88_0000,
    ];
    let check = |h: &Harness, atlas: &Atlas, at: &str| {
        // Bin 2 spans x 4 to 6; cell k of 100 is 5 px, its centre at
        // y = 497.5 − 5k.
        for (k, c) in colors.into_iter().enumerate() {
            let y = 497.5 - 5.0 * k as f32;
            let (rgb, covered) = graph_support::composite(h.batch.quads(), atlas, 5.0, y, None);
            assert!(covered > 0, "{at}: cell {k}");
            let want = [(c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff].map(|v| v as f32);
            graph_support::near(rgb, want, 1.0);
        }
        // Bin 5's hundred taps reach the top cell.
        let (rgb, _) = graph_support::composite(h.batch.quads(), atlas, 11.0, 2.5, None);
        graph_support::near(rgb, [204.0; 3], 1.0);
        // The cursors, full height in white, green or red: the play cursor
        // while the play runs, a practice round's two lines throughout it.
        let cursor = |c: [f32; 4]| {
            [
                [1.0, 1.0, 1.0],
                [128.0 / 255.0, 1.0, 128.0 / 255.0],
                [1.0, 128.0 / 255.0, 128.0 / 255.0],
            ]
            .iter()
            .any(|w| w.iter().zip(c).all(|(a, b)| (a - b).abs() < 1e-3))
        };
        let cursors = h
            .batch
            .quads()
            .iter()
            .filter(|q| q.rect[3] == 500.0 && cursor(q.color))
            .count();
        let want = if at.ends_with("second round, mid-play") {
            3
        } else if at.ends_with("second round, READY") {
            2
        } else if at.ends_with(": ready") || at.ends_with(": rearm") {
            0
        } else {
            1
        };
        assert_eq!(cursors, want, "{at}");
    };
    graph_frames("notes", skin, &chart, score, &check);
}

/// The one-hour case: 3 602 seconds take bins of 8 (`⌈3 602 / 512⌉`), 451
/// of them, and the graph draws the last one: a note at 3 600 s is in bin
/// 450, at 2 px a bin from x = 900.
fn one_hour() {
    let mut chart = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(100 * MS))
        .tap(lane(1), TimeUs(3_600_000 * MS))
        .build()
        .unwrap();
    chart.meta.length = bms_model::ChartSummary::of(&chart).length;
    let profile = bms_model::ChartProfile::of(&chart, &[]);
    assert_eq!(profile.bins().len(), 451);
    assert_eq!(profile.bin_seconds(), 8);
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let skin = graph_skin(
        vec![screen_el(Source::JudgeGraph(0), [0, 0, 902, 100])],
        PlayDefs {
            judge_graphs: vec![skin_model::JudgeGraphDef {
                kind: skin_model::JudgeGraphKind::Notes,
                delay: TimeUs(0),
                reverse: false,
                back: false,
                chips: 0,
                solid: 0,
                fit: skin_model::GraphFit::Stretch,
            }],
            ..PlayDefs::default()
        },
    );
    let check = |h: &Harness, _: &Atlas, at: &str| {
        assert_eq!(at_x(h, 900.0), 1, "{at}");
    };
    graph_frames("one hour", skin, &chart, score, &check);
}
