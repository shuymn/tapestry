//! The program's numerics: the source's interpreter's (LuaJ 3.0.2, Lua 5.2
//! numbers, which are doubles), shared by the executor and the importer's
//! folding, so folding equals evaluation and both equal what the source
//! displays (design.md "beatoraja Lua Skins", numbers). Every value is the
//! double the source holds; an integer is kept as one only within ±2^53,
//! where that double is exact, and every function here is total.
//!
//! - `+ - *` of two integers are exact while the result is within ±2^53;
//!   past it (or past `i64`) the result is the double of the exact result,
//!   rounded once to nearest, as the source's integers overflow into
//!   doubles. With a float operand both go through `f64`.
//! - `/` is the source's `ddiv`: a float division, but a zero divisor
//!   gives +inf for a positive dividend, NaN for zero and -inf otherwise
//!   (NaN included).
//! - `%` is the source's `dmod` in doubles: NaN for a zero divisor or an
//!   infinite dividend, the dividend (or +inf for a negative one) over
//!   +inf, the dividend (or -inf for a positive one) over -inf, else
//!   `a - b * floor(a / b)`; an integer result that is exact stays one.
//! - `//` is not in the source's grammar; it keeps Lua 5.4's floor
//!   division, an integer divisor of 0 a Lua error; `i64::MIN // -1`,
//!   whose quotient no integer holds, gives the float quotient.
//! - Numbers compare as the doubles they are; ordering anything that is
//!   not a number is a Lua error; `==` across types is false.
//! - The source has no negative zero: a zero result is +0.
//! - `floor` and `ceil` keep an integer and give an integer for a float
//!   result within ±2^53, else the float (infinities and NaN included);
//!   unary minus and `abs` of an integer give an integer (of `i64::MIN`,
//!   which no integer negates, the float 2^63); `max` and `min` keep the
//!   first of equal arguments and NaN only where it comes first, as the
//!   source's do.
//! - A result is converted as its consumer converts it
//!   ([`to_bool`], [`to_float`], `SkinLuaAccessor.java:612-673` at
//!   `0.8.9`), but a number's value is truncated and saturated to `i32`
//!   ([`to_int`]) where the source keeps the low 32 bits of its `long`.
//!
//! The differential table `crates/skin-beatoraja/tests/luaj_numerics.txt`,
//! generated from the source's interpreter by `xtask/luaj/Probe.java`,
//! pins each operation.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Arith, Cmp, Fail, MathFn, Val};

/// 2^63 as a float: `-i64::MIN`, the first float past `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;
/// 2^53: the integers within it are exact doubles.
const TWO_53: f64 = 9_007_199_254_740_992.0;
/// 2^53, the largest magnitude of a [`Val::Int`].
const EXACT: u64 = 1 << 53;

/// An integer as a float, rounded to nearest.
#[expect(
    clippy::cast_precision_loss,
    reason = "every Int is within ±2^53, so exact; a wider one rounds once, \
              as the source's `l2d` of its `long` does"
)]
fn float(n: i64) -> f64 {
    n as f64
}

/// Float `f` as an integer when it is one within ±2^53.
#[expect(
    clippy::cast_possible_truncation,
    reason = "checked integral and within ±2^53 before the cast"
)]
fn int_within(f: f64) -> Option<i64> {
    (f.floor() == f && f.abs() <= TWO_53).then_some(f as i64)
}

/// The result of `+ - *` of integers `x` and `y`, `r` its exact value
/// (`None` past `i64`): an integer within ±2^53, else the double `op`
/// gives of their doubles. Both are exact, so that is the exact result
/// rounded once, as the source's `l2d` of its `long` result does.
fn promote(r: Option<i64>, x: i64, y: i64, op: fn(f64, f64) -> f64) -> Val {
    match r {
        Some(n) if n.unsigned_abs() <= EXACT => Val::Int(n),
        _ => out(op(float(x), float(y))),
    }
}

/// A float result as the source holds it: no negative zero.
fn out(f: f64) -> Val {
    Val::Float(if f == 0.0 { 0.0 } else { f })
}

/// A number's double; `None` for a non-number.
fn num(v: Val) -> Option<f64> {
    match v {
        Val::Int(n) => Some(float(n)),
        Val::Float(f) => Some(if f == 0.0 { 0.0 } else { f }),
        Val::Nil | Val::Bool(_) => None,
    }
}

/// Floor division of integers (Lua 5.4's `luaV_idiv`), a Lua error for
/// `b` 0; `i64::MIN // -1`, whose quotient no integer holds, the float
/// quotient.
fn idiv(a: i64, b: i64) -> Result<Val, Fail> {
    if b == 0 {
        return Err(Fail::Error);
    }
    let Some(q) = a.checked_div(b) else {
        return Ok(out((float(a) / float(b)).floor()));
    };
    Ok(Val::Int(if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }))
}

/// The source's float division (`LuaDouble.ddiv`).
fn ddiv(a: f64, b: f64) -> f64 {
    if b != 0.0 {
        a / b
    } else if a > 0.0 {
        f64::INFINITY
    } else if a == 0.0 {
        f64::NAN
    } else {
        f64::NEG_INFINITY
    }
}

/// The source's modulo (`LuaDouble.dmod`).
fn dmod(a: f64, b: f64) -> f64 {
    if b == 0.0 || a.is_infinite() {
        f64::NAN
    } else if b == f64::INFINITY {
        if a < 0.0 { f64::INFINITY } else { a }
    } else if b == f64::NEG_INFINITY {
        if a > 0.0 { f64::NEG_INFINITY } else { a }
    } else {
        a - b * (a / b).floor()
    }
}

/// Arithmetic operator `op` of `a` and `b`.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number operand or an integer `//` by zero.
pub fn arith(op: Arith, a: Val, b: Val) -> Result<Val, Fail> {
    if let (Val::Int(x), Val::Int(y)) = (a, b) {
        return Ok(match op {
            Arith::Add => promote(x.checked_add(y), x, y, |p, q| p + q),
            Arith::Sub => promote(x.checked_sub(y), x, y, |p, q| p - q),
            Arith::Mul => promote(x.checked_mul(y), x, y, |p, q| p * q),
            Arith::Div => out(ddiv(float(x), float(y))),
            Arith::IDiv => idiv(x, y)?,
            Arith::Mod => {
                let m = dmod(float(x), float(y));
                int_within(m).map_or(out(m), Val::Int)
            }
        });
    }
    let (Some(x), Some(y)) = (num(a), num(b)) else {
        return Err(Fail::Error);
    };
    Ok(out(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
        Arith::Div => ddiv(x, y),
        Arith::IDiv => (x / y).floor(),
        Arith::Mod => dmod(x, y),
    }))
}

/// Unary minus of `a`.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number.
pub fn neg(a: Val) -> Result<Val, Fail> {
    match a {
        Val::Int(n) => Ok(n.checked_neg().map_or(Val::Float(TWO_63), Val::Int)),
        Val::Float(f) => Ok(out(-f)),
        Val::Nil | Val::Bool(_) => Err(Fail::Error),
    }
}

/// The state clock `us` as the source's `main_state.time()` gives it,
/// `LuaValue.valueOf(double)` of the `long`: an integer within ±2^53,
/// else its double, negative values included.
#[must_use]
pub fn clock(us: i64) -> Val {
    if us.unsigned_abs() <= EXACT {
        Val::Int(us)
    } else {
        Val::Float(float(us))
    }
}

/// Lua's truthiness: only nil and false are false.
#[must_use]
pub fn truthy(a: Val) -> bool {
    !matches!(a, Val::Nil | Val::Bool(false))
}

/// Whether `a` equals `b`, raw: numbers by their doubles.
#[must_use]
pub fn equal(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::Nil, Val::Nil) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Int(x), Val::Int(y)) => x == y,
        #[expect(clippy::float_cmp, reason = "Lua's float equality is exact")]
        (Val::Float(x), Val::Float(y)) => x == y,
        (Val::Int(_), Val::Float(_)) | (Val::Float(_), Val::Int(_)) => num(a) == num(b),
        _ => false,
    }
}

/// Comparison `op` of `a` and `b`.
///
/// # Errors
///
/// [`Fail::Error`] for an ordering of a non-number.
pub fn compare(op: Cmp, a: Val, b: Val) -> Result<bool, Fail> {
    let le = match op {
        Cmp::Eq => return Ok(equal(a, b)),
        Cmp::Ne => return Ok(!equal(a, b)),
        Cmp::Lt => false,
        Cmp::Le => true,
    };
    Ok(match (a, b) {
        (Val::Int(x), Val::Int(y)) => {
            if le {
                x <= y
            } else {
                x < y
            }
        }
        (Val::Float(x), Val::Float(y)) => {
            if le {
                x <= y
            } else {
                x < y
            }
        }
        (Val::Int(_), Val::Float(_)) | (Val::Float(_), Val::Int(_)) => {
            let (Some(x), Some(y)) = (num(a), num(b)) else {
                return Err(Fail::Error);
            };
            if le { x <= y } else { x < y }
        }
        _ => return Err(Fail::Error),
    })
}

/// A float result of `floor` or `ceil`: an integer when it is one within
/// ±2^53, else the float.
fn int_or_float(f: f64) -> Val {
    int_within(f).map_or(out(f), Val::Int)
}

/// `math.floor`, `math.ceil` or `math.abs` of `a`.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number.
pub fn math(f: MathFn, a: Val) -> Result<Val, Fail> {
    match (f, a) {
        (MathFn::Floor | MathFn::Ceil, Val::Int(n)) => Ok(Val::Int(n)),
        (MathFn::Floor, Val::Float(x)) => Ok(int_or_float(x.floor())),
        (MathFn::Ceil, Val::Float(x)) => Ok(int_or_float(x.ceil())),
        (MathFn::Abs, Val::Int(n)) => Ok(n.checked_abs().map_or(Val::Float(TWO_63), Val::Int)),
        (MathFn::Abs, Val::Float(x)) => Ok(out(x.abs())),
        (_, Val::Nil | Val::Bool(_)) => Err(Fail::Error),
    }
}

/// `math.max(a, b)` (`max`) or `math.min(a, b)`: `b` only when it is
/// strictly larger (smaller), so the first of equal ones stays.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number argument.
pub fn extreme(max: bool, a: Val, b: Val) -> Result<Val, Fail> {
    // `compare` refuses a non-number either way.
    let better = if max {
        compare(Cmp::Lt, a, b)?
    } else {
        compare(Cmp::Lt, b, a)?
    };
    Ok(match if better { b } else { a } {
        Val::Float(f) => out(f),
        v => v,
    })
}

/// Lua's `toboolean()`, as a `draw` reads its result.
#[must_use]
pub fn to_bool(a: Val) -> bool {
    truthy(a)
}

/// A number's `value` as its consumer reads it: the number truncated
/// toward zero and saturated to `i32` (NaN 0); nil and booleans 0. Both
/// ends of the range are a number's no-value markers, so a value past
/// either draws nothing.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "a float to `i32` by `as` truncates toward zero and saturates, NaN 0"
)]
pub fn to_int(a: Val) -> i32 {
    match a {
        Val::Int(n) => {
            i32::try_from(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
        }
        Val::Float(f) => f as i32,
        Val::Nil | Val::Bool(_) => 0,
    }
}

/// The source's `tofloat()` (LuaJ 3.0.2) as a slider's `value` reads it:
/// an integer by `i2f`, a float by `d2f` (round to nearest, ties to even;
/// past `f32` an infinity; NaN kept), nil and booleans 0. Every integer
/// is within ±2^53, where `(n as f64) as f32` equals both `i2f` and `d2f`
/// of its exact double. No sign is normalized: the program's values carry
/// no −0 but the ones luaj's `d2f` gives too.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "Java's i2f and d2f round to nearest, as these casts do"
)]
pub fn to_float(a: Val) -> f32 {
    match a {
        Val::Int(n) => (n as f64) as f32,
        Val::Float(f) => f as f32,
        Val::Nil | Val::Bool(_) => 0.0,
    }
}

/// The source's `tolong()` (LuaJ): an integer as it is, a float
/// saturated to 64 bits with NaN 0 (Java's `(long)`), nil and booleans 0.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "Java's `(long)` of a double: saturating, NaN 0, as Rust's `as`"
)]
pub fn to_long(a: Val) -> i64 {
    match a {
        Val::Int(n) => n,
        Val::Float(f) => f as i64,
        Val::Nil | Val::Bool(_) => 0,
    }
}
