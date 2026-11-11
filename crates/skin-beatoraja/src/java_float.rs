//! A string's `float` as Java's `Float.parseFloat` reads it, which the
//! source's JSON reader uses for a string-valued `float` field
//! (`JsonValue.asFloat` in the pinned `gdx.jar`; `FloatingDecimal`):
//! leading and trailing characters at or below U+0020 trimmed; an
//! optional sign; then `NaN`, `Infinity`, a decimal (digits with an
//! optional point and exponent) or a hexadecimal float (`0x`, hex digits
//! with an optional point, and a required binary exponent `p`); a decimal
//! or hexadecimal one may end in `f`, `F`, `d` or `D`. Every value is
//! rounded once, to nearest with ties to even, as Java's is.

/// `s` as `Float.parseFloat(s)` reads it; `None` where Java throws.
pub(crate) fn parse_float(s: &str) -> Option<f32> {
    let t = s.trim_matches(|c: char| c <= ' ');
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, t.get(1..)?),
        Some(b'+') => (false, t.get(1..)?),
        _ => (false, t),
    };
    let signed = |v: f32| if neg { -v } else { v };
    match body {
        "NaN" => return Some(f32::NAN),
        "Infinity" => return Some(signed(f32::INFINITY)),
        _ => {}
    }
    let body = body.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(body);
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return hex_float(hex).map(signed);
    }
    decimal(body)
        .then(|| body.parse::<f32>().ok())
        .flatten()
        .map(signed)
}

/// Whether `s` is Java's decimal syntax: digits, an optional point with
/// digits (at least one digit in all), and an optional `e` or `E` with an
/// optional sign and at least one digit. (Rust's parser, which rounds the
/// same way, also takes `inf` and `nan`, which Java does not.)
fn decimal(s: &str) -> bool {
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (s.get(..i).unwrap_or(""), Some(s.get(i + 1..).unwrap_or(""))),
        None => (s, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(int) && digits(frac) && !(int.is_empty() && frac.is_empty());
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// A hexadecimal float after its `0x`: hex digits, an optional point with
/// hex digits (at least one digit in all), then `p` or `P`, an optional
/// sign and decimal digits. The value is rounded to `f32` once.
fn hex_float(s: &str) -> Option<f32> {
    let (mantissa, exponent) = s.split_once(['p', 'P'])?;
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let (eneg, edigits) = match exponent.as_bytes().first() {
        Some(b'-') => (true, exponent.get(1..)?),
        Some(b'+') => (false, exponent.get(1..)?),
        _ => (false, exponent),
    };
    if edigits.is_empty() || !edigits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The exponent saturates far past any float's range.
    let e = edigits
        .bytes()
        .fold(0_i64, |e, b| (e * 10 + i64::from(b - b'0')).min(1 << 40));
    let mut e2 = if eneg { -e } else { e };
    // The significand's leading 64 bits and whether any bit below them is
    // set; `e2` the power of two of its last kept bit.
    let mut top: u64 = 0;
    let mut sticky = false;
    let mut kept = 0_u32;
    for (i, c) in int.chars().chain(frac.chars()).enumerate() {
        let d = u64::from(c.to_digit(16)?);
        if i >= int.len() {
            e2 -= 4;
        }
        if top == 0 && d == 0 {
            continue;
        }
        if kept + 4 <= 64 {
            top = top << 4 | d;
            kept += 4;
        } else {
            sticky |= d != 0;
            e2 += 4;
        }
    }
    if top == 0 {
        return Some(0.0);
    }
    Some(round_to_f32(top, sticky, e2))
}

/// `top · 2^e2`, with `sticky` set when bits below `top` are not all 0,
/// rounded to the nearest `f32`, ties to even.
fn round_to_f32(top: u64, sticky: bool, e2: i64) -> f32 {
    let msb = 63 - i64::from(top.leading_zeros());
    // The value's binary exponent: 2^exp ≤ value < 2^(exp+1).
    let exp = msb + e2;
    if exp > 127 {
        return f32::INFINITY;
    }
    // The bits kept: 24 for a normal float, fewer below 2^-126.
    let keep = if exp >= -126 { 24 } else { 24 - (-126 - exp) };
    if keep <= 0 {
        // Below half the smallest subnormal rounds to 0; at it, to even
        // (0); above it, to the smallest subnormal.
        let half = keep == 0 && (top.count_ones() > 1 || sticky);
        return if half { f32::from_bits(1) } else { 0.0 };
    }
    let drop = msb + 1 - keep;
    let (mut m, round, rest) = if drop > 0 {
        let d = u32::try_from(drop).unwrap_or(64);
        let m = top >> d;
        let round = top >> (d - 1) & 1 == 1;
        let rest = top & ((1_u64 << (d - 1)) - 1) != 0 || sticky;
        (m, round, rest)
    } else {
        (top << u32::try_from(-drop).unwrap_or(0), false, sticky)
    };
    if round && (rest || m & 1 == 1) {
        m += 1;
    }
    // `m` holds `keep` bits, or `keep + 1` after a carry.
    let mut exp = exp;
    if m >> u32::try_from(keep).unwrap_or(24) != 0 {
        m >>= 1;
        exp += 1;
    }
    if exp > 127 {
        return f32::INFINITY;
    }
    let bits = if exp >= -126 && m >> 23 == 1 {
        let biased = u32::try_from(exp + 127).unwrap_or(0);
        biased << 23 | (u32::try_from(m).unwrap_or(0) & 0x7f_ffff)
    } else {
        // A subnormal: `m` is its significand, its exponent field 0 (a
        // carry into 2^23 makes it the smallest normal, as the bits say).
        u32::try_from(m).unwrap_or(0)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::parse_float;

    fn bits(s: &str) -> Option<u32> {
        parse_float(s).map(f32::to_bits)
    }

    /// Java's decimal forms, with suffixes, signs and trimming.
    #[test]
    fn decimals_read_as_java_reads_them() {
        for (s, v) in [
            ("0.5", 0.5_f32),
            ("0.5f", 0.5),
            ("0.5D", 0.5),
            (" \t0.5\n", 0.5),
            ("+.5", 0.5),
            ("5.", 5.0),
            ("1e-1", 0.1),
            ("-2.5E+1", -25.0),
            ("1e40", f32::INFINITY),
        ] {
            assert_eq!(bits(s), Some(v.to_bits()), "{s}");
        }
        assert_eq!(bits("Infinity"), Some(f32::INFINITY.to_bits()));
        assert_eq!(bits("-Infinity"), Some(f32::NEG_INFINITY.to_bits()));
        assert!(parse_float("NaN").is_some_and(f32::is_nan));
        for bad in [
            "",
            ".",
            "e5",
            "1e",
            "inf",
            "nan",
            "infinity",
            "Infinityf",
            "1_0",
            "0.5ff",
            "1 0",
        ] {
            assert_eq!(parse_float(bad), None, "{bad}");
        }
    }

    /// Hexadecimal floats, rounded once to nearest, ties to even.
    #[test]
    fn hex_floats_round_once() {
        for (s, b) in [
            ("0x1p-1", 0.5_f32.to_bits()),
            ("0x1.8p1", 3.0_f32.to_bits()),
            ("0X.8P0f", 0.5_f32.to_bits()),
            ("0x10p-4", 1.0_f32.to_bits()),
            // 1 + 2^-24: halfway between 1 and 1 + 2^-23, to even (1).
            ("0x1.000001p0", 1.0_f32.to_bits()),
            // 1 + 3·2^-24: halfway, to even (1 + 2^-22).
            ("0x1.000003p0", (1.0_f32 + 2.0_f32.powi(-22)).to_bits()),
            // Just above halfway rounds up.
            (
                "0x1.0000010000000001p0",
                (1.0_f32 + 2.0_f32.powi(-23)).to_bits(),
            ),
            ("0x1p-149", 1),
            ("0x1p-150", 0),
            ("0x1.1p-150", 1),
            ("0x1p-126", f32::MIN_POSITIVE.to_bits()),
            ("0x1.fffffep127", f32::MAX.to_bits()),
            ("0x1.ffffffp127", f32::INFINITY.to_bits()),
            ("0x1p128", f32::INFINITY.to_bits()),
            ("0x0p0", 0),
        ] {
            assert_eq!(bits(s), Some(b), "{s}");
        }
        for bad in ["0x1", "0xp1", "0x.p1", "0x1p", "0x1g1p0", "0x1p1.5"] {
            assert_eq!(parse_float(bad), None, "{bad}");
        }
    }
}
