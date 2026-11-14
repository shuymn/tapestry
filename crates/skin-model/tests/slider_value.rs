//! A cover paint moved by a program value (`SliderRate::Program`, the
//! source's `skin/SkinSlider.java:104-120` with a `value`): the float
//! conversion (`tofloat()`, `skin/lua/SkinLuaAccessor.java:665-673` at
//! `0.8.9`), its storage as `f32` bits, the displacement `R · value`
//! unclamped, a non-finite value at 0, no value and a displacement past
//! `MAX_SLIDE` not drawn, and the checks: V6 keeps expressions off a float
//! output's value, V9 gives each float output at most one slider.

mod lanes_support;
mod play_support;
mod present_support;
mod widget_support;

use present_support::{Builder, Cb, load, read};
use skin_model::present::num::to_float;
use skin_model::present::{Arith, Conv, Insn, OutTy, Val};
use skin_model::{
    CoverImage, CoverMotion, CoverPaintDef, CoverTrim, ElementDef, LoadError, MAX_SLIDE, PLAY,
    Place, Quad, Ratio, Scope, Skin, Slider, SliderDirection, SliderRate, Source, Ty, WidgetDefs,
    compile,
};
use widget_support::{element, harness};

/// The paint's rect: x 100, y 500, 20×20.
const AT: [i32; 4] = [100, 500, 20, 20];

/// A paint of image 0 moved by callback `k`'s value times `range` whole
/// reference pixels toward `direction`.
fn paint(k: u16, range: u32, direction: SliderDirection) -> CoverPaintDef {
    exact_paint(
        k,
        Ratio {
            numerator: u64::from(range),
            denominator: 1,
        },
        direction,
    )
}

/// A paint of image 0 moved by callback `k`'s value times `range` toward
/// `direction`.
fn exact_paint(k: u16, range: Ratio, direction: SliderDirection) -> CoverPaintDef {
    CoverPaintDef {
        image: CoverImage::Image(0),
        slider: Some(Slider {
            range,
            rate: SliderRate::Program(k),
            direction,
        }),
        motion: CoverMotion::None,
        trim: CoverTrim::None,
        gate_track: None,
    }
}

/// A skin of `b`'s program with one paint element per paint of `paints`.
fn skin(b: Builder, paints: Vec<CoverPaintDef>) -> Result<Skin, LoadError> {
    let elements: Vec<ElementDef> = (0..paints.len())
        .map(|p| {
            element(
                Source::CoverPaint(u32::try_from(p).unwrap()),
                AT,
                Place::SCREEN,
            )
        })
        .collect();
    let widgets = WidgetDefs {
        cover_paints: paints,
        ..WidgetDefs::default()
    };
    load(Some(b.0), Vec::new(), elements, widgets)
}

/// The quads of one frame of a down slider of range 100 moved by `cb`.
fn frame(cb: Cb) -> Vec<Quad> {
    let mut b = Builder::default();
    let k = b.push(cb);
    let s = skin(b, vec![paint(k, 100, SliderDirection::Down)]).unwrap();
    let mut h = harness(s);
    h.frame(1_000, &[]);
    h.batch.quads().to_vec()
}

fn y(cb: Cb) -> Option<f32> {
    let q = frame(cb);
    assert!(q.len() <= 1, "{q:?}");
    q.first().map(|q| q.rect[1])
}

fn float(v: f64) -> Cb {
    Cb::ret(Val::Float(v), Conv::Float)
}

#[test]
fn the_value_moves_the_paint_by_its_range_unclamped() {
    assert_eq!(y(float(0.0)), Some(500.0));
    assert_eq!(y(float(0.25)), Some(525.0));
    assert_eq!(y(float(-0.4)), Some(460.0));
    assert_eq!(y(float(3.0)), Some(800.0));
    assert_eq!(y(Cb::ret(Val::Int(2), Conv::Float)), Some(700.0));
    // Nil and booleans are 0.
    for v in [Val::Nil, Val::Bool(true), Val::Bool(false)] {
        assert_eq!(y(Cb::ret(v, Conv::Float)), Some(500.0), "{v:?}");
    }
}

#[test]
fn each_direction_moves_its_own_way() {
    for (d, dx, dy) in [
        (SliderDirection::Up, 0.0, -50.0),
        (SliderDirection::Right, 50.0, 0.0),
        (SliderDirection::Down, 0.0, 50.0),
        (SliderDirection::Left, -50.0, 0.0),
    ] {
        let mut b = Builder::default();
        let k = b.push(float(0.5));
        let mut h = harness(skin(b, vec![paint(k, 100, d)]).unwrap());
        h.frame(1_000, &[]);
        let r = h.batch.quads()[0].rect;
        assert_eq!([r[0], r[1]], [100.0 + dx, 500.0 + dy], "{d:?}");
    }
}

#[test]
fn a_non_finite_value_draws_at_zero() {
    for v in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 1e300, -1e300] {
        assert_eq!(y(float(v)), Some(500.0), "{v}");
    }
}

#[test]
fn a_caught_lua_error_draws_at_zero() {
    // `nil + 1`: a Lua error, which the source catches as `0f`.
    let code = vec![
        Insn::Const { dst: 0, k: 0 },
        Insn::Const { dst: 1, k: 1 },
        Insn::Arith {
            op: Arith::Add,
            dst: 2,
            a: 0,
            b: 1,
        },
        Insn::Ret { src: 2 },
    ];
    let cb = Cb::code(code, vec![Val::Nil, Val::Int(1)], Vec::new(), Conv::Float);
    let mut b = Builder::default();
    let k = b.push(cb);
    let s = skin(b, vec![paint(k, 100, SliderDirection::Down)]).unwrap();
    let mut h = harness(s);
    h.frame(1_000, &[]);
    assert_eq!(h.batch.quads()[0].rect[1], 500.0);
    let f = h.play.state().fields;
    let [.., v, ok] = f else { panic!() };
    assert_eq!((*v, *ok), (0, 1), "+0's bits, with a value");
}

#[test]
fn no_value_is_not_drawn() {
    // A read whose guard is false: unavailable.
    let code = vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }];
    let r = read("combo", Ty::Int, Some("false"));
    assert_eq!(y(Cb::code(code, Vec::new(), vec![r], Conv::Float)), None);
}

/// The y of a down slider of `n / d` moved by `value`, `None` when not
/// drawn.
fn exact_y(n: u64, d: u64, value: f64) -> Option<f32> {
    let mut b = Builder::default();
    let k = b.push(float(value));
    let range = Ratio {
        numerator: n,
        denominator: d,
    };
    let s = skin(b, vec![exact_paint(k, range, SliderDirection::Down)]).unwrap();
    let mut h = harness(s);
    h.frame(1_000, &[]);
    h.batch.quads().first().map(|q| q.rect[1])
}

#[test]
fn a_fractional_range_moves_the_paint_exactly() {
    // 15/2 · ±1/2 = ±3.75: not a floored 7's 3.5 nor the numerator's 7.5.
    assert_eq!(exact_y(15, 2, 0.5), Some(503.75));
    assert_eq!(exact_y(15, 2, -0.5), Some(496.25));
    // The bound applies after the division: 1/2 of 1.5 · 2^40 is drawn,
    // 1/2 of 2.5 · 2^40 is past it.
    assert!(exact_y(1, 2, MAX_SLIDE * 1.5).is_some());
    assert_eq!(exact_y(1, 2, MAX_SLIDE * 2.5), None);
}

#[test]
fn past_the_bound_it_is_not_drawn() {
    let at = MAX_SLIDE / 100.0;
    assert!(y(float(at)).is_some());
    assert!(y(float(-at)).is_some());
    assert_eq!(y(float(at * 1.000_001)), None);
    assert_eq!(y(float(-at * 1.000_001)), None);
    assert_eq!(y(float(3e38)), None);
}

#[test]
fn the_output_holds_the_float_s_bits() {
    let bits = |v: Val| {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(v, Conv::Float));
        let mut h = harness(skin(b, vec![paint(k, 100, SliderDirection::Down)]).unwrap());
        h.frame(1_000, &[]);
        let f = h.play.state().fields;
        let [.., v, ok] = f else { panic!() };
        assert_eq!(*ok, 1);
        #[expect(clippy::cast_sign_loss, reason = "the slot holds the bits")]
        let b = *v as u32;
        b
    };
    assert_eq!(bits(Val::Float(0.1)), 0.1_f32.to_bits());
    assert_eq!(bits(Val::Float(-0.0)), (-0.0_f32).to_bits());
    assert_eq!(bits(Val::Float(-1e-300)), (-0.0_f32).to_bits());
    assert_eq!(bits(Val::Int(16_777_217)), 16_777_216.0_f32.to_bits());
}

#[test]
fn to_float_is_i2f_and_d2f() {
    assert_eq!(to_float(Val::Int(i64::from(i32::MAX))), 2_147_483_648.0);
    assert_eq!(to_float(Val::Int(-(1 << 53))), -9_007_199_254_740_992.0);
    assert_eq!(to_float(Val::Float(3.5e38)), f32::INFINITY);
    assert_eq!(to_float(Val::Float(-3.5e38)), f32::NEG_INFINITY);
    assert!(to_float(Val::Float(f64::NAN)).is_nan());
    assert_eq!(to_float(Val::Float(1e-46)).to_bits(), 0);
    assert_eq!(to_float(Val::Nil), 0.0);
    assert_eq!(to_float(Val::Bool(true)), 0.0);
}

#[test]
fn an_expression_reads_a_float_output_s_ok_but_not_its_value() {
    let scope = Scope::new(&PLAY).outputs(&[OutTy::Float]);
    assert!(compile("lua.0", &scope, Ty::Int).is_err());
    assert!(compile("lua.0 > 0", &scope, Ty::Bool).is_err());
    assert!(compile("lua.0.ok", &scope, Ty::Bool).is_ok());
}

#[test]
fn each_float_output_has_at_most_one_slider() {
    // Two sliders on one output.
    let mut b = Builder::default();
    let k = b.push(float(0.5));
    let two = vec![
        paint(k, 100, SliderDirection::Down),
        paint(k, 100, SliderDirection::Up),
    ];
    assert!(matches!(
        skin(b, two),
        Err(LoadError::SliderOutput { paint: 1 })
    ));
    // A slider on an integer output.
    let mut b = Builder::default();
    let k = b.push(Cb::ret(Val::Int(1), Conv::Int));
    let on_int = vec![paint(k, 100, SliderDirection::Down)];
    assert!(matches!(
        skin(b, on_int),
        Err(LoadError::SliderOutput { paint: 0 })
    ));
    // A slider on no output.
    let b = Builder::default();
    let none = vec![paint(0, 100, SliderDirection::Down)];
    assert!(matches!(
        skin(b, none),
        Err(LoadError::SliderOutput { paint: 0 })
    ));
    // A float output no slider reads is accepted.
    let mut b = Builder::default();
    b.push(float(0.5));
    assert!(skin(b, Vec::new()).is_ok());
}
