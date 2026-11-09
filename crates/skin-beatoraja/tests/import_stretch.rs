//! Stretch: the affine modes baked per key, the piecewise modes
//! only within one branch, the trimmed modes 3, 5 and 7 fitting or
//! cropping once, 8 and 9 kept in target pixels at placement, and
//! numbers only with the identity, 8 or 9.

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

/// A texel naming its own place in files up to 4 096 texels a side:
/// column `x`'s low byte in red, row `y`'s in green, their high nibbles
/// in blue.
fn wide_coord(x: u32, y: u32) -> [u8; 4] {
    let byte = |v: u32| u8::try_from(v & 0xFF).unwrap();
    [byte(x), byte(y), byte(((x >> 8) << 4) | (y >> 8)), 255]
}

/// The column and row [`wide_coord`] painted `t`.
fn place(t: [u8; 4]) -> [u32; 2] {
    let [r, g, b, _] = t.map(u32::from);
    [r | ((b >> 4) << 8), g | ((b & 0xF) << 8)]
}

/// Image `i`, frame `frame` (`"x": …, "w": …`) of a `file`-sized a.png,
/// with `stretch` over `keys`, in a 1920×1080 skin.
fn cut(file: [u32; 2], frame: &str, stretch: i32, keys: &str) -> common::Loaded {
    import_sized(
        [1_920, 1_080],
        &format!(
            r#""image": [{{"id": "i", "src": "a", {frame}}}],
               "destination": [{{"id": "i", "timer": 41, "loop": 100, "stretch": {stretch},
                                 "dst": [{keys}]}}]"#
        ),
        &[("a.png", file[0], file[1])],
    )
}

/// The one quad of `l` at `ms`, a.png (`file`) painted with
/// [`wide_coord`] first: its rounded rect and the places of the texels at
/// its top left and bottom right, the first and the last it draws.
#[track_caller]
fn drawn(l: common::Loaded, file: [u32; 2], ms: i64) -> ([i32; 4], [[u32; 2]; 2]) {
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    common::png_with(l.dir.path(), "a.png", file[0], file[1], wide_coord);
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let quads = l.quads(ms);
    let [q] = quads.as_slice() else {
        panic!("{quads:?}")
    };
    let at = |u, v| place(common::play::texel_at(&atlas, q, u, v));
    (
        common::play::round(q.rect),
        [at(0.0001, 0.0001), at(0.9999, 0.9999)],
    )
}

#[test]
fn stretch_three_covers_and_crops_the_middle() {
    // An 802×1080 frame over 400×1080: its height's scale is the larger,
    // so it covers the box and keeps its middle 400 columns, 201 to 600,
    // the box unchanged (`StretchType.java:47-56, 131-141`; the pinned
    // source gives region [201, 0, 400, 1080]).
    let file = [802, 1_080];
    let frame = r#""w": 802, "h": 1080"#;
    let l = cut(file, frame, 3, r#"{"x": 0, "y": 0, "w": 400, "h": 1080}"#);
    assert_eq!(
        drawn(l, file, 0),
        ([0, 0, 400, 1_080], [[201, 0], [600, 1_079]])
    );
    // Over 802×540, its middle 540 rows, 270 to 809 (region [0, 270,
    // 802, 540]).
    let l = cut(file, frame, 3, r#"{"x": 0, "y": 0, "w": 802, "h": 540}"#);
    assert_eq!(
        drawn(l, file, 0),
        ([0, 540, 802, 540], [[0, 270], [801, 809]])
    );
    // Over its own size it draws whole.
    let l = cut(file, frame, 3, r#"{"x": 0, "y": 0, "w": 802, "h": 1080}"#);
    assert_eq!(
        drawn(l, file, 0),
        ([0, 0, 802, 1_080], [[0, 0], [801, 1_079]])
    );
    // Keys moving only `y` keep one crop: 100 px up over 100 ms.
    let l = cut(
        file,
        frame,
        3,
        r#"{"time": 0, "x": 0, "y": 0, "w": 400, "h": 1080},
           {"time": 100, "x": 0, "y": 100, "w": 400, "h": 1080}"#,
    );
    assert_eq!(
        drawn(l, file, 50),
        ([0, -50, 400, 1_080], [[201, 0], [600, 1_079]])
    );
    // Cropping keys of one aspect changing size keep one crop: a 100×100
    // frame from 50×100 to 100×200 keeps columns 25 to 74 throughout.
    let file = [100, 100];
    let l = cut(
        file,
        r#""w": 100, "h": 100"#,
        3,
        r#"{"time": 0, "x": 0, "y": 0, "w": 50, "h": 100},
           {"time": 100, "x": 0, "y": 0, "w": 100, "h": 200}"#,
    );
    assert_eq!(drawn(l, file, 50), ([0, 930, 75, 150], [[25, 0], [74, 99]]));
}

#[test]
fn stretch_five_and_seven_fit_or_crop() {
    let file = [100, 100];
    let frame = r#""w": 100, "h": 100"#;
    let at = |mode, keys| drawn(cut(file, frame, mode, keys), file, 0);
    let whole = [[0, 0], [99, 99]];
    // Mode 5 in 100×200: the frame fits, the box shrinks to 100×100 about
    // its middle, y 50 up (the pinned source's rect [0, 50, 100, 100]).
    assert_eq!(
        at(5, r#"{"x": 0, "y": 0, "w": 100, "h": 200}"#),
        ([0, 930, 100, 100], whole)
    );
    // In 100×50 it overflows: the box stays and the middle 50 rows draw
    // (region [0, 25, 100, 50]).
    assert_eq!(
        at(5, r#"{"x": 0, "y": 0, "w": 100, "h": 50}"#),
        ([0, 1_030, 100, 50], [[0, 25], [99, 74]])
    );
    // Mode 7, transposed: 200×100 fits at x 50 (rect [50, 0, 100, 100]),
    // 50×100 keeps columns 25 to 74 (region [25, 0, 50, 100]).
    assert_eq!(
        at(7, r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#),
        ([50, 980, 100, 100], whole)
    );
    assert_eq!(
        at(7, r#"{"x": 0, "y": 0, "w": 50, "h": 100}"#),
        ([0, 980, 50, 100], [[25, 0], [74, 99]])
    );
    // An exact meeting fits: the box and the whole frame.
    assert_eq!(
        at(5, r#"{"x": 0, "y": 0, "w": 100, "h": 100}"#),
        ([0, 980, 100, 100], whole)
    );
    // Fitting keys are each baked: 100×200 then 200×400, at 50 ms
    // 150×300 holding 150×150 from y 75 up.
    let l = cut(
        file,
        frame,
        5,
        r#"{"time": 0, "x": 0, "y": 0, "w": 100, "h": 200},
           {"time": 100, "x": 0, "y": 0, "w": 200, "h": 400}"#,
    );
    assert_eq!(drawn(l, file, 50), ([0, 855, 150, 150], whole));
    // A crop's edges round half up (D1) where the source truncates: 51 of
    // the rows of a frame at row 20 keep 45 to 95, the source's region
    // [10, 44, 100, 51].
    let file = [120, 130];
    let l = cut(
        file,
        r#""x": 10, "y": 20, "w": 100, "h": 100"#,
        5,
        r#"{"x": 0, "y": 0, "w": 100, "h": 51}"#,
    );
    assert_eq!(
        drawn(l, file, 0),
        ([0, 1_029, 100, 51], [[10, 45], [109, 95]])
    );
}

/// The column each of the four columns of the one quad of `l` draws, at
/// its centre, a.png (`file`) painted with [`wide_coord`] first.
#[track_caller]
fn columns(l: common::Loaded, file: [u32; 2]) -> [u32; 4] {
    common::png_with(l.dir.path(), "a.png", file[0], file[1], wide_coord);
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let quads = l.quads(0);
    let [q] = quads.as_slice() else {
        panic!("{quads:?}")
    };
    [0.125, 0.375, 0.625, 0.875].map(|u| place(common::play::texel_at(&atlas, q, u, 0.5))[0])
}

/// A 16×16 frame at column `x` of a.png (64×64) under mode 7 over 4×16.
fn l2(x: u32) -> common::Loaded {
    cut(
        [64, 64],
        &format!(r#""x": {x}, "y": 0, "w": 16, "h": 16"#),
        7,
        r#"{"x": 0, "y": 0, "w": 4, "h": 16}"#,
    )
}

#[test]
fn an_animation_crops_along_an_axis_of_one_cell() {
    // Two 100×100 frames side by side under mode 5 over 100×50: each keeps
    // its rows 25 to 74, the sheet cut once to `[0, 25, 200, 50]` and
    // split by its grid (the source crops each frame alike).
    let file = [200, 100];
    let l = cut(
        file,
        r#""w": 200, "h": 100, "divx": 2, "cycle": 100"#,
        5,
        r#"{"x": 0, "y": 0, "w": 100, "h": 50}"#,
    );
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    common::png_with(l.dir.path(), "a.png", file[0], file[1], wide_coord);
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, common::play::LIMITS, &|| false)
        .unwrap();
    let mut d = l.drawn();
    for (ms, first) in [(0, 0), (50, 100)] {
        let quads = d.frame(ms, &[]);
        let [q] = quads.as_slice() else {
            panic!("{quads:?}")
        };
        let at = |u, v| place(common::play::texel_at(&atlas, q, u, v));
        assert_eq!(common::play::round(q.rect), [0, 1_030, 100, 50], "{ms}");
        assert_eq!(
            [at(0.0001, 0.0001), at(0.9999, 0.9999)],
            [[first, 25], [first + 99, 74]],
            "{ms}"
        );
    }
}

#[test]
fn a_column_crop_past_the_edge_draws_the_edge_column() {
    // A 16×16 frame at x 56 of a 64-wide file runs 8 columns past its
    // edge; mode 7 over 4×16 keeps its middle 4, 62 to 65, padded with
    // the edge column 63.
    let file = [64, 64];
    let l = cut(
        file,
        r#""x": 56, "y": 0, "w": 16, "h": 16"#,
        7,
        r#"{"x": 0, "y": 0, "w": 4, "h": 16}"#,
    );
    assert_eq!(drawn(l, file, 0), ([0, 1_064, 4, 16], [[62, 0], [63, 15]]));
    assert_eq!(columns(l2(56), file), [62, 63, 63, 63]);
    // Starting past the edge (x 60, columns 66 to 69), only the edge
    // column draws, as the source's clamped texture samples it.
    let l = cut(
        file,
        r#""x": 60, "y": 0, "w": 16, "h": 16"#,
        7,
        r#"{"x": 0, "y": 0, "w": 4, "h": 16}"#,
    );
    assert_eq!(drawn(l, file, 0), ([0, 1_064, 4, 16], [[63, 0], [63, 15]]));
    assert_eq!(columns(l2(60), file), [63; 4]);
    // A frame wholly past the edge is cut to the edge column: cropping its
    // columns is refused.
    let l = cut(
        file,
        r#""x": 64, "y": 0, "w": 16, "h": 16"#,
        7,
        r#"{"x": 0, "y": 0, "w": 4, "h": 16}"#,
    );
    assert!(has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
}

#[test]
fn trimmed_modes_the_model_cannot_follow_are_refused() {
    let file = [100, 100];
    let frame = r#""w": 100, "h": 100"#;
    let refused = |mode, frame: &str, keys| {
        let l = cut(file, frame, mode, keys);
        assert!(has(l.report(), NoteKind::Stretch(mode)), "{mode} {keys}");
        assert!(l.quads(0).is_empty(), "{mode} {keys}");
    };
    // Keys changing aspect crop different amounts.
    refused(
        3,
        frame,
        r#"{"time": 0, "x": 0, "y": 0, "w": 50, "h": 100},
           {"time": 100, "x": 0, "y": 0, "w": 40, "h": 100}"#,
    );
    // Keys mixing the branches: fit, then crop.
    refused(
        5,
        frame,
        r#"{"time": 0, "x": 0, "y": 0, "w": 100, "h": 200},
           {"time": 100, "x": 0, "y": 0, "w": 100, "h": 50}"#,
    );
    // Two keys cropping different axes under mode 3.
    refused(
        3,
        frame,
        r#"{"time": 0, "x": 0, "y": 0, "w": 50, "h": 100},
           {"time": 100, "x": 0, "y": 0, "w": 100, "h": 50}"#,
    );
    // An animation can crop only along an axis of one cell: two frames
    // side by side cannot crop their columns, nor two stacked their rows.
    let grid = r#""w": 200, "h": 100, "divx": 2"#;
    refused(7, grid, r#"{"x": 0, "y": 0, "w": 50, "h": 100}"#);
    let stacked = r#""w": 100, "h": 200, "divy": 2"#;
    refused(5, stacked, r#"{"x": 0, "y": 0, "w": 100, "h": 50}"#);
    // Trimmed image groups and image sets are not mapped.
    refused(
        5,
        r#""w": 100, "h": 100, "len": 2"#,
        r#"{"x": 0, "y": 0, "w": 100, "h": 50}"#,
    );
    let l = import_body(
        r#""image": [{"id": "i", "src": "a", "w": 16, "h": 16}],
           "imageset": [{"id": "s", "images": ["i"], "value": 71}],
           "destination": [{"id": "s", "stretch": 3, "dst": [{"x": 0, "y": 0, "w": 8, "h": 16}]}]"#,
    );
    assert!(has(l.report(), NoteKind::Stretch(3)), "{:?}", l.report());
    assert!(l.quads(0).is_empty());
    let l = cut(
        [200, 100],
        grid,
        7,
        r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#,
    );
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    assert_eq!(l.rects(0), [[50, 980, 100, 100]]);
    // Mode 10 has no mapping.
    refused(10, frame, r#"{"x": 0, "y": 0, "w": 50, "h": 100}"#);
    // A track whose keys all meet the frame exactly fits at every key, its
    // exact rationals keeping all three rows where the source's `f32`
    // crops one at 6 ms (D1): a 2×3 frame from 2×3 to 4×6 over 7 ms.
    let l = import_sized(
        [1_920, 1_080],
        r#""image": [{"id": "i", "src": "a", "w": 2, "h": 3}],
           "destination": [{"id": "i", "timer": 41, "loop": 7, "stretch": 5,
                            "dst": [{"time": 0, "w": 2, "h": 3}, {"time": 7, "w": 4, "h": 6}]}]"#,
        &[("a.png", 2, 3)],
    );
    assert_eq!(drawn(l, [2, 3], 6).1, [[0, 0], [1, 2]]);
    assert_eq!(mode5_source_region(6), [2, 2]);
    assert_eq!(mode5_source_region(0), [2, 3]);
    assert_eq!(mode5_source_region(7), [2, 3]);
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
    // 200×100 at 1280×720 is 300×150 on the 1920×1080 canvas, 200×100
    // on a 1280×720 target and 600×300 on a 3840×2160 one: the 64×64
    // frame stays 64 target pixels, centred (`skin/StretchType.java:94-98`).
    let body = whole(9, r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#);
    assert_eq!(target_rects(&body, [1_280, 720], 0), [[68, 638, 64, 64]]);
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
    // Room for the frame: 64 target pixels on every target
    // (`skin/StretchType.java:85-93`).
    let body = whole(8, r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#);
    assert_eq!(target_rects(&body, [1_280, 720], 0), [[68, 638, 64, 64]]);
    assert_eq!(target_rects(&body, [1_920, 1_080], 0), [[118, 973, 64, 64]]);
    assert_eq!(
        target_rects(&body, [3_840, 2_160], 0),
        [[268, 1_978, 64, 64]]
    );
    // 40×20 is 60×30 on the canvas: 20 target pixels at 1280×720, 30 at
    // 1920×1080, and on 3840×2160 120×60, so 60.
    let body = whole(8, r#"{"x": 0, "y": 0, "w": 40, "h": 20}"#);
    assert_eq!(target_rects(&body, [1_280, 720], 0), [[10, 700, 20, 20]]);
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

/// An image set choosing by EX score between a 16×16 and a 64×32 cell of
/// a.png, with `stretch` over `keys`, beside image `i` (the 3×2 frame)
/// at x 100 with stretch 6; a `size` skin.
fn shapes(size: [u32; 2], stretch: i32, keys: &str) -> common::Loaded {
    import_sized(
        size,
        &format!(
            r#""image": [{{"id": "small", "src": "a", "w": 16, "h": 16}},
                         {{"id": "wide", "src": "a", "w": 64, "h": 32}},
                         {{"id": "i", "src": "a", "w": 3, "h": 2}}],
               "imageset": [{{"id": "s", "images": ["small", "wide"], "value": 71}}],
               "destination": [{{"id": "s", "stretch": {stretch}, "dst": [{keys}]}},
                               {{"id": "i", "stretch": 6, "dst": [{{"x": 100, "y": 0, "w": 10, "h": 4}}]}}]"#
        ),
        &[],
    )
}

/// The rects at 0 ms with EX score `ex` (0 selects the 16×16 cell, 1 the
/// 64×32), drawn on `target` and scaled from the 1920×1080 canvas.
fn shape_rects(l: common::Loaded, ex: u32, target: [u32; 2]) -> Vec<[i32; 4]> {
    let mut d = l.drawn();
    d.tally = common::play::tally(ex, 0);
    d.target = target;
    let scale = target[0] as f32 / 1_920.0;
    d.frame(0, &[])
        .iter()
        .map(|q| common::play::round(q.rect.map(|v| v * scale)))
        .collect()
}

#[test]
fn each_selected_image_bakes_its_own_aspect() {
    // Fit height 4 in a 10 wide box: the 16×16 cell is 4 wide at x 3,
    // the 64×32 one 8 wide at x 1; the plain image's 3×2 is 6 wide at 102
    // whichever is selected.
    let keys = r#"{"x": 0, "y": 0, "w": 10, "h": 4}"#;
    let plain = [102, 1_076, 6, 4];
    let rects = |ex| shape_rects(shapes([1_920, 1_080], 6, keys), ex, [1_920, 1_080]);
    assert_eq!(rects(0), [[3, 1_076, 4, 4], plain]);
    assert_eq!(rects(1), [[1, 1_076, 8, 4], plain]);
    // Animated: each choice's own track between its baked keys.
    let keys = r#"{"time": 0, "x": 0, "y": 0, "w": 10, "h": 4},
                  {"time": 100, "x": 0, "y": 0, "w": 10, "h": 8}"#;
    let mut d = shapes([1_920, 1_080], 6, keys).drawn();
    d.tally = common::play::tally(1, 0);
    // 64×32 at height 6 (the midpoint) is 12 wide, centred in 10 at -1.
    assert_eq!(d.rects(50)[0], [-1, 1_074, 12, 6]);
    d.tally = common::play::tally(0, 0);
    assert_eq!(d.rects(50)[0], [2, 1_074, 6, 6]);
}

#[test]
fn each_selected_image_keeps_its_own_native_size() {
    // No resize, 200×100 at 1280×720: each cell keeps its texels in
    // target pixels, centred in the box.
    let keys = r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#;
    let native = |stretch, ex, target| {
        let l = shapes([1_280, 720], stretch, keys);
        shape_rects(l, ex, target)[0]
    };
    for stretch in [8, 9] {
        assert_eq!(native(stretch, 0, [1_280, 720]), [92, 662, 16, 16]);
        assert_eq!(native(stretch, 1, [1_280, 720]), [68, 654, 64, 32]);
        assert_eq!(native(stretch, 0, [1_920, 1_080]), [142, 997, 16, 16]);
        assert_eq!(native(stretch, 1, [1_920, 1_080]), [118, 989, 64, 32]);
        assert_eq!(native(stretch, 0, [3_840, 2_160]), [292, 2_002, 16, 16]);
        assert_eq!(native(stretch, 1, [3_840, 2_160]), [268, 1_994, 64, 32]);
    }
    // No expanding in a 40×20 box: the 16×16 cell always fits; the 64×32
    // one shrinks to the box (40×20, then 60×30 target pixels) until the
    // 3840×2160 target's 120×60 holds it.
    let keys = r#"{"x": 0, "y": 0, "w": 40, "h": 20}"#;
    let shrink = |ex, target| shape_rects(shapes([1_280, 720], 8, keys), ex, target)[0];
    assert_eq!(shrink(0, [1_280, 720]), [12, 702, 16, 16]);
    assert_eq!(shrink(1, [1_280, 720]), [0, 700, 40, 20]);
    assert_eq!(shrink(0, [1_920, 1_080]), [22, 1_057, 16, 16]);
    assert_eq!(shrink(1, [1_920, 1_080]), [0, 1_050, 60, 30]);
    assert_eq!(shrink(0, [3_840, 2_160]), [52, 2_122, 16, 16]);
    assert_eq!(shrink(1, [3_840, 2_160]), [28, 2_114, 64, 32]);
    // No resize ignores the box: the 64×32 cell overflows it.
    let keep = |ex, target| shape_rects(shapes([1_280, 720], 9, keys), ex, target)[0];
    assert_eq!(keep(1, [1_280, 720]), [-12, 694, 64, 32]);
}

/// A frame cut past the file's edge keeps the source's cell as its size:
/// an image and an image set's entry, each an 8×64 frame past a.png's
/// right edge, keep 8×64 target pixels under no resize, and the entry
/// fits that cell inside its box under stretch 1.
#[test]
fn a_frame_past_the_edge_keeps_its_cell() {
    let edge = r#""src": "a", "x": 64, "w": 8, "h": 64"#;
    let keys = r#"{"x": 0, "y": 0, "w": 200, "h": 100}"#;
    let body = format!(
        r#""image": [{{"id": "i", {edge}}}],
           "destination": [{{"id": "i", "timer": 41, "stretch": 9, "dst": [{keys}]}}]"#
    );
    assert_eq!(target_rects(&body, [1_920, 1_080], 0), [[146, 973, 8, 64]]);
    let set = |stretch: i32| {
        format!(
            r#""image": [{{"id": "small", "src": "a", "w": 16, "h": 16}}, {{"id": "e", {edge}}}],
               "imageset": [{{"id": "s", "images": ["small", "e"], "value": 71}}],
               "destination": [{{"id": "s", "stretch": {stretch}, "dst": [{keys}]}}]"#
        )
    };
    let entry = |stretch| {
        let l = import_sized([1_280, 720], &set(stretch), &[]);
        assert!(!has(l.report(), NoteKind::SourceRect), "{:?}", l.report());
        shape_rects(l, 1, [1_920, 1_080])
    };
    assert_eq!(entry(9), [[146, 973, 8, 64]]);
    // 8×64 inside 300×150 is 18.75×150, from x 140.625 to 159.375.
    assert_eq!(entry(1), [[141, 930, 18, 150]]);
}
