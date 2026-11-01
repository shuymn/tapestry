//! Regions on the canvas: layout validation, anchor shifts per aspect, the
//! side inset, the judge's inheritance from the playfield, the
//! translation clamp (each region's logical box keeps `MIN_VISIBLE`
//! pixels on the canvas per axis), hiding, controls and fills. Every
//! expected value is worked out by hand in a comment.

mod lanes_support;
mod play_support;

use bms_model::ChartBuilder;
use lanes_support::{WHITE, image, in_region, lanes_element, simple, skin_with};
use play_support::{Harness, REGIONS, full};
use skin_model::{
    Adjust, Anchor, Controls, ElementDef, Fill, Fit, Follow, LANE_WIDTH_PCT, Layout, LayoutError,
    MAX_INSET, MAX_OFFSET, MIN_VISIBLE, Place, Region, RegionDef, RegionPlacement, SCALE_PCT, Side,
    Skin, SkinDef,
};

/// `REGIONS` with `changes` applied.
fn regions(changes: &[(Region, Anchor, bool)]) -> [RegionDef; Region::COUNT] {
    let mut out = REGIONS;
    for &(r, anchor, optional) in changes {
        out[r.index()] = RegionDef { anchor, optional };
    }
    out
}

fn skin(regions: [RegionDef; Region::COUNT], elements: Vec<ElementDef>) -> Skin {
    skin_with(regions, None, elements).unwrap()
}

/// The quads' rects of `skin` at `target` with `layout`, in element order.
fn drawn(skin: Skin, target: [u32; 2], layout: Layout) -> Vec<[f32; 4]> {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    h.target = target;
    h.layout = layout;
    h.frame(0, &[]);
    h.batch.quads().iter().map(|q| q.rect).collect()
}

fn adjust(x: i32, y: i32, scale: u16) -> Adjust {
    Adjust {
        x,
        y,
        scale,
        visible: true,
    }
}

fn with(changes: &[(Region, Adjust)]) -> Layout {
    let mut l = Layout::AUTHOR;
    for &(r, a) in changes {
        l.set(r, a).unwrap();
    }
    l
}

#[test]
fn layout_values_hold_at_their_limits() {
    let mut l = Layout::AUTHOR;
    let r = Region::Top;
    for scale in [*SCALE_PCT.start(), *SCALE_PCT.end()] {
        assert_eq!(l.set(r, adjust(0, 0, scale)), Ok(()));
        assert_eq!(l.get(r).scale, scale);
    }
    assert_eq!((*SCALE_PCT.start(), *SCALE_PCT.end()), (50, 200));
    for scale in [49, 201] {
        assert_eq!(
            l.set(r, adjust(0, 0, scale)),
            Err(LayoutError::Scale { region: r })
        );
    }
    assert_eq!(MAX_OFFSET, 4_320);
    for v in [MAX_OFFSET, -MAX_OFFSET] {
        assert_eq!(l.set(r, adjust(v, v, 100)), Ok(()));
    }
    for (x, y) in [(MAX_OFFSET + 1, 0), (0, -MAX_OFFSET - 1)] {
        assert_eq!(
            l.set(r, adjust(x, y, 100)),
            Err(LayoutError::Offset { region: r })
        );
    }
    assert_eq!((*LANE_WIDTH_PCT.start(), *LANE_WIDTH_PCT.end()), (50, 200));
    for w in [50, 200] {
        assert_eq!(l.set_lane_width(w), Ok(()));
        assert_eq!(l.lane_width(), w);
    }
    for w in [49, 201] {
        assert_eq!(l.set_lane_width(w), Err(LayoutError::LaneWidth));
    }
    assert_eq!(MAX_INSET, 1_200);
    assert_eq!(l.set_inset(1_200), Ok(()));
    assert_eq!(l.set_inset(1_201), Err(LayoutError::Inset));
    assert_eq!(l.inset(), 1_200);
    assert_eq!(
        l.set(Region::Screen, Adjust::IDENTITY),
        Err(LayoutError::Screen)
    );
    let mut six = [
        (Region::Playfield, Adjust::IDENTITY),
        (Region::Judge, Adjust::IDENTITY),
        (Region::Left, Adjust::IDENTITY),
        (Region::Right, Adjust::IDENTITY),
        (Region::Top, Adjust::IDENTITY),
        (Region::Bottom, Adjust::IDENTITY),
    ];
    assert_eq!(Layout::new(six, 100, 0), Ok(Layout::AUTHOR));
    six[5].0 = Region::Top;
    assert_eq!(
        Layout::new(six, 100, 0),
        Err(LayoutError::Duplicate {
            region: Region::Top
        })
    );
    six[5].0 = Region::Screen;
    assert_eq!(Layout::new(six, 100, 0), Err(LayoutError::Screen));

    // `reset` restores one region and leaves the others; the playfield's
    // takes the lane width with it.
    let mut l = with(&[
        (Region::Top, adjust(5, 6, 120)),
        (Region::Playfield, adjust(7, 8, 90)),
    ]);
    l.set_lane_width(150).unwrap();
    l.reset(Region::Top);
    assert_eq!(l.get(Region::Top), Adjust::IDENTITY);
    assert_eq!(l.get(Region::Playfield), adjust(7, 8, 90));
    assert_eq!(l.lane_width(), 150);
    l.reset(Region::Playfield);
    assert_eq!(l, Layout::AUTHOR);
}

/// A HUD box `[1700, 100, 200, 50]` in `top`, every region anchored at
/// `anchor`, drawn at `target` with `layout`.
fn hud(anchor: Anchor, target: [u32; 2], layout: Layout) -> [f32; 4] {
    let regions = lanes_support::anchored(anchor);
    let el = image(WHITE, [1_700, 100, 200, 50], in_region(Region::Top));
    drawn(skin(regions, vec![el]), target, layout)[0]
}

#[test]
fn anchors_shift_by_their_fraction_of_the_extra_space() {
    // (target, extra width, extra height): 21:9 is 2560×1080 (+640, 0),
    // 32:9 3840×1080 (+1920, 0), 16:10 1920×1200 (0, +120), 4:3 a
    // 1920×1440 canvas (0, +360). W/N take none, C half, E/S all.
    let cases = [
        ([1_920, 1_080], 0.0, 0.0),
        ([2_560, 1_080], 640.0, 0.0),
        ([3_840, 1_080], 1_920.0, 0.0),
        ([1_920, 1_200], 0.0, 120.0),
        ([1_440, 1_080], 0.0, 360.0),
    ];
    for (target, ex, ey) in cases {
        for anchor in Anchor::ALL {
            let [fx, fy] = anchor.halves().map(|h| f32::from(h) / 2.0);
            let want = [1_700.0 + fx * ex, 100.0 + fy * ey, 200.0, 50.0];
            assert_eq!(
                hud(anchor, target, Layout::AUTHOR),
                want,
                "{anchor:?} {target:?}"
            );
        }
    }
    // The literal shifts.
    assert_eq!(
        hud(Anchor::Right, [2_560, 1_080], Layout::AUTHOR)[0],
        2_340.0
    );
    assert_eq!(
        hud(Anchor::Center, [2_560, 1_080], Layout::AUTHOR)[0],
        2_020.0
    );
    assert_eq!(
        hud(Anchor::Right, [3_840, 1_080], Layout::AUTHOR)[0],
        3_620.0
    );
    assert_eq!(
        hud(Anchor::Bottom, [1_920, 1_200], Layout::AUTHOR)[1],
        220.0
    );
    assert_eq!(
        hud(Anchor::Center, [1_920, 1_200], Layout::AUTHOR)[1],
        160.0
    );
    assert_eq!(
        hud(Anchor::Bottom, [1_440, 1_080], Layout::AUTHOR)[1],
        460.0
    );
}

fn inset(px: u16) -> Layout {
    let mut l = Layout::AUTHOR;
    l.set_inset(px).unwrap();
    l
}

#[test]
fn the_side_inset_narrows_the_anchoring_width() {
    // 32:9: 1920 extra. Inset 600: W +600, E 600 + (3840 − 1200 − 1920)
    // = +1320, C 600 + 360 = +960.
    let x = |a, l| hud(a, [3_840, 1_080], l)[0] - 1_700.0;
    assert_eq!(x(Anchor::Left, inset(600)), 600.0);
    assert_eq!(x(Anchor::Right, inset(600)), 1_320.0);
    assert_eq!(x(Anchor::Center, inset(600)), 960.0);
    // The largest inset, 1200, is clamped to (3840 − 1920) / 2 = 960: every
    // anchor lands on the centred 16:9 area.
    for a in [Anchor::Left, Anchor::Right, Anchor::Center] {
        assert_eq!(x(a, inset(MAX_INSET)), 960.0, "{a:?}");
    }
    // At 959 the clamp is not reached: W +959, E +961.
    assert_eq!(x(Anchor::Left, inset(959)), 959.0);
    assert_eq!(x(Anchor::Right, inset(959)), 961.0);
    // No effect at 16:9.
    assert_eq!(hud(Anchor::Left, [1_920, 1_080], inset(600))[0], 1_700.0);
    // `screen` never moves by the inset.
    let regions = lanes_support::anchored(Anchor::Left);
    let el = image(WHITE, [1_700, 100, 200, 50], Place::SCREEN);
    assert_eq!(
        drawn(skin(regions, vec![el]), [3_840, 1_080], inset(600))[0][0],
        1_700.0
    );
}

#[test]
fn pixel_sizes_of_one_aspect_draw_the_same_canvas() {
    let layout = with(&[(Region::Top, adjust(-37, 21, 130))]);
    let build = |target| {
        let el = image(WHITE, [1_700, 100, 200, 50], in_region(Region::Top));
        let s = skin(lanes_support::anchored(Anchor::Center), vec![el]);
        assert_eq!(s.canvas(target).size(), [1_920.0, 1_080.0]);
        drawn(s, target, layout)
    };
    let base = build([1_920, 1_080]);
    for target in [[3_840, 2_160], [2_560, 1_440]] {
        let got = build(target);
        let bits = |v: &Vec<[f32; 4]>| v.iter().map(|r| r.map(f32::to_bits)).collect::<Vec<_>>();
        assert_eq!(bits(&got), bits(&base), "{target:?}");
    }
    let s = skin(REGIONS, Vec::new());
    assert_eq!(s.canvas([1_920, 1_080]).scale(), 1.0);
    assert_eq!(s.canvas([3_840, 2_160]).scale(), 2.0);
    assert_eq!(s.canvas([2_560, 1_440]).scale(), (4.0_f64 / 3.0) as f32);
}

/// A playfield of the simple lanes ([160, 80]–[704, 960]) and a judge box
/// `[860, 500, 200, 80]`, the judge anchored C, the playfield `play`.
fn judged_skin(play: Anchor) -> Skin {
    let regions = regions(&[
        (Region::Judge, Anchor::Center, false),
        (Region::Playfield, play, false),
    ]);
    let judge = image(WHITE, [860, 500, 200, 80], in_region(Region::Judge));
    skin_with(regions, Some(simple()), vec![lanes_element(), judge]).unwrap()
}

/// The judge's rect (the last quad).
fn judge_at(target: [u32; 2], layout: Layout, play: Anchor) -> [f32; 4] {
    *drawn(judged_skin(play), target, layout).last().unwrap()
}

#[test]
fn the_judge_moves_with_the_playfield_after_its_own_scale() {
    // 150 % about its centre (960, 540): 860 → 960 − 150 = 810, 500 →
    // 540 − 60 = 480.
    let judge = adjust(0, 0, 150);
    let l = with(&[(Region::Judge, judge)]);
    assert_eq!(
        judge_at([1_920, 1_080], l, Anchor::Left),
        [810.0, 480.0, 300.0, 120.0]
    );
    // The playfield moved (+100, −40): the judge by exactly as much.
    let l = with(&[
        (Region::Judge, judge),
        (Region::Playfield, adjust(100, -40, 100)),
    ]);
    assert_eq!(
        judge_at([1_920, 1_080], l, Anchor::Left),
        [910.0, 440.0, 300.0, 120.0]
    );
    // The playfield's scale and lane width do not reach it.
    let l = with(&[
        (Region::Judge, judge),
        (Region::Playfield, adjust(0, 0, 80)),
    ]);
    assert_eq!(
        judge_at([1_920, 1_080], l, Anchor::Left),
        [810.0, 480.0, 300.0, 120.0]
    );
    let mut l = with(&[(Region::Judge, judge)]);
    l.set_lane_width(150).unwrap();
    assert_eq!(
        judge_at([1_920, 1_080], l, Anchor::Left),
        [810.0, 480.0, 300.0, 120.0]
    );
    // At 21:9 the judge takes the playfield's shift (W: 0), not its own
    // (C: 320); with the playfield at E, 640.
    let l = with(&[(Region::Judge, judge)]);
    assert_eq!(
        judge_at([2_560, 1_080], l, Anchor::Left),
        [810.0, 480.0, 300.0, 120.0]
    );
    assert_eq!(
        judge_at([2_560, 1_080], l, Anchor::Right),
        [1_450.0, 480.0, 300.0, 120.0]
    );
    // Its own move adds to the playfield's: judge [1500, 1800] at +100 and
    // the playfield at +100 on 1920 put it at 1700, 200 right.
    let regions = regions(&[(Region::Judge, Anchor::Left, false)]);
    let judge = image(WHITE, [1_500, 500, 300, 80], in_region(Region::Judge));
    let s = skin_with(regions, Some(simple()), vec![lanes_element(), judge]).unwrap();
    let l = with(&[
        (Region::Judge, adjust(100, 0, 100)),
        (Region::Playfield, adjust(100, 0, 100)),
    ]);
    let p = s.placement(&s.canvas([1_920, 1_080]), &l, Side::Left);
    assert_eq!(p.region(Region::Playfield).affine.offset, [100.0, 0.0]);
    assert_eq!(p.region(Region::Judge).affine.offset, [200.0, 0.0]);
}

/// A region's logical box on the canvas as `[x0, x1]`.
fn xs(p: &skin_model::Placement, r: Region) -> [f32; 2] {
    let [x, _, w, _] = p.region(r).bounds.unwrap();
    [x, x + w]
}

#[test]
fn the_judge_and_the_playfield_are_clamped_jointly() {
    // Width 1920. The playfield authored over x [−192, 64] at 50 % about
    // its left edge: [−192, −64] unmoved, so P = [64 + 64, 1920 − 64 +
    // 192] = [128, 2048]. The judge over [100, 200]: J = [64 − 200, 1920
    // − 64 − 100] = [−136, 1756]. A local request of +1756 is clamped to
    // Jhi − Plo = 1628, and the playfield to P ∩ (J − 1628) = [128, 128].
    let regions = regions(&[(Region::Judge, Anchor::Left, false)]);
    let play = image(WHITE, [-192, 80, 256, 880], in_region(Region::Playfield));
    let judge = image(WHITE, [100, 500, 100, 80], in_region(Region::Judge));
    let s = skin(regions, vec![play, judge]);
    let l = with(&[
        (Region::Playfield, adjust(0, 0, 50)),
        (Region::Judge, adjust(1_756, 0, 100)),
    ]);
    let p = s.placement(&s.canvas([1_920, 1_080]), &l, Side::Left);
    // Playfield: −192·0.5 + (−192)·0.5 + 128 = −64; judge 100 + 1628 + 128.
    assert_eq!(xs(&p, Region::Playfield), [-64.0, 64.0]);
    assert_eq!(xs(&p, Region::Judge), [1_856.0, 1_956.0]);
    // Both overlap by exactly 64.
    assert_eq!(xs(&p, Region::Playfield)[1], MIN_VISIBLE as f32);
    assert_eq!(1_920.0 - xs(&p, Region::Judge)[0], 64.0);
}

#[test]
fn the_judges_range_counts_the_inherited_shift() {
    // 2560 wide, the playfield anchored E (+640) over [160, 704], the
    // judge over [860, 1060] requesting +4320. With the shift, J = [64 −
    // 1700, 2560 − 64 − 1500] = [−1636, 996] and P = [64 − 1344, 2560 −
    // 64 − 800] = [−1280, 1696]: the local move is 996 + 1280 = 2276 and
    // the playfield's −1280, so the judge's left edge is 860 + 640 + 2276
    // − 1280 = 2496, 64 inside the right edge.
    let regions = regions(&[
        (Region::Judge, Anchor::Left, false),
        (Region::Playfield, Anchor::Right, false),
    ]);
    let judge = image(WHITE, [860, 500, 200, 80], in_region(Region::Judge));
    let s = skin_with(regions, Some(simple()), vec![lanes_element(), judge]).unwrap();
    let l = with(&[(Region::Judge, adjust(MAX_OFFSET, 0, 100))]);
    let p = s.placement(&s.canvas([2_560, 1_080]), &l, Side::Left);
    assert_eq!(xs(&p, Region::Judge), [2_496.0, 2_696.0]);
    assert_eq!(xs(&p, Region::Playfield), [-480.0, 64.0]);
}

/// A box over `x` in `top`, its anchor top left, moved by `dx` at `scale`.
fn moved(x: [i32; 2], dx: i32, scale: u16) -> [f32; 2] {
    let el = image(WHITE, [x[0], 100, x[1] - x[0], 50], in_region(Region::Top));
    let s = skin(REGIONS, vec![el]);
    let l = with(&[(Region::Top, adjust(dx, 0, scale))]);
    let p = s.placement(&s.canvas([1_920, 1_080]), &l, Side::Left);
    xs(&p, Region::Top)
}

#[test]
fn translations_keep_64_pixels_of_each_box_on_the_canvas() {
    // The simple playfield [160, 704] moved −4000 keeps its right edge at
    // 64, +4000 its left edge at 1920 − 64.
    let s = skin_with(REGIONS, Some(simple()), vec![lanes_element()]).unwrap();
    let c = s.canvas([1_920, 1_080]);
    for (dx, want) in [(-4_000, [-480.0, 64.0]), (4_000, [1_856.0, 2_400.0])] {
        let l = with(&[(Region::Playfield, adjust(dx, 0, 100))]);
        assert_eq!(
            xs(&s.placement(&c, &l, Side::Left), Region::Playfield),
            want
        );
    }
    // Vertically: [80, 960] moved down 4000 keeps 64 rows.
    let l = with(&[(Region::Playfield, adjust(0, 4_000, 100))]);
    let [_, y, _, _] = s
        .placement(&c, &l, Side::Left)
        .region(Region::Playfield)
        .bounds
        .unwrap();
    assert_eq!(y, 1_080.0 - 64.0);
    // A request inside the range is taken as it is.
    assert_eq!(moved([1_700, 1_900], 100, 100), [1_800.0, 2_000.0]);
    // 200 % near the right edge: [1800, 2000] about its left edge, kept to
    // 1856.
    assert_eq!(moved([1_800, 1_900], 4_000, 200), [1_856.0, 2_056.0]);
    // A box narrower than 64 stays whole: [100, 140] ends at [1880, 1920]
    // or [0, 40].
    assert_eq!(moved([100, 140], 4_000, 100), [1_880.0, 1_920.0]);
    assert_eq!(moved([100, 140], -4_000, 100), [0.0, 40.0]);
    // A box the author placed with only 50 px on the canvas: its range is
    // widened to include 0, so a move left keeps the author's place and a
    // move right is taken.
    assert_eq!(moved([-50, 50], -100, 100), [-50.0, 50.0]);
    assert_eq!(moved([-50, 50], 100, 100), [50.0, 150.0]);
}

#[test]
fn hidden_regions_and_offered_controls() {
    let regions = regions(&[(Region::Top, Anchor::TopLeft, true)]);
    let el = |r| image(WHITE, [10, 10, 10, 10], in_region(r));
    let s = skin_with(
        regions,
        Some(simple()),
        vec![
            lanes_element(),
            el(Region::Judge),
            el(Region::Top),
            el(Region::Bottom),
            el(Region::Playfield),
        ],
    )
    .unwrap();
    let hide = Adjust {
        visible: false,
        ..Adjust::IDENTITY
    };
    let count = |l: Layout| {
        let s = skin_with(
            regions,
            None,
            vec![
                el(Region::Judge),
                el(Region::Top),
                el(Region::Bottom),
                el(Region::Playfield),
            ],
        )
        .unwrap();
        drawn(s, [1_920, 1_080], l).len()
    };
    assert_eq!(count(Layout::AUTHOR), 4);
    // The judge and the optional top hide; the playfield and the bottom
    // ignore it.
    assert_eq!(count(with(&[(Region::Judge, hide)])), 3);
    assert_eq!(count(with(&[(Region::Top, hide)])), 3);
    assert_eq!(count(with(&[(Region::Bottom, hide)])), 4);
    assert_eq!(count(with(&[(Region::Playfield, hide)])), 4);

    let all = |lane_width, hide| Controls {
        position: true,
        scale: true,
        lane_width,
        hide,
    };
    assert_eq!(s.controls(Region::Playfield), all(true, false));
    assert_eq!(s.controls(Region::Judge), all(false, true));
    assert_eq!(s.controls(Region::Top), all(false, true));
    assert_eq!(s.controls(Region::Bottom), all(false, false));
    // Empty regions offer none, and so does `screen`.
    assert_eq!(s.controls(Region::Left), Controls::default());
    assert_eq!(s.controls(Region::Screen), Controls::default());
    // Without the lanes in the playfield there is no lane width, and the
    // layout's is ignored.
    let s = skin(REGIONS, vec![el(Region::Playfield)]);
    assert_eq!(s.controls(Region::Playfield), all(false, false));
    // A fixed canvas offers nothing.
    let s = Skin::new(SkinDef {
        fit: Fit::Fixed,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: lanes_support::sheet(),
        lanes: None,
        elements: vec![image(WHITE, [0; 4], Place::SCREEN)],
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
    .unwrap();
    for r in Region::ALL {
        assert_eq!(s.controls(r), Controls::default(), "{r:?}");
    }
    // An offered position is ignored where the skin offers none: `left`
    // is empty, and a move of it changes nothing.
    let s = skin(REGIONS, vec![el(Region::Top)]);
    let c = s.canvas([1_920, 1_080]);
    let moved = with(&[(Region::Left, adjust(500, 0, 200))]);
    assert_eq!(
        s.placement(&c, &moved, Side::Left),
        s.placement(&c, &Layout::AUTHOR, Side::Left)
    );
}

#[test]
fn fills_cover_or_stretch_the_canvas() {
    // 21:9, 2560×1080: cover scales by max(2560/1920, 1080/1080) = 4/3 and
    // centres, so [0, −180, 2560, 1440]; stretch [0, 0, 2560, 1080]; no
    // fill with `screen` anchored C shifts by 320.
    let fill = |fill| {
        image(
            WHITE,
            [0, 0, 1_920, 1_080],
            Place {
                fill,
                ..Place::SCREEN
            },
        )
    };
    let regions = regions(&[(Region::Screen, Anchor::Center, false)]);
    let s = skin(
        regions,
        vec![fill(Fill::Cover), fill(Fill::Stretch), fill(Fill::None)],
    );
    assert_eq!(
        drawn(s, [2_560, 1_080], Layout::AUTHOR),
        [
            [0.0, -180.0, 2_560.0, 1_440.0],
            [0.0, 0.0, 2_560.0, 1_080.0],
            [320.0, 0.0, 1_920.0, 1_080.0]
        ]
    );
}

/// The left region, anchored at its centre, with an image following
/// `follow` over `[100, 100, 40, 20]` and one following nothing over
/// `[200, 105, 40, 10]`; the source lanes (600 high) in the playfield.
fn followed(follow: Follow) -> Skin {
    let left = |r, follow| {
        let place = Place {
            follow,
            ..in_region(Region::Left)
        };
        image(WHITE, r, place)
    };
    skin_with(
        regions(&[(Region::Left, Anchor::Center, false)]),
        Some(lanes_support::source()),
        vec![
            lanes_element(),
            left([100, 100, 40, 20], follow),
            left([200, 105, 40, 10], Follow::None),
        ],
    )
    .unwrap()
}

/// The white quads of `skin` with `layout` and a cover of `cover` ‰, and
/// the left region's placement.
fn left_at(skin: Skin, layout: Layout, cover: u16) -> (Vec<[f32; 4]>, RegionPlacement) {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let canvas = skin.canvas([1_920, 1_080]);
    let placed = skin
        .placement(&canvas, &layout, Side::Left)
        .region(Region::Left);
    let mut h = Harness::new(skin, &chart, &full(0));
    h.layout = layout;
    h.view.cover = cover;
    h.frame(0, &[]);
    (lanes_support::rects(&h, WHITE), placed)
}

#[test]
fn followers_are_placed_as_if_they_followed_nothing() {
    // Placement reads the authored box [100, 120] whatever follows the
    // covers. At 200 % the box scales about its centre y 110: the
    // follower's y is 110 − 2·110 + 2·100 = 90 (with the cover's travel
    // of up to 600 counted, the pivot would be 410 and its y −210).
    let scaled = with(&[(Region::Left, adjust(0, 0, 200))]);
    // Pushed up 4000 at 100 %, the 20-high box keeps all of itself on the
    // canvas, moved −100 (counting the travel, −656).
    let pushed = with(&[(Region::Left, adjust(0, -4_000, 100))]);
    for (layout, y) in [(scaled, 90.0), (pushed, 0.0)] {
        let (none, placed_none) = left_at(followed(Follow::None), layout, 0);
        for follow in [Follow::Lift, Follow::Cover, Follow::LiftAndCover] {
            let (got, placed) = left_at(followed(follow), layout, 0);
            assert_eq!(got, none, "{follow:?}");
            assert_eq!(placed, placed_none, "{follow:?}");
        }
        assert_eq!(none[0][1], y);
    }
    let (_, placed) = left_at(followed(Follow::Cover), pushed, 0);
    assert_eq!(placed.affine.offset[1], -100.0);
    // A cover of 500 ‰ travels C = 300: the follower moves 600 at 200 %,
    // the other image and the region stay.
    let (rest, placed_rest) = left_at(followed(Follow::Cover), scaled, 0);
    let (moved, placed_moved) = left_at(followed(Follow::Cover), scaled, 500);
    assert_eq!(placed_moved, placed_rest);
    assert_eq!(moved[1], rest[1]);
    let [x, y, w, h] = rest[0];
    assert_eq!(moved[0], [x, y + 600.0, w, h]);
}
