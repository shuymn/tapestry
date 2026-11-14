//! The presentation program's numerics against the source's interpreter
//! (design.md "beatoraja Lua Skins", numbers).
//!
//! - Every operation the source's interpreter has (`+ - * / %`, the
//!   comparisons, unary minus, `math.floor`, `ceil`, `abs`, `max`, `min`)
//!   and the consumers' `toint()` of a number's value and `tofloat()` of a
//!   slider's (numbers as the value the source builds from them, a numeric
//!   −0 its integer 0; strings read at import as numerals, none 0)
//!   are checked against `luaj_numerics.txt`, the source's results over a
//!   grid of integers and floats (zeros of both signs, infinities, NaN,
//!   values near 2^31 and 2^53), generated from the source's pinned
//!   interpreter by `xtask/luaj/luaj_table.sh`, except the rows listed in
//!   [`INTENDED`], where tapestry's rule differs on purpose.
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
use skin_beatoraja::numeral;
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

/// The rows where tapestry's rule differs from the source's on purpose,
/// each with tapestry's value (design.md "beatoraja Lua Skins",
/// "Differences from the source": number values past `i32`, numeric
/// strings). The table
/// stays as the source's interpreter wrote it; each listed row must exist
/// and differ from it, so a stale entry fails.
const INTENDED: &[(&str, &str)] = &[
    // A number's value truncated and saturated to `i32`, where the source
    // keeps the low 32 bits of its saturated `long`.
    ("toint i:9007199254740991", "i:2147483647"),
    ("toint i:-9007199254740991", "i:-2147483648"),
    ("toint f:7ff0000000000000", "i:2147483647"),
    ("toint f:fff0000000000000", "i:-2147483648"),
    ("toint f:4340000000000000", "i:2147483647"),
    ("toint f:c340000000000000", "i:-2147483648"),
    ("toint f:41f0000000000000", "i:2147483647"),
    ("toint f:41e0000000000000", "i:2147483647"),
    ("toint f:7e37e43c8800759c", "i:2147483647"),
    ("toint f:43647ae13f851eb8", "i:2147483647"),
    ("toint f:43cffffff3400000", "i:2147483647"),
    ("toint f:435b4e81aa3d70a4", "i:2147483647"),
    // A string read as a plain numeral: Lua's whitespace trimmed, the sign
    // before `0x`, hexadecimal digits alone, no wrap past 64 bits, the
    // whole fraction, a −0 kept; where the source trims only spaces, reads
    // the sign after `0x` and `[` as a hexadecimal digit, wraps its `long`
    // and keeps the first 64 bytes.
    ("tofloats x0931", "g:3f800000"),
    ("tofloats x30782d31", "g:0"),
    ("tofloats x2d307831", "g:bf800000"),
    ("tofloats x30785b", "g:0"),
    ("tofloats x2d30", "g:80000000"),
    (
        "tofloats x3138343436373434303733373039353531363136",
        "g:5f800000",
    ),
    (
        "tofloats x312e303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303078",
        "g:0",
    ),
];

/// A table result: a value (`i:`, `f:`, a boolean, nil) or an error, or a
/// slider's `f32` (`g:<bits>`, or `caught`, the consumer's `0f`).
#[derive(Clone, Copy, Debug)]
enum Res {
    Val(Result<Val, Fail>),
    Float(f32),
}

impl Res {
    fn read(s: &str) -> Self {
        if s == "caught" {
            return Self::Float(0.0);
        }
        if let Some(g) = s.strip_prefix("g:") {
            return Self::Float(f32::from_bits(u32::from_str_radix(g, 16).unwrap()));
        }
        Self::Val(parse(s).ok_or(Fail::Error))
    }

    /// Whether `self` is `other`: [`same`] for values, an error for an
    /// error, a float's bits (any NaN equal to any NaN).
    fn agrees(self, other: Self) -> bool {
        match (self, other) {
            (Self::Val(Ok(a)), Self::Val(Ok(b))) => same(a, b),
            (Self::Val(Err(Fail::Error)), Self::Val(Err(Fail::Error))) => true,
            (Self::Float(a), Self::Float(b)) => {
                (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
            }
            _ => false,
        }
    }
}

/// Checks every row of `table` against tapestry's operations, and each
/// row `intended` lists against its listed value instead, which must
/// differ from the table's; every listed row must be in the table. Gives
/// the rows checked.
fn check_table(table: &str, intended: &[(&str, &str)]) -> usize {
    let mut seen = vec![false; intended.len()];
    let mut checked = 0;
    for line in table.lines() {
        let (lhs, rhs) = line.split_once(" = ").unwrap();
        let ours = result(lhs);
        if let Res::Val(r) = ours {
            assert!(!wide_int(r), "{lhs}: {r:?} is an integer past ±2^53");
        }
        let theirs = Res::read(rhs);
        match intended.iter().position(|(row, _)| *row == lhs) {
            Some(i) => {
                assert!(!seen[i], "{lhs}: twice in the table");
                seen[i] = true;
                let want = Res::read(intended[i].1);
                assert!(ours.agrees(want), "{lhs}: {ours:?} vs the listed {want:?}");
                assert!(
                    !ours.agrees(theirs),
                    "{lhs}: listed as intended but agrees with the table's {theirs:?}"
                );
            }
            None => assert!(
                ours.agrees(theirs),
                "{lhs}: {ours:?} vs the source's {theirs:?}"
            ),
        }
        checked += 1;
    }
    for ((row, _), seen) in intended.iter().zip(seen) {
        assert!(seen, "{row}: listed as intended but not in the table");
    }
    checked
}

/// Tapestry's result for the table row `lhs`.
fn result(lhs: &str) -> Res {
    let words: Vec<&str> = lhs.split(' ').collect();
    if words[0].starts_with("tofloat") {
        return Res::Float(tofloat(&words));
    }
    Res::Val(match words[..] {
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
        _ => panic!("{lhs}"),
    })
}

#[test]
fn every_operation_agrees_with_the_table() {
    let checked = check_table(include_str!("luaj_numerics.txt"), INTENDED);
    assert!(checked > 9_800, "{checked}");
}

/// The checker fails on a listed row that agrees with the table (stale),
/// on one the table lacks, on a wrong listed value, on a row listed with
/// no listed difference left, and on an unlisted row that differs.
#[test]
fn the_intended_list_is_checked() {
    let table = "toint f:41f0000000000000 = i:0\ntoint i:7 = i:7\n";
    let good = [("toint f:41f0000000000000", "i:2147483647")];
    assert_eq!(check_table(table, &good), 2);
    let bad: [&[(&str, &str)]; 5] = [
        // Stale: tapestry agrees with the table there.
        &[good[0], ("toint i:7", "i:7")],
        // Not a row of the table.
        &[good[0], ("toint i:8", "i:2147483647")],
        // The table's own value listed.
        &[("toint f:41f0000000000000", "i:0")],
        // A wrong listed value.
        &[("toint f:41f0000000000000", "i:2147483646")],
        // A difference not listed.
        &[],
    ];
    for list in bad {
        let run = std::panic::catch_unwind(|| check_table(table, list));
        assert!(run.is_err(), "{list:?} passed");
    }
}

/// A slider value's `tofloat()` row: `tofloat <operand> as <value the
/// source built>`, converted from that value (a numeric −0 is the
/// source's integer 0, as the importer normalizes it), or `tofloats
/// x<hex bytes>`, a string folded at import and then converted.
fn tofloat(words: &[&str]) -> f32 {
    match words {
        ["tofloat", _, "as", built] => to_float(parse(built).unwrap()),
        ["tofloats", hex] => {
            let hex = hex.strip_prefix('x').unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            to_float(Val::Float(numeral::number(&bytes).unwrap_or(0.0)))
        }
        _ => panic!("{words:?}"),
    }
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

/// A number's value truncated toward zero and saturated to `i32` (NaN,
/// nil and booleans 0), pinned by value.
#[test]
fn a_number_value_truncates_and_saturates() {
    for (v, want) in [
        (Val::Float(4_294_967_296.0), i32::MAX),
        (Val::Float(f64::NAN), 0),
        (Val::Float(-0.5), 0),
        (Val::Float(2_147_483_648.0), i32::MAX),
        (Val::Float(2_147_483_647.9), i32::MAX),
        (Val::Float(-2_147_483_648.9), i32::MIN),
        (Val::Float(1e300), i32::MAX),
        (Val::Float(-1e300), i32::MIN),
        (Val::Float(99.9), 99),
        (Val::Float(-99.9), -99),
        (Val::Int((1 << 32) + 5), i32::MAX),
        (Val::Int(-(1 << 32) - 5), i32::MIN),
        (Val::Int(i64::MAX), i32::MAX),
        (Val::Int(i64::MIN), i32::MIN),
        (Val::Nil, 0),
        (Val::Bool(true), 0),
    ] {
        assert_eq!(to_int(v), want, "{v:?}");
    }
}
