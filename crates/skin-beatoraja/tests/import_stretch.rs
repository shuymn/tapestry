//! Stretch (§4.5): the affine modes baked per key, the piecewise modes
//! only within one branch, the trimmed modes refused, and numbers only
//! with the identity.

mod common;

use common::{has, import_body};
use skin_beatoraja::NoteKind;

/// An image of a 3×2 frame (or `frame`) with `stretch` and `keys`.
fn image(frame: [u32; 2], stretch: i32, keys: &str) -> common::Loaded {
    let [w, h] = frame;
    import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", "w": {w}, "h": {h}}}],
           "destination": [{{"id": "i", "timer": 41, "loop": 100, "stretch": {stretch},
                             "dst": [{keys}]}}]"#
    ))
}

#[test]
fn affine_modes_are_baked_exactly() {
    // Fit height: 3×2 at height 4 is 6 wide, centred in 10.
    let r = image([3, 2], 6, r#"{"x": 0, "y": 0, "w": 10, "h": 4}"#).rects(0);
    assert_eq!(r, [[2, 1_076, 6, 4]]);
    // No resize: 3×2 centred in 10×4, edges x 3.5..6.5 and y 1..3.
    let r = image([3, 2], 9, r#"{"x": 0, "y": 0, "w": 10, "h": 4}"#).rects(0);
    assert_eq!(r, [[4, 1_077, 3, 2]]);
    // Fit width, animated: each key baked, the ease between them.
    let loaded = image(
        [3, 2],
        4,
        r#"{"time": 0, "x": 0, "y": 0, "w": 3, "h": 10},
           {"time": 100, "x": 0, "y": 0, "w": 6, "h": 10}"#,
    );
    let mut d = loaded.drawn();
    // Height 2 centred in 10 (y 4..6), then 4 (y 3..7).
    assert_eq!(d.rects(0), [[0, 1_074, 3, 2]]);
    assert_eq!(d.rects(100), [[0, 1_073, 6, 4]]);
}

#[test]
fn piecewise_modes_need_one_branch() {
    // Fit inside: wider than the frame's aspect at both keys.
    let loaded = image(
        [3, 2],
        1,
        r#"{"time": 0, "x": 0, "y": 0, "w": 30, "h": 10},
           {"time": 100, "x": 0, "y": 0, "w": 60, "h": 10}"#,
    );
    let mut d = loaded.drawn();
    // 15 wide centred in 30 (7.5..22.5), then in 60 (22.5..37.5).
    assert_eq!(d.rects(0), [[8, 1_070, 15, 10]]);
    assert_eq!(d.rects(100), [[23, 1_070, 15, 10]]);
    // Wider, then taller: two branches.
    let loaded = image(
        [3, 2],
        1,
        r#"{"time": 0, "x": 0, "y": 0, "w": 30, "h": 10},
           {"time": 100, "x": 0, "y": 0, "w": 10, "h": 30}"#,
    );
    assert!(has(loaded.report(), NoteKind::Stretch(1)));
    assert!(loaded.quads(0).is_empty());
    // An exact tie counts for either branch.
    let loaded = image(
        [3, 2],
        1,
        r#"{"time": 0, "x": 0, "y": 0, "w": 30, "h": 20},
           {"time": 100, "x": 0, "y": 0, "w": 10, "h": 30}"#,
    );
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
}

#[test]
fn trimmed_modes_are_refused() {
    // The f32 crop of mode 5 drops source rows: a 2×3 frame from 2×3 to
    // 4×6 over 7 ms.
    let loaded = image(
        [2, 3],
        5,
        r#"{"time": 0, "w": 2, "h": 3}, {"time": 7, "w": 4, "h": 6}"#,
    );
    assert!(has(loaded.report(), NoteKind::Stretch(5)));
    assert_eq!(loaded.report().usage.images, 0);
    assert!(loaded.quads(3).is_empty());
    for mode in [3, 7, 10] {
        let loaded = image([2, 3], mode, r#"{"w": 4, "h": 6}"#);
        assert!(has(loaded.report(), NoteKind::Stretch(mode)), "{mode}");
    }
    // Unknown modes are the identity.
    let r = image([3, 2], 11, r#"{"x": 0, "y": 0, "w": 10, "h": 4}"#).rects(0);
    assert_eq!(r, [[0, 1_076, 10, 4]]);
}

#[test]
fn numbers_take_only_the_identity() {
    // A 12×20 cell of n.png (120×20, ten glyphs).
    let number = |keys: &str| {
        import_body(&format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10,
                          "digit": 3, "ref": 110}}],
               "destination": [{{"id": "v", "timer": 41, "stretch": 9, "dst": [{keys}]}}]"#
        ))
    };
    let loaded = number(
        r#"{"time": 0, "x": 0, "y": 0, "w": 12, "h": 20},
           {"time": 100, "x": 0, "y": 0, "w": 24, "h": 40}"#,
    );
    assert!(has(loaded.report(), NoteKind::Stretch(9)));
    assert!(loaded.quads(0).is_empty());
    let loaded = number(
        r#"{"time": 0, "x": 0, "y": 0, "w": 12, "h": 20},
           {"time": 100, "x": 50, "y": 0, "w": 12, "h": 20}"#,
    );
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    // "  0": the last of three cells.
    assert_eq!(loaded.rects(0), [[24, 1_060, 12, 20]]);
}
