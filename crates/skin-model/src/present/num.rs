//! The program's numerics: the source's interpreter's (LuaJ 3.0.2, Lua 5.2
//! numbers, which are doubles), shared by the executor and the importer's
//! folding, so folding equals evaluation and both equal what the source
//! displays (design.md "beatoraja Lua Skins", numbers). An integer is kept
//! as one while it is exact; every value is the double the source holds.
//!
//! - `+ - *` of two integers are checked ([`Fail::Overflow`] past `i64`;
//!   the compiler proves accepted code stays within ±2^53, where the
//!   source's double is the same integer); with a float operand both go
//!   through `f64`.
//! - `/` is the source's `ddiv`: a float division, but a zero divisor
//!   gives +inf for a positive dividend, NaN for zero and -inf otherwise
//!   (NaN included).
//! - `%` is the source's `dmod` in doubles: NaN for a zero divisor or an
//!   infinite dividend, the dividend (or +inf for a negative one) over
//!   +inf, the dividend (or -inf for a positive one) over -inf, else
//!   `a - b * floor(a / b)`; an integer result that is exact stays one.
//! - `//` is not in the source's grammar; it keeps Lua 5.4's floor
//!   division, an integer divisor of 0 a Lua error.
//! - Numbers compare as the doubles they are; ordering anything that is
//!   not a number is a Lua error; `==` across types is false.
//! - The source has no negative zero: a zero result is +0.
//! - `floor` and `ceil` keep an integer and give an integer for a float
//!   result that fits, else the float; `abs` of `i64::MIN` is
//!   [`Fail::Overflow`]; `max` and `min` keep the first of equal
//!   arguments and NaN only where it comes first, as the source's do.
//! - A result is converted as its consumer converts it
//!   ([`to_bool`], [`to_int`], `SkinLuaAccessor.java:612-651` at `0.8.9`).
//!
//! The differential table `crates/skin-beatoraja/tests/luaj_numerics.txt`,
//! generated from the source's interpreter by `xtask/luaj/Probe.java`,
//! pins each operation.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Arith, Cmp, Fail, MathFn, Val};

/// 2^63 as a float: the first float past `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;
/// 2^53: the integers within it are exact doubles.
const TWO_53: f64 = 9_007_199_254_740_992.0;

/// An integer as a float, rounded to nearest.
#[expect(
    clippy::cast_precision_loss,
    reason = "the source's numbers are doubles; accepted integers are exact"
)]
fn float(n: i64) -> f64 {
    n as f64
}

/// Float `f` as an integer when it is one within `i64`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "checked integral and within i64 before the cast"
)]
fn exact_int(f: f64) -> Option<i64> {
    (f.floor() == f && (-TWO_63..TWO_63).contains(&f)).then_some(f as i64)
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

/// Floor division of integers, `b` not 0 (Lua 5.4's `luaV_idiv`).
fn idiv(a: i64, b: i64) -> Result<i64, Fail> {
    if b == 0 {
        return Err(Fail::Error);
    }
    // `i64::MIN // -1` wraps in Lua; it is refused here.
    let q = a.checked_div(b).ok_or(Fail::Overflow)?;
    Ok(if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    })
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
/// [`Fail::Error`] for a non-number operand or an integer `//` by zero,
/// [`Fail::Overflow`] where integer arithmetic would wrap.
pub fn arith(op: Arith, a: Val, b: Val) -> Result<Val, Fail> {
    if let (Val::Int(x), Val::Int(y)) = (a, b) {
        let r = match op {
            Arith::Add => x.checked_add(y).ok_or(Fail::Overflow)?,
            Arith::Sub => x.checked_sub(y).ok_or(Fail::Overflow)?,
            Arith::Mul => x.checked_mul(y).ok_or(Fail::Overflow)?,
            Arith::Div => return Ok(out(ddiv(float(x), float(y)))),
            Arith::IDiv => idiv(x, y)?,
            Arith::Mod => {
                let m = dmod(float(x), float(y));
                return Ok(match exact_int(m) {
                    Some(n) if m.abs() <= TWO_53 => Val::Int(n),
                    _ => out(m),
                });
            }
        };
        return Ok(Val::Int(r));
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
/// [`Fail::Error`] for a non-number, [`Fail::Overflow`] for `-i64::MIN`.
pub fn neg(a: Val) -> Result<Val, Fail> {
    match a {
        Val::Int(n) => n.checked_neg().map(Val::Int).ok_or(Fail::Overflow),
        Val::Float(f) => Ok(out(-f)),
        Val::Nil | Val::Bool(_) => Err(Fail::Error),
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

/// A float result of `floor` or `ceil`: an integer when it is one.
fn int_or_float(f: f64) -> Val {
    exact_int(f).map_or(out(f), Val::Int)
}

/// `math.floor`, `math.ceil` or `math.abs` of `a`.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number, [`Fail::Overflow`] for
/// `abs(i64::MIN)`.
pub fn math(f: MathFn, a: Val) -> Result<Val, Fail> {
    match (f, a) {
        (MathFn::Floor | MathFn::Ceil, Val::Int(n)) => Ok(Val::Int(n)),
        (MathFn::Floor, Val::Float(x)) => Ok(int_or_float(x.floor())),
        (MathFn::Ceil, Val::Float(x)) => Ok(int_or_float(x.ceil())),
        (MathFn::Abs, Val::Int(n)) => n.checked_abs().map(Val::Int).ok_or(Fail::Overflow),
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
    if num(a).is_none() || num(b).is_none() {
        return Err(Fail::Error);
    }
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

/// The source's `toint()` as a number's `value` reads it: an integer's low
/// 32 bits; a float saturated to `i64` (NaN 0), then its low 32 bits; nil
/// and booleans 0.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "Java's d2l saturates and l2i keeps the low 32 bits, as these casts do"
)]
pub fn to_int(a: Val) -> i32 {
    match a {
        Val::Int(n) => n as i32,
        Val::Float(f) => (f as i64) as i32,
        Val::Nil | Val::Bool(_) => 0,
    }
}
