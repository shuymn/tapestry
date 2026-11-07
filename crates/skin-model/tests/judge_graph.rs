//! The result's judge graph through the production path (`Skin::new_result`
//! → `prepare_result` → `update_result` → `build_result`), its pixels
//! rasterized as the renderer draws them (`graph_support::composite`):
//! the chip grid's cells, level-0 sampling of a minified grid, the
//! background, the scale, the stacks and the reveal (design.md
//! "Rendering and Skins", result widgets).
//!
//! The canvas is the fixed 1920 × 1080 layout at scale 1, so canvas
//! points are target pixels. Expected colours are worked out in each test
//! over the compositor's grey backdrop (128).

mod graph_support;

use graph_support::{
    GFx, GRID, GRID_NO_GAP_X, GRID_NO_GAP_Y, GRID_NO_GAPS, composite, element, graph, level, lod,
    near, over_backdrop, screen_element, skin, view,
};
use skin_model::{
    ElementDef, Filter, JudgeGraphKind, LoadError, ResultDefs, ScreenKind, Skin, Source, TimerRef,
    result_timer,
};

const PG: [f32; 3] = [0.0, 136.0, 255.0];

/// A bin of `perfect` PERFECTs and nothing else.
fn perfects(perfect: u32) -> [u32; 9] {
    [perfect, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// The texels under quad `q`'s uv at level 0, as `#` (painted) and `.`
/// (transparent): its rows read from the bottom up along its first
/// column, and its columns along its bottom row.
fn cells(fx: &GFx, q: &skin_model::Quad) -> (String, String) {
    let side = fx.atlas.side() as f32;
    let [l, t, r, b] = q.uv.map(|v| (v * side).round() as u32);
    let page = usize::from(q.layer - 1);
    let px = fx.atlas.pages()[page].level(0).unwrap();
    let at = |x: u32, y: u32| {
        let i = ((y * fx.atlas.side() + x) * 4) as usize;
        let texel = &px[i..i + 4];
        match texel {
            [255, 255, 255, 255] => '#',
            [0, 0, 0, 0] => '.',
            other => panic!("texel {other:?} at ({x}, {y})"),
        }
    };
    let rows = (t..b).rev().map(|y| at(l, y)).collect();
    let columns = (l..r).map(|x| at(x, b - 1)).collect();
    (rows, columns)
}

/// A run of three chips on each of the four grids reads the source's
/// cells from the packed atlas: in each 5 × 5 cell, 4 or 5 texels lit up
/// from the bottom and 4 or 5 from the left (5 on the axis without a
/// gap), the rest transparent. A grid painted solid white fails.
#[test]
fn the_chip_grid_has_the_sources_cells() {
    let cases = [
        (GRID, "####.####.####.", "####."),
        (GRID_NO_GAP_X, "####.####.####.", "#####"),
        (GRID_NO_GAP_Y, "###############", "####."),
        (GRID_NO_GAPS, "###############", "#####"),
    ];
    for (grid, rows, columns) in cases {
        let s = skin(
            vec![screen_element(0, [0, 0, 100, 100], 255)],
            vec![graph(JudgeGraphKind::Judgment, grid, false, 0)],
        );
        let mut fx = GFx::new(s);
        fx.view(&view(vec![perfects(3)]));
        fx.frame(0);
        let chips = fx.chips();
        assert_eq!(chips.len(), 1, "grid {grid}");
        let got = cells(&fx, &chips[0]);
        assert_eq!(got, (rows.to_owned(), columns.to_owned()), "grid {grid}");
    }
}

/// A 40-cell graph (35 chips) and a 100-cell graph (150 chips, cut at
/// 100) in 82-pixel regions, A's height: the grid's 5 × 500 texels shrink
/// by a level of detail of about 1.29 and 2.61, so a nearest mipmap
/// filter would read levels 1 and 3, whose box filter blends the gap rows
/// into translucent ones. Each chip quad is `NearestBase`, read at level
/// 0: down a lit column every pixel is the chip colour or the backdrop
/// (a gap), both present; the same quads read at the levels a nearest
/// filter picks show translucent rows.
#[test]
fn a_minified_chip_grid_reads_level_zero() {
    let s = skin(
        vec![
            screen_element(0, [0, 0, 10, 82], 255),
            screen_element(0, [100, 0, 10, 82], 255),
        ],
        vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
    );
    let mut fx = GFx::new(s);
    for (count, x, lod_want, level_want) in [(35, 5.5, 1.29, 1), (150, 105.5, 2.61, 3)] {
        fx.view(&view(vec![perfects(count)]));
        fx.frame(0);
        let chips = fx.chips();
        let q = chips
            .iter()
            .find(|q| q.rect[0] <= x && x < q.rect[0] + q.rect[2])
            .unwrap();
        assert!((lod(q, fx.atlas.side()) - lod_want).abs() < 0.01, "{count}");
        assert_eq!(level(q, fx.atlas.side(), Filter::Nearest), level_want);
        assert_eq!(level(q, fx.atlas.side(), q.filter), 0);
        let [_, top, _, h] = q.rect;
        let (mut lit, mut gaps, mut blended) = (0, 0, 0);
        for row in top.ceil() as i32..(top + h).floor() as i32 {
            let y = row as f32 + 0.5;
            let got = composite(fx.quads(), &fx.atlas, x, y, None).0;
            if got.iter().zip(PG).all(|(g, w)| (g - w).abs() <= 1.0) {
                lit += 1;
            } else if got.iter().all(|g| (g - 128.0).abs() <= 1.0) {
                gaps += 1;
            } else {
                panic!("row {row} of {count}: {got:?}");
            }
            let nearest = composite(fx.quads(), &fx.atlas, x, y, Some(Filter::Nearest)).0;
            let pure = |c: [f32; 3]| {
                c.iter().zip(PG).all(|(g, w)| (g - w).abs() <= 1.0)
                    || c.iter().all(|g| (g - 128.0).abs() <= 1.0)
            };
            if !pure(nearest) {
                blended += 1;
            }
        }
        assert!(lit > 0 && gaps > 0, "{count}: {lit} lit, {gaps} gaps");
        assert!(blended > 0, "{count}: the nearest level shows no blend");
    }
}

/// The background of a 70-bin graph whose largest bin is 25 (scale 30),
/// in a 700 × 300 region (bins 10 wide, cells 10 tall), at alpha 255 and
/// 200: below cell 10 the base `000000cc`, cells 10-19 `111100`, 20-29
/// `232300`, each opaque; a line a fifth of a bin wide at bins 10 (and
/// every 10th) `1f1f1f` and at bins 0 and 60 `3f3f3f`, over the full
/// height. Each colour is composed once under the element's alpha, every
/// sampled point covered by one quad. At screen time 0 with a 500 ms
/// delay no chip draws and the background spans the region.
#[test]
fn the_background_is_the_sources_and_whole_during_the_reveal() {
    let s = skin(
        vec![
            screen_element(0, [0, 0, 700, 300], 255),
            screen_element(0, [0, 400, 700, 300], 200),
        ],
        vec![graph(JudgeGraphKind::Judgment, GRID, true, 500)],
    );
    let mut fx = GFx::new(s);
    let mut bins = vec![[0; 9]; 70];
    bins[3] = perfects(25);
    fx.view(&view(bins));
    fx.frame(0);
    assert!(fx.chips().is_empty());
    for (top, alpha) in [(0.0, 1.0), (400.0, 200.0 / 255.0)] {
        let cases = [
            // The base at cell 5, the bands at cells 15 and 25.
            (155.0, 245.0, [0.0; 3], 0.8),
            (155.0, 145.0, [17.0, 17.0, 0.0], 1.0),
            (155.0, 45.0, [35.0, 35.0, 0.0], 1.0),
            // Lines at bins 10, 20 (over the bands), 0 and 60.
            (101.0, 245.0, [31.0; 3], 1.0),
            (201.0, 45.0, [31.0; 3], 1.0),
            (1.0, 145.0, [63.0; 3], 1.0),
            (601.0, 5.0, [63.0; 3], 1.0),
            // Beside a line: the stripe.
            (102.5, 245.0, [0.0; 3], 0.8),
            // The corners.
            (0.5, 299.5, [63.0; 3], 1.0),
            (699.5, 0.5, [35.0, 35.0, 0.0], 1.0),
        ];
        for (x, y, rgb, a) in cases {
            let (got, covered) = composite(fx.quads(), &fx.atlas, x, top + y, None);
            near(got, over_backdrop(rgb, a * alpha), 2.0);
            assert_eq!(covered, 1, "({x}, {y})");
        }
        // Nothing outside the region.
        let (_, covered) = composite(fx.quads(), &fx.atlas, 700.5, top + 150.0, None);
        assert_eq!(covered, 0);
    }
    // Once revealed, bin 3's chips draw over the background.
    fx.frame(500);
    let (got, covered) = composite(fx.quads(), &fx.atlas, 31.0, 245.0, None);
    near(got, PG, 2.0);
    assert_eq!(covered, 2);
}

/// The scale is the step of the largest bin count, whatever the bins'
/// order: largest 20 → 20 cells, 21 → 30, 30 → 40, 95 → 100, 150 → 100;
/// bins [21, 30] and [30, 21] both → 40. A run of `k` chips is `k` cells
/// of `h / scale` from the bottom.
#[test]
fn the_scale_is_the_step_of_the_largest_bin() {
    let s = skin(
        vec![screen_element(0, [0, 0, 100, 400], 255)],
        vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
    );
    let mut fx = GFx::new(s);
    let cases: [(&[u32], u32); 7] = [
        (&[20], 20),
        (&[21], 30),
        (&[30], 40),
        (&[95], 100),
        (&[150], 100),
        (&[21, 30], 40),
        (&[30, 21], 40),
    ];
    for (counts, scale) in cases {
        fx.view(&view(counts.iter().map(|&c| perfects(c)).collect()));
        fx.frame(0);
        let first = fx.chips()[0];
        let k = counts[0].min(scale);
        let cell = 400.0 / scale as f32;
        assert!(
            (first.rect[3] - cell * k as f32).abs() < 1e-3,
            "{counts:?}: {:?}",
            first.rect
        );
        assert!((first.rect[1] + first.rect[3] - 400.0).abs() < 1e-3);
    }
}

/// The runs of one bin, bottom up: each its colour (8-bit RGB) and cells.
fn runs(fx: &GFx, scale: f32, h: f32) -> Vec<([u8; 3], u32)> {
    let mut chips = fx.chips();
    chips.sort_by(|a, b| b.rect[1].total_cmp(&a.rect[1]));
    chips
        .iter()
        .map(|q| {
            let rgb = [0, 1, 2].map(|c| (q.color[c] * 255.0).round() as u8);
            (rgb, (q.rect[3] / (h / scale)).round() as u32)
        })
        .collect()
}

/// A bin stacks its channels from the bottom in the source's order,
/// reversed with `orderReverse`, cut at the scale: the type-1 bin of
/// PERFECT 60, GREAT 30, GOOD 10, BAD 5 and POOR 8 (113, scale 100) shows
/// PERFECT ×60, GREAT ×30, GOOD ×10, or reversed POOR ×8, BAD ×5, GOOD
/// ×10, GREAT ×30, PERFECT ×47; a grade's early and late add up in the
/// type-1 graph. The type-2 bin of PERFECT 2, GREAT early 1, POOR early
/// 1, GREAT late 2, BAD late 1, POOR late 3, reversed, shows `442200` ×3,
/// `884400` ×1, `ff8800` ×2, `002244` ×1, `0088ff` ×1, `44ff44` ×2.
#[test]
fn each_bin_stacks_as_the_source_and_cuts_at_the_scale() {
    let def = |kind, reverse| skin_model::JudgeGraphDef {
        reverse,
        ..graph(kind, GRID, false, 0)
    };
    let s = skin(
        vec![screen_element(0, [0, 0, 10, 500], 255)],
        vec![def(JudgeGraphKind::Judgment, false)],
    );
    let mut fx = GFx::new(s);
    // GREAT, GOOD, BAD and POOR split between early and late.
    let bin = [60, 10, 4, 2, 3, 20, 6, 3, 5];
    fx.view(&view(vec![bin]));
    fx.frame(0);
    let blue = [0x00, 0x88, 0xff];
    let green = [0x00, 0xff, 0x88];
    let yellow = [0xff, 0xff, 0x00];
    let orange = [0xff, 0x88, 0x00];
    let red = [0xff, 0x00, 0x00];
    assert_eq!(
        runs(&fx, 100.0, 500.0),
        [(blue, 60), (green, 30), (yellow, 10)]
    );
    let s = skin(
        vec![screen_element(0, [0, 0, 10, 500], 255)],
        vec![def(JudgeGraphKind::Judgment, true)],
    );
    let mut fx = GFx::new(s);
    fx.view(&view(vec![bin]));
    fx.frame(0);
    assert_eq!(
        runs(&fx, 100.0, 500.0),
        [(red, 8), (orange, 5), (yellow, 10), (green, 30), (blue, 47)]
    );
    let s = skin(
        vec![screen_element(0, [0, 0, 10, 500], 255)],
        vec![def(JudgeGraphKind::EarlyLate, true)],
    );
    let mut fx = GFx::new(s);
    fx.view(&view(vec![[2, 1, 0, 0, 1, 2, 0, 1, 3]]));
    fx.frame(0);
    assert_eq!(
        runs(&fx, 20.0, 500.0),
        [
            ([0x44, 0x22, 0x00], 3),
            ([0x88, 0x44, 0x00], 1),
            ([0xff, 0x88, 0x00], 2),
            ([0x00, 0x22, 0x44], 1),
            ([0x00, 0x88, 0xff], 1),
            ([0x44, 0xff, 0x44], 2),
        ]
    );
}

/// The chips reveal on the screen timer, not the element's: with the
/// screen started at 3 s and the element's own timer (fade-out) at 5 s,
/// at 5.25 s they span the width (the element's timer would show half).
/// With the element's timer started with the screen, at screen time
/// `delay / 2` (3.25 s) they reach half the width, cut with their uv, and
/// at 3.5 s all of it; a view switch at 3.25 s continues the reveal. The
/// background is whole throughout.
#[test]
fn the_chips_reveal_on_the_screen_timer() {
    let s = skin(
        vec![element(0, [0, 0, 400, 100], 255, result_timer::FADEOUT)],
        vec![graph(JudgeGraphKind::Judgment, GRID, true, 500)],
    );
    let mut fx = GFx::new(s);
    fx.timers.screen = Some(bms_model::TimeUs(3_000_000));
    fx.timers.fadeout = Some(bms_model::TimeUs(5_000_000));
    let full = view(vec![perfects(5); 4]);
    fx.view(&full);
    let right = |fx: &GFx| {
        fx.chips()
            .iter()
            .map(|q| q.rect[0] + q.rect[2])
            .fold(0.0_f32, f32::max)
    };
    fx.frame(5_250);
    assert_eq!(right(&fx), 400.0);
    assert!(!fx.back().is_empty());
    fx.timers.fadeout = Some(bms_model::TimeUs(3_000_000));
    fx.frame(3_250);
    assert!((right(&fx) - 200.0).abs() < 1e-3, "{}", right(&fx));
    let other = view(vec![perfects(7); 4]);
    fx.view(&other);
    fx.frame(3_250);
    assert!((right(&fx) - 200.0).abs() < 1e-3);
    fx.frame(3_375);
    assert!((right(&fx) - 300.0).abs() < 1e-3);
    // A cut chip's uv is cut alike: bin 1 (100..200) at 150.
    let cut = fx.chips().into_iter().find(|q| q.rect[0] == 200.0).unwrap();
    assert!((cut.rect[2] - 100.0).abs() < 1e-3);
    fx.frame(3_500);
    assert_eq!(right(&fx), 400.0);
    // No positive delay: whole at once.
    let s = skin(
        vec![screen_element(0, [0, 0, 400, 100], 255)],
        vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
    );
    let mut fx = GFx::new(s);
    fx.view(&full);
    fx.frame(0);
    assert_eq!(right(&fx), 400.0);
}

/// A judge graph draws only on a result skin, its graph and images
/// exist, and each element is charged its worst quads: the background's
/// 582 when on, then 512 bins × 6 channels (type 1) or 10 (type 2).
#[test]
fn a_judge_graph_is_checked_and_charged_at_load() {
    let base = skin(Vec::new(), Vec::new()).quad_capacity();
    for (kind, back, charge) in [
        (JudgeGraphKind::Judgment, false, 3_072),
        (JudgeGraphKind::Judgment, true, 3_654),
        (JudgeGraphKind::EarlyLate, false, 5_120),
        (JudgeGraphKind::EarlyLate, true, 5_702),
    ] {
        let s = skin(
            vec![screen_element(0, [0, 0, 10, 10], 255)],
            vec![graph(kind, GRID, back, 0)],
        );
        assert_eq!(s.quad_capacity() - base, charge, "{kind:?} {back}");
    }
    let dir = tempfile::tempdir().unwrap();
    let def = |elements: Vec<ElementDef>| skin_model::SkinDef {
        fit: skin_model::Fit::Fixed,
        regions: [skin_model::RegionDef {
            anchor: skin_model::Anchor::TopLeft,
            optional: false,
        }; skin_model::Region::COUNT],
        whens: Vec::new(),
        sheet: graph_support::sheet_builder(dir.path()).finish(),
        lanes: None,
        elements,
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
    };
    let el = || screen_element(0, [0, 0, 10, 10], 255);
    let graphs = |g| ResultDefs {
        judge_graphs: vec![g],
    };
    let ok = graph(JudgeGraphKind::Judgment, GRID, true, 0);
    // Not on a play skin.
    let play = ElementDef {
        timer: TimerRef::Builtin(0),
        ..el()
    };
    assert!(matches!(
        Skin::new(def(vec![play])),
        Err(LoadError::SourceKind {
            element: 0,
            kind: ScreenKind::Play
        })
    ));
    // No graph 1.
    let missing = ElementDef {
        source: Source::JudgeGraph(1),
        ..el()
    };
    assert!(matches!(
        Skin::new_result(def(vec![missing]), graphs(ok)),
        Err(LoadError::UnknownWidget { element: 0 })
    ));
    // A missing image.
    for g in [
        skin_model::JudgeGraphDef { chips: 9, ..ok },
        skin_model::JudgeGraphDef { solid: 9, ..ok },
    ] {
        assert!(matches!(
            Skin::new_result(def(vec![el()]), graphs(g)),
            Err(LoadError::JudgeGraphImage { graph: 0 })
        ));
    }
}
