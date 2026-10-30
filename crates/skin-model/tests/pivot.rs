//! Rotation and the 9-point pivot: a track's `(x, y)` is its rect's top
//! left corner; the pivot is the named point of the sampled rect, mapped
//! with the rect; the angle in degrees interpolates numerically and is
//! reduced into one turn.

mod lanes_support;
mod play_support;

use bms_model::{ChartBuilder, TimeUs};
use lanes_support::{WHITE, image, in_region, still};
use play_support::{Harness, MS, REGIONS, full};
use skin_model::{
    Adjust, Anchor, Ease, ElementDef, End, KeyDef, Layout, Place, Quad, Region, Track,
};

/// A track from `from` at 0 to `to` at 1 s, eased by `ease`.
fn track(from: [Option<i32>; 9], to: [Option<i32>; 9], ease: Ease) -> Track {
    Track::new(
        &[
            KeyDef {
                at: TimeUs(0),
                props: from,
            },
            KeyDef {
                at: TimeUs(1_000 * MS),
                props: to,
            },
        ],
        ease,
        End::Hold,
    )
    .unwrap()
}

/// Props `[x, y, w, h]` and `angle`.
fn props([x, y, w, h]: [i32; 4], angle: i32) -> [Option<i32>; 9] {
    let mut p = [None; 9];
    for (slot, v) in p.iter_mut().zip([x, y, w, h]) {
        *slot = Some(v);
    }
    p[8] = Some(angle);
    p
}

/// The single quad of `elements` at `ms` after `ready` with `layout`.
fn quad_at(elements: Vec<ElementDef>, ms: i64, layout: Layout) -> Quad {
    let skin = lanes_support::skin_with(REGIONS, None, elements).unwrap();
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    h.layout = layout;
    h.frame(0, &[]);
    let start = h.start(0).unwrap();
    h.frame_us(start + ms * MS, &[]);
    let [q] = h.batch.quads()[..] else {
        panic!("one quad");
    };
    q
}

fn with_track(track: Track, place: Place) -> ElementDef {
    ElementDef {
        track,
        ..image(WHITE, [0; 4], place)
    }
}

fn pivoted(pivot: Anchor, region: Region) -> Place {
    Place {
        pivot,
        ..in_region(region)
    }
}

#[test]
fn a_rotating_track_loads_and_draws_its_angle() {
    let el = with_track(still(props([100, 100, 40, 20], 30)), Place::SCREEN);
    let q = quad_at(vec![el], 0, Layout::AUTHOR);
    assert_eq!(q.angle, 30.0_f64.to_radians() as f32);
    assert_eq!(q.rect, [100.0, 100.0, 40.0, 20.0]);
}

#[test]
fn the_nine_pivots_are_points_of_the_rect() {
    // The rect stays where its key puts it; the pivot is its named point.
    for pivot in Anchor::ALL {
        let [hx, hy] = pivot.halves().map(f32::from);
        let el = with_track(
            still(props([100, 100, 40, 20], 0)),
            pivoted(pivot, Region::Screen),
        );
        let q = quad_at(vec![el], 0, Layout::AUTHOR);
        assert_eq!(q.rect, [100.0, 100.0, 40.0, 20.0], "{pivot:?}");
        assert_eq!(q.pivot, [100.0 + 20.0 * hx, 100.0 + 10.0 * hy], "{pivot:?}");
    }
    let corner = with_track(
        still(props([100, 100, 40, 20], 0)),
        pivoted(Anchor::TopLeft, Region::Screen),
    );
    assert_eq!(
        quad_at(vec![corner], 0, Layout::AUTHOR).pivot,
        [100.0, 100.0]
    );
    // The playfield at 200 % about its anchor point (its box's top left,
    // (100, 100)): the rect doubles from there, and the centre pivot
    // (120, 110) maps like any point, to (140, 120).
    let el = with_track(
        still(props([100, 100, 40, 20], 0)),
        pivoted(Anchor::Center, Region::Playfield),
    );
    let mut layout = Layout::AUTHOR;
    layout
        .set(
            Region::Playfield,
            Adjust {
                scale: 200,
                ..Adjust::IDENTITY
            },
        )
        .unwrap();
    let q = quad_at(vec![el], 0, layout);
    assert_eq!(q.rect, [100.0, 100.0, 80.0, 40.0]);
    assert_eq!(q.pivot, [140.0, 120.0]);
}

#[test]
fn a_sized_track_keeps_its_corner_and_turns_about_its_pivot() {
    // An odd width 41 → 81 and height 21 → 41, quadratic in: the corner
    // stays at (100, 100) at every sample and the centre pivot is at
    // half the sampled size from it, so it moves (120.5 → 140.5).
    let from = props([100, 100, 41, 21], 0);
    let to = props([100, 100, 81, 41], 90);
    for ease in [Ease::Linear, Ease::In, Ease::Out] {
        let mut centres = Vec::new();
        for ms in [0, 250, 333, 500, 999, 1_000] {
            let el = with_track(
                track(from, to, ease),
                pivoted(Anchor::Center, Region::Screen),
            );
            let q = quad_at(vec![el], ms, Layout::AUTHOR);
            let [x, y, w, h] = q.rect;
            assert_eq!([x, y], [100.0, 100.0], "{ease:?} at {ms}");
            assert_eq!(q.pivot, [x + w / 2.0, y + h / 2.0], "{ease:?} at {ms}");
            centres.push(q.pivot[0]);
        }
        assert_eq!(centres.first(), Some(&120.5));
        assert_eq!(centres.last(), Some(&140.5));
    }
    // Quadratic in at half time: 41 + 40 / 4 = 51 wide, centre 125.5.
    let el = with_track(
        track(from, to, Ease::In),
        pivoted(Anchor::Center, Region::Screen),
    );
    let q = quad_at(vec![el], 500, Layout::AUTHOR);
    assert_eq!(q.rect[2], 51.0);
    assert_eq!(q.pivot[0], 125.5);
    // Linear w 40 → 80 with the pivot on the right edge: x + w.
    let el = with_track(
        track(
            props([100, 100, 40, 20], 0),
            props([100, 100, 80, 20], 0),
            Ease::Linear,
        ),
        pivoted(Anchor::Right, Region::Screen),
    );
    let q = quad_at(vec![el], 500, Layout::AUTHOR);
    assert_eq!(q.pivot, [160.0, 110.0]);
}

#[test]
fn angles_interpolate_in_degrees_and_reduce_into_a_turn() {
    let angle = |from: i32, to: i32, ms: i64| {
        let el = with_track(
            track(
                props([0, 0, 10, 10], from),
                props([0, 0, 10, 10], to),
                Ease::Linear,
            ),
            Place::SCREEN,
        );
        quad_at(vec![el], ms, Layout::AUTHOR).angle
    };
    let rad = |deg: f64| deg.to_radians() as f32;
    assert_eq!(angle(0, 90, 500), rad(45.0));
    assert_eq!(angle(0, 90, 500), std::f32::consts::FRAC_PI_4);
    assert_eq!(angle(450, 450, 0), rad(90.0));
    assert_eq!(angle(-90, -90, 0), rad(270.0));
    assert_eq!(angle(360, 360, 0), 0.0);
    // 2^24 = 46 603 turns and 136°.
    assert_eq!(angle(1 << 24, 1 << 24, 0), rad(136.0));
    assert_eq!(angle(-(1 << 24), -(1 << 24), 0), rad(224.0));
    // Numerically, not the short way round: 350 → 10 passes 180 at half
    // time.
    assert_eq!(angle(350, 10, 500), rad(180.0));
}
