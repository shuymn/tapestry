//! The multiply blend through a build, composed as the renderer composes
//! it (`composite_runs`): the destination's RGB times the quad's straight
//! colour, texel times tint, the alpha taking no part
//! (`skin/Skin.java:663` at `0.8.9`), the texel's colour being the
//! atlas's 8-bit premultiplied one unpremultiplied (D23).

mod graph_support;

use bms_model::TimeUs;
use graph_support::{BACKDROP, GFx, SOLID, composite_runs, near, skin_more};
use skin_model::{
    Blend, ElementDef, Filter, KeyDef, Place, SheetBuilder, SolidImageDef, Source, TimerRef, Track,
    result_timer,
};

/// The images added after the generated ones: opaque grey 212, grey 128
/// at alpha 128, grey 127 at alpha 1, and transparent.
const GREY: u32 = graph_support::GRID_NO_GAPS + 1;
const HALF: u32 = GREY + 1;
const FAINT: u32 = GREY + 2;
const CLEAR: u32 = GREY + 3;

fn images(b: &mut SheetBuilder) {
    for (i, color) in [
        [212, 212, 212, 255],
        [128, 128, 128, 128],
        [127, 127, 127, 1],
        [0, 0, 0, 0],
    ]
    .into_iter()
    .enumerate()
    {
        let at = b
            .add_solid_image(SolidImageDef {
                size: [8, 8],
                color,
                filter: Filter::Nearest,
                grid: None,
            })
            .unwrap();
        assert_eq!(at, GREY + u32::try_from(i).unwrap());
    }
}

/// A key at `ms` over the 100×100 box at the top left, at `alpha`, tinted
/// `rgb`.
fn key(ms: i64, alpha: i32, rgb: i32) -> KeyDef {
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip([0, 0, 100, 100, alpha, rgb, rgb, rgb]) {
        *p = Some(v);
    }
    KeyDef {
        at: TimeUs(ms * 1_000),
        props,
    }
}

/// Image `image` drawn with `blend` through `keys`, on the screen timer.
fn element(image: u32, blend: Blend, keys: &[KeyDef]) -> ElementDef {
    ElementDef {
        source: Source::Image(image),
        track: Track::new(keys, skin_model::Ease::Linear, skin_model::End::Hold).unwrap(),
        timer: TimerRef::Builtin(result_timer::SCREEN),
        visible: None,
        blend,
        clip: None,
        place: Place::SCREEN,
    }
}

/// The white solid over the box, then `top`.
fn over_white(top: ElementDef) -> GFx {
    GFx::new(skin_more(
        vec![element(SOLID, Blend::Alpha, &[key(0, 255, 255)]), top],
        images,
    ))
}

/// The frame at `ms`: its runs' blends and the RGB at the box's centre.
fn at(fx: &mut GFx, ms: i64) -> (Vec<Blend>, [f32; 3]) {
    fx.frame(ms);
    let blends = fx.batch.runs().iter().map(|r| r.key.blend).collect();
    (blends, composite_runs(&fx.batch, &fx.atlas, [50.0, 50.0]).0)
}

fn all(v: f32) -> [f32; 3] {
    [v; 3]
}

#[test]
fn an_opaque_grey_multiplies_by_its_colour() {
    let mut fx = over_white(element(GREY, Blend::Multiply, &[key(0, 255, 255)]));
    let (blends, rgb) = at(&mut fx, 0);
    assert_eq!(blends, [Blend::Alpha, Blend::Multiply]);
    near(rgb, all(212.0), 0.5);
    // Over the backdrop alone: the product.
    let mut fx = GFx::new(skin_more(
        vec![element(GREY, Blend::Multiply, &[key(0, 255, 255)])],
        images,
    ));
    near(at(&mut fx, 0).1, all(BACKDROP * 212.0 / 255.0), 0.5);
}

#[test]
fn the_tint_s_alpha_takes_no_part() {
    // Grey 128 at alpha 64 multiplies as fully as at 255 (O1 A).
    let mut fx = over_white(element(SOLID, Blend::Multiply, &[key(0, 64, 128)]));
    near(at(&mut fx, 0).1, all(128.0), 0.5);
}

#[test]
fn a_texel_s_colour_is_its_premultiplied_colour_unpremultiplied() {
    // Grey 128 at alpha 128 is stored 64 at 128: 128 within rounding.
    let mut fx = over_white(element(HALF, Blend::Multiply, &[key(0, 255, 255)]));
    near(at(&mut fx, 0).1, all(128.0), 1.0);
    // Grey 127 at alpha 1 is stored black at 1: black, where the source
    // multiplies by 127 (D23).
    let mut fx = over_white(element(FAINT, Blend::Multiply, &[key(0, 255, 255)]));
    near(at(&mut fx, 0).1, all(0.0), 0.5);
    // A transparent texel: black.
    let mut fx = over_white(element(CLEAR, Blend::Multiply, &[key(0, 255, 255)]));
    near(at(&mut fx, 0).1, all(0.0), 0.5);
}

#[test]
fn a_fading_multiply_is_not_drawn_at_alpha_zero() {
    // Alpha 0 at 0 ms, 255 at 100 ms: no multiply quad at 0 ms
    // (`SkinObject.java:623-625`), the full product at 1 ms.
    let mut fx = over_white(element(
        GREY,
        Blend::Multiply,
        &[key(0, 0, 255), key(100, 255, 255)],
    ));
    let (blends, rgb) = at(&mut fx, 0);
    assert_eq!(
        (blends.as_slice(), fx.quads().len()),
        ([Blend::Alpha].as_slice(), 1)
    );
    near(rgb, all(255.0), 0.5);
    let (blends, rgb) = at(&mut fx, 1);
    assert_eq!(blends, [Blend::Alpha, Blend::Multiply]);
    near(rgb, all(212.0), 0.5);
}

#[test]
fn multiply_runs_draw_in_push_order() {
    // White, then grey multiplied (212), then the white again over it.
    let mut fx = GFx::new(skin_more(
        vec![
            element(SOLID, Blend::Alpha, &[key(0, 255, 255)]),
            element(GREY, Blend::Multiply, &[key(0, 255, 255)]),
            element(SOLID, Blend::Alpha, &[key(0, 128, 255)]),
        ],
        images,
    ));
    let (blends, rgb) = at(&mut fx, 0);
    assert_eq!(blends, [Blend::Alpha, Blend::Multiply, Blend::Alpha]);
    let a = 128.0 / 255.0;
    near(rgb, all(255.0 * a + 212.0 * (1.0 - a)), 1.0);
}
