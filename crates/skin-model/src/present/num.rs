//! The program's numerics: Lua 5.4's (`lvm.c`, `lmathlib.c`), shared by
//! the executor and the importer's folding, so folding equals evaluation
//! (design.md "beatoraja Lua Skins", numbers).
//!
//! - `+ - *` of two integers are checked ([`Fail::Overflow`] where Lua
//!   would wrap); with a float operand both go through `f64`.
//! - `/` is always a float division.
//! - `//` and `%` floor; an integer divisor of 0 is a Lua error (the
//!   compiler refuses one that can be 0); a float `%` is `fmod` with Lua's
//!   sign adjustment.
//! - An integer and a float compare exactly; ordering anything that is not
//!   a number is a Lua error; `==` across types is false (an integer and a
//!   float are one type, numbers).
//! - `floor` and `ceil` keep an integer and give an integer for a float
//!   result that fits, else the float; `abs` of `i64::MIN` is
//!   [`Fail::Overflow`]; `max` and `min` keep the first of equal
//!   arguments, as the sandbox's library does.
//! - A result is converted as its consumer converts it
//!   ([`to_bool`], [`to_int`], `SkinLuaAccessor.java:612-651` at `0.8.9`).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::{Arith, Cmp, Fail, MathFn, Val};

/// 2^63 as a float: the first float past `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// An integer as a float, rounded to nearest as Lua's `cast_num` does.
#[expect(
    clippy::cast_precision_loss,
    reason = "Lua's integer-to-float conversion rounds to nearest"
)]
fn float(n: i64) -> f64 {
    n as f64
}

/// Float `f` as an integer when it is one within `i64`
/// (`lua_numbertointeger`).
#[expect(
    clippy::cast_possible_truncation,
    reason = "checked integral and within i64 before the cast"
)]
fn exact_int(f: f64) -> Option<i64> {
    (f.floor() == f && (-TWO_63..TWO_63).contains(&f)).then_some(f as i64)
}

/// A number's float value; `None` for a non-number.
fn num(v: Val) -> Option<f64> {
    match v {
        Val::Int(n) => Some(float(n)),
        Val::Float(f) => Some(f),
        Val::Nil | Val::Bool(_) => None,
    }
}

/// Floor division of integers, `b` not 0 (`luaV_idiv`).
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

/// Floor modulo of integers, `b` not 0 (`luaV_mod`).
fn imod(a: i64, b: i64) -> Result<i64, Fail> {
    if b == 0 {
        return Err(Fail::Error);
    }
    // `i64::MIN % -1` is 0, as in Lua.
    let m = a.checked_rem(b).unwrap_or(0);
    Ok(if m != 0 && ((m ^ b) < 0) { m + b } else { m })
}

/// Float modulo (`luai_nummod`): C's `fmod`, then `b` added where a
/// nonzero result's sign differs from the divisor's.
fn fmod(a: f64, b: f64) -> f64 {
    let m = a % b;
    if (m > 0.0 && b < 0.0) || (m < 0.0 && b > 0.0) {
        m + b
    } else {
        m
    }
}

/// Arithmetic operator `op` of `a` and `b`.
///
/// # Errors
///
/// [`Fail::Error`] for a non-number operand or an integer `//` or `%` by
/// zero, [`Fail::Overflow`] where integer arithmetic would wrap.
pub fn arith(op: Arith, a: Val, b: Val) -> Result<Val, Fail> {
    if let (Val::Int(x), Val::Int(y)) = (a, b) {
        let r = match op {
            Arith::Add => x.checked_add(y).ok_or(Fail::Overflow)?,
            Arith::Sub => x.checked_sub(y).ok_or(Fail::Overflow)?,
            Arith::Mul => x.checked_mul(y).ok_or(Fail::Overflow)?,
            Arith::Div => return Ok(Val::Float(float(x) / float(y))),
            Arith::IDiv => idiv(x, y)?,
            Arith::Mod => imod(x, y)?,
        };
        return Ok(Val::Int(r));
    }
    let (Some(x), Some(y)) = (num(a), num(b)) else {
        return Err(Fail::Error);
    };
    Ok(Val::Float(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
        Arith::Div => x / y,
        Arith::IDiv => (x / y).floor(),
        Arith::Mod => fmod(x, y),
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
        Val::Float(f) => Ok(Val::Float(-f)),
        Val::Nil | Val::Bool(_) => Err(Fail::Error),
    }
}

/// Lua's truthiness: only nil and false are false.
#[must_use]
pub fn truthy(a: Val) -> bool {
    !matches!(a, Val::Nil | Val::Bool(false))
}

/// Whether integer `i` is less than float `f` (`LTintfloat`), or, with
/// `eq`, at most it (`LEintfloat`); NaN orders nothing.
fn int_lt_float(i: i64, f: f64, eq: bool) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO_63 {
        return true;
    }
    if f < -TWO_63 {
        return false;
    }
    // Within i64 now: `i < f` is `i < ceil(f)`, `i <= f` is `i <= floor(f)`.
    let bound = if eq { f.floor() } else { f.ceil() };
    match exact_int(bound) {
        Some(b) => {
            if eq {
                i <= b
            } else {
                i < b
            }
        }
        // `ceil(f)` is 2^63: every integer is below it.
        None => true,
    }
}

/// Whether float `f` is less than integer `i` (`LTfloatint`), or, with
/// `eq`, at most it (`LEfloatint`); NaN orders nothing.
fn float_lt_int(f: f64, i: i64, eq: bool) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= TWO_63 {
        return false;
    }
    if f < -TWO_63 {
        return true;
    }
    // `f < i` is `floor(f) < i`, `f <= i` is `ceil(f) <= i`.
    let bound = if eq { f.ceil() } else { f.floor() };
    match exact_int(bound) {
        Some(b) => {
            if eq {
                b <= i
            } else {
                b < i
            }
        }
        None => false,
    }
}

/// Whether `a` equals `b` (`luaV_equalobj`, raw): an integer and a float
/// are equal when the float is exactly that integer.
#[must_use]
pub fn equal(a: Val, b: Val) -> bool {
    match (a, b) {
        (Val::Nil, Val::Nil) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Int(x), Val::Int(y)) => x == y,
        #[expect(clippy::float_cmp, reason = "Lua's float equality is exact")]
        (Val::Float(x), Val::Float(y)) => x == y,
        (Val::Int(i), Val::Float(f)) | (Val::Float(f), Val::Int(i)) => exact_int(f) == Some(i),
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
        (Val::Int(i), Val::Float(f)) => int_lt_float(i, f, le),
        (Val::Float(f), Val::Int(i)) => float_lt_int(f, i, le),
        _ => return Err(Fail::Error),
    })
}

/// A float result of `floor` or `ceil` as Lua pushes it (`pushnumint`).
fn int_or_float(f: f64) -> Val {
    exact_int(f).map_or(Val::Float(f), Val::Int)
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
        (MathFn::Abs, Val::Float(x)) => Ok(Val::Float(x.abs())),
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
    Ok(if better { b } else { a })
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
