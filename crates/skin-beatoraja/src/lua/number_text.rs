//! Number text (design.md "beatoraja Lua Skins", number text) at import:
//! the one rule that writes a number as text, here for a number a play
//! text's `..` joins ([`super::residual`]), as the library's
//! `number_text` writes it for Lua's own coercion (`library.lua`), whose
//! VM is gone when the residual runs. An integer is its exact decimal
//! digits; a float whose value is an integer `x` with `-2^63 <= x < 2^63`
//! is that integer's digits (negative zero `0`); NaN is `nan` whatever its
//! sign, the infinities `inf` and `-inf`; any other float is `%.14g`: 14
//! significant digits, fixed for a decimal exponent from −4 to 13 and
//! otherwise `d.ddde±XX`, trailing zeros dropped. Run at import only.

use std::fmt::Write as _;

use skin_model::present::Val;

/// The longest text [`number_text`] writes, in bytes.
pub(crate) const NUMBER_TEXT_MAX: usize = 24;

/// Significant digits of `%.14g`.
const PRECISION: usize = 14;

/// `v` written by the number text rule, at most [`NUMBER_TEXT_MAX`]
/// bytes; `None` for nil and a boolean, which no number text writes
/// (Lua's `..` raises on them).
pub(crate) fn number_text(v: Val) -> Option<String> {
    let mut out = String::with_capacity(NUMBER_TEXT_MAX);
    match v {
        Val::Nil | Val::Bool(_) => return None,
        Val::Int(n) => write!(out, "{n}").ok()?,
        Val::Float(f) if f.is_nan() => out.push_str("nan"),
        Val::Float(f) if f.is_infinite() => out.push_str(if f > 0.0 { "inf" } else { "-inf" }),
        Val::Float(f) => match whole(f) {
            Some(n) => write!(out, "{n}").ok()?,
            None => general(f, &mut out)?,
        },
    }
    Some(out)
}

/// `f`'s integer when it holds one in `-2^63 <= x < 2^63`.
fn whole(f: f64) -> Option<i64> {
    // Both bounds are powers of two, so exact as doubles.
    const LOW: f64 = -9_223_372_036_854_775_808.0;
    (f.fract() == 0.0 && (LOW..-LOW).contains(&f)).then(|| {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a whole value inside the i64 range, converted exactly"
        )]
        let n = f as i64;
        n
    })
}

/// `%.14g` of a finite `f`: its 14 significant digits rounded exactly
/// (Rust's float formatting is exact, so the same on every platform),
/// laid out by the decimal exponent.
fn general(f: f64, out: &mut String) -> Option<()> {
    let sci = format!("{:.*e}", PRECISION - 1, f);
    let (mantissa, exp) = sci.split_once('e')?;
    let exp: i32 = exp.parse().ok()?;
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    // Trailing zeros dropped, as `%g` drops them.
    let used = digits.iter().rposition(|&d| d != b'0').map_or(1, |i| i + 1);
    let digits = digits.get(..used)?;
    if negative {
        out.push('-');
    }
    if (-4..i32::try_from(PRECISION).ok()?).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            digits.iter().for_each(|&d| out.push(char::from(d)));
        } else {
            let int = usize::try_from(exp).ok()?.saturating_add(1);
            for i in 0..int.max(used) {
                if i == int {
                    out.push('.');
                }
                out.push(char::from(digits.get(i).copied().unwrap_or(b'0')));
            }
        }
    } else {
        let (first, rest) = digits.split_first()?;
        out.push(char::from(*first));
        if !rest.is_empty() {
            out.push('.');
            rest.iter().for_each(|&d| out.push(char::from(d)));
        }
        write!(
            out,
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        )
        .ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{NUMBER_TEXT_MAX, number_text};
    use skin_model::present::Val;

    /// A row's Rust value: `int:<decimal>` or `f64:<IEEE 754 bits in hex>`.
    fn value(s: &str) -> Val {
        if let Some(n) = s.strip_prefix("int:") {
            Val::Int(n.parse().expect("a decimal integer"))
        } else if let Some(h) = s.strip_prefix("f64:") {
            Val::Float(f64::from_bits(
                u64::from_str_radix(h, 16).expect("hex bits"),
            ))
        } else {
            panic!("an unknown value column: {s}")
        }
    }

    #[test]
    fn number_text_follows_the_shared_rows() {
        let rows = include_str!("../../tests/number_text.txt");
        let mut n = 0;
        for line in rows
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            let cols: Vec<&str> = line.split('\t').collect();
            let [lua, rust, text] = cols[..] else {
                panic!("a row of three columns: {line}");
            };
            let got = number_text(value(rust)).expect("a number writes a text");
            assert_eq!(got, text, "the row {lua}");
            assert!(got.len() <= NUMBER_TEXT_MAX, "the row {lua} fits");
            n += 1;
        }
        assert_eq!(n, 21, "every row read");
        assert_eq!(number_text(Val::Nil), None);
        assert_eq!(number_text(Val::Bool(true)), None);
    }
}
