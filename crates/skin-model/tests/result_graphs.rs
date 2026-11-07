//! Every result graph follows the view (design.md "Play Lifecycle", the
//! result's views): through `update_result`, the next `build_result`
//! draws the judge graph, the gauge graph and the timing distribution
//! from the new view's data, and an unavailable view's from none.

mod graph_support;

use graph_support::{GFx, GRID, SOLID, graph, near, ready, skin_with, source_element, timelines};
use rules::GaugeValue;
use skin_model::{
    DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeGraphColors, GaugeGraphDef, GaugeHistory,
    GaugeLook, JudgeGraphKind, ResultDefs, ResultStatus, ResultView, Rgba8, Source, TimingGraphDef,
    TimingStats, Unavailable, result_timer,
};

/// A ready view: `perfects` PERFECTs in one bin, the gauge flat at `pct`
/// percent, 5 samples at `ms`, and a PERFECT area of ±`perfect` ms with
/// each later area 20 ms wider.
fn view(perfects: u32, pct: i64, ms: usize, perfect: i16) -> ResultView {
    let v = GaugeValue(pct * GaugeValue::PERCENT);
    let mut deviations = [0; DEVIATION_SLOTS];
    deviations[150 + ms] = 5;
    ResultView {
        timelines: Some(timelines(vec![[perfects, 0, 0, 0, 0, 0, 0, 0, 0]])),
        gauge: Some(Box::new(GaugeHistory {
            columns: [GaugeColumn {
                low: v,
                high: v,
                last: v,
            }; GRAPH_COLUMNS],
            max: GaugeValue(100 * GaugeValue::PERCENT),
            border: GaugeValue(80 * GaugeValue::PERCENT),
            look: GaugeLook::Normal,
            reached: 288,
            no_input_from: None,
        })),
        deviations: Some(Box::new(deviations)),
        timing: Some(TimingStats {
            samples: 5,
            mean_x100: 20_000,
            sd_x100: 0,
        }),
        timing_areas: Some(std::array::from_fn(|k| {
            let k = i16::try_from(k).unwrap();
            [-perfect - 20 * k, perfect + 20 * k]
        })),
        ..ready()
    }
}

/// Three views in turn, then the first again: each build shows the
/// view it follows, the judge graph's chips as tall as its PERFECTs
/// (5-pixel cells), the gauge line at its value's row, the distribution's
/// bar in its column and its areas (+15 ms in PERFECT's colour under a
/// ±20 ms PERFECT, GREAT's under ±10 ms); the unavailable view draws no
/// graph.
#[test]
fn every_graph_follows_the_view() {
    let line = Rgba8::rgb(0xff_ffff, 0xff);
    let colors = [GaugeGraphColors {
        border_line: line,
        border_back: Rgba8::rgb(0, 0),
        line,
        back: Rgba8::rgb(0, 0),
    }; GaugeLook::COUNT];
    let el = |s, r| source_element(s, r, 255, result_timer::SCREEN);
    let mut fx = GFx::new(skin_with(
        vec![
            el(Source::JudgeGraph(0), [0, 0, 100, 100]),
            el(Source::GaugeGraph(0), [0, 200, 576, 102]),
            el(Source::TimingGraph(0), [0, 400, 301, 100]),
        ],
        ResultDefs {
            judge_graphs: vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
            gauge_graphs: vec![GaugeGraphDef {
                colors,
                solid: SOLID,
            }],
            timing_graphs: vec![TimingGraphDef {
                gx: 301,
                colors: [
                    Rgba8::rgb(0xff_ffff, 0xff),
                    Rgba8::rgb(0xff_ffff, 0xff),
                    Rgba8::rgb(0xff_ffff, 0xff),
                    Rgba8::rgb(0x00_00ff, 0xff),
                    Rgba8::rgb(0x00_ff00, 0xff),
                    Rgba8::rgb(0xff_0000, 0xff),
                    Rgba8::rgb(0x80_0080, 0xff),
                    Rgba8::rgb(0x00_0000, 0xff),
                ],
                average: false,
                dev: false,
                solid: SOLID,
            }],
        },
    ));
    let first = view(5, 50, 5, 20);
    let mut unavailable = view(9, 90, 9, 20);
    unavailable.status = ResultStatus::Unavailable(Unavailable::Failed);
    let views = [
        (&first, Some((25.0, 50, 5, [0.0, 0.0, 255.0]))),
        (
            &view(8, 20, 30, 10),
            Some((40.0, 20, 30, [0.0, 255.0, 0.0])),
        ),
        (&unavailable, None),
        (&first, Some((25.0, 50, 5, [0.0, 0.0, 255.0]))),
    ];
    for (v, want) in views {
        fx.view(v);
        fx.frame(1_500);
        let Some((chips, pct, ms, area)) = want else {
            assert!(fx.quads().is_empty());
            continue;
        };
        let c = fx.chips();
        assert_eq!(c.len(), 1);
        assert!((c[0].rect[3] - chips).abs() < 1e-3, "{:?}", c[0].rect);
        // The gauge line: rows `pct·(102 − 2)/100` and the next.
        let row = pct as f32 + 0.5;
        for (r, covered) in [(row, 1), (row + 1.0, 1), (row + 2.0, 0), (row - 1.0, 0)] {
            let n = fx.pixel([300.5, 302.0 - r]).1;
            assert_eq!(n, covered, "{pct}% row {r}");
        }
        // The bar of 5 at scale 10 in column `150 + ms`, nowhere else.
        let at = |x: f32, y: f32| fx.pixel([x, y]).0;
        let white = |x: f32| at(x, 499.5).iter().all(|&c| c > 254.0);
        assert!(white(150.5 + ms as f32));
        assert!(!white(153.5 + ms as f32));
        assert!(!white(147.5 + ms as f32));
        // The areas: +15 ms halfway up.
        near(at(165.5, 450.0), area, 1.0);
    }
}
