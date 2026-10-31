//! Stretch: the affine modes baked per key, the piecewise modes
//! only within one branch, the trimmed modes refused, 8 and 9 kept in
//! target pixels at placement, and numbers only with the identity, 8
//! or 9.

mod common;

use common::{has, import_body, import_sized};
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

/// The source region, columns and rows, the source draws of the 2×3
/// frame at `ms` of the mode-5 track (2×3 at 0 ms, 4×6 at 7 ms): its
/// width fixes the scale, and a box shorter than the scaled frame crops
/// the rows to `(int)(h / scale)`.
#[expect(clippy::cast_possible_truncation, reason = "the source truncates")]
fn mode5_source_region(ms: u8) -> [i32; 2] {
    let rate = f32::from(ms) / 7.0;
    let w = 2.0 + (4.0 - 2.0) * rate;
    let h = 3.0 + (6.0 - 3.0) * rate;
    let scale = w / 2.0;
    if h < scale * 3.0 {
        [2, (h / scale) as i32]
    } else {
        [2, 3]
    }
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
    // At 6 ms the source crops its frame in f32 (`StretchType.java:16-146`
    // at `0.8.9`, interpolating as `SkinObject.java:384-411`): the exact
    // region keeps all three rows, the source's keeps two.
    assert_eq!(mode5_source_region(6), [2, 2]);
    assert_eq!(mode5_source_region(0), [2, 3]);
    assert_eq!(mode5_source_region(7), [2, 3]);
    for mode in [3, 7, 10] {
        let loaded = image([2, 3], mode, r#"{"w": 4, "h": 6}"#);
        assert!(has(loaded.report(), NoteKind::Stretch(mode)), "{mode}");
    }
    // Unknown modes are the identity.
    let r = image([3, 2], 11, r#"{"x": 0, "y": 0, "w": 10, "h": 4}"#).rects(0);
    assert_eq!(r, [[0, 1_076, 10, 4]]);
}

/// The quad rects at `ms` of the 1280×720 skin of `body`, drawn on a
/// `target`, in target pixels.
fn target_rects(body: &str, target: [u32; 2], ms: i64) -> Vec<[i32; 4]> {
    let loaded = import_sized([1_280, 720], body, &[]);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut d = loaded.drawn();
    d.target = target;
    let scale = target[0] as f32 / 1_920.0;
    d.frame(ms, &[])
        .iter()
        .map(|q| common::play::round(q.rect.map(|v| v * scale)))
        .collect()
}

/// The whole of a.png (64×64) with `stretch` over the 1280×720 `keys`.
fn whole(stretch: i32, keys: &str) -> String {
    format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{{"id": "i", "timer": 41, "stretch": {stretch}, "dst": [{keys}]}}]"#
    )
}

#[test]
fn no_resize_keeps_its_texels_on_any_target() {
    // 200×100 at 1280×720 is 300×150 on the 1920×1080 canvas, 600×300
    // on a 3840×2160 target: the 64×64 frame stays 64 target pixels,
    // centred (`skin/StretchType.java:94-98`).
    let body = whole(9, r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#);
    assert_eq!(target_rects(&body, [1_920, 1_080], 0), [[118, 973, 64, 64]]);
    assert_eq!(
        target_rects(&body, [3_840, 2_160], 0),
        [[268, 1_978, 64, 64]]
    );
    // An animation's midpoint keeps it too: 300 to 600 wide is 450.
    let body = whole(
        9,
        r#"{"time": 0, "x": 0, "y": 0, "w": 200, "h": 100},
           {"time": 100, "x": 0, "y": 0, "w": 400, "h": 100}"#,
    );
    assert_eq!(
        target_rects(&body, [3_840, 2_160], 50),
        [[418, 1_978, 64, 64]]
    );
}

#[test]
fn no_expanding_shrinks_only_below_its_texels() {
    // Room for the frame: 64 target pixels on either target
    // (`skin/StretchType.java:85-93`).
    let body = whole(8, r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#);
    assert_eq!(target_rects(&body, [1_920, 1_080], 0), [[118, 973, 64, 64]]);
    assert_eq!(
        target_rects(&body, [3_840, 2_160], 0),
        [[268, 1_978, 64, 64]]
    );
    // 40×20 is 60×30 on the canvas: 30 target pixels at 1920×1080, and
    // on 3840×2160 120×60, so 60.
    let body = whole(8, r#"{"x": 0, "y": 0, "w": 40, "h": 20}"#);
    assert_eq!(
        target_rects(&body, [1_920, 1_080], 0),
        [[15, 1_050, 30, 30]]
    );
    assert_eq!(
        target_rects(&body, [3_840, 2_160], 0),
        [[30, 2_100, 60, 60]]
    );
    // Keys on both sides of the frame's size share one element: 3×2 from
    // 2×4 (shrunk to 2×1.33) to 6×4 (3×2); at 50 ms 4×4 holds 3×2.
    let loaded = image(
        [3, 2],
        8,
        r#"{"time": 0, "x": 0, "y": 0, "w": 2, "h": 4},
           {"time": 100, "x": 0, "y": 0, "w": 6, "h": 4}"#,
    );
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut d = loaded.drawn();
    assert_eq!(d.rects(50), [[1, 1_077, 3, 2]]);
}

#[test]
fn numbers_take_the_identity_eight_or_nine() {
    // A 12×20 cell of n.png (120×20, ten glyphs).
    let number = |size: [u32; 2], stretch: i32, keys: &str| {
        import_sized(
            size,
            &format!(
                r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10,
                              "digit": 3, "ref": 110}}],
                   "destination": [{{"id": "v", "timer": 41, "stretch": {stretch},
                                     "dst": [{keys}]}}]"#
            ),
            &[],
        )
    };
    let keys = r#"{"time": 0, "x": 0, "y": 0, "w": 12, "h": 20},
                  {"time": 100, "x": 50, "y": 0, "w": 12, "h": 20}"#;
    let loaded = number([1_920, 1_080], 9, keys);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    // "  0": the last of three cells.
    assert_eq!(loaded.rects(0), [[24, 1_060, 12, 20]]);
    // Each glyph keeps its 12×20 texels inside its own cell
    // (`skin/SkinNumber.java:191`): 24×40 cells, the last at x 48.
    let keys = r#"{"x": 0, "y": 0, "w": 24, "h": 40}"#;
    let loaded = number([1_920, 1_080], 9, keys);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    assert_eq!(loaded.rects(0), [[54, 1_050, 12, 20]]);
    // At 1280×720 on 3840×2160 the cell is 72×120 target pixels and the
    // glyph still 12×20.
    let loaded = number([1_280, 720], 8, keys);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    let mut d = loaded.drawn();
    d.target = [3_840, 2_160];
    let q = d.frame(0, &[]);
    let r: Vec<_> = q
        .iter()
        .map(|q| common::play::round(q.rect.map(|v| v * 2.0)))
        .collect();
    assert_eq!(r, [[144 + 30, 2_040 + 50, 12, 20]]);
    // Any other mode must leave every cell as it is: fit inside a 24×30
    // cell is 18×30.
    let loaded = number([1_920, 1_080], 1, r#"{"x": 0, "y": 0, "w": 24, "h": 30}"#);
    assert!(has(loaded.report(), NoteKind::Stretch(1)));
    assert!(loaded.quads(0).is_empty());
}
