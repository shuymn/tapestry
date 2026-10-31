//! The node arena: every parsed value of one resolution in one `Vec`,
//! linked by first child and next sibling, with byte spans into the
//! retained file text. Nothing is decoded until a field is read.
//!
//! Upstream facts are cited at libGDX tag `gdx-parent-1.9.9` (the reader
//! the source loads skins with) and at the source's `0.8.9` tag
//! (83574048); they describe the format, no code is ported.

use std::str::Chars;

use crate::error::{ImportError, InputLimit, Loc, Syntax};
use crate::limits::MAX_NODES;

/// No node: the end of a sibling chain, an empty container, no key.
pub(crate) const NONE: u32 = u32::MAX;

/// What a node is. `Bare` is an unquoted token: a number, `true`,
/// `false`, `null` or a string, told apart only when it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Object,
    Array,
    Quoted,
    Bare,
}

/// The node is an object member: `key_start..key_end` is its name.
pub(crate) const KEYED: u8 = 1;
/// The name holds escapes.
pub(crate) const KEY_ESCAPED: u8 = 2;
/// The value (a string or an unquoted token) holds escapes.
pub(crate) const ESCAPED: u8 = 4;

/// One parsed value. Spans are byte offsets into its file's text: a
/// string's or a token's content, or a container from its opening bracket
/// to just past its closing one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Node {
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) key_start: u32,
    pub(crate) key_end: u32,
    pub(crate) first_child: u32,
    pub(crate) next_sibling: u32,
    pub(crate) file: u16,
    pub(crate) kind: Kind,
    pub(crate) flags: u8,
}

/// A definition file as read: its normalised name, the folder its
/// includes resolve from, and its bytes.
pub(crate) struct Text {
    pub(crate) name: String,
    pub(crate) dir: String,
    pub(crate) bytes: Box<[u8]>,
}

/// The parsed files of one resolution and their nodes.
pub(crate) struct Arena {
    pub(crate) nodes: Vec<Node>,
    pub(crate) files: Vec<Text>,
}

/// A value as a field reads it, following the pinned reader's typing:
/// only unquoted tokens are numbers, booleans or null
/// (JsonReader.rl:117-176).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scalar<'a> {
    Object,
    Array,
    Null,
    Bool(bool),
    /// A token of digits and signs that parses as an `i64`.
    Long(&'a str),
    /// A token of digits, signs, `.`, `e` and `E` with at least one of
    /// the last three, that parses as a double.
    Double(&'a str),
    /// A quoted string, or a token that is none of the above.
    Str {
        raw: &'a str,
        escaped: bool,
    },
}

impl Scalar<'_> {
    /// Whether the pinned reader made this a number node.
    pub(crate) fn is_number(self) -> bool {
        matches!(self, Self::Long(_) | Self::Double(_))
    }
}

/// The text of `bytes[start..end]`. The whole file was validated as
/// UTF-8 and spans fall on character boundaries.
fn text(bytes: &[u8], start: u32, end: u32) -> &str {
    let slice = bytes.get(start as usize..end as usize).unwrap_or_default();
    std::str::from_utf8(slice).unwrap_or_default()
}

/// The characters of a string as written, with escapes resolved. Only
/// validated strings reach it ([`check_escapes`]); an impossible escape
/// decodes as U+FFFD rather than panicking.
pub(crate) struct Unescape<'a> {
    chars: Chars<'a>,
    escaped: bool,
}

impl<'a> Unescape<'a> {
    pub(crate) fn new(raw: &'a str, escaped: bool) -> Self {
        Self {
            chars: raw.chars(),
            escaped,
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0;
        for _ in 0..4 {
            v = v * 16 + self.chars.next()?.to_digit(16)?;
        }
        Some(v)
    }
}

impl Iterator for Unescape<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if !self.escaped || c != '\\' {
            return Some(c);
        }
        // The escapes of JsonReader.rl:398-438.
        Some(match self.chars.next()? {
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                let hi = self.hex4().unwrap_or(0xFFFD);
                if (0xD800..0xDC00).contains(&hi) {
                    self.chars.next();
                    self.chars.next();
                    let lo = self.hex4().unwrap_or(0xDC00);
                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00)))
                        .unwrap_or('\u{FFFD}')
                } else {
                    char::from_u32(hi).unwrap_or('\u{FFFD}')
                }
            }
            other => other,
        })
    }
}

/// Checks every escape in `raw`: a backslash followed by one of
/// `"\/bfnrt`, or by `u` and four hex digits, a high surrogate paired with
/// a `\u` low one. Returns the offset of the first bad escape.
pub(crate) fn check_escapes(raw: &[u8]) -> Result<(), (usize, Syntax)> {
    let hex4 = |at: usize| -> Option<u32> {
        let digits = raw.get(at..at + 4)?;
        let s = std::str::from_utf8(digits).ok()?;
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(s, 16).ok()
    };
    let mut i = 0;
    while let Some(&b) = raw.get(i) {
        if b != b'\\' {
            i += 1;
            continue;
        }
        match raw.get(i + 1) {
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
            Some(b'u') => {
                let unit = hex4(i + 2).ok_or((i, Syntax::InvalidEscape))?;
                if (0xDC00..0xE000).contains(&unit) {
                    return Err((i, Syntax::LoneSurrogate));
                }
                if (0xD800..0xDC00).contains(&unit) {
                    let low = (raw.get(i + 6..i + 8) == Some(b"\\u"))
                        .then(|| hex4(i + 8))
                        .flatten();
                    if !low.is_some_and(|l| (0xDC00..0xE000).contains(&l)) {
                        return Err((i, Syntax::LoneSurrogate));
                    }
                    i += 12;
                } else {
                    i += 6;
                }
            }
            _ => return Err((i, Syntax::InvalidEscape)),
        }
    }
    Ok(())
}

impl Arena {
    /// An arena with room for `estimate` nodes, at most [`MAX_NODES`].
    pub(crate) fn with_estimate(estimate: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(estimate.min(MAX_NODES)),
            files: Vec::new(),
        }
    }

    pub(crate) fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    fn bytes(&self, node: &Node) -> &[u8] {
        &self.files[usize::from(node.file)].bytes
    }

    /// The node's text as written: a string's content, a token.
    pub(crate) fn raw(&self, id: u32) -> &str {
        let n = self.node(id);
        text(self.bytes(n), n.start, n.end)
    }

    /// The member name's characters, decoded.
    fn key(&self, id: u32) -> Option<Unescape<'_>> {
        let n = self.node(id);
        (n.flags & KEYED != 0).then(|| {
            Unescape::new(
                text(self.bytes(n), n.key_start, n.key_end),
                n.flags & KEY_ESCAPED != 0,
            )
        })
    }

    /// The children of a container, in order.
    pub(crate) fn children(&self, id: u32) -> Children<'_> {
        Children {
            arena: self,
            next: self.node(id).first_child,
        }
    }

    /// The last member of object `id` named exactly `name`: an ordinary
    /// field, assigned in order so the last one wins (Json.java:799-803).
    pub(crate) fn last_member(&self, id: u32, name: &str) -> Option<u32> {
        self.children(id)
            .filter(|&c| self.key(c).is_some_and(|k| k.eq(name.chars())))
            .last()
    }

    /// The first member of object `id` whose name equals `name` ignoring
    /// ASCII case: a wrapper lookup, as `JsonValue.get` does
    /// (JsonValue.java:95-99). Java compares Unicode-aware; matching ASCII
    /// case only is an intentional subset.
    pub(crate) fn first_member_ci(&self, id: u32, name: &str) -> Option<u32> {
        if self.node(id).kind != Kind::Object {
            return None;
        }
        self.children(id).find(|&c| {
            self.key(c).is_some_and(|k| {
                k.map(|c| c.to_ascii_lowercase())
                    .eq(name.chars().map(|c| c.to_ascii_lowercase()))
            })
        })
    }

    /// The value as a field reads it.
    pub(crate) fn scalar(&self, id: u32) -> Scalar<'_> {
        let n = *self.node(id);
        let escaped = n.flags & ESCAPED != 0;
        let raw = self.raw(id);
        match n.kind {
            Kind::Object => Scalar::Object,
            Kind::Array => Scalar::Array,
            Kind::Quoted => Scalar::Str { raw, escaped },
            Kind::Bare => {
                // `true`, `false` and `null` are compared after unescaping
                // (JsonReader.rl:118-128); the number check runs on the
                // token as written (JsonReader.rl:130-176).
                let is = |word: &str| Unescape::new(raw, escaped).eq(word.chars());
                if is("true") {
                    return Scalar::Bool(true);
                }
                if is("false") {
                    return Scalar::Bool(false);
                }
                if is("null") {
                    return Scalar::Null;
                }
                let long = raw
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'+' || b == b'-');
                let double = !long
                    && raw
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b));
                if long && raw.parse::<i64>().is_ok() {
                    Scalar::Long(raw)
                } else if double && raw.parse::<f64>().is_ok() {
                    Scalar::Double(raw)
                } else {
                    Scalar::Str { raw, escaped }
                }
            }
        }
    }

    /// Where node `id` starts.
    pub(crate) fn loc(&self, id: u32) -> Loc {
        let n = self.node(id);
        self.loc_at(n.file, n.start)
    }

    /// Line and column of byte `offset` of file `file`, counted only now:
    /// lines by `\n`, columns in characters, a leading byte order mark not
    /// counted.
    pub(crate) fn loc_at(&self, file: u16, offset: u32) -> Loc {
        let t = &self.files[usize::from(file)];
        let before = t.bytes.get(..offset as usize).unwrap_or(&t.bytes);
        let line_start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let mut col = before[line_start..]
            .iter()
            .filter(|&&b| b & 0xC0 != 0x80)
            .count();
        if line_start == 0 && before.starts_with(BOM) {
            col -= 1;
        }
        let line = before.iter().filter(|&&b| b == b'\n').count();
        Loc {
            file: t.name.clone(),
            line: u32::try_from(line + 1).unwrap_or(u32::MAX),
            col: u32::try_from(col + 1).unwrap_or(u32::MAX),
        }
    }

    pub(crate) fn syntax(&self, file: u16, offset: usize, what: Syntax) -> ImportError {
        ImportError::Syntax {
            at: self.loc_at(file, u32::try_from(offset).unwrap_or(u32::MAX)),
            what,
        }
    }

    pub(crate) fn input(&self, file: u16, offset: usize, limit: InputLimit) -> ImportError {
        ImportError::Input {
            at: Some(self.loc_at(file, u32::try_from(offset).unwrap_or(u32::MAX))),
            limit,
        }
    }
}

/// A UTF-8 byte order mark, skipped before parsing (an intentional
/// policy: the pinned reader would see U+FEFF and fail).
pub(crate) const BOM: &[u8] = b"\xEF\xBB\xBF";

/// The children of a container, by following next-sibling links.
pub(crate) struct Children<'a> {
    arena: &'a Arena,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.arena.node(id).next_sibling;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, check_escapes};
    use crate::error::Syntax;

    #[test]
    fn a_node_is_28_bytes() {
        assert_eq!(size_of::<Node>(), 28);
    }

    /// `\u` and four hex digits, spelled so that no tool reads it as an
    /// escape.
    fn u(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    #[test]
    fn escapes_are_checked() {
        let pair = format!("{}{}", u("D83D"), u("DE00"));
        for good in [
            r#"a\"b\\c\/d\b\f\n\r\t"#.to_owned(),
            u("0041"),
            u("00e9"),
            pair,
        ] {
            assert_eq!(check_escapes(good.as_bytes()), Ok(()), "{good}");
        }
        for (bad, at, what) in [
            (r"ab\x".to_owned(), 2, Syntax::InvalidEscape),
            (u("12"), 0, Syntax::InvalidEscape),
            (u("12G4"), 0, Syntax::InvalidEscape),
            (u("+123"), 0, Syntax::InvalidEscape),
            (r"a\".to_owned(), 1, Syntax::InvalidEscape),
            (u("DE00"), 0, Syntax::LoneSurrogate),
            (format!("{}x", u("D83D")), 0, Syntax::LoneSurrogate),
            (
                format!("{}{}", u("D83D"), u("0041")),
                0,
                Syntax::LoneSurrogate,
            ),
        ] {
            assert_eq!(check_escapes(bad.as_bytes()), Err((at, what)), "{bad}");
        }
    }
}
