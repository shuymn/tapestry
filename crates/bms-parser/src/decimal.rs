//! Decimal text straight to exact rationals, and object ids.

use num_bigint::BigInt;
use num_rational::Ratio;

/// An exact rational of arbitrary-precision integers (D3).
pub type Rational = Ratio<BigInt>;

/// At most this many fractional digits (D3).
pub const MAX_FRACTION_DIGITS: usize = 6;
/// At most this many integer digits, so every value times 10^6 fits `i64`.
pub const MAX_INTEGER_DIGITS: usize = 12;

/// Why a decimal was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalError {
    /// Not a decimal number.
    Malformed,
    /// More than [`MAX_FRACTION_DIGITS`] fractional digits.
    TooManyFractionDigits,
    /// More than [`MAX_INTEGER_DIGITS`] integer digits.
    TooManyIntegerDigits,
}

impl DecimalError {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "not a decimal number",
            Self::TooManyFractionDigits => "more than 6 fractional digits",
            Self::TooManyIntegerDigits => "more than 12 integer digits",
        }
    }
}

/// Parses `[+-]digits[.digits]` exactly.
pub fn parse_decimal(text: &str) -> Result<Rational, DecimalError> {
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty() {
        return Err(DecimalError::Malformed);
    }
    if !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(DecimalError::Malformed);
    }
    let int = int.trim_start_matches('0');
    if int.len() > MAX_INTEGER_DIGITS {
        return Err(DecimalError::TooManyIntegerDigits);
    }
    if frac.len() > MAX_FRACTION_DIGITS {
        return Err(DecimalError::TooManyFractionDigits);
    }
    // At most 18 digits, so the parse cannot overflow.
    let digits = format!("{int}{frac}");
    let numer: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().map_err(|_| DecimalError::Malformed)?
    };
    let denom = 10_i128.pow(u32::try_from(frac.len()).map_err(|_| DecimalError::Malformed)?);
    let numer = if negative { -numer } else { numer };
    Ok(Ratio::new(BigInt::from(numer), BigInt::from(denom)))
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
        assert_eq!(parse_decimal("120"), Ok(r(120, 1)));
        assert_eq!(parse_decimal("333.333"), Ok(r(333_333, 1_000)));
        assert_eq!(parse_decimal("0.000001"), Ok(r(1, 1_000_000)));
        assert_eq!(parse_decimal("-1.5"), Ok(r(-3, 2)));
        assert_eq!(parse_decimal(".5"), Ok(r(1, 2)));
        assert_eq!(parse_decimal("2."), Ok(r(2, 1)));
        assert_eq!(
            parse_decimal("0.0000001"),
            Err(DecimalError::TooManyFractionDigits)
        );
        assert_eq!(
            parse_decimal("1234567890123"),
            Err(DecimalError::TooManyIntegerDigits)
        );
        assert_eq!(parse_decimal("000000000000001"), Ok(r(1, 1)));
        for bad in ["", ".", "1e3", "1.2.3", "abc", "- 1"] {
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
