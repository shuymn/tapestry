//! The result's gauge graph through the production path
//! (`Skin::new_result` → `prepare_result` → `update_result` →
//! `build_result`), its pixels rasterized as the renderer draws them
//! (`graph_support::composite`): the back and line colours layered as the
//! source layers them under the element's alpha once, one colour per
//! stroke unless it crosses the border, strokes kept in their columns,
//! the reveal, the end of an aborted play and the no-input mark (design.md
//! "Rendering and Skins", result widgets).
//!
//! The canvas is the fixed 1920 × 1080 layout at scale 1, so canvas
//! points are target pixels; a graph's rows count up from its bottom.
//! Expected colours are the source's layers composed by straight-alpha
//! over, then the element's alpha, over the compositor's grey backdrop.

mod graph_support;

use bms_model::TimeUs;
use graph_support::{GFx, SOLID, composite, near, ready, skin_with, source_element};
use rules::GaugeValue;
use skin_model::{
    GRAPH_COLUMNS, GaugeColumn, GaugeGraphColors, GaugeGraphDef, GaugeHistory, GaugeLook,
    ResultDefs, ResultStatus, ResultTimers, ResultView, Rgba8, Source, Unavailable, result_timer,
};

/// `p` percent of the gauge.
fn pct(p: f64) -> GaugeValue {
    GaugeValue((p * GaugeValue::PERCENT as f64).round() as i64)
}

/// A column from `low` to `high` ending at `last`, in percent.
fn col(low: f64, high: f64, last: f64) -> GaugeColumn {
    GaugeColumn {
        low: pct(low),
        high: pct(high),
        last: pct(last),
    }
}

/// A flat column at `v` percent.
fn flat(v: f64) -> GaugeColumn {
    col(v, v, v)
}

/// A history of `columns` on a 100 % gauge whose border is 80 %, every
/// column covered.
fn history(columns: [GaugeColumn; GRAPH_COLUMNS], look: GaugeLook) -> GaugeHistory {
    GaugeHistory {
        columns,
        max: pct(100.0),
        border: pct(80.0),
        look,
        reached: 288,
        no_input_from: None,
    }
}

/// A ready view showing `gauge`.
fn view(gauge: GaugeHistory) -> ResultView {
    ResultView {
        gauge: Some(Box::new(gauge)),
        ..ready()
    }
}

/// The surveyed skins' colours (their named fields): translucent backs,
/// the end-threshold looks' above-border back `440000aa`, the survival
/// looks' back the same colour both sides; opaque lines, green below the
/// border and red at or above it.
fn surveyed() -> [GaugeGraphColors; GaugeLook::COUNT] {
    let t = |rgb| Rgba8::rgb(rgb, 0xaa);
    let backs = [
        0x44_0044, 0x00_4444, 0x00_4400, 0x44_0000, 0x44_4400, 0x44_4444,
    ];
    std::array::from_fn(|i| GaugeGraphColors {
        border_line: Rgba8::rgb(0xff_0000, 0xff),
        border_back: if i < 3 { t(0x44_0000) } else { t(backs[i]) },
        line: Rgba8::rgb(0x00_ff00, 0xff),
        back: t(backs[i]),
    })
}

/// [`surveyed`] with translucent lines: `ff000080` below the border,
/// `0000ff80` at or above it.
fn translucent() -> [GaugeGraphColors; GaugeLook::COUNT] {
    surveyed().map(|c| GaugeGraphColors {
        border_line: Rgba8::rgb(0x00_00ff, 0x80),
        line: Rgba8::rgb(0xff_0000, 0x80),
        ..c
    })
}

/// A skin of `graphs`, each element `(graph, rect, alpha)` on the screen
/// timer.
fn fixture(graphs: Vec<[GaugeGraphColors; 6]>, elements: &[(u32, [i32; 4], i32)]) -> GFx {
    let elements = elements
        .iter()
        .map(|&(i, r, a)| source_element(Source::GaugeGraph(i), r, a, result_timer::SCREEN))
        .collect();
    let gauge_graphs = graphs
        .into_iter()
        .map(|colors| GaugeGraphDef {
            colors,
            solid: SOLID,
        })
        .collect();
    GFx::new(skin_with(
        elements,
        ResultDefs {
            gauge_graphs,
            ..ResultDefs::default()
        },
    ))
}

/// The canvas point `x` from the left of `rect` and `row` up from its
/// bottom.
fn at([x0, y0, _, h]: [i32; 4], x: f32, row: f32) -> [f32; 2] {
    [x0 as f32 + x, (y0 + h) as f32 - row]
}

/// The RGB the frame shows at `p`, and the quads covering it.
fn pixel(fx: &GFx, [x, y]: [f32; 2]) -> ([f32; 3], u32) {
    composite(fx.quads(), &fx.atlas, x, y, None)
}

/// A colour in `0..=1` units as 8-bit units.
fn units(c: [f32; 3]) -> [f32; 3] {
    c.map(|v| v * 255.0)
}

/// With the surveyed colours, a 519 × 259 graph at alpha 200 and 255 (a
/// column 1.8 wide): below the border with no line 0.2395, 0.3789,
/// 0.2395; above it `440000aa` over `004400aa`, 0.2914, 0.1985, 0.1520
/// (0.2336, 0.1150, 0.0558 at 255), up to the top row; a HARD look's
/// back, one colour both sides, 0.3379, 0.1520, 0.1520; the green line
/// over the below-border back 0.0517, 0.8660, 0.0517; the red border
/// line at 80 % between the line's border row (`0.8·257`) and the back's
/// (`0.8·259`), over the below-border back, 0.8360, 0.0817, 0.0517. With
/// translucent lines on a 1152 × 50 graph (a column 4 wide): the corner
/// of a column crossing up from 20 % to 90 % over the above-border back;
/// a column crossing down from 95 % to 78.125 %, its corner below and
/// above the line's border row, its vertical alone, its horizontal alone.
#[test]
fn the_gauge_graph_layers_its_colours_as_the_source() {
    let big = |x| [x, 0, 519, 259];
    let small = |y| [0, y, 1_152, 50];
    let mut fx = fixture(
        vec![surveyed(), translucent()],
        &[
            (0, big(0), 200),
            (0, big(600), 255),
            (1, small(300), 200),
            (1, small(400), 255),
        ],
    );
    let mut columns = [flat(50.0); GRAPH_COLUMNS];
    columns[10] = col(20.0, 90.0, 90.0);
    columns[20] = col(78.125, 95.0, 78.125);
    columns[200] = flat(80.0);
    fx.view(&view(history(columns, GaugeLook::Normal)));
    fx.frame(1_500);
    // Column `c` of the 519-wide graph at its centre.
    let x519 = |c: f32| 519.0 * (c + 0.5) / 288.0;
    let cases_200 = [
        (x519(150.0), 50.5, [0.2395, 0.3789, 0.2395]),
        (x519(150.0), 230.5, [0.2914, 0.1985, 0.1520]),
        (x519(150.0), 258.5, [0.2914, 0.1985, 0.1520]),
        (x519(100.0), 129.5, [0.0517, 0.8660, 0.0517]),
        (x519(200.0), 206.5, [0.8360, 0.0817, 0.0517]),
    ];
    for (x, row, want) in cases_200 {
        near(pixel(&fx, at(big(0), x, row)).0, units(want), 2.0);
    }
    near(
        pixel(&fx, at(big(600), x519(150.0), 230.5)).0,
        units([0.2336, 0.1150, 0.0558]),
        2.0,
    );
    // The translucent lines: column 10 spans 40-44, column 20 80-84, each
    // vertical the first 2.
    let cases = [
        (
            41.0,
            43.5,
            [0.1196, 0.0814, 0.6521],
            [0.0579, 0.0285, 0.7658],
        ),
        (
            41.0,
            44.5,
            [0.1196, 0.0814, 0.6521],
            [0.0579, 0.0285, 0.7658],
        ),
        (
            81.0,
            37.75,
            [0.6880, 0.1554, 0.0982],
            [0.7935, 0.0856, 0.0415],
        ),
        (
            81.0,
            38.5,
            [0.4919, 0.1554, 0.2943],
            [0.5435, 0.0856, 0.2915],
        ),
        (
            81.0,
            39.75,
            [0.1452, 0.2297, 0.5389],
            [0.0833, 0.1719, 0.5853],
        ),
        (
            83.0,
            38.5,
            [0.5389, 0.2297, 0.1452],
            [0.5853, 0.1719, 0.0833],
        ),
    ];
    for (x, row, at_200, at_255) in cases {
        near(pixel(&fx, at(small(300), x, row)).0, units(at_200), 2.0);
        near(pixel(&fx, at(small(400), x, row)).0, units(at_255), 2.0);
    }
    // A HARD look: its back one translucent colour, over itself above
    // the border.
    fx.view(&view(history(columns, GaugeLook::Hard)));
    fx.frame(1_500);
    near(
        pixel(&fx, at(big(0), x519(150.0), 230.5)).0,
        units([0.3379, 0.1520, 0.1520]),
        2.0,
    );
}

/// Opaque lines on a 259-tall graph whose line's border row is
/// `0.8·257 = 205.6`: a column from 79.5 % down to 70 % (its vertical up
/// to row 206.3) is all line colour, its rows above the border's row
/// included; a column from 79 % to 81 % is split at that row, line
/// colour below and border-line colour above, its horizontal at 81 % in
/// the border-line colour.
#[test]
fn a_stroke_takes_one_colour_unless_it_crosses() {
    let r = [0, 0, 1_152, 259];
    let mut fx = fixture(vec![surveyed()], &[(0, r, 255)]);
    let mut columns = [flat(0.0); GRAPH_COLUMNS];
    columns[10] = col(70.0, 79.5, 70.0);
    columns[20] = col(79.0, 81.0, 81.0);
    fx.view(&view(history(columns, GaugeLook::Normal)));
    fx.frame(1_500);
    let green = [0.0, 255.0, 0.0];
    let red = [255.0, 0.0, 0.0];
    for row in [190.0, 205.0, 205.9, 206.2] {
        near(pixel(&fx, at(r, 41.0, row)).0, green, 1.0);
    }
    near(pixel(&fx, at(r, 81.0, 205.0)).0, green, 1.0);
    near(pixel(&fx, at(r, 81.0, 205.8)).0, red, 1.0);
    near(pixel(&fx, at(r, 83.0, 209.0)).0, red, 1.0);
}

/// A 519-wide graph (columns 1.8 wide, under the line's 2) whose columns
/// swing between 10 % and 90 %: every point is covered by the back once
/// and by at most one line piece, so no stroke reaches into the next
/// column. Stopped at an aborted play's column 100, nothing of the line
/// draws past it.
#[test]
fn the_line_stays_in_its_columns_and_stops_at_the_end() {
    let r = [0, 0, 519, 259];
    let mut fx = fixture(vec![surveyed()], &[(0, r, 255)]);
    let columns: [GaugeColumn; GRAPH_COLUMNS] = std::array::from_fn(|i| {
        if i % 2 == 0 {
            col(10.0, 90.0, 90.0)
        } else {
            col(10.0, 90.0, 10.0)
        }
    });
    fx.view(&view(history(columns, GaugeLook::Normal)));
    fx.frame(1_500);
    let rows = [20.3, 25.9, 100.5, 150.2, 231.6, 232.5];
    let mut lined = 0;
    for step in 0..(519 * 4) {
        let x = step as f32 / 4.0 + 0.125;
        for row in rows {
            let (_, covered) = pixel(&fx, at(r, x, row));
            assert!((1..=2).contains(&covered), "({x}, {row}): {covered}");
            lined += usize::from(covered == 2);
        }
    }
    assert!(lined > 519 * 4, "{lined}");
    // An aborted play: the line ends at column 100's right.
    let mut aborted = history(columns, GaugeLook::Normal);
    aborted.reached = 100;
    fx.view(&view(aborted));
    fx.frame(1_500);
    let end = 519.0 * 100.0 / 288.0;
    let covered = |x: f32, row: f32| pixel(&fx, at(r, x, row)).1;
    assert_eq!(covered(end - 0.5, 100.5), 2);
    for step in 0..400 {
        let x = end + 0.01 + step as f32;
        if x >= 519.0 {
            break;
        }
        for row in rows {
            assert_eq!(covered(x, row), 1, "({x}, {row})");
        }
    }
}

/// On a screen timer started at 3 s the line reveals left to right over
/// 1.5 s: none at 3 s, to half the width at 3.75 s, all of it at 4.5 s;
/// the back is whole from the first frame. A view switch at 3.75 s keeps
/// the reveal where it is.
#[test]
fn the_line_reveals_on_the_screen_timer() {
    let r = [0, 0, 576, 259];
    let mut fx = fixture(vec![surveyed()], &[(0, r, 255)]);
    fx.timers = ResultTimers {
        screen: Some(TimeUs(3_000_000)),
        fadeout: None,
    };
    fx.view(&view(history(
        [flat(50.0); GRAPH_COLUMNS],
        GaugeLook::Normal,
    )));
    let line = |fx: &GFx, x: f32| pixel(fx, at(r, x, 129.5)).1 == 2;
    fx.frame(3_000);
    assert!(!line(&fx, 0.5));
    for [x, row] in [[0.5, 0.5], [575.5, 0.5], [0.5, 258.5], [575.5, 258.5]] {
        assert_eq!(pixel(&fx, at(r, x, row)).1, 1);
    }
    fx.frame(3_750);
    assert!(line(&fx, 0.5) && line(&fx, 287.5));
    assert!(!line(&fx, 288.5));
    fx.view(&view(history(
        [flat(60.0); GRAPH_COLUMNS],
        GaugeLook::Normal,
    )));
    fx.frame(3_750);
    let new_line = |fx: &GFx, x: f32| pixel(fx, at(r, x, 154.7)).1 == 2;
    assert!(new_line(&fx, 287.5) && !new_line(&fx, 288.5));
    fx.frame(4_500);
    assert!(new_line(&fx, 575.5));
}

/// A what-if view whose input ends in column 144 of a 576-wide graph
/// (columns 2 wide) shows a white line one unit wide at that column's
/// left, over the full height; a view without the mark shows none.
#[test]
fn the_no_input_mark_sits_at_its_column() {
    let r = [0, 0, 576, 259];
    let mut fx = fixture(vec![surveyed()], &[(0, r, 255)]);
    let mut g = history([flat(50.0); GRAPH_COLUMNS], GaugeLook::Normal);
    g.no_input_from = Some(144);
    fx.view(&view(g));
    fx.frame(1_500);
    let white = [255.0; 3];
    for row in [0.5, 129.5, 230.5, 258.5] {
        near(pixel(&fx, at(r, 288.5, row)).0, white, 1.0);
    }
    for x in [287.5, 289.5] {
        let got = pixel(&fx, at(r, x, 50.5)).0;
        assert!(got.iter().any(|&c| c < 200.0), "{x}: {got:?}");
    }
    g.no_input_from = None;
    fx.view(&view(g));
    fx.frame(1_500);
    assert!(pixel(&fx, at(r, 288.5, 50.5)).0.iter().any(|&c| c < 200.0));
}

/// A ready view without an emphasized gauge draws no gauge graph at all,
/// its back included; nor does an unavailable view that has one.
#[test]
fn no_gauge_draws_nothing() {
    let mut fx = fixture(vec![surveyed()], &[(0, [0, 0, 519, 259], 255)]);
    fx.view(&ready());
    fx.frame(1_500);
    assert!(fx.quads().is_empty());
    let mut v = view(history([flat(50.0); GRAPH_COLUMNS], GaugeLook::Normal));
    fx.view(&v);
    fx.frame(1_500);
    assert!(!fx.quads().is_empty());
    v.status = ResultStatus::Unavailable(Unavailable::Failed);
    fx.view(&v);
    fx.frame(1_500);
    assert!(fx.quads().is_empty());
}

/// The worst frame stays within the graph's load-time charge of 1 443
/// quads (the back's 2, 5 a column, the mark) with every push accepted:
/// every column split at the border with its corner straddling it, on a
/// graph whose columns are wider than the line, and the no-input mark.
#[test]
fn the_worst_frame_fits_the_charge() {
    let mut fx = fixture(vec![translucent()], &[(0, [0, 0, 1_152, 259], 200)]);
    let mut g = history([col(10.0, 95.0, 79.9); GRAPH_COLUMNS], GaugeLook::Normal);
    g.no_input_from = Some(287);
    fx.view(&view(g));
    fx.frame(1_500);
    assert_eq!(fx.quads().len(), 1_443);
}
