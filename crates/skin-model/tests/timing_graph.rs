//! The result's timing distribution through the production path
//! (`Skin::new_result` → `prepare_result` → `update_result` →
//! `build_result`), its pixels rasterized as the renderer draws them
//! (`graph_support::composite`): columns by ms, the judge areas, the
//! ticks, the mean and ±σ lines, the bars and their scale (design.md
//! "Rendering and Skins", result widgets).
//!
//! The canvas is the fixed 1920 × 1080 layout at scale 1, so canvas
//! points are target pixels. Expected colours are the source's layers
//! composed by straight-alpha over, then the element's alpha, over the
//! compositor's grey backdrop.

mod graph_support;

use graph_support::{GFx, SOLID, composite, near, over_backdrop, ready, skin_with, source_element};
use skin_model::{
    DEVIATION_SLOTS, ResultDefs, ResultStatus, ResultView, Rgba8, Source, TimingGraphDef,
    TimingStats, Unavailable, result_timer,
};

/// The surveyed skins' colours: bars `f5f5f5`, white lines, the default
/// opaque areas PERFECT `000088`, GREAT `008800`, GOOD `888800`, BAD
/// `880000`, POOR `000000`.
const COLORS: [Rgba8; 8] = [
    Rgba8::rgb(0xf5_f5f5, 0xff),
    Rgba8::rgb(0xff_ffff, 0xff),
    Rgba8::rgb(0xff_ffff, 0xff),
    Rgba8::rgb(0x00_0088, 0xff),
    Rgba8::rgb(0x00_8800, 0xff),
    Rgba8::rgb(0x88_8800, 0xff),
    Rgba8::rgb(0x88_0000, 0xff),
    Rgba8::rgb(0x00_0000, 0xff),
];
const PG: [f32; 3] = [0.0, 0.0, 136.0];
const GR: [f32; 3] = [0.0, 136.0, 0.0];
const GD: [f32; 3] = [136.0, 136.0, 0.0];
const BAR: [f32; 3] = [245.0; 3];
const WHITE: [f32; 3] = [255.0; 3];

/// 7-key windows: PERFECT ±20 ms, GREAT ±60, GOOD ±160, BAD ±220, the
/// empty POOR range −500 to +150.
const AREAS: [[i16; 2]; 5] = [[-20, 20], [-60, 60], [-160, 160], [-220, 220], [-500, 150]];

/// A distribution of `gx` columns.
fn graph(gx: u16) -> TimingGraphDef {
    TimingGraphDef {
        gx,
        colors: COLORS,
        average: true,
        dev: true,
        solid: SOLID,
    }
}

/// A skin of `graphs`, each element `(graph, rect)` at alpha 140 on the
/// screen timer.
fn fixture(graphs: Vec<TimingGraphDef>, elements: &[(u32, [i32; 4])]) -> GFx {
    let elements = elements
        .iter()
        .map(|&(i, r)| source_element(Source::TimingGraph(i), r, 140, result_timer::SCREEN))
        .collect();
    GFx::new(skin_with(
        elements,
        ResultDefs {
            timing_graphs: graphs,
            ..ResultDefs::default()
        },
    ))
}

/// A ready view with `slots` (ms, count) and the timing statistics
/// `mean_x100` and `sd_x100`, or none.
fn view(slots: &[(i32, u32)], stats: Option<(i32, u32)>) -> ResultView {
    let mut d = [0_u32; DEVIATION_SLOTS];
    for &(ms, n) in slots {
        d[usize::try_from(ms + 150).unwrap()] = n;
    }
    ResultView {
        timing: stats.map(|(mean_x100, sd_x100)| TimingStats {
            samples: 1,
            mean_x100,
            sd_x100,
        }),
        deviations: Some(Box::new(d)),
        timing_areas: Some(AREAS),
        ..ready()
    }
}

/// The RGB the frame shows at `(x, y)`, and the quads covering it.
fn pixel(fx: &GFx, x: f32, y: f32) -> ([f32; 3], u32) {
    composite(fx.quads(), &fx.atlas, x, y, None)
}

/// `rgb` under the element's alpha 140.
fn shown(rgb: [f32; 3]) -> [f32; 3] {
    over_backdrop(rgb, 140.0 / 255.0)
}

/// A 301-column graph over 301 × 100 at alpha 140 (a column a pixel, a
/// row 10 pixels at scale 10): the areas by column (GOOD to 89, GREAT 90
/// to 129, PERFECT 130 to 170, GREAT 171 to 210, GOOD from 211), each
/// sampled point covered once; ticks in rows 0 and 1 of every 10th
/// column, black at a quarter over GOOD 0.4460, 0.4460, 0.2264 and over
/// PERFECT 0.2264, 0.2264, 0.4460; the average at column 151 and the ±σ
/// lines at 154 and 148 for a mean of 1.40 ms and σ 2.60 ms; bars at ±149
/// and at the ±150 ms endpoint columns.
#[test]
fn the_distribution_places_and_layers_as_the_source() {
    let mut fx = fixture(vec![graph(301)], &[(0, [0, 0, 301, 100])]);
    fx.view(&view(
        &[(-150, 5), (-149, 5), (149, 5), (150, 5)],
        Some((140, 260)),
    ));
    fx.frame(0);
    let bands = [
        (0, GD),
        (89, GD),
        (90, GR),
        (129, GR),
        (130, PG),
        (150, PG),
        (170, PG),
        (171, GR),
        (210, GR),
        (211, GD),
        (300, GD),
    ];
    for (column, rgb) in bands {
        let (got, covered) = pixel(&fx, column as f32 + 0.5, 45.0);
        near(got, shown(rgb), 2.0);
        assert_eq!(covered, 1, "column {column}");
    }
    let tick_good = [0.4460, 0.4460, 0.2264].map(|c| c * 255.0);
    let tick_perfect = [0.2264, 0.2264, 0.4460].map(|c| c * 255.0);
    for y in [0.5, 19.5] {
        near(pixel(&fx, 80.5, y).0, tick_good, 2.0);
        near(pixel(&fx, 140.5, y).0, tick_perfect, 2.0);
    }
    near(pixel(&fx, 81.5, 5.0).0, shown(GD), 2.0);
    near(pixel(&fx, 80.5, 20.5).0, shown(GD), 2.0);
    for column in [148, 151, 154] {
        near(pixel(&fx, column as f32 + 0.5, 45.0).0, shown(WHITE), 2.0);
    }
    for column in [147, 149, 150, 152, 153, 155] {
        near(pixel(&fx, column as f32 + 0.5, 45.0).0, shown(PG), 2.0);
    }
    // Bars of 5 at scale 10: the bottom half of their columns.
    for column in [0, 1, 299, 300] {
        let x = column as f32 + 0.5;
        near(pixel(&fx, x, 50.5).0, shown(BAR), 2.0);
        near(pixel(&fx, x, 99.5).0, shown(BAR), 2.0);
        near(pixel(&fx, x, 49.5).0, shown(GD), 2.0);
    }
    assert_eq!(pixel(&fx, 301.5, 50.0).1, 0);
}

/// The scale is 10 rows, or the next step of 10 above the largest slot:
/// 10 → 10, 11 → 20, 250 → 260, read from the bar's top. It depends on
/// the largest slot alone: 21 at −1 ms and 30 at 0 ms, or 30 at −1 ms
/// and 21 at 0 ms, both scale 40, the bar of 30 three quarters of the
/// height.
#[test]
fn the_scale_is_the_step_of_the_largest_slot() {
    let mut fx = fixture(vec![graph(301)], &[(0, [0, 0, 301, 100])]);
    let x = 155.5;
    for (count, top) in [(10, 0.0), (11, 45.0), (250, 1_000.0 / 260.0)] {
        fx.view(&view(&[(5, count)], Some((0, 0))));
        fx.frame(0);
        near(pixel(&fx, x, top + 0.2).0, shown(BAR), 2.0);
        if top > 0.0 {
            near(pixel(&fx, x, top - 0.2).0, shown(PG), 2.0);
        }
    }
    for slots in [[(-1, 21), (0, 30)], [(-1, 30), (0, 21)]] {
        fx.view(&view(&slots, None));
        fx.frame(0);
        // Without timing statistics no bar draws; with them, scale 40.
        near(pixel(&fx, 150.5, 99.5).0, shown(PG), 2.0);
        fx.view(&view(&slots, Some((20_000, 0))));
        fx.frame(0);
        let thirty = 150.0 + f32::from(slots[0].1 == 21);
        near(pixel(&fx, thirty - 0.5, 25.5).0, shown(BAR), 2.0);
        near(pixel(&fx, thirty - 0.5, 24.5).0, shown(PG), 2.0);
    }
}

/// The scale comes from every slot, whatever range the graph shows: a
/// 101-column graph (±50 ms) with 25 at +100 ms, which it does not show,
/// and 3 at 0 has scale 30, its bar at 0 a tenth of the height. With two
/// texels a column (301 wide, 150 columns), the graph shows ±75 ms, each
/// column one ms: the centre at column 75, +74 ms at the last column,
/// −75 ms at the first.
#[test]
fn the_range_shown_follows_the_columns() {
    let mut fx = fixture(
        vec![graph(101), graph(150)],
        &[(0, [400, 0, 101, 100]), (1, [0, 200, 301, 100])],
    );
    fx.view(&view(
        &[(100, 25), (0, 3), (74, 2), (-75, 2)],
        Some((20_000, 0)),
    ));
    fx.frame(0);
    near(pixel(&fx, 450.5, 90.5).0, shown(BAR), 2.0);
    near(pixel(&fx, 450.5, 89.5).0, shown(PG), 2.0);
    assert_eq!(pixel(&fx, 501.5, 50.0).1, 0);
    let col = 301.0 / 150.0;
    let x = |j: f32| (j + 0.5) * col;
    near(pixel(&fx, x(75.0), 245.0).0, shown(PG), 2.0);
    near(pixel(&fx, x(74.0), 245.0).0, shown(PG), 2.0);
    near(pixel(&fx, x(149.0), 299.5).0, shown(BAR), 2.0);
    near(pixel(&fx, x(0.0), 299.5).0, shown(BAR), 2.0);
    near(pixel(&fx, x(1.0), 299.5).0, shown(GD), 2.0);
}

/// Without a sample the distribution draws its areas and ticks only: no
/// average or ±σ line and no bar. An unavailable view draws nothing.
#[test]
fn without_a_sample_only_areas_and_ticks_draw() {
    let mut fx = fixture(vec![graph(301)], &[(0, [0, 0, 301, 100])]);
    let mut v = view(&[(-149, 5), (1, 5)], None);
    fx.view(&v);
    fx.frame(0);
    for x in [150.5, 151.5] {
        near(pixel(&fx, x, 45.0).0, shown(PG), 2.0);
    }
    near(pixel(&fx, 151.5, 99.5).0, shown(PG), 2.0);
    near(pixel(&fx, 1.5, 99.5).0, shown(GD), 2.0);
    near(
        pixel(&fx, 80.5, 0.5).0,
        [0.4460, 0.4460, 0.2264].map(|c| c * 255.0),
        2.0,
    );
    v.status = ResultStatus::Unavailable(Unavailable::Failed);
    fx.view(&v);
    fx.frame(0);
    assert!(fx.quads().is_empty());
}

/// The worst frames stay within the graph's load-time charge of `5K + 11`
/// quads (`K = min(gx, 304 + ⌈gx/10⌉)`), every push accepted: every slot
/// holding a bar of its own height, both lines inside the areas, at 301
/// columns (charge 1 516) and at the 4 096-column limit (charge 3 581).
#[test]
fn the_worst_frames_fit_the_charge() {
    let mut fx = fixture(
        vec![graph(301), graph(4_096)],
        &[(0, [0, 0, 301, 100]), (1, [0, 200, 1_800, 100])],
    );
    let slots: Vec<(i32, u32)> = (-150..=150)
        .map(|ms| (ms, (ms + 151) as u32 % 37 + 1))
        .collect();
    fx.view(&view(&slots, Some((140, 1_260))));
    fx.frame(0);
    let quads = fx.quads();
    let n301 = quads.iter().filter(|q| q.rect[1] < 150.0).count();
    let n4096 = quads.len() - n301;
    assert!(n301 <= 1_516, "{n301}");
    assert!(n4096 <= 3_581, "{n4096}");
    assert!(n301 > 2 * 301, "{n301}");
}
