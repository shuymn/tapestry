//! Decimal text straight to exact rationals, and object ids.

use num_bigint::BigInt;
use num_rational::Ratio;

/// An exact rational of arbitrary-precision integers (D3).
pub type Rational = Ratio<BigInt>;

/// Fractional digits kept, counted after the exponent applies; further
/// digits are cut off, rounding toward zero (D3).
pub const MAX_FRACTION_DIGITS: i64 = 18;
/// At most this many integer digits, counted after the exponent applies and
/// without leading zeros, so every value times 10^6 fits `i64`.
pub const MAX_INTEGER_DIGITS: i64 = 12;
/// The largest positive exponent. A negative one only moves digits past
/// the kept fractional digits, so it may be larger.
pub const MAX_EXPONENT: i64 = 18;
/// The most significant digits a negative exponent may have, so it fits
/// `i64` with room to spare.
const MAX_EXPONENT_DIGITS: usize = 18;

/// Why a decimal was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalError {
    /// Not a decimal number.
    Malformed,
    /// A positive exponent beyond [`MAX_EXPONENT`], or a negative one of
    /// more than 18 digits.
    ExponentOutOfRange,
    /// More than [`MAX_INTEGER_DIGITS`] integer digits.
    TooManyIntegerDigits,
}

impl DecimalError {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "not a decimal number",
            Self::ExponentOutOfRange => "exponent out of range",
            Self::TooManyIntegerDigits => "more than 12 integer digits",
        }
    }
}

/// Splits an optional leading sign: whether it is `-`, and the rest.
fn split_sign(text: &str) -> (bool, &str) {
    match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    }
}

fn all_digits(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_digit())
}

/// Parses `[+-]?(digits[.digits?] | .digits)([eE][+-]?digits)?` exactly.
///
/// The value is `digits × 10^(exponent - fractional digits written)`. Its
/// digits are counted without the leading and trailing zeros that do not
/// change it: `0012.50e1` has 3 integer digits and none fractional. Digits
/// past the 18th fractional one are cut off (toward zero), so a value may
/// become 0. Every bound is checked on these counts before any number is
/// built, so a long run of digits costs a scan and nothing more.
pub fn parse_decimal(text: &str) -> Result<Rational, DecimalError> {
    let (negative, body) = split_sign(text);
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int.is_empty() && frac.is_empty()) || !all_digits(int) || !all_digits(frac) {
        return Err(DecimalError::Malformed);
    }
    let exponent = match exponent {
        None => 0,
        Some(e) => {
            let (negative, digits) = split_sign(e);
            if digits.is_empty() || !all_digits(digits) {
                return Err(DecimalError::Malformed);
            }
            let digits = digits.trim_start_matches('0');
            // A longer run is refused before it is read.
            if digits.len() > MAX_EXPONENT_DIGITS {
                return Err(DecimalError::ExponentOutOfRange);
            }
            let magnitude: i64 = if digits.is_empty() {
                0
            } else {
                digits.parse().map_err(|_| DecimalError::Malformed)?
            };
            if !negative && magnitude > MAX_EXPONENT {
                return Err(DecimalError::ExponentOutOfRange);
            }
            if negative { -magnitude } else { magnitude }
        }
    };
    let digits = || int.bytes().chain(frac.bytes());
    let total = int.len() + frac.len();
    let leading = digits().take_while(|&b| b == b'0').count();
    if leading == total {
        return Ok(Rational::from_integer(BigInt::from(0)));
    }
    let trailing = digits().rev().take_while(|&b| b == b'0').count();
    let count = |n: usize| i64::try_from(n).map_err(|_| DecimalError::Malformed);
    let significant = count(total - leading - trailing)?;
    // The value is `significant digits × 10^scale`. Within `i64`: the
    // exponent has at most 18 digits and the text is far shorter than
    // 10^18 bytes.
    let scale = exponent - count(frac.len())? + count(trailing)?;
    if significant + scale > MAX_INTEGER_DIGITS {
        return Err(DecimalError::TooManyIntegerDigits);
    }
    // Digits past the 18th fractional one are cut off.
    let cut = (-scale - MAX_FRACTION_DIGITS).max(0);
    if cut >= significant {
        return Ok(Rational::from_integer(BigInt::from(0)));
    }
    let scale = scale + cut;
    let kept = usize::try_from(significant - cut).map_err(|_| DecimalError::Malformed)?;
    // At most 12 + 18 = 30 digits, below 10^30 < 2^100.
    let coefficient = digits()
        .skip(leading)
        .take(kept)
        .fold(0_u128, |n, d| n * 10 + u128::from(d - b'0'));
    let power = |e: i64| BigInt::from(10_u64.pow(u32::try_from(e).unwrap_or(0)));
    let numer = BigInt::from(coefficient) * power(scale.max(0));
    let numer = if negative { -numer } else { numer };
    Ok(Ratio::new(numer, power((-scale).max(0))))
}

/// Parses a two-character base-36 id (`00`-`ZZ`, any case).
pub fn parse_base36(pair: &[u8]) -> Option<u16> {
    let digit = |b: u8| char::from(b).to_digit(36);
    match pair {
        [a, b] => Some(u16::try_from(digit(*a)? * 36 + digit(*b)?).ok()?),
        _ => None,
    }
}

/// Parses a two-character hexadecimal value (`00`-`FF`, any case).
pub fn parse_hex(pair: &[u8]) -> Option<u16> {
    let digit = |b: u8| char::from(b).to_digit(16);
    match pair {
        [a, b] => Some(u16::try_from(digit(*a)? * 16 + digit(*b)?).ok()?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigInt;

    use super::{DecimalError, Rational, parse_base36, parse_decimal, parse_hex};

    /// `n / d` as a [`Rational`].
    fn r(n: i128, d: i128) -> Rational {
        Rational::new(BigInt::from(n), BigInt::from(d))
    }

    #[test]
    fn decimals_parse_exactly() {
        for (text, value) in [
            ("120", r(120, 1)),
            ("+121", r(121, 1)),
            ("333.333", r(333_333, 1_000)),
            ("0.000001", r(1, 1_000_000)),
            ("-1.5", r(-3, 2)),
            (".5", r(1, 2)),
            ("2.", r(2, 1)),
            ("000000000000001", r(1, 1)),
            ("-0", r(0, 1)),
            // More than 6 fractional digits, and the exponent forms.
            ("0.0078125", r(1, 128)),
            ("1.00001E+07", r(10_000_100, 1)),
            ("4.16E-02", r(26, 625)),
            ("1e3", r(1_000, 1)),
            ("2.5e0", r(5, 2)),
            ("0012.50e1", r(125, 1)),
            ("1e0000000000000000005", r(100_000, 1)),
            ("0e18", r(0, 1)),
            // The bounds, reached: 18 fractional and 12 integer digits.
            ("0.000000000000000003", r(3, 1_000_000_000_000_000_000)),
            ("7E-0018", r(7, 1_000_000_000_000_000_000)),
            ("999999999999", r(999_999_999_999, 1)),
            ("9.99999999999e11", r(999_999_999_999, 1)),
            (
                "999999999999.999999999999999999",
                r(10_i128.pow(30) - 1, 10_i128.pow(18)),
            ),
            // Zeros that do not change the value do not count.
            ("0.40000000000000000000000", r(2, 5)),
            ("00000000000000000000000000000000006", r(6, 1)),
            ("8000000000000000000000000e-18", r(8_000_000, 1)),
        ] {
            assert_eq!(parse_decimal(text), Ok(value), "{text}");
        }
    }

    #[test]
    fn digits_past_the_eighteenth_fractional_one_are_cut_off() {
        let e18 = 10_i128.pow(18);
        let ones = "1".repeat(100_000);
        for (text, value) in [
            (
                "0.7654321098765432109876543210987654321098",
                r(765_432_109_876_543_210, e18),
            ),
            // Toward zero for a negative value.
            ("-2.1234567890123456789", r(-2_123_456_789_012_345_678, e18)),
            (&format!("0.{ones}"), r(111_111_111_111_111_111, e18)),
            (&format!("1{}e-100018", "0".repeat(100_000)), r(1, e18)),
            // Values below 10^-18 become 0.
            ("0.0000000000000000001", r(0, 1)),
            ("-0.0000000000000000009", r(0, 1)),
            ("0.1e-18", r(0, 1)),
            ("1e-19", r(0, 1)),
            ("1E-100", r(0, 1)),
            ("9e-999999999999999999", r(0, 1)),
        ] {
            assert_eq!(
                parse_decimal(text),
                Ok(value),
                "{}",
                &text[..text.len().min(40)]
            );
        }
    }

    #[test]
    fn decimals_out_of_bounds_are_refused_before_they_are_built() {
        let zeros = "0".repeat(100_000);
        for (text, error) in [
            ("1000000000000", DecimalError::TooManyIntegerDigits),
            ("1e12", DecimalError::TooManyIntegerDigits),
            ("1e19", DecimalError::ExponentOutOfRange),
            ("0e19", DecimalError::ExponentOutOfRange),
            ("1e100", DecimalError::ExponentOutOfRange),
            // Exponents too long to read as a number.
            ("1e1000000000000000000", DecimalError::ExponentOutOfRange),
            ("1e-1000000000000000000", DecimalError::ExponentOutOfRange),
            (
                "1234567890123.0000000000000000001",
                DecimalError::TooManyIntegerDigits,
            ),
            (&format!("4{zeros}"), DecimalError::TooManyIntegerDigits),
            (&format!("5{zeros}e-18"), DecimalError::TooManyIntegerDigits),
        ] {
            assert_eq!(
                parse_decimal(text),
                Err(error),
                "{}",
                &text[..text.len().min(40)]
            );
        }
        assert_eq!(parse_decimal(&format!("{zeros}3")), Ok(r(3, 1)));
    }

    #[test]
    fn non_decimals_are_malformed() {
        for bad in [
            "", ".", "+", "1.2.3", "abc", "- 1", "1 5", "1,5", "NaN", "Infinity", "inf", "0x1p3",
            "1e", "e3", ".e3", "1e+", "1e1.5", "++1", "1e--1", "1.5f", "1e 3",
        ] {
            assert_eq!(parse_decimal(bad), Err(DecimalError::Malformed), "{bad}");
        }
    }

    #[test]
    fn ids_parse_in_base36_and_hex() {
        assert_eq!(parse_base36(b"01"), Some(1));
        assert_eq!(parse_base36(b"zz"), Some(1295));
        assert_eq!(parse_base36(b"0A"), Some(10));
        assert_eq!(parse_base36(b"0!"), None);
        assert_eq!(parse_hex(b"FF"), Some(255));
        assert_eq!(parse_hex(b"0g"), None);
    }
}
