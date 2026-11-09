//! Blends through import and a frame, composed in software as the
//! renderer composes each run (design.md "Rendering and Skins",
//! premultiplied colour): an opaque white image standing where a title
//! is drawn, and a grey gradient drawn over it.

mod common;

use common::play::{LIMITS, texel_at};
use skin_model::{Atlas, Blend, Quad, QuadBatch};

/// The gradient's grey at row `y` of its 100×100 file: 212 at the middle
/// row, brighter toward both edges.
fn grey(y: u32) -> u8 {
    u8::try_from(212 + y.abs_diff(50).min(40)).unwrap()
}

/// A 1920×1080 skin with an opaque white image `w` (64×64 `a.png`) and a
/// gradient `g` (100×100 `g.png`), drawn as `dsts`.
fn skin(dsts: &str) -> common::Loaded {
    let json = common::hd(&format!(
        r#""source": [{{"id": "a", "path": "a.png"}}, {{"id": "gs", "path": "g.png"}}],
           "image": [{{"id": "w", "src": "a", "w": 64, "h": 64}},
                     {{"id": "g", "src": "gs", "w": 100, "h": 100}}],
           "destination": [{dsts}]"#
    ));
    let dir = common::skin_with(&json, &[("a.png", 64, 64)]);
    common::png_with(dir.path(), "g.png", 100, 100, |_, y| {
        let c = grey(y);
        [c, c, c, 255]
    });
    let result = common::import_dir(dir.path());
    common::Loaded { dir, result }
}

/// The white image and then the gradient with `fields`, over the same
/// 200×100 box.
fn header(fields: &str) -> String {
    let key = r#"{"x": 100, "y": 100, "w": 200, "h": 100}"#;
    format!(r#"{{"id": "w", "dst": [{key}]}}, {{"id": "g"{fields}, "dst": [{key}]}}"#)
}

/// The RGB (0 to 255) at canvas point `p`: black under every quad, then
/// each run's quads covering `p` in push order, each texel (premultiplied,
/// nearest, level 0) times its straight tint premultiplied once, blended
/// by its run's blend.
fn composite(batch: &QuadBatch, atlas: &Atlas, [x, y]: [f32; 2]) -> [f32; 3] {
    let mut dst = [0.0_f32; 3];
    for run in batch.runs() {
        let quads: &[Quad] = &batch.quads()[run.start as usize..run.end as usize];
        for q in quads {
            let [qx, qy, qw, qh] = q.rect;
            if !(x >= qx && x < qx + qw && y >= qy && y < qy + qh) {
                continue;
            }
            let t = texel_at(atlas, q, (x - qx) / qw, (y - qy) / qh).map(|c| f32::from(c) / 255.0);
            let [r, g, b, a] = q.color;
            let tint = [r * a, g * a, b * a, a];
            let src: [f32; 4] = std::array::from_fn(|c| t[c] * tint[c]);
            for (c, d) in dst.iter_mut().enumerate() {
                *d = match run.key.blend {
                    Blend::Alpha => src[c] + *d * (1.0 - src[3]),
                    Blend::Add => src[c] + *d,
                };
            }
        }
    }
    dst.map(|c| c * 255.0)
}

/// The import's frame at `ms`: its batch's run blends and the RGB at the
/// centre of the box rows `row` of the gradient's 100 draw at.
fn frame(l: common::Loaded, ms: i64, row: u32) -> (Vec<Blend>, [f32; 3]) {
    assert!(l.report().notes.is_empty(), "{:?}", l.report());
    let atlas = l
        .ok()
        .skin
        .atlas(skin_model::Variant::Lo, LIMITS, &|| false)
        .unwrap();
    let mut d = l.drawn();
    let quads = d.frame(ms, &[]);
    let [q, ..] = quads.as_slice() else {
        panic!("{quads:?}")
    };
    let [x, y, w, h] = q.rect;
    #[expect(clippy::cast_precision_loss, reason = "rows are small")]
    let at = [x + w / 2.0, y + (row as f32 + 0.5) * h / 100.0];
    let blends = d.batch.runs().iter().map(|r| r.key.blend).collect();
    (blends, composite(&d.batch, &atlas, at))
}

#[track_caller]
fn near(got: [f32; 3], want: f32) {
    assert!(
        got.iter().all(|g| (g - want).abs() <= 1.0),
        "{got:?}, expected {want}"
    );
}

#[test]
fn the_white_image_alone_composes_white() {
    let key = r#"{"x": 100, "y": 100, "w": 200, "h": 100}"#;
    let (blends, rgb) = frame(skin(&format!(r#"{{"id": "w", "dst": [{key}]}}"#)), 0, 50);
    assert_eq!(blends, [Blend::Alpha]);
    near(rgb, 255.0);
}

#[test]
fn an_opaque_gradient_drawn_normally_replaces_the_white_under_it() {
    for row in [0, 50, 99] {
        let (blends, rgb) = frame(skin(&header("")), 0, row);
        assert_eq!(blends, [Blend::Alpha]);
        near(rgb, f32::from(grey(row)));
    }
}
