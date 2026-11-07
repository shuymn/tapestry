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

/// Every texel under quad `q`'s uv at level 0, as `#` (painted) and `.`
/// (transparent): one string per row, top row first.
fn cells(fx: &GFx, q: &skin_model::Quad) -> Vec<String> {
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
    (t..b).map(|y| (l..r).map(|x| at(x, y)).collect()).collect()
}

/// Three cells of a grid lit `x` columns from the left and `y` rows up
/// from the bottom of each 5 × 5 cell, top row first.
fn mask(x: usize, y: usize) -> Vec<String> {
    let lit = "#".repeat(x) + &".".repeat(5 - x);
    let cell = (0..5).map(|row| {
        if row >= 5 - y {
            lit.clone()
        } else {
            ".....".to_owned()
        }
    });
    cell.clone().chain(cell.clone()).chain(cell).collect()
}

/// A run of three chips on each of the four grids reads the source's
/// cells from the packed atlas, every texel of each: in each 5 × 5 cell,
/// 4 or 5 rows lit up from the bottom and 4 or 5 columns from the left (5
/// on the axis without a gap), the rest transparent; read from the bottom
/// up, a column runs `####.####.####.` or `###############`. A grid
/// painted solid white, or a cell lit in any other shape, fails.
#[test]
fn the_chip_grid_has_the_sources_cells() {
    let cases = [
        (GRID, mask(4, 4)),
        (GRID_NO_GAP_X, mask(5, 4)),
        (GRID_NO_GAP_Y, mask(4, 5)),
        (GRID_NO_GAPS, mask(5, 5)),
    ];
    assert_eq!(
        mask(4, 4)[..5],
        [".....", "####.", "####.", "####.", "####."]
    );
    for (grid, want) in cases {
        let s = skin(
            vec![screen_element(0, [0, 0, 100, 100], 255)],
            vec![graph(JudgeGraphKind::Judgment, grid, false, 0)],
        );
        let mut fx = GFx::new(s);
        fx.view(&view(vec![perfects(3)]));
        fx.frame(0);
        let chips = fx.chips();
        assert_eq!(chips.len(), 1, "grid {grid}");
        assert_eq!(cells(&fx, &chips[0]), want, "grid {grid}");
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

/// The runs of one bin in a region from y 0 to `h`, bottom up: each its
/// colour (8-bit RGB) and cells. The runs must sit on one another from
/// the region's bottom, each a whole number of cells.
fn runs(fx: &GFx, scale: f32, h: f32) -> Vec<([u8; 3], u32)> {
    let mut chips = fx.chips();
    chips.sort_by(|a, b| b.rect[1].total_cmp(&a.rect[1]));
    let cell = h / scale;
    let mut bottom = h;
    chips
        .iter()
        .map(|q| {
            let [_, top, _, height] = q.rect;
            assert!(
                (top + height - bottom).abs() < 1e-3,
                "{:?} not on {bottom}",
                q.rect
            );
            bottom = top;
            let k = (height / cell).round();
            assert!((height - k * cell).abs() < 1e-3, "{:?}", q.rect);
            let rgb = [0, 1, 2].map(|c| (q.color[c] * 255.0).round() as u8);
            (rgb, k as u32)
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
    // At 3.3 s the reveal is at 240, inside bin 2 (200..300): its chips
    // are cut to 40 wide, their uv to the same 0.4 of bin 0's.
    fx.frame(3_300);
    let chips = fx.chips();
    let whole = chips.iter().find(|q| q.rect[0] == 0.0).unwrap();
    let cut = chips.iter().find(|q| q.rect[0] == 200.0).unwrap();
    assert!((cut.rect[2] - 40.0).abs() < 1e-3, "{:?}", cut.rect);
    let width = |q: &skin_model::Quad| q.uv[2] - q.uv[0];
    assert!((width(cut) - 0.4 * width(whole)).abs() < 1e-6);
    assert_eq!(
        (cut.uv[0], cut.uv[1], cut.uv[3]),
        (whole.uv[0], whole.uv[1], whole.uv[3])
    );
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

/// A minification the horizontal derivative dominates: one bin a pixel
/// wide over the grid's 5 texels (LOD about 2.32), three chips 150 pixels
/// tall over 15 texels (magnified). A nearest mipmap filter would read
/// level 2 from the horizontal step alone, blending; the `NearestBase`
/// quad reads level 0, every pixel down its column the chip colour or the
/// backdrop.
#[test]
fn a_horizontally_minified_chip_grid_reads_level_zero() {
    let s = skin(
        vec![screen_element(0, [0, 0, 1, 1_000], 255)],
        vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
    );
    let mut fx = GFx::new(s);
    fx.view(&view(vec![perfects(3)]));
    fx.frame(0);
    let q = fx.chips()[0];
    assert!((lod(&q, fx.atlas.side()) - 5.0_f32.log2()).abs() < 0.01);
    assert_eq!(level(&q, fx.atlas.side(), Filter::Nearest), 2);
    let [_, top, _, h] = q.rect;
    let (mut lit, mut gaps, mut blended) = (0, 0, 0);
    for row in top.ceil() as i32..(top + h).floor() as i32 {
        let y = row as f32 + 0.5;
        let got = composite(fx.quads(), &fx.atlas, 0.5, y, None).0;
        if got.iter().zip(PG).all(|(g, w)| (g - w).abs() <= 1.0) {
            lit += 1;
        } else if got.iter().all(|g| (g - 128.0).abs() <= 1.0) {
            gaps += 1;
        } else {
            panic!("row {row}: {got:?}");
        }
        let nearest = composite(fx.quads(), &fx.atlas, 0.5, y, Some(Filter::Nearest)).0;
        if !nearest.iter().zip(PG).all(|(g, w)| (g - w).abs() <= 1.0)
            && !nearest.iter().all(|g| (g - 128.0).abs() <= 1.0)
        {
            blended += 1;
        }
    }
    assert!(lit > 0 && gaps > 0, "{lit} lit, {gaps} gaps");
    assert!(blended > 0);
}

/// A 20-bin graph with its background and five chips in bin 3.
fn rotation_view() -> skin_model::ResultView {
    let mut bins = vec![[0; 9]; 20];
    bins[3] = perfects(5);
    bins[12] = [2, 1, 0, 0, 0, 0, 0, 0, 0];
    view(bins)
}

/// The track's angle turns the whole graph about the element's pivot in
/// its region, as the source turns its finished textures: a graph over
/// (400, 300, 200, 100) turned 90° clockwise about its centre (500, 350)
/// draws at each point `c + R(p − c)` what the unturned graph draws at
/// `p`, background, lines, chips and gaps alike. Every quad shares that
/// pivot and angle; turning each run about its own pivot fails.
#[test]
fn the_angle_turns_the_whole_graph_about_its_pivot() {
    use skin_model::Anchor;
    let def = || vec![graph(JudgeGraphKind::Judgment, GRID, true, 0)];
    let frame = |angle| {
        let e = graph_support::placed_element(0, [400, 300, 200, 100], angle, Anchor::Center, None);
        let mut fx = GFx::new(skin(vec![e], def()));
        fx.view(&rotation_view());
        fx.frame(0);
        fx
    };
    let (flat, turned) = (frame(0), frame(90));
    // Two stripes in two pieces each, two lines, three runs.
    assert_eq!(turned.quads().len(), 2 * 2 + 2 + 3);
    for q in turned.quads() {
        assert_eq!(q.pivot, [500.0, 350.0]);
        assert!((q.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }
    let mut checked = 0;
    for i in 0..40 {
        for j in 0..20 {
            let (x, y) = (400.3 + 5.0 * i as f32, 300.3 + 5.0 * j as f32);
            let want = composite(flat.quads(), &flat.atlas, x, y, None).0;
            let (dx, dy) = (x - 500.0, y - 350.0);
            let got = composite(turned.quads(), &turned.atlas, 500.0 - dy, 350.0 + dx, None).0;
            near(got, want, 0.5);
            checked += 1;
        }
    }
    assert_eq!(checked, 800);
}

/// The element's native size fits the whole graph as the source's
/// stretch fits its texture of 5 texels a bin and 5 a cell: 70 bins at
/// scale 30 are 350 × 150 texels. Exact (stretch 9) centres that size in
/// a 700 × 300 region, at (175, 75); no larger (stretch 8) shrinks it
/// into a 100 × 300 region, 100 × 42.86 at y 128.57. The lines, stripes
/// and chips follow the fitted region; nothing draws outside it.
#[test]
fn a_native_size_fits_the_whole_graph() {
    use skin_model::{Anchor, Native};
    let mut bins = vec![[0; 9]; 70];
    bins[3] = perfects(25);
    let frame = |r, native| {
        let e = graph_support::placed_element(0, r, 0, Anchor::TopLeft, Some(native));
        let mut fx = GFx::new(skin(
            vec![e],
            vec![graph(JudgeGraphKind::Judgment, GRID, true, 0)],
        ));
        fx.view(&view(bins.clone()));
        fx.frame(0);
        fx
    };
    let fx = frame([0, 0, 700, 300], Native::Exact([1, 1]));
    let at = |x: f32, y: f32| composite(fx.quads(), &fx.atlas, x, y, None);
    assert_eq!(at(170.0, 150.0).1, 0);
    assert_eq!(at(530.0, 150.0).1, 0);
    assert_eq!(at(300.0, 70.0).1, 0);
    assert_eq!(at(300.0, 230.0).1, 0);
    // The base at cell 5 beside the lines; the line at bin 10 (x 225).
    near(at(300.0, 197.5).0, over_backdrop([0.0; 3], 0.8), 2.0);
    near(at(225.5, 197.5).0, [31.0; 3], 2.0);
    near(at(180.0, 125.0).0, [17.0, 17.0, 0.0], 2.0);
    // Bin 3's chips: x 190..195, a lit texel column at 191.
    near(at(191.5, 222.0).0, PG, 2.0);
    let fx = frame([0, 0, 100, 300], Native::NoLarger([1, 1]));
    let at = |x: f32, y: f32| composite(fx.quads(), &fx.atlas, x, y, None).1;
    assert_eq!(at(50.0, 128.0), 0);
    assert_eq!(at(50.0, 129.5), 1);
    assert_eq!(at(50.0, 170.5), 1);
    assert_eq!(at(50.0, 172.0), 0);
}

/// Timelines of 1 s bins with their own notes per bin and an uncovered
/// mark.
fn timelines_of(
    width_s: i64,
    bins: Vec<[u32; 9]>,
    notes: Vec<u32>,
    uncovered: Option<u16>,
) -> skin_model::ResultView {
    skin_model::ResultView {
        timelines: Some(
            skin_model::Timelines::new(
                bms_model::TimeUs(width_s * 1_000_000),
                bins,
                notes,
                uncovered,
            )
            .unwrap(),
        ),
        ..graph_support::ready()
    }
}

/// The runs of the bin whose column starts at `x`, bottom up: colour and
/// cells, in a region from y `top` to `top + h` at `scale`.
fn runs_at(fx: &GFx, x: f32, scale: f32, [top, h]: [f32; 2]) -> Vec<([u8; 3], u32)> {
    let mut chips: Vec<_> = fx
        .chips()
        .into_iter()
        .filter(|q| q.rect[0] == x && q.rect[1] >= top && q.rect[1] < top + h)
        .collect();
    chips.sort_by(|a, b| b.rect[1].total_cmp(&a.rect[1]));
    let cell = h / scale;
    let mut bottom = top + h;
    chips
        .iter()
        .map(|q| {
            assert!(
                (q.rect[1] + q.rect[3] - bottom).abs() < 1e-3,
                "{:?}",
                q.rect
            );
            bottom = q.rect[1];
            let rgb = [0, 1, 2].map(|c| (q.color[c] * 255.0).round() as u8);
            (rgb, (q.rect[3] / cell).round() as u32)
        })
        .collect()
}

const GREY: [u8; 3] = [0x55; 3];

/// The widget's rule over aggregate timelines: each bin's notes less its
/// counted judgments draw grey, covered bins or not, and count in the
/// scale. Bin 0 (covered) holds 7 notes with 5 PERFECTs counted: 2 grey
/// chips under the PERFECTs (above them reversed). Bins 1 and 2 are past
/// the coverage: 4 and 25 notes, all grey; the 25 grey chips alone raise
/// the scale from 20 to 30. Which notes the counts hold (an LN's head bin,
/// a judgment finalized after the coverage, the conversion) is the data's
/// claim, tested upstream: `game`'s `notes_count_in_their_final_bins` and
/// `judgments_count_by_finalization_and_sit_in_their_notes_bins`, and
/// `player`'s `the_graph_data_converts_value_for_value`.
#[test]
fn every_unjudged_note_draws_grey_and_counts_in_the_scale() {
    let blue = [0x00, 0x88, 0xff];
    let mut pg = [0; 9];
    pg[0] = 5;
    let v = timelines_of(1, vec![pg, [0; 9], [0; 9]], vec![7, 4, 25], Some(1));
    for (reverse, bin0) in [
        (false, vec![(GREY, 2), (blue, 5)]),
        (true, vec![(blue, 5), (GREY, 2)]),
    ] {
        let def = skin_model::JudgeGraphDef {
            reverse,
            ..graph(JudgeGraphKind::Judgment, GRID, false, 0)
        };
        let mut fx = GFx::new(skin(
            vec![screen_element(0, [0, 0, 30, 300], 255)],
            vec![def],
        ));
        fx.view(&v);
        fx.frame(0);
        assert_eq!(runs_at(&fx, 0.0, 30.0, [0.0, 300.0]), bin0);
        assert_eq!(runs_at(&fx, 10.0, 30.0, [0.0, 300.0]), [(GREY, 4)]);
        assert_eq!(runs_at(&fx, 20.0, 30.0, [0.0, 300.0]), [(GREY, 25)]);
    }
    // Every note judged: no grey, and the scale stays 20.
    let v = timelines_of(1, vec![pg, [0; 9], [0; 9]], vec![5, 0, 0], Some(1));
    let mut fx = GFx::new(skin(
        vec![screen_element(0, [0, 0, 30, 300], 255)],
        vec![graph(JudgeGraphKind::Judgment, GRID, false, 0)],
    ));
    fx.view(&v);
    fx.frame(0);
    assert_eq!(runs_at(&fx, 0.0, 20.0, [0.0, 300.0]), [(blue, 5)]);
    assert_eq!(fx.chips().len(), 1);
}

/// A dense widened timeline (512 bins of 2 s, every channel 9 and 19
/// grey: 100 notes a bin, scale 100) under both graph types with their
/// backgrounds, on a batch the skin's load-time charge sizes: every push
/// is accepted, every bin draws all its runs (6 and 10), and each
/// background is 10 stripes in 52 pieces and 52 lines.
#[test]
fn a_dense_widened_timeline_builds_with_no_dropped_quad() {
    let v = timelines_of(2, vec![[9; 9]; 512], vec![100; 512], None);
    let s = skin(
        vec![
            screen_element(0, [0, 0, 1_024, 200], 255),
            screen_element(1, [0, 300, 1_024, 200], 255),
        ],
        vec![
            graph(JudgeGraphKind::Judgment, GRID, true, 0),
            graph(JudgeGraphKind::EarlyLate, GRID, true, 0),
        ],
    );
    assert_eq!(s.quad_capacity(), 3_654 + 5_702);
    let mut fx = GFx::new(s);
    fx.view(&v);
    fx.frame(0);
    assert_eq!(fx.batch.dropped(), 0);
    let back = 10 * 52 + 52;
    assert_eq!(fx.back().len(), 2 * back);
    assert_eq!(fx.chips().len(), 512 * 6 + 512 * 10);
    // Bin 0 of each graph: grey 19 at the bottom, then every channel.
    let one = runs_at(&fx, 0.0, 100.0, [0.0, 200.0]);
    assert_eq!(one.first(), Some(&(GREY, 19)));
    assert_eq!(
        one.iter().map(|r| r.1).collect::<Vec<_>>(),
        [19, 9, 18, 18, 18, 18]
    );
    let two = runs_at(&fx, 0.0, 100.0, [300.0, 200.0]);
    assert_eq!(two.first(), Some(&(GREY, 19)));
    assert_eq!(two.len(), 10);
}
