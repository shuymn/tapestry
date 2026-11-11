//! The presentation program's numerics against the source's interpreter
//! (design.md "beatoraja Lua Skins", numbers).
//!
//! - Every operation the source's interpreter has (`+ - * / %`, the
//!   comparisons, unary minus, `math.floor`, `ceil`, `abs`, `max`, `min`)
//!   and the consumers' `toint()` of a number's value and `tofloat()` of a
//!   slider's (numbers as the value the source builds from them, a numeric
//!   −0 its integer 0; strings folded at import as luaj's `scannumber`)
//!   are checked against `luaj_numerics.txt`, the source's results over a
//!   grid of integers and floats (zeros of both signs, infinities, NaN,
//!   values near 2^31 and 2^53), generated from the source's pinned
//!   interpreter by `xtask/luaj/luaj_table.sh`.
//! - Where the Lua 5.4 VM the sandbox runs agrees with it (`+ - *` of
//!   integers within ±2^53 whose result stays within it, or with a float,
//!   `//`, which the source does not have, and the `math` functions by
//!   value, away from a zero's sign), the grid is also checked against
//!   that VM.
//!
//! Every value is the double the source holds and an integer is kept as
//! one only within ±2^53: an integer result past it is the double of the
//! exact result, rounded once, which the table pins; no result is an
//! integer outside ±2^53, even where its double matches.

use mlua::{Function, Lua, Value};
use skin_beatoraja::lua::num_scan::todouble;
use skin_model::present::num::{arith, compare, extreme, math, neg, to_float, to_int};
use skin_model::present::{Arith, Cmp, Fail, MathFn, Val};

const TWO_53: i64 = 1 << 53;

/// A value of the table: `i:<integer>` or `f:<IEEE bits in hex>`, or a
/// boolean or nil.
fn parse(s: &str) -> Option<Val> {
    if let Some(n) = s.strip_prefix("i:") {
        return Some(Val::Int(n.parse().unwrap()));
    }
    if let Some(b) = s.strip_prefix("f:") {
        return Some(Val::Float(f64::from_bits(
            u64::from_str_radix(b, 16).unwrap(),
        )));
    }
    match s {
        "true" => Some(Val::Bool(true)),
        "false" => Some(Val::Bool(false)),
        "nil" => Some(Val::Nil),
        "error" => None,
        other => panic!("{other}"),
    }
}

/// The double a number is, `None` for nil and booleans.
fn double(v: Val) -> Option<f64> {
    match v {
        Val::Int(n) => Some(n as f64),
        Val::Float(f) => Some(f),
        _ => None,
    }
}

/// Whether `ours` is the source's `theirs`: the same boolean, or the same
/// double (any NaN equal to any NaN, a zero's sign compared: the source
/// has only +0).
fn same(ours: Val, theirs: Val) -> bool {
    match (double(ours), double(theirs)) {
        (Some(x), Some(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        (None, None) => ours == theirs,
        _ => false,
    }
}

/// Whether `r` is an integer outside ±2^53, which no result may be.
fn wide_int(r: Result<Val, Fail>) -> bool {
    matches!(r, Ok(Val::Int(n)) if n.unsigned_abs() > TWO_53.unsigned_abs())
}

#[test]
fn every_operation_matches_the_source_interpreter() {
    let table = include_str!("luaj_numerics.txt");
    let mut checked = 0;
    for line in table.lines() {
        let (lhs, rhs) = line.split_once(" = ").unwrap();
        let words: Vec<&str> = lhs.split(' ').collect();
        if words[0].starts_with("tofloat") {
            check_tofloat(&words, rhs);
            checked += 1;
            continue;
        }
        let theirs = parse(rhs);
        let ours: Result<Val, Fail> = match words[..] {
            [op, a, b] => {
                let (a, b) = (parse(a).unwrap(), parse(b).unwrap());
                let arith_op = |o| arith(o, a, b);
                match op {
                    "add" => arith_op(Arith::Add),
                    "sub" => arith_op(Arith::Sub),
                    "mul" => arith_op(Arith::Mul),
                    "div" => arith_op(Arith::Div),
                    "mod" => arith_op(Arith::Mod),
                    "lt" => compare(Cmp::Lt, a, b).map(Val::Bool),
                    "le" => compare(Cmp::Le, a, b).map(Val::Bool),
                    "eq" => compare(Cmp::Eq, a, b).map(Val::Bool),
                    "max" => extreme(true, a, b),
                    "min" => extreme(false, a, b),
                    other => panic!("{other}"),
                }
            }
            [op, a] => {
                let a = parse(a).unwrap();
                match op {
                    "neg" => neg(a),
                    "floor" => math(MathFn::Floor, a),
                    "ceil" => math(MathFn::Ceil, a),
                    "abs" => math(MathFn::Abs, a),
                    "toint" => Ok(Val::Int(i64::from(to_int(a)))),
                    other => panic!("{other}"),
                }
            }
            _ => panic!("{line}"),
        };
        assert!(!wide_int(ours), "{lhs}: {ours:?} is an integer past ±2^53");
        match (ours, theirs) {
            (Ok(o), Some(t)) => assert!(same(o, t), "{lhs}: {o:?} vs the source's {t:?}"),
            (Err(Fail::Error), None) => {}
            (o, t) => panic!("{lhs}: {o:?} vs the source's {t:?}"),
        }
        checked += 1;
    }
    assert!(checked > 9_800, "{checked}");
}

/// A slider value's `tofloat()` line: `tofloat <operand> as <value the
/// source built> = g:<f32 bits>`, converted from that value (a numeric
/// −0 is the source's integer 0, as the importer normalizes it), or
/// `tofloats x<hex bytes> = g:<bits>` (or `caught`, the consumer's `0f`),
/// a string folded at import and then converted.
fn check_tofloat(words: &[&str], rhs: &str) {
    let ours = match words {
        ["tofloat", _, "as", built] => to_float(parse(built).unwrap()),
        ["tofloats", hex] => {
            let hex = hex.strip_prefix('x').unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            to_float(Val::Float(todouble(&bytes)))
        }
        _ => panic!("{words:?}"),
    };
    let theirs = match rhs {
        "caught" => 0.0_f32,
        g => f32::from_bits(u32::from_str_radix(g.strip_prefix("g:").unwrap(), 16).unwrap()),
    };
    assert!(
        (ours.is_nan() && theirs.is_nan()) || ours.to_bits() == theirs.to_bits(),
        "{words:?}: {ours:?} vs the source's {theirs:?}"
    );
}

#[test]
fn a_numeric_negative_zero_is_built_as_integer_zero() {
    let table = include_str!("luaj_numerics.txt");
    assert!(table.contains(
        "tofloat f:8000000000000000 as i:0 = g:0
"
    ));
    // A string's keeps its sign.
    assert!(table.contains(
        "tofloats x2d302e30 = g:80000000
"
    ));
}

fn grid() -> Vec<Val> {
    let ints = [
        0,
        1,
        -1,
        2,
        -2,
        3,
        -3,
        7,
        -7,
        2_147_483_647,
        -2_147_483_648,
        TWO_53 - 1,
        -TWO_53 + 1,
        TWO_53,
        -TWO_53,
    ];
    let floats = [
        0.0,
        0.5,
        -0.5,
        1.5,
        -1.5,
        2.0,
        -7.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        9_007_199_254_740_992.0,
        9_223_372_036_854_775_808.0,
        -9_223_372_036_854_775_808.0,
        4_294_967_296.0,
        1e300,
    ];
    ints.into_iter()
        .map(Val::Int)
        .chain(floats.into_iter().map(Val::Float))
        .collect()
}

fn to_lua(v: Val) -> Value {
    match v {
        Val::Nil => Value::Nil,
        Val::Bool(b) => Value::Boolean(b),
        Val::Int(n) => Value::Integer(n),
        Val::Float(f) => Value::Number(f),
    }
}

fn from_lua(v: &Value) -> Val {
    match *v {
        Value::Nil => Val::Nil,
        Value::Boolean(b) => Val::Bool(b),
        Value::Integer(n) => Val::Int(n),
        Value::Number(f) => Val::Float(f),
        ref other => panic!("{other:?}"),
    }
}

/// The same Lua 5.4 value: one subtype, a float by its bits (any NaN equal
/// to any NaN, the two zeros equal: the source has only +0).
fn same_54(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::Float(x), Val::Float(y)) => {
            (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits() || (x == 0.0 && y == 0.0)
        }
        _ => a == b,
    }
}

fn vm(f: &Function, args: &[Val]) -> Option<Val> {
    let args: Vec<Value> = args.iter().copied().map(to_lua).collect();
    f.call::<Value>(mlua::MultiValue::from_iter(args))
        .ok()
        .map(|v| from_lua(&v))
}

/// The program's result against the VM's, compared by `eq`: equal, or an
/// error where the VM raises.
fn check(what: &str, ours: Result<Val, Fail>, theirs: Option<Val>, eq: fn(Val, Val) -> bool) {
    match (ours, theirs) {
        (Ok(a), Some(b)) => assert!(eq(a, b), "{what}: {a:?} vs the VM's {b:?}"),
        (Err(Fail::Error), None) => {}
        (o, t) => panic!("{what}: {o:?} vs the VM's {t:?}"),
    }
}

/// The same number by value (any NaN equal to any NaN, the two zeros
/// equal), or the same boolean or nil: Lua 5.4's subtype aside.
fn same_value(a: Val, b: Val) -> bool {
    match (double(a), double(b)) {
        (Some(x), Some(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (None, None) => a == b,
        _ => false,
    }
}

/// Where Lua 5.4 and the source agree: `+ - *` and `//`, integers only
/// where the exact result stays within ±2^53 (past it Lua 5.4 keeps an
/// integer, or wraps, where the source rounds to a double, which the
/// table pins).
#[test]
fn arithmetic_matches_the_lua_vm_where_it_agrees() {
    let lua = Lua::new();
    let ops = [
        (Arith::Add, "+", (|a, b| a + b) as fn(i128, i128) -> i128),
        (Arith::Sub, "-", |a, b| a - b),
        (Arith::Mul, "*", |a, b| a * b),
        (Arith::IDiv, "//", |a, b| if b == 0 { 0 } else { a / b }),
    ];
    let mut skipped = 0;
    let g = grid();
    for (op, sym, exact) in ops {
        let f: Function = lua
            .load(format!("return function(a, b) return a {sym} b end"))
            .eval()
            .unwrap();
        for &a in &g {
            for &b in &g {
                if let (Val::Int(x), Val::Int(y)) = (a, b)
                    && exact(i128::from(x), i128::from(y)).abs() > i128::from(TWO_53)
                {
                    skipped += 1;
                    continue;
                }
                check(
                    &format!("{a:?} {sym} {b:?}"),
                    arith(op, a, b),
                    vm(&f, &[a, b]),
                    same_54,
                );
            }
        }
        for bad in [Val::Nil, Val::Bool(true)] {
            check(
                &format!("{bad:?} {sym}"),
                arith(op, bad, Val::Int(1)),
                vm(&f, &[bad, Val::Int(1)]),
                same_54,
            );
        }
    }
    // The grid reaches past ±2^53, so the restriction above is not empty.
    assert!(skipped > 0);
}

#[test]
fn math_matches_the_lua_vm_where_it_agrees() {
    let lua = Lua::new();
    let g = grid();
    for (f, name) in [
        (MathFn::Floor, "floor"),
        (MathFn::Ceil, "ceil"),
        (MathFn::Abs, "abs"),
    ] {
        let lf: Function = lua.load(format!("return math.{name}")).eval().unwrap();
        for &a in &g {
            let ours = math(f, a);
            check(&format!("{name}({a:?})"), ours, vm(&lf, &[a]), same_value);
            representation(&format!("{name}({a:?})"), f, a, ours);
        }
        check(
            &format!("{name}(nil)"),
            math(f, Val::Nil),
            vm(&lf, &[Val::Nil]),
            same_value,
        );
    }
    let lneg: Function = lua.load("return function(a) return -a end").eval().unwrap();
    for &a in &g {
        let ours = neg(a);
        check(&format!("-{a:?}"), ours, vm(&lneg, &[a]), same_value);
        representation(&format!("-{a:?}"), MathFn::Abs, a, ours);
    }
}

/// The representation the comparison by value leaves open: no integer
/// past ±2^53, and `floor` or `ceil` (`f`) of a float whose result is
/// past it (an infinity and NaN included) that float, where Lua 5.4 gives
/// an integer for one that fits `i64` (`floor(-2^63)`).
fn representation(what: &str, f: MathFn, a: Val, ours: Result<Val, Fail>) {
    assert!(!wide_int(ours), "{what}: {ours:?} is an integer past ±2^53");
    if f != MathFn::Abs
        && let (Val::Float(x), Ok(r)) = (a, ours)
        && (x.is_nan() || x.floor().abs() > 9_007_199_254_740_992.0)
    {
        assert!(matches!(r, Val::Float(_)), "{what}: {r:?} is not a float");
    }
}

/// The source's `toint()` (Java's `(int) (long) d` of a double, an
/// integer's low 32 bits; nil and booleans 0), pinned by value.
#[test]
fn a_number_value_converts_as_the_source() {
    for (v, want) in [
        (Val::Float(4_294_967_296.0), 0),
        (Val::Float(f64::NAN), 0),
        (Val::Float(-0.5), 0),
        (Val::Float(2_147_483_648.0), i32::MIN),
        (Val::Float(1e300), -1),
        (Val::Float(-1e300), 0),
        (Val::Float(99.9), 99),
        (Val::Int((1 << 32) + 5), 5),
        (Val::Nil, 0),
        (Val::Bool(true), 0),
    ] {
        assert_eq!(to_int(v), want, "{v:?}");
    }
}
