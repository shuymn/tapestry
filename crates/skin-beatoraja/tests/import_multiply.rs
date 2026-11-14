//! Blend 4, multiply, through import and a frame, composed in software as
//! the renderer composes each run (design.md "Rendering and Skins",
//! premultiplied colour): an opaque white image standing where a title is
//! drawn, and a grey gradient drawn over it (`skin/Skin.java:663` at
//! `0.8.9`). Images, image choices and player-drawn images multiply;
//! other objects keep dropping blend 4.

mod common;

use common::lanes::{cover, with};
use common::play::{LIMITS, texel_at};
use common::{has, import_body};
use skin_beatoraja::{ImportError, NoteKind, Severity};
use skin_model::{Atlas, Blend, Quad, QuadBatch};

/// The 200×100 box every destination here draws in.
const KEY: &str = r#"{"x": 100, "y": 100, "w": 200, "h": 100}"#;

/// The gradient's grey at row `y` of its 100×100 file: 212 at the middle
/// row, brighter toward both edges.
fn grey(y: u32) -> u8 {
    u8::try_from(212 + y.abs_diff(50).min(40)).unwrap()
}

/// A 1920×1080 skin with an opaque white image `w` (64×64 `a.png`), a
/// gradient `g` (100×100 `g.png`), `objects` besides and `dsts`.
fn skin_of(objects: &str, dsts: &str) -> common::Loaded {
    let sep = if objects.is_empty() { "" } else { ", " };
    let json = common::hd(&format!(
        r#""source": [{{"id": "a", "path": "a.png"}}, {{"id": "gs", "path": "g.png"}}],
           "image": [{{"id": "w", "src": "a", "w": 64, "h": 64}},
                     {{"id": "g", "src": "gs", "w": 100, "h": 100}},
                     {{"id": "gg", "src": "gs", "w": 100, "h": 100, "divy": 2, "len": 2,
                       "ref": 500}}]{sep}{objects},
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

fn skin(dsts: &str) -> common::Loaded {
    skin_of("", dsts)
}

/// The white image and then `id` with `fields`, in the same box.
fn over_white(id: &str, fields: &str) -> String {
    format!(r#"{{"id": "w", "dst": [{KEY}]}}, {{"id": "{id}"{fields}, "dst": [{KEY}]}}"#)
}

/// The white image and then the gradient with `fields`.
fn header(fields: &str) -> String {
    over_white("g", fields)
}

/// The RGB (0 to 255) at canvas point `p`: black under every quad, then
/// each run's quads covering `p` in push order, each texel (premultiplied,
/// nearest, level 0) times its straight tint premultiplied once, blended
/// by its run's blend; multiply takes the straight colour, the product
/// divided by its alpha, black at alpha 0.
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
                    Blend::Multiply if src[3] > 0.0 => *d * src[c] / src[3],
                    Blend::Multiply => 0.0,
                };
            }
        }
    }
    dst.map(|c| c * 255.0)
}

/// A frame of an import: its runs' blends, its quads, and what
/// [`composite`] gives at a point.
struct Frame {
    blends: Vec<Blend>,
    quads: Vec<Quad>,
    batch: QuadBatch,
    atlas: Atlas,
}

impl Frame {
    /// The frame at `ms` of `l`, whatever its notes.
    fn of(l: common::Loaded, ms: i64) -> Self {
        let atlas = l
            .ok()
            .skin
            .atlas(skin_model::Variant::Lo, LIMITS, &|| false)
            .unwrap();
        let mut d = l.drawn();
        let quads = d.frame(ms, &[]);
        let blends = d.batch.runs().iter().map(|r| r.key.blend).collect();
        Self {
            blends,
            quads,
            batch: d.batch,
            atlas,
        }
    }

    /// The frame at `ms` of `l`, which has no note.
    #[track_caller]
    fn clean(l: common::Loaded, ms: i64) -> Self {
        assert!(l.report().notes.is_empty(), "{:?}", l.report());
        Self::of(l, ms)
    }

    /// The RGB at the centre of row `row` of 100 of the box.
    fn at_row(&self, row: u32) -> [f32; 3] {
        let [x, y, w, h] = self.quads[0].rect;
        #[expect(clippy::cast_precision_loss, reason = "rows are small")]
        let at = [x + w / 2.0, y + (row as f32 + 0.5) * h / 100.0];
        composite(&self.batch, &self.atlas, at)
    }

    /// The RGB at the centre of the box, and the red of the texel the
    /// last quad samples there.
    fn centre(&self) -> ([f32; 3], u8) {
        let q = self.quads.last().unwrap();
        let [x, y, w, h] = q.rect;
        let rgb = composite(&self.batch, &self.atlas, [x + w / 2.0, y + h / 2.0]);
        (rgb, texel_at(&self.atlas, q, 0.5, 0.5)[0])
    }
}

#[track_caller]
fn near(got: [f32; 3], want: f32) {
    assert!(
        got.iter().all(|g| (g - want).abs() <= 1.0),
        "{got:?}, expected {want}"
    );
}

/// Whether `l`'s report drops something for blend `b`.
fn drops_blend(l: &common::Loaded, b: i32) -> bool {
    l.report()
        .notes
        .iter()
        .any(|n| n.severity == Severity::Dropped && n.kind == NoteKind::Blend(b))
}

#[test]
fn the_white_image_alone_composes_white() {
    let f = Frame::clean(skin(&format!(r#"{{"id": "w", "dst": [{KEY}]}}"#)), 0);
    assert_eq!(f.blends, [Blend::Alpha]);
    near(f.at_row(50), 255.0);
}

#[test]
fn an_opaque_gradient_drawn_normally_replaces_the_white_under_it() {
    let f = Frame::clean(skin(&header("")), 0);
    assert_eq!(f.blends, [Blend::Alpha]);
    for row in [0, 50, 99] {
        near(f.at_row(row), f32::from(grey(row)));
    }
}

#[test]
fn a_gradient_of_blend_four_multiplies_what_is_under_it() {
    // The header's shape: the gradient drawn after the title, in a run
    // of its own.
    let f = Frame::clean(skin(&header(r#", "blend": 4"#)), 0);
    assert_eq!(f.blends, [Blend::Alpha, Blend::Multiply]);
    for row in [0, 25, 50, 99] {
        near(f.at_row(row), f32::from(grey(row)));
    }
    // Over a grey of 128 (the white tinted), the product: 128 × 212 / 255.
    let dsts = format!(
        r#"{{"id": "w", "dst": [{{"x": 100, "y": 100, "w": 200, "h": 100,
                                  "r": 128, "g": 128, "b": 128}}]}},
           {{"id": "g", "blend": 4, "dst": [{KEY}]}}"#
    );
    let f = Frame::clean(skin(&dsts), 0);
    near(f.at_row(50), 128.0 * 212.0 / 255.0);
}

/// Blend 3 draws as blend 2, additive, in a JSON skin and a Lua skin
/// alike, with one note at its destination: the source sets the
/// subtract equation and resets it to add before its quads draw
/// (`skin/Skin.java:657-662` at `0.8.9`).
// Source: SF-006
#[test]
fn blend_three_draws_additive() {
    let notes = |r: &skin_beatoraja::Report| -> Vec<_> {
        r.notes.iter().map(|n| (n.severity, n.kind)).collect()
    };
    let add = Frame::clean(skin(&header(r#", "blend": 2"#)), 0);
    let l = skin(&header(r#", "blend": 3"#));
    assert_eq!(
        notes(l.report()),
        [(Severity::Ignored, NoteKind::SubtractBlend)]
    );
    let f = Frame::of(l, 0);
    assert_eq!(f.blends, [Blend::Alpha, Blend::Add]);
    for row in [0, 50, 99] {
        near(f.at_row(row), 255.0 + f32::from(grey(row)));
        assert_eq!(f.at_row(row), add.at_row(row), "{row}");
    }
    let lua = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
        if not skin_config then return skin end\n\
        skin.source = { { id = \"a\", path = \"a.png\" } }\n\
        skin.image = { { id = \"w\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
        skin.destination = {\n\
          { id = \"w\", dst = { { x = 0, y = 0, w = 64, h = 64 } } },\n\
          { id = \"w\", blend = 3, dst = { { x = 0, y = 0, w = 64, h = 64 } } } }\n\
        return skin\n";
    let d = common::skin(&[("s.luaskin", lua)]);
    common::png(d.path(), "a.png", 64, 64);
    let i = common::lua_import(d.path()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        notes(&i.imported.report),
        [(Severity::Ignored, NoteKind::SubtractBlend)]
    );
    let mut d = common::play::Drawn::new(i.imported.skin, 0);
    d.frame(0, &[]);
    let blends: Vec<_> = d.batch.runs().iter().map(|r| r.key.blend).collect();
    assert_eq!(blends, [Blend::Alpha, Blend::Add]);
}

#[test]
fn multiply_ignores_the_tint_alpha_but_not_its_colour() {
    // A tint of 128 grey at alpha 64: the white times 212/255 times
    // 128/255, not a quarter of it (O1 A, `Skin.java:663`).
    let dsts = format!(
        r#"{{"id": "w", "dst": [{KEY}]}},
           {{"id": "g", "blend": 4, "dst": [{{"x": 100, "y": 100, "w": 200, "h": 100,
                                             "a": 64, "r": 128, "g": 128, "b": 128}}]}}"#
    );
    let f = Frame::clean(skin(&dsts), 0);
    near(f.at_row(50), 212.0 * 128.0 / 255.0);
}

#[test]
fn a_fading_multiply_draws_nothing_at_alpha_zero_and_fully_after() {
    // A fade from alpha 0 at 200 ms to 255 at 700 ms, as a fade-in of a
    // standby gradation is written. At 200 ms no multiply quad
    // (`SkinObject.java:623-625`): the white shows; one ms later the full
    // product.
    let dsts = r#"{"id": "w", "dst": [{"x": 100, "y": 100, "w": 200, "h": 100}]},
        {"id": "g", "blend": 4, "dst": [{"time": 200, "x": 100, "y": 100, "w": 200, "h": 100,
                                         "a": 0},
                                        {"time": 700, "a": 255}]}"#;
    let f = Frame::clean(skin(dsts), 200);
    assert_eq!(f.blends, [Blend::Alpha]);
    assert_eq!(f.quads.len(), 1);
    near(f.at_row(50), 255.0);
    let f = Frame::clean(skin(dsts), 201);
    assert_eq!(f.blends, [Blend::Alpha, Blend::Multiply]);
    near(f.at_row(50), 212.0);
}

#[test]
fn image_choices_multiply_and_an_absent_entry_draws_nothing() {
    // An image choice by `len` (two 100×50 frames, ref 500 selecting the
    // first) and an image set of the gradient.
    let set = r#""imageset": [{"id": "s", "images": ["g"]}, {"id": "hole", "images": [null]}]"#;
    for id in ["gg", "s"] {
        let f = Frame::clean(skin_of(set, &over_white(id, r#", "blend": 4"#)), 0);
        assert_eq!(f.blends, [Blend::Alpha, Blend::Multiply], "{id}");
        let (rgb, texel) = f.centre();
        assert!(texel < 255, "{id}: {texel}");
        near(rgb, f32::from(texel));
    }
    // A set whose selected entry is a hole draws no quad: the white shows.
    let f = Frame::of(skin_of(set, &over_white("hole", r#", "blend": 4"#)), 0);
    assert_eq!(
        (f.blends.as_slice(), f.quads.len()),
        ([Blend::Alpha].as_slice(), 1)
    );
    near(f.centre().0, 255.0);
}

#[test]
fn player_drawn_images_multiply() {
    // −110 is black: the white becomes black. −111 is white, here tinted
    // 128: the white becomes 128.
    let f = Frame::clean(skin(&over_white("-110", r#", "blend": 4"#)), 0);
    assert_eq!(f.blends, [Blend::Alpha, Blend::Multiply]);
    near(f.centre().0, 0.0);
    let dsts = format!(
        r#"{{"id": "w", "dst": [{KEY}]}},
           {{"id": "-111", "blend": 4, "dst": [{{"x": 100, "y": 100, "w": 200, "h": 100,
                                                "r": 128, "g": 128, "b": 128}}]}}"#
    );
    let f = Frame::clean(skin(&dsts), 0);
    assert_eq!(f.blends, [Blend::Alpha, Blend::Multiply]);
    near(f.centre().0, 128.0);
}

#[test]
fn blend_four_stays_unsupported_off_images() {
    // A number drops (numbers are deferred).
    let l = import_body(
        r#""value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 110}],
           "destination": [{"id": "v", "blend": 4, "dst": [{"w": 12, "h": 20}]}]"#,
    );
    assert!(drops_blend(&l, 4), "{:?}", l.report());
    assert_eq!(l.report().usage.elements, 1);
    // SUDDEN+'s paint, play-critical, refuses the skin.
    match with("", &[&cover(r#""blend": 4"#)]).err() {
        ImportError::Critical { why, .. } => assert_eq!(why, NoteKind::Blend(4)),
        e => panic!("{e:?}"),
    }
    // Blend 9 still drops on an image.
    let l = skin(&header(r#", "blend": 9"#));
    assert!(drops_blend(&l, 9), "{:?}", l.report());
    assert!(!has(l.report(), NoteKind::Blend(4)));
}
