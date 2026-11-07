//! The presentation program's numerics against the Lua 5.4 VM the
//! sandbox runs (design.md "beatoraja Lua Skins", numbers): over a grid of
//! integers and floats (negative operands, ±0.0, infinities, NaN, and
//! values near 2^53 and 2^63), every arithmetic operator, comparison and
//! `math` function gives the VM's value with its subtype, or fails where
//! the VM raises. Integer results that wrap in Lua are outside the accepted
//! domain (the compiler proves accepted code never reaches them), so there
//! the program must report an overflow instead.

use mlua::{Function, Lua, Value};
use skin_model::present::num::{arith, compare, extreme, math, to_int};
use skin_model::present::{Arith, Cmp, Fail, MathFn, Val};

const TWO_53: i64 = 1 << 53;

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
        TWO_53,
        TWO_53 + 1,
        -TWO_53 - 1,
        1 << 62,
        i64::MAX,
        i64::MIN,
    ];
    let floats = [
        0.0,
        -0.0,
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
        9_007_199_254_740_993.0,
        9_223_372_036_854_775_808.0,
        -9_223_372_036_854_775_808.0,
        4_294_967_296.0,
        2_147_483_648.0,
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

/// Whether `a` and `b` are the same Lua value: one subtype, a float by
/// its bits (any NaN equal to any NaN).
fn same(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::Float(x), Val::Float(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// The VM's result of `f(args)`: its value, or `None` for an error.
fn vm(f: &Function, args: &[Val]) -> Option<Val> {
    let args: Vec<Value> = args.iter().copied().map(to_lua).collect();
    f.call::<Value>(mlua::MultiValue::from_iter(args))
        .ok()
        .map(|v| from_lua(&v))
}

/// The program's result against the VM's: equal, an error where the VM
/// raises, or an overflow where the VM's integer result wrapped.
fn check(what: &str, ours: Result<Val, Fail>, theirs: Option<Val>, wraps: bool) {
    match (ours, theirs) {
        (Ok(a), Some(b)) => assert!(same(a, b), "{what}: {a:?} vs the VM's {b:?}"),
        (Err(Fail::Error), None) => {}
        (Err(Fail::Overflow), Some(_)) if wraps => {}
        (o, t) => panic!("{what}: {o:?} vs the VM's {t:?}"),
    }
}

#[test]
fn arithmetic_matches_the_vm() {
    let lua = Lua::new();
    let ops = [
        (Arith::Add, "+"),
        (Arith::Sub, "-"),
        (Arith::Mul, "*"),
        (Arith::Div, "/"),
        (Arith::IDiv, "//"),
        (Arith::Mod, "%"),
    ];
    let g = grid();
    for (op, sym) in ops {
        let f: Function = lua
            .load(format!("return function(a, b) return a {sym} b end"))
            .eval()
            .unwrap();
        for &a in &g {
            for &b in &g {
                let wraps = matches!((a, b), (Val::Int(_), Val::Int(_)));
                check(
                    &format!("{a:?} {sym} {b:?}"),
                    arith(op, a, b),
                    vm(&f, &[a, b]),
                    wraps,
                );
            }
        }
        // Non-numbers raise.
        for bad in [Val::Nil, Val::Bool(true)] {
            check(
                &format!("nil {sym}"),
                arith(op, bad, Val::Int(1)),
                vm(&f, &[bad, Val::Int(1)]),
                false,
            );
        }
    }
}

#[test]
fn comparisons_match_the_vm() {
    let lua = Lua::new();
    let g = grid();
    for (op, sym) in [
        (Cmp::Eq, "=="),
        (Cmp::Ne, "~="),
        (Cmp::Lt, "<"),
        (Cmp::Le, "<="),
    ] {
        let f: Function = lua
            .load(format!("return function(a, b) return a {sym} b end"))
            .eval()
            .unwrap();
        for &a in g.iter().chain(&[Val::Nil, Val::Bool(false)]) {
            for &b in g.iter().chain(&[Val::Nil, Val::Bool(false)]) {
                let ours = compare(op, a, b).map(Val::Bool);
                check(&format!("{a:?} {sym} {b:?}"), ours, vm(&f, &[a, b]), false);
            }
        }
    }
}

#[test]
fn math_matches_the_vm() {
    let lua = Lua::new();
    let g = grid();
    for (f, name) in [
        (MathFn::Floor, "floor"),
        (MathFn::Ceil, "ceil"),
        (MathFn::Abs, "abs"),
    ] {
        let lf: Function = lua.load(format!("return math.{name}")).eval().unwrap();
        for &a in &g {
            let wraps = f == MathFn::Abs && a == Val::Int(i64::MIN);
            check(&format!("{name}({a:?})"), math(f, a), vm(&lf, &[a]), wraps);
        }
        check(
            &format!("{name}(nil)"),
            math(f, Val::Nil),
            vm(&lf, &[Val::Nil]),
            false,
        );
    }
    for (max, name) in [(true, "max"), (false, "min")] {
        let lf: Function = lua.load(format!("return math.{name}")).eval().unwrap();
        for &a in &g {
            for &b in &g {
                check(
                    &format!("{name}({a:?}, {b:?})"),
                    extreme(max, a, b),
                    vm(&lf, &[a, b]),
                    false,
                );
            }
        }
    }
    let neg: Function = lua.load("return function(a) return -a end").eval().unwrap();
    for &a in &g {
        let wraps = a == Val::Int(i64::MIN);
        check(
            &format!("-{a:?}"),
            skin_model::present::num::neg(a),
            vm(&neg, &[a]),
            wraps,
        );
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
