//! A string read as a number (design.md "beatoraja Lua Skins", a slider's
//! value and the runtime; "Play graphs and visualizers"): ASCII whitespace
//! ignored at both ends (space, tab, line feed, vertical tab, form feed and
//! carriage return, Lua's `isspace`), an optional `+` or `-`, then a
//! decimal numeral, `(d+(.d*)?|.d+)([eE][+-]?d+)?`, the grammar a chart's
//! decimals use (`bms-parser`'s `decimal`), or, for [`number`], `0x` or
//! `0X` and hexadecimal digits whose value fits `u64`. The value is rounded
//! once to the nearest `f64` (an exponent past its range gives an
//! infinity); anything else, `inf`, `nan`, `1_0`, fullwidth digits or a
//! sign after `0x` included, is no number.

/// Whether `b` is one of the six ASCII whitespace bytes.
const fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// `s` without whitespace at either end, its sign split off: whether it
/// is `-`, and the rest.
fn signed(s: &[u8]) -> (bool, &[u8]) {
    let start = s.iter().position(|&b| !space(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|&b| !space(b)).map_or(start, |i| i + 1);
    match s.get(start..end).unwrap_or_default() {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        t => (false, t),
    }
}

/// `s` without its leading ASCII digits, and how many there were.
fn digits(s: &[u8]) -> (usize, &[u8]) {
    let n = s.iter().take_while(|b| b.is_ascii_digit()).count();
    (n, s.get(n..).unwrap_or_default())
}

/// An unsigned decimal numeral's value.
fn unsigned_decimal(s: &[u8]) -> Option<f64> {
    let (whole, rest) = digits(s);
    let (fraction, rest) = match rest {
        [b'.', r @ ..] => digits(r),
        r => (0, r),
    };
    if whole == 0 && fraction == 0 {
        return None;
    }
    let exponent = match rest {
        [] => true,
        [b'e' | b'E', b'+' | b'-', r @ ..] | [b'e' | b'E', r @ ..] => {
            let (n, r) = digits(r);
            n > 0 && r.is_empty()
        }
        _ => false,
    };
    if !exponent {
        return None;
    }
    // Every byte is ASCII, and Rust's parse of this grammar rounds once.
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Hexadecimal digits' value, `None` past `u64`.
fn unsigned_hex(s: &[u8]) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in s {
        let d = char::from(b).to_digit(16)?;
        v = v.checked_mul(16)?.checked_add(u64::from(d))?;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a `u64` to the nearest `f64`, rounded once"
    )]
    Some(v as f64)
}

/// `s` as a decimal numeral; `None` when it is none.
#[must_use]
pub fn decimal(s: &[u8]) -> Option<f64> {
    let (neg, t) = signed(s);
    unsigned_decimal(t).map(|v| if neg { -v } else { v })
}

/// `s` as a decimal numeral or `0x` and hexadecimal digits; `None` when
/// it is neither.
#[must_use]
pub fn number(s: &[u8]) -> Option<f64> {
    let (neg, t) = signed(s);
    let v = match t {
        [b'0', b'x' | b'X', h @ ..] => unsigned_hex(h),
        _ => unsigned_decimal(t),
    }?;
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::{decimal, number};

    /// The grammar, read directly: each row's `number` and `decimal`.
    #[test]
    fn the_grammar_is_read_directly() {
        let two_64 = 18_446_744_073_709_551_616.0;
        for (s, n, d) in [
            ("1", Some(1.0), Some(1.0)),
            (".5", Some(0.5), Some(0.5)),
            ("5.", Some(5.0), Some(5.0)),
            ("+.5e1", Some(5.0), Some(5.0)),
            ("1E+2", Some(100.0), Some(100.0)),
            ("1e-2", Some(0.01), Some(0.01)),
            ("00012", Some(12.0), Some(12.0)),
            ("-0", Some(-0.0), Some(-0.0)),
            ("18446744073709551616", Some(two_64), Some(two_64)),
            ("1e400", Some(f64::INFINITY), Some(f64::INFINITY)),
            ("-1e400", Some(f64::NEG_INFINITY), Some(f64::NEG_INFINITY)),
            ("0x10", Some(16.0), None),
            ("+0X10", Some(16.0), None),
            ("-0x1", Some(-1.0), None),
            ("0xffffffffffffffff", Some(two_64), None),
            ("0x10000000000000000", None, None),
            ("0x", None, None),
            ("0x-1", None, None),
            ("0x[", None, None),
            ("0x1g", None, None),
            ("0x1p-1", None, None),
            ("", None, None),
            (" ", None, None),
            ("+", None, None),
            ("-", None, None),
            (".", None, None),
            ("1e", None, None),
            ("1e+", None, None),
            ("--5", None, None),
            ("+-5", None, None),
            ("1 2", None, None),
            ("1\t", Some(1.0), Some(1.0)),
            ("- 1", None, None),
            ("12abc", None, None),
            ("1x", None, None),
            ("0.5f", None, None),
            ("0.5D", None, None),
            ("inf", None, None),
            ("nan", None, None),
            ("NaN", None, None),
            ("Infinity", None, None),
            ("1_0", None, None),
            ("１２", None, None),
            ("\u{1} 0.5", None, None),
            ("\u{a0}1", None, None),
        ] {
            let bits = |v: Option<f64>| v.map(f64::to_bits);
            assert_eq!(bits(number(s.as_bytes())), bits(n), "number {s:?}");
            assert_eq!(bits(decimal(s.as_bytes())), bits(d), "decimal {s:?}");
        }
    }

    /// Each of the six whitespace bytes is ignored at either end, alone or
    /// together, and nowhere inside.
    #[test]
    fn whitespace_is_lua_s_six_bytes_at_either_end() {
        for w in [" ", "\t", "\n", "\u{b}", "\u{c}", "\r"] {
            for s in [format!("{w}12"), format!("12{w}"), format!("{w}{w}12{w}")] {
                assert_eq!(number(s.as_bytes()), Some(12.0), "{s:?}");
                assert_eq!(decimal(s.as_bytes()), Some(12.0), "{s:?}");
            }
            assert_eq!(number(format!("{w}0x10{w}").as_bytes()), Some(16.0));
            assert_eq!(number(format!("1{w}2").as_bytes()), None, "{w:?}");
            assert_eq!(number(format!("-{w}2").as_bytes()), None, "{w:?}");
        }
        assert_eq!(
            number(" \t\n\u{b}\u{c}\r7 \t\n\u{b}\u{c}\r".as_bytes()),
            Some(7.0)
        );
    }

    /// A long fraction is read whole, rounded once: 68 bytes, past any
    /// 64-byte prefix, and a fraction whose last digit decides.
    #[test]
    fn a_long_numeral_is_read_whole() {
        let s = format!("0.{}1e80", "0".repeat(62));
        assert_eq!(number(s.as_bytes()), Some(1e17));
        let s = format!("1.{}x", "0".repeat(70));
        assert_eq!(number(s.as_bytes()), None);
        let s = format!("1.{}", "0".repeat(70));
        assert_eq!(number(s.as_bytes()), Some(1.0));
    }
}
