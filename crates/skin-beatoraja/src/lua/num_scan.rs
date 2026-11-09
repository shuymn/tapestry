//! A string's number as the source's interpreter reads it for `tofloat()`
//! (LuaJ 3.0.2, `LuaString.todouble()`: `scannumber()`, NaN read as 0),
//! ported step for step from the pinned jar's bytecode so a slider's
//! `value` known at import as a string folds to the double the source
//! converts. The differential table `tests/luaj_numerics.txt` pins it.

/// `LuaString.todouble()`: [`scan_number`] with NaN read as 0. A `-0.0`
/// stays −0, as the source's does.
pub fn todouble(s: &[u8]) -> f64 {
    let d = scan_number(s);
    if d.is_nan() { 0.0 } else { d }
}

/// `LuaString.scannumber()`: only ASCII spaces are trimmed, from both
/// ends; nothing left is NaN; `0x` or `0X` reads the rest in base 16;
/// otherwise base 10, and on NaN as a decimal float.
fn scan_number(s: &[u8]) -> f64 {
    let mut i = 0;
    let mut j = s.len();
    while i < j && s.get(i) == Some(&b' ') {
        i += 1;
    }
    while i < j && s.get(j - 1) == Some(&b' ') {
        j -= 1;
    }
    let Some(t) = s.get(i..j) else {
        return f64::NAN;
    };
    if t.is_empty() {
        return f64::NAN;
    }
    if let [b'0', b'x' | b'X', rest @ ..] = t {
        return scan_long(16, rest);
    }
    let l = scan_long(10, t);
    if l.is_nan() { scan_double(t) } else { l }
}

/// `LuaString.scanlong(base, start, end)`: an optional leading `-`, then
/// each byte's digit accumulated in a wrapping Java `long`, NaN as soon as
/// a digit is outside `0..base` or the accumulator is negative after a
/// step; the result negated, as a double. A digit is the byte (Java's
/// signed `byte`) less `'0'` when `base ≤ 10` or the byte is `0-9`, less
/// 55 for `A-Z`, and less 87 otherwise, exactly as the source computes it:
/// so in base 16 `[ \ ] ^ _ `` read as 4 to 9, as they do there.
fn scan_long(base: i64, s: &[u8]) -> f64 {
    let (neg, digits) = match s {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, s),
    };
    let mut x: i64 = 0;
    for &b in digits {
        #[expect(clippy::cast_possible_wrap, reason = "Java's byte is signed")]
        let c = i64::from(b as i8);
        let d = if base <= 10 || b.is_ascii_digit() {
            c - i64::from(b'0')
        } else if b.is_ascii_uppercase() {
            c - 55
        } else {
            c - 87
        };
        if d < 0 || d >= base {
            return f64::NAN;
        }
        x = x.wrapping_mul(base).wrapping_add(d);
        if x < 0 {
            return f64::NAN;
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "Java's l2d rounds to nearest, as this cast does"
    )]
    let v = if neg { x.wrapping_neg() } else { x } as f64;
    v
}

/// `LuaString.scandouble(start, end)`: the first 64 bytes only; any byte
/// but `+ - . 0-9 E e` is NaN; then `Double.parseDouble`, whose failures
/// are NaN. Rust's parse agrees with Java's on what the filter lets
/// through (the table checks it).
fn scan_double(s: &[u8]) -> f64 {
    let t = s.get(..s.len().min(64)).unwrap_or(s);
    if !t
        .iter()
        .all(|&b| matches!(b, b'+' | b'-' | b'.' | b'0'..=b'9' | b'E' | b'e'))
    {
        return f64::NAN;
    }
    std::str::from_utf8(t)
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(f64::NAN)
}
