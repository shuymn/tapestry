//! The presentation program's numerics against the source's interpreter
//! (design.md "beatoraja Lua Skins", numbers).
//!
//! - Every operation the source's interpreter has (`+ - * / %`, the
//!   comparisons, unary minus, `math.floor`, `ceil`, `abs`, `max`, `min`)
//!   and the consumer's `toint()` of a number's value
//!   is checked against `luaj_numerics.txt`, the source's results over a
//!   grid of integers and floats (zeros of both signs, infinities, NaN,
//!   values near 2^31 and 2^53), generated from the source's pinned
//!   interpreter by `xtask/luaj/luaj_table.sh`.
//! - Where the Lua 5.4 VM the sandbox runs agrees with it (`+ - *`, `//`,
//!   which the source does not have, and the `math` functions away from a
//!   zero's sign), the grid is also checked against that VM.
//!
//! An integer result past ±2^53 is outside the accepted domain (the
//! compiler proves accepted code never reaches it): there the program
//! reports an overflow where `i64` would wrap, and is not compared.

use mlua::{Function, Lua, Value};
use skin_model::present::num::{arith, compare, extreme, math, neg, to_int};
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

/// Whether integer operation `op` of `a` and `b` leaves ±2^53, outside the
/// accepted domain.
fn outside(op: &str, a: Val, b: Val) -> bool {
    let (Val::Int(x), Val::Int(y)) = (a, b) else {
        return false;
    };
    let (x, y) = (i128::from(x), i128::from(y));
    let r = match op {
        "add" => x + y,
        "sub" => x - y,
        "mul" => x * y,
        _ => return false,
    };
    r.abs() > i128::from(TWO_53)
}

#[test]
fn every_operation_matches_the_source_interpreter() {
    let table = include_str!("luaj_numerics.txt");
    let mut checked = 0;
    for line in table.lines() {
        let (lhs, rhs) = line.split_once(" = ").unwrap();
        let theirs = parse(rhs);
        let words: Vec<&str> = lhs.split(' ').collect();
        let ours: Result<Val, Fail> = match words[..] {
            [op, a, b] => {
                let (a, b) = (parse(a).unwrap(), parse(b).unwrap());
                if outside(op, a, b) {
                    continue;
                }
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
        match (ours, theirs) {
            (Ok(o), Some(t)) => assert!(same(o, t), "{lhs}: {o:?} vs the source's {t:?}"),
            (Err(Fail::Error), None) => {}
            (o, t) => panic!("{lhs}: {o:?} vs the source's {t:?}"),
        }
        checked += 1;
    }
    assert!(checked > 9_000, "{checked}");
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
        TWO_53,
        TWO_53 + 1,
        -TWO_53 - 1,
        1 << 62,
        i64::MAX,
        i64::MIN,
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

/// The program's result against the VM's: equal, an error where the VM
/// raises, or an overflow exactly where `wraps`.
fn check(what: &str, ours: Result<Val, Fail>, theirs: Option<Val>, wraps: bool) {
    match (ours, theirs) {
        (Ok(a), Some(b)) if !wraps => assert!(same_54(a, b), "{what}: {a:?} vs the VM's {b:?}"),
        (Err(Fail::Error), None) => {}
        (Err(Fail::Overflow), Some(_)) if wraps => {}
        (o, t) => panic!("{what} (wraps {wraps}): {o:?} vs the VM's {t:?}"),
    }
}

/// Where Lua 5.4 and the source agree: `+ - *` and `//`, each integer
/// operation's overflow computed exactly.
#[test]
fn arithmetic_matches_the_lua_vm_where_it_agrees() {
    let lua = Lua::new();
    let ops = [
        (
            Arith::Add,
            "+",
            i64::checked_add as fn(i64, i64) -> Option<i64>,
        ),
        (Arith::Sub, "-", i64::checked_sub),
        (Arith::Mul, "*", i64::checked_mul),
        (Arith::IDiv, "//", |a: i64, b: i64| {
            if b == 0 { Some(0) } else { a.checked_div(b) }
        }),
    ];
    let g = grid();
    for (op, sym, checked) in ops {
        let f: Function = lua
            .load(format!("return function(a, b) return a {sym} b end"))
            .eval()
            .unwrap();
        for &a in &g {
            for &b in &g {
                let wraps = match (a, b) {
                    (Val::Int(x), Val::Int(y)) => checked(x, y).is_none(),
                    _ => false,
                };
                check(
                    &format!("{a:?} {sym} {b:?}"),
                    arith(op, a, b),
                    vm(&f, &[a, b]),
                    wraps,
                );
            }
        }
        for bad in [Val::Nil, Val::Bool(true)] {
            check(
                &format!("{bad:?} {sym}"),
                arith(op, bad, Val::Int(1)),
                vm(&f, &[bad, Val::Int(1)]),
                false,
            );
        }
    }
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
    let lneg: Function = lua.load("return function(a) return -a end").eval().unwrap();
    for &a in &g {
        let wraps = a == Val::Int(i64::MIN);
        check(&format!("-{a:?}"), neg(a), vm(&lneg, &[a]), wraps);
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
