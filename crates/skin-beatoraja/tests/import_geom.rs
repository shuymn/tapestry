//! The geometry rule (owner decision 1): exact rationals scaled,
//! offset and stretched, reflected to y down and rounded half up once per
//! edge; the pivot from `center` and the angle negated.

mod common;

use std::f32::consts::PI;

use common::play::round;
use common::{import_config, import_sized};
use skin_beatoraja::{ImportError, OffsetValue};

const OFFSET: &str = r#""offset": [{"name": "o", "id": 40, "x": true, "y": true}]"#;

/// One image of `a.png`'s `src` rect drawn with destination `dst`.
fn one(size: [u32; 2], src: &str, dst: &str) -> skin_model::Quad {
    let body = format!(
        r#""image": [{{"id": "i", "src": "a", {src}}}],
           "destination": [{{"id": "i", {dst}}}]"#
    );
    let q = import_sized(size, &body, &[]).quads(0);
    assert_eq!(q.len(), 1, "{q:?}");
    q[0]
}

#[test]
fn an_asymmetric_rect_turns_about_its_pivot() {
    // r = 3/2: (10, 20, 30, 40) y up becomes (15, 30, 45, 60), its top
    // at 1080 − 90.
    let q = one(
        [1_280, 720],
        r#""w": 64, "h": 64"#,
        r#""center": 7, "dst": [{"x": 10, "y": 20, "w": 30, "h": 40, "angle": 30}]"#,
    );
    assert_eq!(round(q.rect), [15, 990, 45, 60]);
    assert_eq!(q.pivot, [15.0, 990.0]);
    // The source turns counter-clockwise; the model clockwise.
    assert!(
        (q.angle - (2.0 * PI - PI / 6.0)).abs() < 1e-4,
        "{}",
        q.angle
    );
}

#[test]
fn centers_outside_0_to_9_pivot_at_the_bottom_left() {
    for (center, pivot) in [
        (-1, [15.0, 1_050.0]),
        (10, [15.0, 1_050.0]),
        (11, [15.0, 1_050.0]),
        (0, [37.5, 1_020.0]),
        (3, [60.0, 1_050.0]),
        (9, [60.0, 990.0]),
    ] {
        let q = one(
            [1_280, 720],
            r#""w": 64, "h": 64"#,
            &format!(r#""center": {center}, "dst": [{{"x": 10, "y": 20, "w": 30, "h": 40}}]"#),
        );
        assert_eq!(q.pivot, pivot, "center {center}");
    }
}

#[test]
fn odd_aspects_are_centred_before_rounding() {
    // r = 1, a 3×2 frame fit to width 2: height 4/3 centred at y = 2,
    // edges 4/3 and 8/3, reflected to 1077.33 and 1078.67. Truncating
    // the height first would give (1078, 1079).
    let q = one(
        [1_920, 1_080],
        r#""w": 3, "h": 2"#,
        r#""stretch": 4, "dst": [{"x": 0, "y": 0, "w": 2, "h": 4}]"#,
    );
    assert_eq!(round(q.rect), [0, 1_077, 2, 2]);
    // r = 6/5, a 7×3 frame in a 6×6 box: height 18/7 from y = 12/7,
    // reflected to 1075.71 and 1078.29.
    let q = one(
        [1_600, 900],
        r#""w": 7, "h": 3"#,
        r#""stretch": 4, "dst": [{"x": 0, "y": 0, "w": 5, "h": 5}]"#,
    );
    assert_eq!(round(q.rect), [0, 1_076, 6, 2]);
}

#[test]
fn halves_round_up_on_every_edge() {
    // r = 3/2: x 1 → 1.5 (2), w 1 → right 3; y 1 → 1.5, h 1 → top 3:
    // reflected 1077 and 1078.5 (1079).
    let q = one(
        [1_280, 720],
        r#""w": 64, "h": 64"#,
        r#""dst": [{"x": 1, "y": 1, "w": 1, "h": 1}]"#,
    );
    assert_eq!(round(q.rect), [2, 1_077, 1, 2]);
}

#[test]
fn geometry_past_the_model_is_a_range_error() {
    let body = format!(
        r#"{OFFSET}, "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{{"id": "i", "offset": 40, "dst": [{{"w": 1, "h": 1}}]}}]"#
    );
    let r = import_config([1_920, 1_080], &body, &[], |c| {
        c.offsets.insert(
            40,
            OffsetValue {
                x: i32::MAX,
                ..OffsetValue::default()
            },
        );
    });
    assert!(matches!(r.err(), ImportError::Range { field: "dst", .. }));
    // Past the model's coordinate limit by one, and at it.
    let r = import_config([1_920, 1_080], &body, &[], |c| {
        c.offsets.insert(
            40,
            OffsetValue {
                x: (1 << 20) + 1,
                ..OffsetValue::default()
            },
        );
    });
    assert!(matches!(r.err(), ImportError::Range { field: "dst", .. }));
    let r = import_config([1_920, 1_080], &body, &[], |c| {
        c.offsets.insert(
            40,
            OffsetValue {
                x: 1 << 20,
                ..OffsetValue::default()
            },
        );
    });
    assert_eq!(r.rects(0), [[1 << 20, 1_079, 1, 1]]);
}
