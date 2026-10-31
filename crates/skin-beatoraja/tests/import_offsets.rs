//! Offsets: ids from both fields once each, values from the
//! configuration in unscaled reference pixels, alpha folded only on fixed
//! colour with one sign, run-time ids refused.

mod common;

use std::f32::consts::PI;

use common::play::round;
use common::{has, import_config};
use skin_beatoraja::{Config, NoteKind, OffsetValue};

/// Offsets 40 (axis flags all off) and 41 declared.
const HEADER: &str = r#""offset": [
    {"name": "a", "id": 40, "x": false, "y": false, "w": false, "h": false},
    {"name": "b", "id": 41, "x": true, "w": true},
    {"name": "c", "id": 4},
    {"name": "d", "id": 10}
]"#;

fn set(c: &mut Config, id: i32, v: OffsetValue) {
    c.offsets.insert(id, v);
}

fn ov(x: i32, y: i32, w: i32, h: i32) -> OffsetValue {
    OffsetValue {
        x,
        y,
        w,
        h,
        ..OffsetValue::default()
    }
}

fn load(size: [u32; 2], dst: &str, edit: impl FnOnce(&mut Config)) -> common::Loaded {
    import_config(
        size,
        &format!(
            r#"{HEADER}, "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
               "destination": [{{"id": "i", {dst}}}]"#
        ),
        &[],
        edit,
    )
}

const RECT: &str = r#""dst": [{"x": 100, "y": 100, "w": 20, "h": 20}]"#;

#[test]
fn ids_apply_once_from_both_fields() {
    let dst = format!(r#""offsets": [40, 41, 40], "offset": 41, {RECT}"#);
    let loaded = load([1_920, 1_080], &dst, |c| {
        set(c, 40, ov(3, -2, 5, -3));
        set(c, 41, ov(1, 0, 1, 0));
    });
    // x 100 + 4 − 3 = 101, w 26; y 100 − 2 + 1.5 = 99.5, h 17: the
    // y-up edges 99.5 and 116.5 reflect to 980.5 and 963.5.
    assert_eq!(loaded.rects(0), [[101, 964, 26, 17]]);
}

#[test]
fn signed_odd_deltas_are_unscaled_and_exact() {
    let dst = format!(r#""offset": 40, {RECT}"#);
    // r = 3/2: (150, 150, 30, 30), then x + 3 − 2.5 and w + 5.
    let loaded = load([1_280, 720], &dst, |c| set(c, 40, ov(3, -2, 5, -3)));
    // x 150.5..185.5, y-up 149.5..176.5: top 903.5, bottom 930.5.
    assert_eq!(loaded.rects(0), [[151, 904, 35, 27]]);
}

#[test]
fn the_angle_offset_adds_before_negation() {
    let dst = r#""offset": 40, "dst": [{"w": 10, "h": 10, "angle": 10}]"#;
    let loaded = load([1_920, 1_080], dst, |c| {
        set(
            c,
            40,
            OffsetValue {
                r: 20,
                ..OffsetValue::default()
            },
        );
    });
    let q = loaded.quads(0);
    assert!((q[0].angle - (2.0 * PI - PI / 6.0)).abs() < 1e-4);
}

#[test]
fn alpha_folds_on_fixed_colour_with_one_sign() {
    let alpha = |a| OffsetValue {
        a,
        ..OffsetValue::default()
    };
    let fixed = r#""offsets": [40, 41], "dst": [{"w": 10, "h": 10, "a": 200}]"#;
    let loaded = load([1_920, 1_080], fixed, |c| {
        set(c, 40, alpha(-50));
        set(c, 41, alpha(-30));
    });
    let q = loaded.quads(0);
    assert!((q[0].color[3] - 120.0 / 255.0).abs() < 1e-6);
    // Clamped at 0.
    let loaded = load([1_920, 1_080], fixed, |c| set(c, 40, alpha(-300)));
    assert!(loaded.ok().report.notes.is_empty());
    // Mixed signs, or animated colour: unsupported.
    let loaded = load([1_920, 1_080], fixed, |c| {
        set(c, 40, alpha(10));
        set(c, 41, alpha(-10));
    });
    assert!(has(loaded.report(), NoteKind::OffsetAlpha));
    assert!(loaded.quads(0).is_empty());
    let animated = r#""offset": 40, "dst": [{"time": 0, "w": 10, "h": 10, "a": 255},
                                             {"time": 100, "a": 100}]"#;
    let loaded = load([1_920, 1_080], animated, |c| set(c, 40, alpha(-10)));
    assert!(has(loaded.report(), NoteKind::OffsetAlpha));
    // Without an alpha delta, animated colour takes the offset.
    let loaded = load([1_920, 1_080], animated, |c| set(c, 40, ov(5, 0, 0, 0)));
    assert!(loaded.report().notes.is_empty());
}

#[test]
fn runtime_and_unknown_offsets() {
    // 4 is set by the play: unsupported.
    let loaded = load([1_920, 1_080], &format!(r#""offset": 4, {RECT}"#), |_| {});
    assert!(has(loaded.report(), NoteKind::RuntimeOffset(4)));
    // 10 belongs to the player's settings; an undeclared id is ignored.
    let dst = format!(r#""offsets": [10, 50], {RECT}"#);
    let loaded = load([1_920, 1_080], &dst, |c| {
        set(c, 10, ov(7, 0, 0, 0));
        set(c, 50, ov(9, 0, 0, 0));
    });
    let notes = &loaded.report().notes;
    assert!(notes.iter().all(|n| n.at.is_none()), "{notes:?}");
    assert_eq!(
        loaded.report().notes.len(),
        2,
        "offset 10 and the undeclared 50"
    );
    let q = loaded.quads(0);
    assert_eq!(round(q[0].rect), [100, 960, 20, 20]);
}
