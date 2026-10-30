//! The lexer. Sources are ASCII; durations are converted to integer ms.

use super::MAX_NAME_BYTES;
use super::error::{CompileError, ErrorKind, Span};

/// The magnitude of `i32::MIN`: the largest integer literal, valid only as
/// the direct operand of unary `-`.
pub(super) const MIN_MAGNITUDE: u32 = 0x8000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// An integer literal, at most [`MIN_MAGNITUDE`].
    Int(u32),
    /// A duration, in ms.
    Duration(i32),
    Name,
    If,
    Then,
    Else,
    True,
    False,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    LParen,
    RParen,
    Comma,
    Eof,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Token {
    pub(super) tok: Tok,
    pub(super) span: Span,
}

/// Lexes `src` into tokens ending with [`Tok::Eof`].
pub(super) fn lex(src: &str) -> Result<Vec<Token>, CompileError> {
    let b = src.as_bytes();
    let at = |i: usize| b.get(i).copied();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = at(i) {
        let start = i;
        let tok = match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'0'..=b'9' => {
                let (tok, end) = number(b, start)?;
                i = end;
                tok
            }
            b'a'..=b'z' | b'_' => {
                i = name_end(b, start);
                if i - start > MAX_NAME_BYTES {
                    return Err(CompileError::new(
                        Span::new(start, i),
                        ErrorKind::NameTooLong,
                    ));
                }
                match src.get(start..i) {
                    Some("if") => Tok::If,
                    Some("then") => Tok::Then,
                    Some("else") => Tok::Else,
                    Some("true") => Tok::True,
                    Some("false") => Tok::False,
                    _ => Tok::Name,
                }
            }
            _ => {
                let two = |second: u8, tok: Tok| (at(start + 1) == Some(second)).then_some(tok);
                let (tok, len) = match c {
                    b'+' => (Some(Tok::Plus), 1),
                    b'-' => (Some(Tok::Minus), 1),
                    b'*' => (Some(Tok::Star), 1),
                    b'/' => (Some(Tok::Slash), 1),
                    b'%' => (Some(Tok::Percent), 1),
                    b'(' => (Some(Tok::LParen), 1),
                    b')' => (Some(Tok::RParen), 1),
                    b',' => (Some(Tok::Comma), 1),
                    b'=' => (two(b'=', Tok::EqEq), 2),
                    b'&' => (two(b'&', Tok::AndAnd), 2),
                    b'|' => (two(b'|', Tok::OrOr), 2),
                    b'!' => two(b'=', Tok::Ne).map_or((Some(Tok::Bang), 1), |t| (Some(t), 2)),
                    b'<' => two(b'=', Tok::Le).map_or((Some(Tok::Lt), 1), |t| (Some(t), 2)),
                    b'>' => two(b'=', Tok::Ge).map_or((Some(Tok::Gt), 1), |t| (Some(t), 2)),
                    _ => (None, 1),
                };
                let Some(tok) = tok else {
                    let ch = src
                        .get(start..)
                        .and_then(|s| s.chars().next())
                        .unwrap_or('\0');
                    return Err(CompileError::new(
                        Span::new(start, start + ch.len_utf8()),
                        ErrorKind::UnexpectedChar(ch),
                    ));
                };
                i = start + len;
                tok
            }
        };
        out.push(Token {
            tok,
            span: Span::new(start, i),
        });
    }
    out.push(Token {
        tok: Tok::Eof,
        span: Span::new(src.len(), src.len()),
    });
    Ok(out)
}

/// Whether `c` may continue a name segment: `[a-z0-9_]`.
pub(crate) fn is_tail(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'
}

/// The end of the name starting at `start`:
/// `[a-z_][a-z0-9_]*(\.[a-z0-9_]+)*`.
fn name_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    loop {
        while b.get(i).copied().is_some_and(is_tail) {
            i += 1;
        }
        if b.get(i) == Some(&b'.') && b.get(i + 1).copied().is_some_and(is_tail) {
            i += 1;
        } else {
            return i;
        }
    }
}

/// An integer or duration literal starting at `start`: digits, an optional
/// `.digits`, then `ms` or `s` for a duration. Returns the token and its
/// end.
fn number(b: &[u8], start: usize) -> Result<(Tok, usize), CompileError> {
    let mut i = start;
    let mut value: Option<u64> = Some(0);
    while let Some(d) = b.get(i).copied().filter(u8::is_ascii_digit) {
        value = value
            .and_then(|v| v.checked_mul(10))
            .and_then(|v| v.checked_add(u64::from(d - b'0')));
        i += 1;
    }
    let mut frac: Option<(u64, u32)> = None;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let (mut f, mut digits) = (0u64, 0u32);
        while let Some(d) = b.get(i).copied().filter(u8::is_ascii_digit) {
            if digits < 4 {
                f = f * 10 + u64::from(d - b'0');
            }
            digits = digits.saturating_add(1);
            i += 1;
        }
        frac = Some((f, digits));
    }
    let suffix_start = i;
    while b
        .get(i)
        .copied()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
    {
        i += 1;
    }
    let span = Span::new(start, i);
    let bad = |kind| Err(CompileError::new(span, kind));
    let ms = match (b.get(suffix_start..i), frac) {
        (Some(b""), None) => {
            return match value
                .filter(|&v| v <= u64::from(MIN_MAGNITUDE))
                .and_then(|v| u32::try_from(v).ok())
            {
                Some(v) => Ok((Tok::Int(v), i)),
                None => bad(ErrorKind::IntTooLarge),
            };
        }
        (Some(b"ms"), None) => value,
        (Some(b"s"), None) => value.and_then(|v| v.checked_mul(1000)),
        (Some(b"s"), Some((f, digits @ 1..=3))) => {
            let scaled = f * 10u64.pow(3 - digits);
            value
                .and_then(|v| v.checked_mul(1000))
                .and_then(|v| v.checked_add(scaled))
        }
        _ => return bad(ErrorKind::BadDuration),
    };
    match ms.and_then(|v| i32::try_from(v).ok()) {
        Some(v) => Ok((Tok::Duration(v), i)),
        None => bad(ErrorKind::IntTooLarge),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_operators_and_names() {
        assert_eq!(
            toks("a.b_1 <= !c != d && e || f >= g"),
            [
                Tok::Name,
                Tok::Le,
                Tok::Bang,
                Tok::Name,
                Tok::Ne,
                Tok::Name,
                Tok::AndAnd,
                Tok::Name,
                Tok::OrOr,
                Tok::Name,
                Tok::Ge,
                Tok::Name,
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("if then else true false"),
            [
                Tok::If,
                Tok::Then,
                Tok::Else,
                Tok::True,
                Tok::False,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn lexes_durations() {
        assert_eq!(toks("120ms"), [Tok::Duration(120), Tok::Eof]);
        assert_eq!(toks("1.5s"), [Tok::Duration(1500), Tok::Eof]);
        assert_eq!(toks("1.005s"), [Tok::Duration(1005), Tok::Eof]);
        assert_eq!(toks("2s"), [Tok::Duration(2000), Tok::Eof]);
        assert_eq!(toks("2147483647ms"), [Tok::Duration(i32::MAX), Tok::Eof]);
        assert_eq!(toks("2147483648"), [Tok::Int(MIN_MAGNITUDE), Tok::Eof]);
    }
}
