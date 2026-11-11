//! The program's numerics past ±2^53 (design.md "beatoraja Lua Skins",
//! numbers): every value is the double the source holds, an integer kept
//! as one only within ±2^53, each `+ - *` of integers rounding its exact
//! result once, and every function total. The source's values are pinned
//! by `crates/skin-beatoraja/tests/luaj_numerics.txt`; these pin the
//! representation, which no comparison by value sees.

use skin_model::present::num::{arith, clock, extreme, math, neg, to_int};
use skin_model::present::{Arith, Fail, MathFn, Val};

const TWO_53: i64 = 1 << 53;
const TWO_53_F: f64 = 9_007_199_254_740_992.0;
const TWO_63_F: f64 = 9_223_372_036_854_775_808.0;

/// `v` as its tag and IEEE bits, so a float and an integer of the same
/// value differ, and so do two floats one ulp apart.
fn bits(v: Val) -> (char, u64) {
    match v {
        Val::Int(n) => ('i', n.cast_unsigned()),
        Val::Float(f) => ('f', f.to_bits()),
        Val::Bool(b) => ('b', u64::from(b)),
        Val::Nil => ('n', 0),
    }
}

fn assert_val(got: Result<Val, Fail>, want: Val, what: &str) {
    let got = got.unwrap_or_else(|e| panic!("{what}: {e:?}"));
    assert_eq!(bits(got), bits(want), "{what}: {got:?}, want {want:?}");
}

/// `+ - *` of two integers is the integer within ±2^53 and past it the
/// double of the exact result, never an integer, past `i64` too.
#[test]
fn integer_arithmetic_past_2_53_is_the_rounded_double() {
    let cases = [
        // Within: integers.
        (Arith::Add, TWO_53 - 1, 1, Val::Int(TWO_53)),
        (Arith::Sub, -TWO_53 + 1, 1, Val::Int(-TWO_53)),
        (
            Arith::Mul,
            94_906_265,
            94_906_265,
            Val::Int(9_007_199_136_250_225),
        ),
        // Just past: 2^53 + 1 rounds to even, 2^53.
        (Arith::Add, TWO_53, 1, Val::Float(TWO_53_F)),
        (Arith::Sub, -TWO_53, 1, Val::Float(-TWO_53_F)),
        // 2^53 + 3 rounds to even, up.
        (Arith::Add, TWO_53, 3, Val::Float(TWO_53_F + 4.0)),
        // The source's tie (2^53 + 2^31 - 1 times 2^22 + 1).
        (
            Arith::Mul,
            2_147_483_647,
            4_194_305,
            Val::Float(f64::from_bits(0x4340_0000_3fe0_0000)),
        ),
        // Past `i64`: the exact product's double, not a wrapped one.
        (Arith::Mul, TWO_53, TWO_53, Val::Float(2.0_f64.powi(106))),
        (
            Arith::Mul,
            -TWO_53,
            TWO_53,
            Val::Float(-(2.0_f64.powi(106))),
        ),
        (Arith::Add, i64::MAX, i64::MAX, Val::Float(2.0_f64.powi(64))),
        (
            Arith::Mul,
            2_147_483_647,
            2_147_483_647,
            Val::Float(4_611_686_014_132_420_609.0),
        ),
    ];
    for (op, x, y, want) in cases {
        assert_val(
            arith(op, Val::Int(x), Val::Int(y)),
            want,
            &format!("{op:?} {x} {y}"),
        );
    }
}

/// The values the source shows for a chain, each operation rounded once
/// (`Ex.out`): rounded only at the end they differ.
#[test]
fn a_chain_rounds_at_each_operation() {
    let p = arith(Arith::Mul, Val::Int(2_147_483_647), Val::Int(4_194_305)).unwrap();
    let a = arith(Arith::Mul, p, Val::Int(3)).unwrap();
    assert_eq!(to_int(a), 2_134_900_736);
    let n = Val::Int(TWO_53);
    let b = arith(
        Arith::Add,
        arith(Arith::Add, n, Val::Int(1)).unwrap(),
        Val::Int(1),
    )
    .unwrap();
    assert_eq!(to_int(b), 0);
    let c = arith(
        Arith::Add,
        arith(Arith::Add, n, Val::Int(1)).unwrap(),
        Val::Int(2),
    )
    .unwrap();
    assert_eq!(to_int(c), 2);
}

/// `floor` and `ceil` of a float give an integer only within ±2^53, the
/// float past it, an infinity or NaN.
#[test]
fn floor_and_ceil_give_an_integer_only_within_2_53() {
    let cases = [
        (
            MathFn::Floor,
            2.0_f64.powi(60),
            Val::Float(2.0_f64.powi(60)),
        ),
        (MathFn::Ceil, 2.0_f64.powi(60), Val::Float(2.0_f64.powi(60))),
        (MathFn::Floor, -TWO_63_F, Val::Float(-TWO_63_F)),
        (MathFn::Ceil, TWO_63_F, Val::Float(TWO_63_F)),
        (MathFn::Floor, TWO_53_F + 2.0, Val::Float(TWO_53_F + 2.0)),
        (
            MathFn::Floor,
            f64::from_bits(0x4320_0000_0000_0001),
            Val::Int(1 << 51),
        ),
        (
            MathFn::Ceil,
            f64::from_bits(0x4320_0000_0000_0001),
            Val::Int((1 << 51) + 1),
        ),
        (MathFn::Ceil, -TWO_53_F, Val::Int(-TWO_53)),
        (MathFn::Floor, TWO_53_F, Val::Int(TWO_53)),
        (MathFn::Floor, -2.5, Val::Int(-3)),
        (MathFn::Floor, f64::INFINITY, Val::Float(f64::INFINITY)),
        (
            MathFn::Ceil,
            f64::NEG_INFINITY,
            Val::Float(f64::NEG_INFINITY),
        ),
    ];
    for (f, x, want) in cases {
        assert_val(math(f, Val::Float(x)), want, &format!("{f:?} {x}"));
    }
    for f in [MathFn::Floor, MathFn::Ceil] {
        let r = math(f, Val::Float(f64::NAN)).unwrap();
        assert!(matches!(r, Val::Float(x) if x.is_nan()), "{f:?} NaN: {r:?}");
    }
}

/// Unary minus and `abs` of an integer within ±2^53 give an integer.
#[test]
fn neg_and_abs_of_an_integer_are_integers() {
    for n in [
        TWO_53,
        -TWO_53,
        TWO_53 - 1,
        -TWO_53 + 1,
        i64::from(i32::MIN),
        -21_474_836,
    ] {
        assert_val(neg(Val::Int(n)), Val::Int(-n), &format!("-{n}"));
        assert_val(
            math(MathFn::Abs, Val::Int(n)),
            Val::Int(n.abs()),
            &format!("abs {n}"),
        );
    }
}

/// `i64::MIN`, which no integer within ±2^53 is (the verifier refuses
/// such a constant), still gives a value: its negation, absolute value
/// and quotient by -1 are the double 2^63, no failure.
#[test]
fn i64_min_falls_back_to_the_double() {
    let min = Val::Int(i64::MIN);
    assert_val(neg(min), Val::Float(TWO_63_F), "-MIN");
    assert_val(math(MathFn::Abs, min), Val::Float(TWO_63_F), "abs MIN");
    assert_val(
        arith(Arith::IDiv, min, Val::Int(-1)),
        Val::Float(TWO_63_F),
        "MIN // -1",
    );
}

/// The state clock as the source holds it: an integer within ±2^53, past
/// it the double, negative values included.
#[test]
fn the_clock_is_the_source_s_double() {
    for (us, want) in [
        (-1, Val::Int(-1)),
        (-7_000_000, Val::Int(-7_000_000)),
        (TWO_53, Val::Int(TWO_53)),
        (-TWO_53, Val::Int(-TWO_53)),
        (TWO_53 + 1, Val::Float(TWO_53_F)),
        (TWO_53 + 2, Val::Float(TWO_53_F + 2.0)),
        (-TWO_53 - 2, Val::Float(-TWO_53_F - 2.0)),
        (i64::MAX, Val::Float(TWO_63_F)),
    ] {
        assert_eq!(bits(clock(us)), bits(want), "{us}");
    }
}

/// The boundary grid: every operation over integers within ±2^53 and
/// floats gives no integer outside ±2^53 and no failure but a Lua error.
#[test]
fn no_operation_leaves_2_53_as_an_integer_or_fails_otherwise() {
    let ints: Vec<i64> = [
        0,
        1,
        2,
        7,
        2_147_483_647,
        4_194_305,
        21_474_836,
        2_147_483_646,
        TWO_53 - 1,
        TWO_53,
    ]
    .into_iter()
    .flat_map(|n| [n, -n])
    .chain([i64::from(i32::MIN), -1, -2, -7])
    .collect();
    let floats = [
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.0_f64.powi(60),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ];
    let vals: Vec<Val> = ints
        .iter()
        .copied()
        .map(Val::Int)
        .chain(floats.iter().copied().map(Val::Float))
        .collect();
    let ok = |r: Result<Val, Fail>, what: &str| match r {
        Ok(Val::Int(n)) => assert!(
            n.unsigned_abs() <= TWO_53.unsigned_abs(),
            "{what}: Int({n})"
        ),
        Ok(_) | Err(Fail::Error) => {}
        Err(e) => panic!("{what}: {e:?}"),
    };
    let mut n = 0;
    for &a in &vals {
        ok(neg(a), &format!("-{a:?}"));
        for f in [MathFn::Floor, MathFn::Ceil, MathFn::Abs] {
            ok(math(f, a), &format!("{f:?} {a:?}"));
        }
        for &b in &vals {
            for op in [
                Arith::Add,
                Arith::Sub,
                Arith::Mul,
                Arith::Div,
                Arith::IDiv,
                Arith::Mod,
            ] {
                ok(arith(op, a, b), &format!("{a:?} {op:?} {b:?}"));
                n += 1;
            }
            ok(extreme(true, a, b), "max");
            ok(extreme(false, a, b), "min");
        }
    }
    assert!(n > 4_000, "{n}");
}
