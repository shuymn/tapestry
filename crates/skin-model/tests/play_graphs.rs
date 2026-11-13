//! A play skin's notes graph, BPM graph and visualizers (stage 3, S10-10):
//! each through `Skin::new_play` → `prepare` → `build`, its output read as
//! composited pixels at chosen canvas points (design.md "Rendering and
//! Skins", play graphs and visualizers). Every expected colour and place is
//! worked out by hand in the test's comments, apart from the code.
//!
//! The skins are fixed canvases at scale 1 on a 1 920 × 1 080 target over
//! one white solid image, so a reference pixel is a canvas pixel.

mod graph_support;
mod play_support;

use bms_model::{
    Chart, ChartBuilder, ChartProfile, ChartSummary, Speed, Tempo, TimeUs, TimelineId,
};
use eval::Section;
use skin_model::{
    Anchor, Atlas, Attempt, Blend, BpmColors, BpmGraphDef, ElementDef, EmaMode, Filter, Fit,
    HitErrorDef, JudgeGraphDef, JudgeGraphKind, KeyDef, Place, PlayContext, PlayDefs, PlayScore,
    Region, RegionDef, Rgba8, Root, SheetBuilder, Skin, SkinDef, SolidImageDef, Source, TimerRef,
    TimingVisualizerDef, Track, Variant, timer,
};

use graph_support::{BACKDROP, composite_runs, near, over_backdrop};
use play_support::{GAUGES, Harness, MS, full, lane};

/// The white solid every graph tints.
const SOLID: u32 = 0;

/// A sheet of one white 8 × 8 solid.
fn sheet() -> skin_model::Sheet {
    let dir = tempfile::tempdir().unwrap();
    let mut b = SheetBuilder::new(&Root::new(dir.path()).unwrap());
    let i = b
        .add_solid_image(SolidImageDef {
            size: [8, 8],
            color: [255; 4],
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    assert_eq!(i, SOLID);
    b.finish()
}

/// An element drawing `source` over `r` on `screen`, its colour `rgb`, at
/// `alpha` and turned `angle` degrees about its top-left corner.
fn el_with(source: Source, r: [i32; 4], alpha: i32, rgb: [i32; 3], angle: i32) -> ElementDef {
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip(r) {
        *p = Some(v);
    }
    props[4] = Some(alpha);
    props[5] = Some(rgb[0]);
    props[6] = Some(rgb[1]);
    props[7] = Some(rgb[2]);
    props[8] = Some(angle);
    ElementDef {
        source,
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            skin_model::Ease::Linear,
            skin_model::End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place {
            pivot: Anchor::TopLeft,
            ..Place::SCREEN
        },
    }
}

/// [`el_with`] white, opaque and unturned.
fn el(source: Source, r: [i32; 4]) -> ElementDef {
    el_with(source, r, 255, [255; 3], 0)
}

/// A fixed-canvas play skin of `elements` drawing `defs`.
fn skin(elements: Vec<ElementDef>, defs: PlayDefs) -> Skin {
    Skin::new_play(
        SkinDef {
            fit: Fit::Fixed,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet: sheet(),
            lanes: None,
            elements,
            widgets: skin_model::WidgetDefs::default(),
            bomb_up_to: None,
            close: None,
            loadend: None,
            present: None,
        },
        defs,
    )
    .unwrap()
}

/// A skin's play and the atlas its frames composite over.
struct Fx {
    h: Harness,
    atlas: Atlas,
}

impl Fx {
    fn new(skin: Skin, chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        let atlas = skin
            .sheet()
            .atlas(Variant::Lo, play_support::LIMITS, &|| false)
            .unwrap();
        let mut h = Harness::new(skin, chart, ctx);
        h.uvs = atlas.uvs().clone();
        Self { h, atlas }
    }

    fn frame(&mut self, ms: i64) {
        self.h.frame(ms, &[]);
    }

    /// The RGB at canvas point `[x, y]` (0 to 255) and how many quads
    /// cover it.
    fn px(&self, x: f32, y: f32) -> ([f32; 3], u32) {
        composite_runs(&self.h.batch, &self.atlas, [x, y])
    }

    #[track_caller]
    fn is(&self, x: f32, y: f32, rgb: [f32; 3]) {
        near(self.px(x, y).0, rgb, 1.0);
    }

    #[track_caller]
    fn empty(&self, x: f32, y: f32) {
        assert_eq!(self.px(x, y).1, 0, "({x}, {y}) is drawn");
    }
}

fn rgb(c: u32) -> [f32; 3] {
    let [_, r, g, b] = c.to_be_bytes();
    [r, g, b].map(f32::from)
}

/// The context of the whole chart with `score`.
fn ctx(score: PlayScore<'_>) -> PlayContext<'_> {
    PlayContext { score, ..full(0) }
}

/// The whole chart with `score` and a 1 s READY.
fn ready(score: PlayScore<'_>) -> PlayContext<'_> {
    PlayContext {
        score,
        ..full(1_000)
    }
}

// ---------------------------------------------------------------------
// The notes graph.

/// A chart whose 1× seconds hold, by category: second 0 two scratch taps,
/// a key tap and a mine; second 1 thirty key taps; second 2 nothing (the
/// length 1.5 s gives `⌊1.5⌋ + 2 = 3` seconds).
fn notes_chart() -> Chart {
    let mut b = ChartBuilder::new(150_000)
        .tap(lane(0), TimeUs(100 * MS))
        .tap(lane(0), TimeUs(200 * MS))
        .tap(lane(1), TimeUs(300 * MS))
        .mine(lane(2), TimeUs(400 * MS), 10);
    for k in 0..30_i64 {
        b = b.tap(
            lane(1 + u8::try_from(k % 7).unwrap()),
            TimeUs((1_000 + 30 * k) * MS),
        );
    }
    let mut chart = b.build().unwrap();
    chart.meta.length = TimeUs(1_500 * MS);
    chart
}

fn notes_graph(reverse: bool) -> JudgeGraphDef {
    JudgeGraphDef {
        kind: JudgeGraphKind::Notes,
        delay: TimeUs(0),
        reverse,
        back: false,
        chips: SOLID,
        solid: SOLID,
        fit: skin_model::GraphFit::Stretch,
    }
}

/// C20: the notes graph stacks each bin's categories from the bottom in
/// the source's colours and order (reversed with `orderReverse`), scaled by
/// its largest bin: bins of 4 and 30 notes give a scale of 40 cells, so a
/// cell is 10 px of 400.
#[test]
fn the_notes_graph_stacks_its_categories() {
    let chart = notes_chart();
    let profile = ChartProfile::of(&chart, &[]);
    assert_eq!(
        profile.bins(),
        [[0, 0, 2, 0, 0, 1, 1], [0, 0, 0, 0, 0, 30, 0], [0; 7]]
    );
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    for reverse in [false, true] {
        let s = skin(
            vec![el(Source::JudgeGraph(0), [0, 0, 300, 400])],
            PlayDefs {
                judge_graphs: vec![notes_graph(reverse)],
                ..PlayDefs::default()
            },
        );
        // During READY, before the play cursor.
        let mut fx = Fx::new(s, &chart, &ready(score));
        fx.frame(-500);
        // Cell k of bin 0 is centred at y = 400 − 10k − 5.
        let cell = |k: f32| 400.0 - 10.0 * k - 5.0;
        let (scratch, key, mine) = (rgb(0xff_4444), rgb(0xcc_cccc), rgb(0x88_0000));
        let order = if reverse {
            [mine, key, scratch, scratch]
        } else {
            [scratch, scratch, key, mine]
        };
        for (k, want) in order.into_iter().enumerate() {
            fx.is(50.0, cell(k as f32), want);
        }
        fx.empty(50.0, cell(4.0));
        // Bin 1: 30 key taps, cells 0 to 29.
        fx.is(150.0, cell(29.0), key);
        fx.empty(150.0, cell(30.0));
        fx.empty(250.0, cell(0.0));
    }
}

/// A graph's reveal runs on the screen timer, which runs from the play
/// state's screen origin (docs/stage3.md D26, D3): after 1 234 ms of
/// loading, a 2 000 ms reveal has shown 61.7 % of the width at READY (to
/// x = 185 of 300), not none of it.
#[test]
fn a_graph_reveal_runs_on_the_screen() {
    let chart = notes_chart();
    let profile = ChartProfile::of(&chart, &[]);
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let s = skin(
        vec![el(Source::JudgeGraph(0), [0, 0, 300, 400])],
        PlayDefs {
            judge_graphs: vec![JudgeGraphDef {
                delay: TimeUs(2_000 * MS),
                ..notes_graph(false)
            }],
            ..PlayDefs::default()
        },
    );
    let mut fx = Fx::new(s, &chart, &ready(score));
    fx.h.screen_from = (-1_000 - 1_234) * MS;
    // READY's first frame.
    fx.frame(-1_000);
    let key = rgb(0xcc_cccc);
    // Bin 1's bottom cell, x 100 to 200: drawn up to x = 185 only.
    fx.is(150.0, 395.0, key);
    fx.is(180.0, 395.0, key);
    fx.empty(190.0, 395.0);
}

/// C20: the scale depends on the largest bin alone: `[21, 30]` and
/// `[30, 21]` both scale to 40.
#[test]
fn the_notes_graph_scale_is_order_independent() {
    for (first, second) in [(21, 30), (30, 21)] {
        let mut b = ChartBuilder::new(150_000);
        for (s, n) in [(0, first), (1, second)] {
            for k in 0..i64::from(n) {
                let at = 1_000 * s + 30 * k;
                b = b.tap(lane(1 + u8::try_from(k % 7).unwrap()), TimeUs(at * MS));
            }
        }
        let mut chart = b.build().unwrap();
        chart.meta.length = TimeUs(1_500 * MS);
        let profile = ChartProfile::of(&chart, &[]);
        let s = skin(
            vec![el(Source::JudgeGraph(0), [0, 0, 300, 400])],
            PlayDefs {
                judge_graphs: vec![notes_graph(false)],
                ..PlayDefs::default()
            },
        );
        let score = PlayScore {
            profile: Some(&profile),
            ..PlayScore::NONE
        };
        let mut fx = Fx::new(s, &chart, &ready(score));
        fx.frame(-500);
        for (bin, n) in [(0.0, first), (1.0, second)] {
            let x = 100.0 * bin + 50.0;
            let top = 400.0 - 10.0 * f32::from(u8::try_from(n).unwrap());
            fx.is(x, top + 5.0, rgb(0xcc_cccc));
            fx.empty(x, top - 5.0);
        }
    }
}

/// C21: the play cursor sits at the 1× chart time `t·W/(len·1000)`, 3/5 of
/// a second wide, while the play runs; in practice the chart time is the
/// play time times the speed, and the section's start (green) and end
/// (red) lines are drawn, each cut where a later one covers it.
#[test]
fn the_cursors_follow_the_1x_chart_time() {
    let chart = notes_chart();
    let profile = ChartProfile::of(&chart, &[]);
    // 3 seconds over 300 px: 100 px a second, a cursor 60 px wide. The
    // graph's top 50 px hold no chip.
    let s = || {
        skin(
            vec![el(Source::JudgeGraph(0), [0, 500, 300, 100])],
            PlayDefs {
                judge_graphs: vec![notes_graph(false)],
                ..PlayDefs::default()
            },
        )
    };
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let y = 505.0;
    let white = rgb(0xff_ffff);
    let mut fx = Fx::new(
        s(),
        &chart,
        &PlayContext {
            score,
            ..full(1_000)
        },
    );
    fx.frame(-500);
    fx.empty(130.0, y);
    fx.frame(1_000);
    fx.is(101.0, y, white);
    fx.is(159.0, y, white);
    fx.empty(99.0, y);
    fx.empty(161.0, y);
    fx.empty(90.0, y);
    // A whole chart has no section lines.
    fx.empty(30.0, y);

    // ×1.5: play time 1.0 s is 1× 1.5 s (x 150 to 210); the section from
    // 0.4 to 1.2 s of play time is 0.6 to 1.8 s at 1×: green 60 to 120,
    // red 180 to 240, cut by the play cursor to 210 to 240.
    let section = Section {
        start: TimeUs(400 * MS),
        end: TimeUs(1_200 * MS),
        lead_in: TimeUs(400 * MS),
        initial_gauges: GAUGES.iter().map(|g| g.start).collect(),
    };
    let practice = PlayContext {
        attempt: Attempt::Section(&section),
        score: PlayScore {
            speed: Speed::from_twentieths(30).unwrap(),
            ..score
        },
        ..full(0)
    };
    let mut fx = Fx::new(s(), &chart, &practice);
    fx.frame(1_000);
    fx.is(90.0, y, rgb(0x80_ff80));
    fx.empty(130.0, y);
    fx.is(151.0, y, white);
    fx.is(205.0, y, white);
    fx.is(215.0, y, rgb(0xff_8080));
    fx.is(239.0, y, rgb(0xff_8080));
    fx.empty(241.0, y);
}

/// C21: a cursor is drawn only inside the graph, as the source paints it
/// into the graph's own pixmap: at 2.9 s of 3 the play cursor covers 290 to
/// 300, not 350; past the chart (an audio tail) none is drawn.
#[test]
fn the_cursors_stay_inside_the_graph() {
    let chart = notes_chart();
    let profile = ChartProfile::of(&chart, &[]);
    let s = skin(
        vec![el(Source::JudgeGraph(0), [0, 500, 300, 100])],
        PlayDefs {
            judge_graphs: vec![notes_graph(false)],
            ..PlayDefs::default()
        },
    );
    let score = PlayScore {
        profile: Some(&profile),
        ..PlayScore::NONE
    };
    let mut fx = Fx::new(s, &chart, &ctx(score));
    let y = 505.0;
    fx.frame(2_900);
    fx.is(291.0, y, rgb(0xff_ffff));
    fx.is(299.5, y, rgb(0xff_ffff));
    fx.empty(300.5, y);
    fx.empty(340.0, y);
    let drawn = fx.h.batch.quads().len();
    fx.frame(3_500);
    fx.empty(355.0, y);
    assert_eq!(fx.h.batch.quads().len(), drawn - 1);
}

// ---------------------------------------------------------------------
// The BPM graph.

fn tempo(bpm: u128) -> Tempo {
    Tempo::from_units(bpm * Tempo::UNITS_PER_BPM).unwrap()
}

/// The 1× profile of `chart`, its length the summary's and every STOP at
/// `stops` (µs) a stopped row.
fn profile_of(chart: &mut Chart, stops: &[i64]) -> ChartProfile {
    chart.meta.length = ChartSummary::of(chart).length;
    let ids: Vec<TimelineId> = stops
        .iter()
        .map(|&at| {
            let i = chart
                .timeline
                .points()
                .iter()
                .position(|p| p.at == TimeUs(at))
                .unwrap();
            TimelineId(u32::try_from(i).unwrap())
        })
        .collect();
    ChartProfile::of(chart, &ids)
}

fn bpm_graph(line_width: u32) -> BpmGraphDef {
    BpmGraphDef {
        delay: TimeUs(0),
        line_width,
        colors: BpmColors::DEFAULT,
        solid: SOLID,
    }
}

/// 150 BPM, 300 from 1 s, a 0.5 s STOP at 2 s, 75 from 3 s, the last note
/// at 4 s; five notes at 150 make it MAIN.
fn tempo_chart() -> (Chart, ChartProfile) {
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(1_000 * MS), 300_000)
        .stop(TimeUs(2_000 * MS), TimeUs(500 * MS))
        .bpm(TimeUs(3_000 * MS), 75_000);
    for k in 1..=5 {
        b = b.tap(lane(1), TimeUs(100 * k * MS));
    }
    b = b
        .tap(lane(2), TimeUs(1_500 * MS))
        .tap(lane(3), TimeUs(3_500 * MS))
        .tap(lane(3), TimeUs(4_000 * MS));
    let mut chart = b.build().unwrap();
    let profile = profile_of(&mut chart, &[2_000 * MS]);
    (chart, profile)
}

/// C22: the BPM graph over 100 × 100 texels with 2-texel strokes. The
/// rows are 150 at 0, 300 at 1 000, stopped at 2 000, 75 at 3 000 and 75 at
/// 4 000 (the last point); `last` is `4 000 + 1 000`, so x is `⌊ms/50⌋`: 0,
/// 20, 40, 60, 80. y over MAIN 150 is `⌊(log10 r + log10 8)/(2 log10 8) ·
/// 98⌋`: 150 → 49, 300 → 65, stop (1/8) → 0, 75 → 32. The strokes: green
/// 150 over columns 0-21 rows 49-50; a grey change at 20-21 rows 51-64; red
/// 300 over 20-41 rows 65-66; a grey change at 40-41 rows 2-64; magenta stop
/// over 40-61 rows 0-1; a grey change at 60-61 rows 2-31; blue 75 (the
/// lowest) over 60-81 and, as the last row, 80-99, rows 32-33.
#[test]
fn the_bpm_graph_draws_the_sources_strokes() {
    let (chart, profile) = tempo_chart();
    let row = |ms, t| bms_model::SpeedRow { ms, tempo: t };
    assert_eq!(
        profile.speeds(),
        [
            row(0, Some(tempo(150))),
            row(1_000, Some(tempo(300))),
            row(2_000, None),
            row(3_000, Some(tempo(75))),
            row(4_000, Some(tempo(75))),
        ]
    );
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(tempo(150)),
        ..PlayScore::NONE
    };
    let s = |alpha| {
        skin(
            vec![el_with(
                Source::BpmGraph(0),
                [0, 0, 100, 100],
                alpha,
                [255; 3],
                0,
            )],
            PlayDefs {
                bpm_graphs: vec![bpm_graph(2)],
                ..PlayDefs::default()
            },
        )
    };
    let mut fx = Fx::new(s(255), &chart, &ctx(score));
    fx.frame(0);
    // Row r of column c is centred at (c + 0.5, 100 − r − 0.5).
    let at = |c: f32, r: f32| (c + 0.5, 99.5 - r);
    let check = |fx: &Fx, c, r, want: Option<u32>| {
        let (x, y) = at(c, r);
        match want {
            Some(w) => fx.is(x, y, rgb(w)),
            None => fx.empty(x, y),
        }
    };
    check(&fx, 10.0, 50.0, Some(0x00_ff00));
    check(&fx, 10.0, 60.0, None);
    check(&fx, 21.0, 58.0, Some(0x7f_7f7f));
    check(&fx, 30.0, 66.0, Some(0xff_0000));
    check(&fx, 41.0, 30.0, Some(0x7f_7f7f));
    check(&fx, 50.0, 1.0, Some(0xff_00ff));
    check(&fx, 61.0, 20.0, Some(0x7f_7f7f));
    check(&fx, 70.0, 33.0, Some(0x00_00ff));
    check(&fx, 90.0, 33.0, Some(0x00_00ff));
    check(&fx, 90.0, 35.0, None);

    // Under the destination's alpha ½ the overlapping strokes of 75 at
    // columns 80-81 compose once: ½, not ¾.
    let mut fx = Fx::new(s(128), &chart, &ctx(score));
    fx.frame(0);
    let half = 128.0 / 255.0;
    let (x, y) = at(80.0, 33.0);
    near(fx.px(x, y).0, over_backdrop(rgb(0x00_00ff), half), 1.0);
}

/// C22: both clamp ends and a ratio that is not a power of two: 150 at 0,
/// 173 at 1 000, 1 500 at 2 000 (ten times MAIN), the last note at 4 000.
/// y of 173 is `⌊(log10(173/150) + log10 8)/(2 log10 8) · 98⌋ = 52`; 1 500
/// clamps to 8, y 98 (unclamped it would be 103, off the pixmap). The
/// strokes: green 150 over 0-21 rows 49-50; grey 20-21 row 51; yellow 173
/// over 20-41 rows 52-53; grey 40-41 rows 54-97; red 1 500 (the highest)
/// over 40-99 rows 98-99.
#[test]
fn the_bpm_graph_clamps_both_ends() {
    let mut b = ChartBuilder::new(150_000)
        .bpm(TimeUs(1_000 * MS), 173_000)
        .bpm(TimeUs(2_000 * MS), 1_500_000);
    for k in 1..=5 {
        b = b.tap(lane(1), TimeUs(100 * k * MS));
    }
    b = b.tap(lane(1), TimeUs(4_000 * MS));
    let mut chart = b.build().unwrap();
    let profile = profile_of(&mut chart, &[]);
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(tempo(150)),
        ..PlayScore::NONE
    };
    let s = skin(
        vec![el(Source::BpmGraph(0), [0, 0, 100, 100])],
        PlayDefs {
            bpm_graphs: vec![bpm_graph(2)],
            ..PlayDefs::default()
        },
    );
    let mut fx = Fx::new(s, &chart, &ctx(score));
    fx.frame(0);
    let at = |c: f32, r: f32| (c + 0.5, 99.5 - r);
    let (x, y) = at(10.0, 49.0);
    fx.is(x, y, rgb(0x00_ff00));
    let (x, y) = at(21.0, 51.0);
    fx.is(x, y, rgb(0x7f_7f7f));
    let (x, y) = at(30.0, 52.0);
    fx.is(x, y, rgb(0xff_ff00));
    let (x, y) = at(30.0, 54.0);
    fx.empty(x, y);
    let (x, y) = at(41.0, 70.0);
    fx.is(x, y, rgb(0x7f_7f7f));
    let (x, y) = at(60.0, 98.0);
    fx.is(x, y, rgb(0xff_0000));
    let (x, y) = at(90.0, 99.0);
    fx.is(x, y, rgb(0xff_0000));
}

/// C22 (G8): a column holding four changes keeps its first three runs and
/// draws the rest of its span as one run in the transition colour: 150 at
/// 0, then 200, 300, 600 and 1 200 from 1 000 to 1 030 ms, all in column 20
/// (`⌊100 · 1 030 / 5 000⌋`), at rows 55, 65, 81 and 98. Column 20 composes
/// green 49-50, grey 51-54, yellow 55-56, grey 57-64, yellow 65-66, grey
/// 67-80, yellow 81-82, grey 83-97 and red 98-99: capped to the first three
/// and grey from row 57 to 99, so row 98 is grey there and red in column
/// 30, which only the last row reaches.
#[test]
fn a_busy_column_keeps_four_runs() {
    let mut b = ChartBuilder::new(150_000);
    for (k, bpm) in [200, 300, 600, 1_200].into_iter().enumerate() {
        let k = i64::try_from(k).unwrap();
        b = b.bpm(TimeUs((1_000 + 10 * k) * MS), bpm * 1_000);
    }
    for k in 1..=5 {
        b = b.tap(lane(1), TimeUs(100 * k * MS));
    }
    b = b.tap(lane(1), TimeUs(4_000 * MS));
    let mut chart = b.build().unwrap();
    let profile = profile_of(&mut chart, &[]);
    let score = PlayScore {
        profile: Some(&profile),
        main: Some(tempo(150)),
        ..PlayScore::NONE
    };
    let s = skin(
        vec![el(Source::BpmGraph(0), [0, 0, 100, 100])],
        PlayDefs {
            bpm_graphs: vec![bpm_graph(2)],
            ..PlayDefs::default()
        },
    );
    let mut fx = Fx::new(s, &chart, &ctx(score));
    fx.frame(0);
    let in_column = |c: f32| fx.h.batch.quads().iter().filter(|q| q.rect[0] == c).count();
    assert!(in_column(20.0) <= 4, "{}", in_column(20.0));
    assert_eq!(in_column(20.0), 4);
    fx.is(20.5, 99.5 - 98.0, rgb(0x7f_7f7f));
    fx.is(20.5, 99.5 - 55.0, rgb(0xff_ff00));
    fx.is(30.5, 99.5 - 98.0, rgb(0xff_0000));
}

/// C23: a chart without a note, or with fewer than two rows, draws no BPM
/// graph.
#[test]
fn a_bpm_graph_needs_notes_and_rows() {
    let s = || {
        skin(
            vec![el(Source::BpmGraph(0), [0, 0, 100, 100])],
            PlayDefs {
                bpm_graphs: vec![bpm_graph(2)],
                ..PlayDefs::default()
            },
        )
    };
    let mines = ChartBuilder::new(150_000)
        .bpm(TimeUs(1_000 * MS), 300_000)
        .mine(lane(1), TimeUs(2_000 * MS), 10)
        .build()
        .unwrap();
    let one_row = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(0))
        .build()
        .unwrap();
    for mut chart in [mines, one_row] {
        let profile = profile_of(&mut chart, &[]);
        let score = PlayScore {
            profile: Some(&profile),
            main: Some(tempo(150)),
            ..PlayScore::NONE
        };
        let mut fx = Fx::new(s(), &chart, &ctx(score));
        fx.frame(0);
        assert!(fx.h.batch.quads().is_empty());
    }
}

// ---------------------------------------------------------------------
// The timing visualizer.

const RED: u32 = 0xff_0000;
const GREEN: u32 = 0x00_ff00;
const BLUE: u32 = 0x00_00ff;
const YELLOW: u32 = 0xff_ff00;
const CYAN: u32 = 0x00_ffff;

/// The judge areas: PERFECT ±20, GREAT ±60, GOOD ±100, BAD ±150, the empty
/// POOR range ±200.
const AREAS: [[i16; 2]; 5] = [[-20, 20], [-60, 60], [-100, 100], [-150, 150], [-200, 200]];

/// A timing visualizer at jw 150 over 301 px (a ms a pixel), 1-px lines in
/// cyan at alpha 0x80, white centre, bands red, green, blue, yellow and a
/// clear empty POOR range.
fn timing_vis() -> TimingVisualizerDef {
    TimingVisualizerDef {
        judge_width: 150,
        width: 301,
        line_width: 1,
        line: Rgba8::rgb(CYAN, 0x80),
        center: Rgba8::rgb(0xff_ffff, 0xff),
        bands: [
            Rgba8::rgb(RED, 0xff),
            Rgba8::rgb(GREEN, 0xff),
            Rgba8::rgb(BLUE, 0xff),
            Rgba8::rgb(YELLOW, 0xff),
            Rgba8::rgb(0, 0),
        ],
        decay: false,
        solid: SOLID,
    }
}

fn areas_score() -> PlayScore<'static> {
    PlayScore {
        areas: Some(AREAS),
        ..PlayScore::NONE
    }
}

/// `rgb` under a quarter of black, as a grid line lies over a band.
fn gridded(c: [f32; 3]) -> [f32; 3] {
    c.map(|v| v * 0.75)
}

/// `line` at `a` over `under`, all 0 to 255.
fn line_over(line: [f32; 3], a: f32, under: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| line[i] * a + under[i] * (1.0 - a))
}

/// C24: the band is the centre texel, each window outward, and the grid
/// every 10th texel at a quarter of black, under the destination's colour;
/// each line is its own quad in the line colour at alpha `a·(i+1)/100`,
/// oldest first: two lines at +5 ms compose newer over older.
#[test]
fn the_timing_visualizer_bands_and_lines() {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let score = areas_score();
    let s = |rgb: [i32; 3], alpha| {
        skin(
            vec![el_with(
                Source::TimingVisualizer(0),
                [0, 200, 301, 20],
                alpha,
                rgb,
                0,
            )],
            PlayDefs {
                timing_visualizers: vec![timing_vis()],
                ..PlayDefs::default()
            },
        )
    };
    let mut fx = Fx::new(s([255; 3], 255), &chart, &ctx(score));
    fx.frame(0);
    let y = 210.0;
    // Texel 150 + v is ms v; the grid at every texel ≡ 0 (mod 10).
    fx.is(150.5, y, gridded(rgb(0xff_ffff)));
    fx.is(155.5, y, rgb(RED));
    fx.is(160.5, y, gridded(rgb(RED)));
    fx.is(131.5, y, rgb(RED));
    fx.is(125.5, y, rgb(GREEN));
    fx.is(51.5, y, rgb(BLUE));
    fx.is(1.5, y, rgb(YELLOW));
    fx.is(0.5, y, gridded(rgb(YELLOW)));
    // Empty ring: the band alone.
    let band_quads = fx.h.batch.quads().len();
    // Two lines at +5 ms: x = (301 − 1)/2 + 5 = 155.
    fx.h.record(5);
    fx.h.record(5);
    fx.frame(10);
    assert_eq!(fx.h.batch.quads().len(), band_quads + 2);
    let a = 128.0 / 255.0;
    let older = line_over(rgb(CYAN), a / 100.0 * 99.0, rgb(RED));
    fx.is(155.5, y, line_over(rgb(CYAN), a / 100.0 * 100.0, older));
    // Lines are emitted oldest first, each at its own age's alpha: +5 then
    // +10 give 155 at `a·99/100`, then 160 at `a`.
    let mut order = Fx::new(s([255; 3], 255), &chart, &ctx(score));
    order.h.record(5);
    order.h.record(10);
    order.frame(0);
    let lines: Vec<(f32, f32)> = order.h.batch.quads()[band_quads..]
        .iter()
        .map(|q| (q.rect[0], q.color[3]))
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].0, 155.0);
    assert!((lines[0].1 - 128.0 / 255.0 / 100.0 * 99.0).abs() < 1e-6);
    assert_eq!(lines[1].0, 160.0);
    assert!((lines[1].1 - 128.0 / 255.0).abs() < 1e-6);
    // A line out of range draws nothing.
    fx.h.record(151);
    fx.frame(20);
    assert_eq!(fx.h.batch.quads().len(), band_quads + 2);

    // A magenta destination tints the band, never the lines.
    let mut fx = Fx::new(s([255, 0, 255], 255), &chart, &ctx(score));
    fx.h.record(5);
    fx.frame(0);
    fx.is(150.5, 205.0, gridded(rgb(0xff_00ff)));
    fx.is(155.5, y, line_over(rgb(CYAN), a, rgb(RED)));
    // Destination alpha 0: the band is gone, the lines stay.
    let mut fx = Fx::new(s([255; 3], 0), &chart, &ctx(score));
    fx.h.record(5);
    fx.frame(0);
    fx.is(155.5, y, line_over(rgb(CYAN), a, [BACKDROP; 3]));
    fx.empty(150.5, y);
}

/// C24: a destination turned 90° about its top-left corner turns the band
/// about that corner; the lines stay unturned at their place.
#[test]
fn the_timing_band_turns_about_its_pivot_and_the_lines_do_not() {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let s = skin(
        vec![el_with(
            Source::TimingVisualizer(0),
            [400, 200, 301, 20],
            255,
            [255; 3],
            90,
        )],
        PlayDefs {
            timing_visualizers: vec![timing_vis()],
            ..PlayDefs::default()
        },
    );
    let mut fx = Fx::new(s, &chart, &ctx(areas_score()));
    fx.h.record(5);
    fx.frame(0);
    // Texel 155 at (555.5, 210) turns clockwise about (400, 200) to
    // (390, 355.5).
    fx.is(390.0, 355.5, rgb(RED));
    fx.is(
        555.5,
        210.0,
        line_over(rgb(CYAN), 128.0 / 255.0, [BACKDROP; 3]),
    );
}

// ---------------------------------------------------------------------
// The hit-error visualizer.

/// A hit-error visualizer 201 texels wide at jw 100 (a ms a texel, early
/// to the left: `x = 100 − v`), a 10-line window (20 rows), 1-texel lines.
fn hit_error(f: impl FnOnce(&mut HitErrorDef)) -> HitErrorDef {
    let mut d = HitErrorDef {
        width: 201,
        judge_width: 100,
        line_width: 1,
        window: 10,
        lines: true,
        band_colors: false,
        ema_mode: EmaMode::Off,
        alpha: 0.5,
        line: Rgba8::rgb(RED, 0xff),
        center: Rgba8::rgb(0, 0),
        ema: Rgba8::rgb(GREEN, 0xff),
        bands: [Rgba8::rgb(0, 0); 5],
        decay: false,
        solid: SOLID,
    };
    f(&mut d);
    d
}

/// A fixture drawing `defs` at `rects`, each with `alpha` and `angle`.
fn hit_fx(defs: Vec<HitErrorDef>, rects: &[[i32; 4]], alpha: i32, angle: i32) -> Fx {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let elements = (0..defs.len())
        .zip(rects)
        .map(|(i, &r)| {
            el_with(
                Source::HitErrorVisualizer(u32::try_from(i).unwrap()),
                r,
                alpha,
                [255; 3],
                angle,
            )
        })
        .collect();
    let s = skin(
        elements,
        PlayDefs {
            hit_errors: defs,
            ..PlayDefs::default()
        },
    );
    Fx::new(s, &chart, &ctx(areas_score()))
}

/// C25: two opaque lines at one place under the destination's alpha ½
/// give ½, not the ¾ of two quads.
#[test]
fn overlapping_hit_lines_compose_under_the_destination() {
    let mut fx = hit_fx(vec![hit_error(|_| {})], &[[0, 300, 201, 20]], 128, 0);
    fx.h.record(5);
    fx.h.record(5);
    fx.frame(0);
    near(
        fx.px(95.5, 305.0).0,
        over_backdrop(rgb(RED), 128.0 / 255.0),
        1.0,
    );
}

/// C25: by band, older lines over newer: over 101 texels at jw 50, a GOOD
/// line at 90 ms then a BAD one at 140 (both clamped to 50 ms, texel 0),
/// each at alpha ½, compose the older blue over the newer yellow: alpha
/// ¾, colour (85, 85, 170).
#[test]
fn translucent_hit_lines_compose_older_over_newer() {
    let mut fx = hit_fx(
        vec![hit_error(|d| {
            d.width = 101;
            d.judge_width = 50;
            d.band_colors = true;
            d.bands = [
                Rgba8::rgb(RED, 0x80),
                Rgba8::rgb(RED, 0x80),
                Rgba8::rgb(BLUE, 0x80),
                Rgba8::rgb(YELLOW, 0x80),
                Rgba8::rgb(0, 0),
            ];
        })],
        &[[0, 300, 101, 20]],
        255,
        0,
    );
    fx.h.record(90);
    fx.h.record(140);
    fx.frame(0);
    let a = 128.0 / 255.0;
    let both = a + a * (1.0 - a);
    let c = std::array::from_fn(|i| (rgb(BLUE)[i] * a + rgb(YELLOW)[i] * a * (1.0 - a)) / both);
    near(fx.px(0.5, 305.0).0, line_over(c, both, [BACKDROP; 3]), 1.5);
}

/// C25: by age in the line colour at `a·i/(wl/2)`, at most 1, and with
/// decay `2i` rows tall, centred: the oldest of ten (i = 1) at 0.2 over
/// rows 9-10.
#[test]
fn hit_lines_fade_and_shorten_with_age() {
    let mut fx = hit_fx(
        vec![hit_error(|d| d.decay = true)],
        &[[0, 300, 201, 20]],
        255,
        0,
    );
    for v in (-50..-40).rev() {
        fx.h.record(v);
    }
    fx.frame(0);
    // The oldest, -41 ms, is at texel 141.
    fx.is(141.5, 310.0, line_over(rgb(RED), 0.2, [BACKDROP; 3]));
    fx.empty(141.5, 308.5);
    // The newest, -50, at texel 150: full height, alpha clamped to 1.
    fx.is(150.5, 300.5, rgb(RED));
}

/// C25: the line's place truncates toward 0: at 200 texels over jw 100 a
/// ms is 200/201 of a texel, so +3 ms is `99 − 2` and −3 ms `99 + 2`
/// (floor would give 96 for +3).
#[test]
fn hit_line_places_truncate_toward_zero() {
    let mut fx = hit_fx(
        vec![hit_error(|d| d.width = 200)],
        &[[0, 300, 200, 20]],
        255,
        0,
    );
    fx.h.record(3);
    fx.h.record(-3);
    fx.frame(0);
    fx.is(97.5, 305.0, rgb(RED));
    fx.empty(96.5, 305.0);
    fx.is(101.5, 305.0, rgb(RED));
}

/// C25: the average at alpha ½ after one offset of 40 is 20, at texel 80:
/// mode 1 a full-height line, mode 2 a triangle from the top (half width
/// `⌊201/100⌋` made even, 2; depth `⌊20/3⌋`, 6 rows), mode 3 both.
#[test]
fn the_average_draws_its_line_and_triangle() {
    for mode in [EmaMode::Line, EmaMode::Triangle, EmaMode::Both] {
        let mut fx = hit_fx(
            vec![hit_error(|d| {
                d.lines = false;
                d.ema_mode = mode;
            })],
            &[[0, 300, 201, 20]],
            255,
            0,
        );
        fx.h.record(40);
        fx.frame(0);
        let line = matches!(mode, EmaMode::Line | EmaMode::Both);
        let triangle = matches!(mode, EmaMode::Triangle | EmaMode::Both);
        let lit = |fx: &Fx, x: f32, y: f32, on: bool| {
            if on {
                fx.is(x, y, rgb(GREEN));
            } else {
                fx.empty(x, y);
            }
        };
        lit(&fx, 80.5, 315.0, line);
        lit(&fx, 80.5, 306.5, line || triangle);
        lit(&fx, 79.5, 303.5, triangle);
        lit(&fx, 79.5, 304.5, false);
        lit(&fx, 78.5, 300.5, triangle);
        lit(&fx, 77.5, 300.5, false);
    }
}

/// C25: the whole pixmap turns about the destination's corner and takes its
/// colour: a red line at +5 (texel 95) turned 90° about (400, 300) lies at
/// (394.5, 395.5).
#[test]
fn the_hit_error_pixmap_turns_under_the_destination() {
    let mut fx = hit_fx(vec![hit_error(|_| {})], &[[400, 300, 201, 20]], 255, 90);
    fx.h.record(5);
    fx.frame(0);
    fx.is(394.5, 395.5, rgb(RED));
    fx.empty(495.5, 305.0);
}

/// The average after `values` strictly inside ±150, from 0, unrounded,
/// as the column [`ema_only`] draws it: truncated toward 0.
fn reference_ema(alpha: f32, values: impl IntoIterator<Item = i32>) -> i64 {
    let mut ema = 0.0_f64;
    for v in values {
        if -150 < v && v < 150 {
            ema += f64::from(alpha) * (f64::from(v) - ema);
        }
    }
    (ema * 2_001.0 / 2_001.0).trunc() as i64
}

/// An average-only visualizer over 2 001 texels at jw 1 000 (`x = 1 000 −
/// ema`) with weight `alpha`.
fn ema_only(alpha: f32) -> HitErrorDef {
    hit_error(|d| {
        d.width = 2_001;
        d.judge_width = 1_000;
        d.lines = false;
        d.ema_mode = EmaMode::Line;
        d.alpha = alpha;
    })
}

/// The average element `k` shows: its line's column.
fn shown_ema(fx: &Fx, k: usize) -> i64 {
    let q = fx.h.batch.quads()[k];
    1_000 - q.rect[0] as i64
}

/// C26: every entry still in the ring is consumed, in order, whatever the
/// frames: one a frame and all at once agree; exactly 100 new in a frame
/// are all taken; of 130 at once only the newest 100; an empty ring leaves
/// 0; two elements keep their own averages; a rearm starts over.
#[test]
fn each_average_consumes_the_ring_once() {
    let values: Vec<i32> = (0..60)
        .map(|k| (k * 37 % 200) - 100)
        .filter(|&v| v != 0)
        .collect();
    let rects = [[0, 300, 2_001, 20], [0, 400, 2_001, 20]];
    let two = || hit_fx(vec![ema_only(0.5), ema_only(0.25)], &rects, 255, 0);
    // Empty ring.
    let mut fx = two();
    fx.frame(0);
    assert_eq!((shown_ema(&fx, 0), shown_ema(&fx, 1)), (0, 0));
    // One a frame.
    for (i, &v) in values.iter().enumerate() {
        fx.h.record(v);
        fx.frame(i64::try_from(i).unwrap());
    }
    let want = [
        reference_ema(0.5, values.clone()),
        reference_ema(0.25, values.clone()),
    ];
    assert_eq!([shown_ema(&fx, 0), shown_ema(&fx, 1)], want);
    assert_ne!(want[0], want[1]);
    // All at once.
    let mut batch = two();
    for &v in &values {
        batch.h.record(v);
    }
    batch.frame(0);
    assert_eq!([shown_ema(&batch, 0), shown_ema(&batch, 1)], want);
    // Exactly 100 more in one frame: a whole revolution, all taken.
    let more: Vec<i32> = (0..100).map(|k| 90 - (k * 13 % 180)).collect();
    for &v in &more {
        fx.h.record(v);
    }
    fx.frame(100);
    let all = values.iter().chain(&more).copied();
    assert_eq!(shown_ema(&fx, 0), reference_ema(0.5, all));
    // 130 at once from a fresh state: the first 30 were overwritten.
    let many: Vec<i32> = (0..130).map(|k| (k * 29 % 260) - 130).collect();
    let mut fresh = two();
    for &v in &many {
        fresh.h.record(v);
    }
    fresh.frame(0);
    assert_eq!(
        shown_ema(&fresh, 0),
        reference_ema(0.5, many[30..].iter().copied())
    );
    // A rearm starts over at 0.
    fresh.h.play.rearm();
    fresh.h.clear_ring();
    fresh.frame(10);
    assert_eq!(shown_ema(&fresh, 0), 0);
}

/// C26: the average is kept unrounded. At the weight 0.1, five offsets of
/// +5 bring it to `5 · (1 − 0.9^5)` = 2.05: texel 998; a sixth, to 2.34;
/// four more, to 3.26: texel 997. Each step is below 1 ms, which a
/// per-step truncation would drop, leaving the line at 0.
#[test]
fn the_average_moves_by_steps_below_one_ms() {
    let mut fx = hit_fx(vec![ema_only(0.1)], &[[0, 300, 2_001, 20]], 255, 0);
    for (n, want) in [(5, 2), (1, 2), (4, 3)] {
        for _ in 0..n {
            fx.h.record(5);
        }
        fx.frame(0);
        assert_eq!(shown_ema(&fx, 0), want);
    }
    // −5 ten times from 0: −3.26, texel 1 003.
    let mut late = hit_fx(vec![ema_only(0.1)], &[[0, 300, 2_001, 20]], 255, 0);
    for _ in 0..10 {
        late.h.record(-5);
    }
    late.frame(0);
    assert_eq!(shown_ema(&late, 0), -3);
}

/// C26: any finite weight steps by plain arithmetic and the average is
/// drawn within the judge width. At 1e20 after 1 and then −2 it is held at
/// −2^53 (the value itself is pinned by `ema_step`'s unit tests in
/// `widget/hit_error.rs`), drawn at −1 000: texel 2 000. At −0.5 after 40
/// the average is −20: texel 1 020.
#[test]
fn extreme_weights_stay_finite() {
    for (alpha, values, want) in [(1e20_f32, vec![1, -2], -1_000), (-0.5, vec![40], -20)] {
        let mut fx = hit_fx(vec![ema_only(alpha)], &[[0, 300, 2_001, 20]], 255, 0);
        for v in values {
            fx.h.record(v);
            fx.frame(0);
        }
        assert_eq!(shown_ema(&fx, 0), want, "{alpha}");
    }
}
