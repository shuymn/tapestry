//! The node arena: every parsed value of one resolution in one `Vec`,
//! linked by first child and next sibling, with byte spans into the
//! retained file text. Nothing is decoded until a field is read.
//!
//! Upstream facts are cited at libGDX tag `gdx-parent-1.9.9` (the reader
//! the source loads skins with) and at the source's `0.8.9` tag
//! (83574048); they describe the format, no code is ported.

use std::str::Chars;

use crate::error::{ImportError, InputLimit, Loc, Syntax};

/// No node: the end of a sibling chain, an empty container, no key.
pub(crate) const NONE: u32 = u32::MAX;

/// What a node is. `Bare` is an unquoted token: a number, `true`,
/// `false`, `null` or a string, told apart only when it is read. `Lua` is
/// a scalar a Lua skin's returned table held, typed as Lua typed it: its
/// value is [`LuaText::values`] entry `end` ([`crate::lua`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Object,
    Array,
    Quoted,
    Bare,
    Lua,
    /// An empty Lua table: no members and no items, which an array reader
    /// and an object reader each read as their own empty value.
    Empty,
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

/// The parsed files of one resolution and their nodes. A Lua skin's
/// converted table is file 0, with `lua` set: its nodes come first, so a
/// node's id is its ordinal, held in its `start`.
pub(crate) struct Arena {
    pub(crate) nodes: Vec<Node>,
    pub(crate) files: Vec<Text>,
    pub(crate) lua: Option<Box<LuaText>>,
}

/// The typed values and provenance of a Lua skin's converted table
/// (file 0 of its arena): its keys and strings are spans of that file's
/// bytes, which hold no other text.
#[derive(Debug, Default)]
pub(crate) struct LuaText {
    /// The scalars, by a `Lua` node's `end`.
    pub(crate) values: Vec<LuaScalar>,
    /// The Lua files the passes loaded, the entry first: the sources of
    /// the report and of a callable's location.
    pub(crate) modules: Vec<String>,
    /// The functions the table held, each instance once.
    pub(crate) callables: Vec<Callable>,
}

/// A Lua scalar, its subtype and exact value kept: an integer as the
/// `i64` Lua holds, a float as its exact binary value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LuaScalar {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A string, by its span of the file's bytes.
    Str {
        start: u32,
        end: u32,
    },
    /// A function, by its index in [`LuaText::callables`].
    Callable(u32),
}

/// A function a Lua skin's table held: where it was defined, and what it
/// is. It is never run; the importer refuses it ([`crate::lua`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Callable {
    /// The module that defined it, an index of [`LuaText::modules`];
    /// `None` for a host or library function.
    pub(crate) module: Option<u16>,
    /// The line its definition starts on, 0 for a host function.
    pub(crate) line: u32,
    /// What it is.
    pub(crate) kind: CallableKind,
}

/// What a function a Lua skin's table held is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableKind {
    /// A function the skin's Lua defined.
    Lua,
    /// A host module's API function, by its index in
    /// [`crate::lua::HOST_APIS`].
    Host(u16),
    /// A library function (`string`, `table`, `math` or a base function).
    Library,
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
    /// A Lua integer, exactly.
    Int(i64),
    /// A Lua float, its exact binary value.
    Float(f64),
    /// A Lua function.
    Callable,
}

impl Scalar<'_> {
    /// Whether the pinned reader made this a number node, or Lua held a
    /// number.
    pub(crate) fn is_number(self) -> bool {
        matches!(
            self,
            Self::Long(_) | Self::Double(_) | Self::Int(_) | Self::Float(_)
        )
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
    /// An empty arena; the parser grows it, capped at
    /// [`crate::limits::MAX_NODES`].
    pub(crate) fn new() -> Self {
        Self {
            nodes: Vec::new(),
            files: Vec::new(),
            lua: None,
        }
    }

    /// The Lua values of `file`, when it is a converted Lua table.
    fn lua_of(&self, file: u16) -> Option<&LuaText> {
        self.lua.as_deref().filter(|_| file == 0)
    }

    /// The Lua scalar of node `n`, when it is one.
    fn lua_scalar(&self, n: &Node) -> Option<LuaScalar> {
        if n.kind != Kind::Lua {
            return None;
        }
        self.lua_of(n.file)?.values.get(n.end as usize).copied()
    }

    /// The location of node `offset` of a converted Lua table, as an index
    /// of [`Self::sources`], a line and a column: a callable's definition,
    /// its module and line; any other value, the entry with line 0 and its
    /// ordinal in the table as the column (a Lua table has no source
    /// position).
    pub(crate) fn lua_loc(&self, file: u16, offset: u32) -> Option<(u16, u32, u32)> {
        let lua = self.lua_of(file)?;
        let n = self.nodes.get(offset as usize)?;
        if let Some(LuaScalar::Callable(i)) = self.lua_scalar(n)
            && let Some(c) = lua.callables.get(i as usize)
            && let Some(module) = c.module
        {
            return Some((module, c.line, 0));
        }
        Some((0, 0, offset.saturating_add(1)))
    }

    /// The names of the sources locations index: the files, or for a Lua
    /// skin its modules and then the JSON files its table includes.
    pub(crate) fn sources(&self) -> Vec<String> {
        match &self.lua {
            Some(lua) => lua
                .modules
                .iter()
                .cloned()
                .chain(self.files.iter().skip(1).map(|f| f.name.clone()))
                .collect(),
            None => self.files.iter().map(|f| f.name.clone()).collect(),
        }
    }

    /// The index in [`Self::sources`] of file `file`, a JSON file.
    pub(crate) fn source_of(&self, file: u16) -> u16 {
        match &self.lua {
            Some(lua) if file > 0 => {
                let shift = u16::try_from(lua.modules.len()).unwrap_or(u16::MAX);
                file.saturating_add(shift).saturating_sub(1)
            }
            _ => file,
        }
    }

    pub(crate) fn node(&self, id: u32) -> &Node {
        &self.nodes[id as usize]
    }

    fn bytes(&self, node: &Node) -> &[u8] {
        &self.files[usize::from(node.file)].bytes
    }

    /// The node's text as written: a string's content, a token; a Lua
    /// string's bytes and a Lua boolean's word. A Lua number or function
    /// has no text: it is read by its value ([`Self::scalar`]).
    pub(crate) fn raw(&self, id: u32) -> &str {
        let n = self.node(id);
        match self.lua_scalar(n) {
            Some(LuaScalar::Str { start, end }) => text(self.bytes(n), start, end),
            Some(LuaScalar::Bool(true)) => "true",
            Some(LuaScalar::Bool(false)) => "false",
            Some(_) => "",
            None => text(self.bytes(n), n.start, n.end),
        }
    }

    /// The member name's characters, decoded.
    pub(crate) fn key(&self, id: u32) -> Option<Unescape<'_>> {
        let n = self.node(id);
        (n.flags & KEYED != 0).then(|| {
            Unescape::new(
                text(self.bytes(n), n.key_start, n.key_end),
                n.flags & KEY_ESCAPED != 0,
            )
        })
    }

    /// Whether node `id` is read as an array: a JSON array, or an empty
    /// Lua table.
    pub(crate) fn is_array(&self, id: u32) -> bool {
        matches!(self.node(id).kind, Kind::Array | Kind::Empty)
    }

    /// Whether node `id` is read as an object: a JSON object, or an empty
    /// Lua table.
    pub(crate) fn is_object(&self, id: u32) -> bool {
        matches!(self.node(id).kind, Kind::Object | Kind::Empty)
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
        if !self.is_object(id) {
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
        match n.kind {
            Kind::Object | Kind::Empty => Scalar::Object,
            Kind::Array => Scalar::Array,
            Kind::Quoted => Scalar::Str {
                raw: self.raw(id),
                escaped,
            },
            Kind::Lua => match self.lua_scalar(&n) {
                Some(LuaScalar::Int(v)) => Scalar::Int(v),
                Some(LuaScalar::Float(v)) => Scalar::Float(v),
                Some(LuaScalar::Bool(b)) => Scalar::Bool(b),
                Some(LuaScalar::Str { .. }) => Scalar::Str {
                    raw: self.raw(id),
                    escaped: false,
                },
                Some(LuaScalar::Callable(_)) => Scalar::Callable,
                None => Scalar::Null,
            },
            Kind::Bare => {
                let raw = self.raw(id);
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
        #[cfg(test)]
        RESCANS.with(|n| n.set(n.get() + 1));
        if let Some((module, line, col)) = self.lua_loc(file, offset) {
            let modules = self.lua.as_deref().map_or(&[][..], |l| &l.modules[..]);
            return Loc {
                file: modules
                    .get(usize::from(module))
                    .cloned()
                    .unwrap_or_default(),
                line,
                col,
            };
        }
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

#[cfg(test)]
thread_local! {
    /// How many locations this thread has built, each a rescan of its
    /// file: successful resolution must build none.
    pub(crate) static RESCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

    #[test]
    fn escapes_are_checked() {
        for good in [
            r#"a\"b\\c\/d\b\f\n\r\t"#,
            r"\u0041",
            r"\u00e9",
            r"\uD83D\uDE00",
        ] {
            assert_eq!(check_escapes(good.as_bytes()), Ok(()), "{good}");
        }
        for (bad, at, what) in [
            (r"ab\x", 2, Syntax::InvalidEscape),
            (r"\u12", 0, Syntax::InvalidEscape),
            (r"\u12G4", 0, Syntax::InvalidEscape),
            (r"\u+123", 0, Syntax::InvalidEscape),
            (r"a\", 1, Syntax::InvalidEscape),
            (r"\uDE00", 0, Syntax::LoneSurrogate),
            (r"\uD83Dx", 0, Syntax::LoneSurrogate),
            (r"\uD83D\u0041", 0, Syntax::LoneSurrogate),
        ] {
            assert_eq!(check_escapes(bad.as_bytes()), Err((at, what)), "{bad}");
        }
    }
}
