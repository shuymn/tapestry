//! The reader: one file's bytes into arena nodes.
//!
//! It accepts the grammar of the pinned libGDX 1.9.9 reader
//! (`gdx/src/com/badlogic/gdx/utils/JsonReader.rl:291-302` at tag
//! `gdx-parent-1.9.9`), read as a format description:
//!
//! - whitespace is space, tab, CR and LF, and `//` and `/* */` comments
//!   (`ws`, rl:292); after a value on the same line only space, tab and
//!   comments (`ws2`, rl:293);
//! - members and items are separated by a comma, or by a line break with
//!   an optional comma after it (`comma`, rl:294); a block comment's line
//!   breaks separate nothing, and two values on one line with no comma
//!   between them (`}{`) are refused;
//! - one comma may open a container (`[,1]`) or close it (`[1,]`), but a
//!   comma on the line after a value may not (`[1\n,]` is refused);
//! - names and values may be unquoted (rl:296-297). An unquoted name ends
//!   at `:`, a line break or a comment; an unquoted value at `}`, `]`, `,`,
//!   a line break or a comment (rl:216-268). Inner spaces are kept and
//!   trailing space, tab and form feed are trimmed;
//! - a quoted string ends at the first `"` not after a backslash and may
//!   span lines (rl:270-289). Escapes are those of rl:398-438.
//!
//! Intentional policies beyond the grammar: a UTF-8 byte order mark is
//! skipped; the file must be UTF-8, checked before parsing; every escape,
//! in strings and unquoted tokens alike, is checked while parsing and a
//! `\u` escape must be four hex digits, half a surrogate pair refused; a
//! value may not start with `}`, where the pinned reader produces an empty
//! string or fails depending on the spacing before it.
//!
//! Every bound is checked before its node exists: the nesting depth before
//! a container opens, the string length before the node is inserted, the
//! node count before each insertion.

use crate::error::{ImportError, InputLimit, Syntax};
use crate::limits::{MAX_DEPTH, MAX_NODES, MAX_STRING_BYTES};
use crate::tree::{Arena, BOM, ESCAPED, KEY_ESCAPED, KEYED, Kind, NONE, Node, check_escapes};

/// Why parsing stopped, at a byte offset.
enum Fail {
    Syntax(usize, Syntax),
    Limit(usize, InputLimit),
}

/// Where a container is between its members or items.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// Just opened: a value, a comma or the close may follow.
    Start,
    /// After a comma: a value or the close.
    Comma,
    /// After a value: space and comments on the line, then a comma, a
    /// line break or the close.
    Item,
    /// After a line break that follows a value: a comma, a value or the
    /// close.
    Newline,
    /// After a line break and a comma: a value.
    NewlineComma,
}

/// An open container: its node, its last child so far (to link the next
/// one in O(1)) and where it is.
struct Frame {
    node: u32,
    last: u32,
    object: bool,
    sep: Sep,
}

/// A string's or token's content span and whether it holds escapes.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    escaped: bool,
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    nodes: &'a mut Vec<Node>,
    file: u16,
    base_depth: u32,
}

/// Parses file `file` of `arena` into its nodes and returns the root's id.
/// `base_depth` is the depth of the value the file stands for (an include
/// sits at the depth of the object that names it); the file's root is one
/// deeper.
pub(crate) fn parse(arena: &mut Arena, file: u16, base_depth: u32) -> Result<u32, ImportError> {
    let Arena { nodes, files } = arena;
    let b = &files[usize::from(file)].bytes;
    let result = match std::str::from_utf8(b) {
        Err(e) => Err(Fail::Syntax(e.valid_up_to(), Syntax::InvalidUtf8)),
        Ok(_) => Parser {
            b,
            pos: if b.starts_with(BOM) { BOM.len() } else { 0 },
            nodes,
            file,
            base_depth,
        }
        .document(),
    };
    result.map_err(|fail| match fail {
        Fail::Syntax(at, what) => arena.syntax(file, at, what),
        Fail::Limit(at, limit) => arena.input(file, at, limit),
    })
}

/// Whether `c` may start an unquoted name (rl:296).
fn name_start(c: u8) -> bool {
    !b"\":,}/\r\n\t ".contains(&c)
}

/// Whether `c` may start an unquoted value (rl:297), and is not `}`.
fn value_start(c: u8) -> bool {
    !b"\":,{[]}/\r\n\t ".contains(&c)
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    /// `main := ws* value ws*` (rl:302).
    fn document(&mut self) -> Result<u32, Fail> {
        self.skip_ws(true)?;
        let mut stack: Vec<Frame> = Vec::with_capacity(MAX_DEPTH as usize);
        let root = self.value(None, &mut stack)?;
        while let Some(top) = stack.last_mut() {
            let sep = top.sep;
            let object = top.object;
            let close = if object { b'}' } else { b']' };
            if sep == Sep::Item {
                self.skip_ws(false)?;
            } else {
                self.skip_ws(true)?;
            }
            let Some(c) = self.peek() else {
                return Err(Fail::Syntax(self.pos, Syntax::UnexpectedEnd));
            };
            match (sep, c) {
                (Sep::Item, b',') | (Sep::Start, b',') => {
                    top.sep = Sep::Comma;
                    self.pos += 1;
                }
                (Sep::Item, b'\r' | b'\n') => {
                    top.sep = Sep::Newline;
                    self.pos += 1;
                }
                (Sep::Newline, b',') => {
                    top.sep = Sep::NewlineComma;
                    self.pos += 1;
                }
                // In an object only `}` closes; a `]` where a name may
                // start begins an unquoted name (rl:216-268).
                (Sep::Start | Sep::Comma | Sep::Newline | Sep::NewlineComma, b']') if object => {
                    top.sep = Sep::Item;
                    let key = self.name()?;
                    self.value(Some(key), &mut stack)?;
                }
                (Sep::Start | Sep::Comma | Sep::Item | Sep::Newline, b'}' | b']') => {
                    if c != close {
                        return Err(Fail::Syntax(self.pos, Syntax::MismatchedClose));
                    }
                    let node = top.node;
                    self.pos += 1;
                    self.nodes[node as usize].end = offset(self.pos);
                    stack.pop();
                }
                (Sep::Item, _) => return Err(Fail::Syntax(self.pos, Syntax::ExpectedSeparator)),
                (Sep::Comma | Sep::NewlineComma, b',' | b'}' | b']') => {
                    return Err(Fail::Syntax(self.pos, Syntax::Unexpected));
                }
                _ => {
                    top.sep = Sep::Item;
                    let key = if object { Some(self.name()?) } else { None };
                    self.value(key, &mut stack)?;
                }
            }
        }
        self.skip_ws(true)?;
        if self.pos < self.b.len() {
            return Err(Fail::Syntax(self.pos, Syntax::TrailingContent));
        }
        Ok(root)
    }

    /// Skips whitespace and comments; line breaks too when `newlines`.
    fn skip_ws(&mut self, newlines: bool) -> Result<(), Fail> {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' => self.pos += 1,
                b'\r' | b'\n' if newlines => self.pos += 1,
                b'/' => match self.at(self.pos + 1) {
                    // A line comment stops before its line break, which
                    // then separates (rl:203-214).
                    Some(b'/') => {
                        self.pos = self.b[self.pos..]
                            .iter()
                            .position(|&b| b == b'\n')
                            .map_or(self.b.len(), |i| self.pos + i);
                    }
                    Some(b'*') => {
                        let close = self.b[self.pos + 2..]
                            .windows(2)
                            .position(|w| w == b"*/")
                            .ok_or(Fail::Syntax(self.pos, Syntax::UnterminatedComment))?;
                        self.pos += 2 + close + 2;
                    }
                    _ => return Err(Fail::Syntax(self.pos, Syntax::Unexpected)),
                },
                _ => break,
            }
        }
        Ok(())
    }

    /// `nameString ws* ':' ws*`.
    fn name(&mut self) -> Result<Span, Fail> {
        let span = match self.peek() {
            None => return Err(Fail::Syntax(self.pos, Syntax::UnexpectedEnd)),
            Some(b'"') => self.quoted()?,
            Some(c) if name_start(c) => self.bare(true)?,
            Some(_) => return Err(Fail::Syntax(self.pos, Syntax::Unexpected)),
        };
        self.skip_ws(true)?;
        match self.peek() {
            Some(b':') => self.pos += 1,
            None => return Err(Fail::Syntax(self.pos, Syntax::UnexpectedEnd)),
            Some(_) => return Err(Fail::Syntax(self.pos, Syntax::ExpectedColon)),
        }
        self.skip_ws(true)?;
        Ok(span)
    }

    /// A value: a container opens a frame; a string or token is one node.
    /// Linked as the next child of the top frame, if any.
    fn value(&mut self, key: Option<Span>, stack: &mut Vec<Frame>) -> Result<u32, Fail> {
        let start = self.pos;
        let (kind, span) = match self.peek() {
            None => return Err(Fail::Syntax(self.pos, Syntax::UnexpectedEnd)),
            Some(c @ (b'{' | b'[')) => {
                let depth = self.base_depth as usize + stack.len() + 1;
                if depth > MAX_DEPTH as usize {
                    return Err(Fail::Limit(start, InputLimit::Depth));
                }
                self.pos += 1;
                let kind = if c == b'{' { Kind::Object } else { Kind::Array };
                (
                    kind,
                    Span {
                        start,
                        end: start,
                        escaped: false,
                    },
                )
            }
            Some(b'"') => (Kind::Quoted, self.quoted()?),
            Some(c) if value_start(c) => (Kind::Bare, self.bare(false)?),
            Some(_) => return Err(Fail::Syntax(self.pos, Syntax::Unexpected)),
        };
        let mut flags = if span.escaped { ESCAPED } else { 0 };
        let (key_start, key_end) = match key {
            Some(k) => {
                flags |= KEYED;
                if k.escaped {
                    flags |= KEY_ESCAPED;
                }
                (offset(k.start), offset(k.end))
            }
            None => (0, 0),
        };
        let id = self.push(
            start,
            Node {
                start: offset(span.start),
                end: offset(span.end),
                key_start,
                key_end,
                first_child: NONE,
                next_sibling: NONE,
                file: self.file,
                kind,
                flags,
            },
        )?;
        if let Some(parent) = stack.last_mut() {
            if parent.last == NONE {
                self.nodes[parent.node as usize].first_child = id;
            } else {
                self.nodes[parent.last as usize].next_sibling = id;
            }
            parent.last = id;
        }
        if matches!(kind, Kind::Object | Kind::Array) {
            stack.push(Frame {
                node: id,
                last: NONE,
                object: kind == Kind::Object,
                sep: Sep::Start,
            });
        }
        Ok(id)
    }

    /// Inserts a node after checking the count; grows the arena exactly,
    /// doubling up to [`MAX_NODES`] and never past it.
    fn push(&mut self, at: usize, node: Node) -> Result<u32, Fail> {
        let len = self.nodes.len();
        if len >= MAX_NODES {
            return Err(Fail::Limit(at, InputLimit::Nodes));
        }
        if len == self.nodes.capacity() {
            self.nodes
                .reserve_exact(len.max(1_024).min(MAX_NODES - len));
        }
        self.nodes.push(node);
        Ok(u32::try_from(len).unwrap_or(NONE))
    }

    /// A string's or token's length and escapes, checked before its node
    /// exists.
    fn checked(&self, span: Span) -> Result<Span, Fail> {
        if span.end - span.start > MAX_STRING_BYTES {
            return Err(Fail::Limit(span.start, InputLimit::StringBytes));
        }
        if span.escaped {
            check_escapes(&self.b[span.start..span.end])
                .map_err(|(i, what)| Fail::Syntax(span.start + i, what))?;
        }
        Ok(span)
    }

    /// A quoted string from its opening quote; its content (rl:270-289).
    fn quoted(&mut self) -> Result<Span, Fail> {
        let open = self.pos;
        let mut i = open + 1;
        let mut escaped = false;
        loop {
            match self.at(i) {
                None => return Err(Fail::Syntax(open, Syntax::UnterminatedString)),
                Some(b'"') => break,
                Some(b'\\') => {
                    escaped = true;
                    i += 2;
                }
                Some(_) => i += 1,
            }
        }
        self.pos = i + 1;
        self.checked(Span {
            start: open + 1,
            end: i,
            escaped,
        })
    }

    /// An unquoted name or value from its first character (rl:216-268).
    /// A backslash marks escapes but does not hide the next character.
    fn bare(&mut self, name: bool) -> Result<Span, Fail> {
        let start = self.pos;
        let mut i = start;
        let mut escaped = false;
        while let Some(c) = self.at(i) {
            let stop = match c {
                b'\\' => {
                    escaped = true;
                    false
                }
                b'/' => matches!(self.at(i + 1), Some(b'/' | b'*')),
                b'\r' | b'\n' => true,
                b':' => name,
                b'}' | b']' | b',' => !name,
                _ => false,
            };
            if stop {
                break;
            }
            i += 1;
        }
        // `Character.isSpace`: space, tab, LF, FF and CR (rl:267).
        while i > start && matches!(self.b[i - 1], b' ' | b'\t' | b'\n' | 0x0C | b'\r') {
            i -= 1;
        }
        self.pos = i;
        self.checked(Span {
            start,
            end: i,
            escaped,
        })
    }
}

#[cfg(test)]
mod tests;
