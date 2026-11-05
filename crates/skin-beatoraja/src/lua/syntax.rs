//! The importer's own Lua 5.4 lexer and parser (design.md "beatoraja Lua
//! Skins", per-frame closures): the whole syntax, into an arena with each
//! node's line, lexical scopes resolved as it parses, and each function
//! constructor recorded with its byte span, so that the configured pass
//! can tag every closure before it runs ([`super::instrument`]) and the
//! callbacks are evaluated on the tree ([`super::residual`]), never by
//! matching source text.
//!
//! Every name is resolved where it is read: to a local declaration (a
//! `local`, a parameter, `self`, a loop variable), to the chunk's own
//! `_ENV` cell for a bare `_ENV` no local shadows, or to a global, read
//! through the `_ENV` in scope. A declaration records whether an
//! assignment after its own targets it, in any function. A function lists
//! the declarations of enclosing functions it reads directly, its
//! upvalues, `_ENV` among them when it reads a global, and its static
//! access paths: each chain of indexes by constant strings from one of
//! its upvalues or a global, the entries its captures copy. Every
//! statement keeps its parts, a float numeral its value, and an integer
//! numeral written past 2^53 is recorded as wide
//! ([`Ast::wide_numerals`]); each `local d = X.timer_off_value` with one
//! name and one expression is recorded for the tagger's marker
//! ([`Ast::marks`]).
//!
//! Bounds, each charged before its work: [`MAX_AST_NODES`] nodes
//! (expressions, statements, declarations, functions, path steps and list
//! entries)
//! across a candidate's files, and [`MAX_AST_DEPTH`] levels of statements
//! and subexpressions, checked before recursing, as Lua's own parser
//! counts them (`LUAI_MAXCCALLS`); cancellation is seen every
//! [`CHECK_EVERY`] nodes. Besides its arena, the parser keeps a shared
//! scratch stack and, for the scopes open at the current position, a
//! stack of declarations, a map from each name to its declarations and
//! each open function's upvalue set.

use std::collections::{HashMap, HashSet};

use super::limits::{MAX_AST_DEPTH, MAX_AST_NODES};

/// How many nodes are added between two looks at the cancellation
/// request.
const CHECK_EVERY: u32 = 4_096;

/// No node.
pub(crate) const NONE: u32 = u32::MAX;

/// The prefix of the tagger's name ([`super::instrument`]).
pub(crate) const TAG_PREFIX: &[u8] = b"tapestry_tag";

/// The `_ENV` a chunk's functions start with: the chunk's own upvalue,
/// the pass's globals.
pub(crate) const CHUNK_ENV: u32 = u32::MAX - 1;

/// The most upvalues a function records for its capture reader: a
/// function past it gets no reader, so none of its callbacks is lowered.
pub(crate) const MAX_READER: usize = 200;

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bin {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Un {
    Neg,
    Not,
    Len,
    BNot,
}

/// A list of ids: `(start, len)` in [`Ast::lists`].
pub(crate) type List = (u32, u32);

/// An expression.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Expr {
    Nil,
    True,
    False,
    Vararg,
    /// An integer numeral.
    Int(i64),
    /// A float numeral, by its value's bits.
    Float(u64),
    /// A string literal, by its index in [`Ast::strs`].
    Str(u32),
    /// A function constructor, by its index in [`Ast::funcs`].
    Func(u32),
    /// A local declaration's value, or [`CHUNK_ENV`] for a bare `_ENV`
    /// no local `_ENV` shadows: the chunk's own `_ENV` cell.
    Local(u32),
    /// A global, by its name's index in [`Ast::strs`], read through `env`:
    /// a declaration named `_ENV`, or [`CHUNK_ENV`].
    Global {
        name: u32,
        env: u32,
    },
    Index {
        obj: u32,
        key: u32,
    },
    Call {
        callee: u32,
        args: List,
    },
    Method {
        obj: u32,
        name: u32,
        args: List,
    },
    Binary(Bin, u32, u32),
    Unary(Un, u32),
    Paren(u32),
    /// A table constructor: key and value pairs, the key [`NONE`] for a
    /// positional item.
    Table(List),
}

/// A statement, with its parts: a block is a list of statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stmt {
    /// `local` names (declarations) `=` expressions.
    Local {
        names: List,
        exprs: List,
    },
    /// `return` expressions.
    Return(List),
    /// `;`.
    Empty,
    /// Targets (each a local, a global or an index) `=` expressions.
    Assign {
        targets: List,
        exprs: List,
    },
    /// A call statement: its call or method call expression.
    Call(u32),
    /// `if`: three ids an arm, its condition and its block's list (start,
    /// length), the `else` arm's condition [`NONE`].
    If(List),
    While {
        cond: u32,
        body: List,
    },
    Repeat {
        body: List,
        cond: u32,
    },
    /// A numeric `for`: its variable's declaration and its bounds, the
    /// step [`NONE`] when absent.
    NumericFor {
        var: u32,
        start: u32,
        limit: u32,
        step: u32,
        body: List,
    },
    /// A generic `for`: its variables' declarations and its expressions.
    GenericFor {
        names: List,
        exprs: List,
        body: List,
    },
    Do(List),
    /// `local function`: its declaration and its function.
    LocalFunction {
        decl: u32,
        func: u32,
    },
    /// A function statement: its target (a local, a global or an index)
    /// and its function.
    Function {
        target: u32,
        func: u32,
    },
    Break,
    /// `goto` and a label, by their name's index in [`Ast::strs`].
    Goto(u32),
    Label(u32),
}

impl Stmt {
    /// The construct's name, as a refusal names it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Return(_) => "return",
            Self::Empty => ";",
            Self::Assign { .. } => "assignment",
            Self::Call(_) => "call statement",
            Self::If(_) => "if",
            Self::While { .. } => "while",
            Self::Repeat { .. } => "repeat",
            Self::NumericFor { .. } => "numeric for",
            Self::GenericFor { .. } => "generic for",
            Self::Do(_) => "do",
            Self::LocalFunction { .. } | Self::Function { .. } => "function",
            Self::Break => "break",
            Self::Goto(_) => "goto",
            Self::Label(_) => "label",
        }
    }
}

/// One step of a static access path: the constant string `key` (an index
/// of [`Ast::strs`]) read from the value of step `parent` (an index among
/// its function's paths) or, for a first step (`parent` [`NONE`]), from
/// binding `root`, an upvalue of the function: an enclosing function's
/// declaration or [`CHUNK_ENV`]. A global read is a first step from the
/// `_ENV` it is read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Path {
    pub(crate) parent: u32,
    pub(crate) root: u32,
    pub(crate) key: u32,
}

/// What declared a local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeclKind {
    Local,
    LocalFunction,
    Param,
    SelfParam,
    ForVar,
}

/// A local's attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attrib {
    None,
    Const,
    Close,
}

/// A local declaration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Decl {
    /// The name's bytes in the source.
    pub(crate) name: (u32, u32),
    /// The function it belongs to.
    pub(crate) func: u32,
    pub(crate) kind: DeclKind,
    pub(crate) attrib: Attrib,
    /// The expression whose first value initializes it (`local x = e`),
    /// else [`NONE`]; a `local function`'s is its function's expression.
    pub(crate) init: u32,
    /// Whether it is named `_ENV`.
    pub(crate) env: bool,
    /// Whether an assignment after its declaration targets it, in any
    /// function.
    pub(crate) assigned: bool,
}

/// How a function constructor is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// `function (...) ... end` as an expression.
    Expr,
    /// `local function name`.
    Local { decl: u32 },
    /// `function a.b:c`: the names' source spans in [`Ast::lists`]
    /// (pairs), and whether the last is a method.
    Named { names: List, method: bool },
}

/// A function constructor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Func {
    pub(crate) form: Form,
    /// Where its `function` keyword (or `local`) starts, and where the
    /// token after its parameter list's `(` starts.
    pub(crate) start: u32,
    pub(crate) open: u32,
    /// Where the byte after its `end` is.
    pub(crate) end: u32,
    pub(crate) line: u32,
    pub(crate) params: List,
    pub(crate) vararg: bool,
    pub(crate) body: List,
    /// The declarations of enclosing functions it reads, or
    /// [`CHUNK_ENV`]: its capture reader's order.
    pub(crate) upvalues: List,
    /// More upvalues than [`MAX_READER`]: it has no reader.
    pub(crate) unread: bool,
    /// Its static access paths, a span of [`Ast::paths`] (start, length),
    /// each step after its parent: the entries its captures copy.
    pub(crate) paths: (u32, u32),
}

/// One file's tree.
#[derive(Debug, Default)]
pub(crate) struct Ast {
    pub(crate) exprs: Vec<Expr>,
    pub(crate) expr_lines: Vec<u32>,
    pub(crate) stmts: Vec<Stmt>,
    pub(crate) stmt_lines: Vec<u32>,
    pub(crate) decls: Vec<Decl>,
    pub(crate) funcs: Vec<Func>,
    pub(crate) lists: Vec<u32>,
    /// Every function's static access paths, each function's together.
    pub(crate) paths: Vec<Path>,
    /// String literals and field names: spans of [`Self::bytes`].
    pub(crate) strs: Vec<(u32, u32)>,
    pub(crate) bytes: Vec<u8>,
    /// Where each statement but a `return` ends: the byte after its last
    /// token, in increasing order.
    pub(crate) stmt_ends: Vec<u32>,
    /// The numbers `n` of the names `tapestry_tag<n>` the source uses,
    /// which the tagger's name avoids ([`super::instrument`]).
    pub(crate) tag_names: Vec<u64>,
    /// The integer numerals whose value as written is past [`EXACT`], by
    /// expression id in increasing order, each charged as a node: the
    /// source's LuaJ holds another double ([`Ast::wide`]).
    pub(crate) wide_numerals: Vec<u32>,
    /// The `local d = X.timer_off_value` declarations, one name and one
    /// expression, each charged as a node: the tagger marks them
    /// ([`super::instrument`]).
    pub(crate) marks: Vec<Mark>,
}

/// A `local d = X.timer_off_value` declaration: `d`, where `X` starts,
/// and where the `.` after it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) decl: u32,
    pub(crate) start: u32,
    pub(crate) dot: u32,
}

impl Ast {
    /// The ids of list `l`.
    pub(crate) fn list(&self, (start, len): List) -> &[u32] {
        let (s, e) = (start as usize, start as usize + len as usize);
        self.lists.get(s..e).unwrap_or_default()
    }

    pub(crate) fn expr(&self, id: u32) -> Expr {
        self.exprs.get(id as usize).copied().unwrap_or(Expr::Nil)
    }

    pub(crate) fn expr_line(&self, id: u32) -> u32 {
        self.expr_lines.get(id as usize).copied().unwrap_or(0)
    }

    pub(crate) fn stmt(&self, id: u32) -> Stmt {
        self.stmts.get(id as usize).copied().unwrap_or(Stmt::Empty)
    }

    pub(crate) fn stmt_line(&self, id: u32) -> u32 {
        self.stmt_lines.get(id as usize).copied().unwrap_or(0)
    }

    pub(crate) fn decl(&self, id: u32) -> Option<&Decl> {
        self.decls.get(id as usize)
    }

    pub(crate) fn func(&self, id: u32) -> Option<&Func> {
        self.funcs.get(id as usize)
    }

    /// Function `f`'s static access paths.
    pub(crate) fn paths(&self, f: &Func) -> &[Path] {
        let (s, n) = (f.paths.0 as usize, f.paths.1 as usize);
        self.paths.get(s..s + n).unwrap_or_default()
    }

    /// Whether expression `e` is a wide integer numeral
    /// ([`Self::wide_numerals`]): a binary search.
    pub(crate) fn wide(&self, e: u32) -> bool {
        self.wide_numerals.binary_search(&e).is_ok()
    }

    /// The bytes of string `id`.
    pub(crate) fn str(&self, id: u32) -> &[u8] {
        self.strs
            .get(id as usize)
            .and_then(|&(s, e)| self.bytes.get(s as usize..e as usize))
            .unwrap_or_default()
    }
}

/// Why the parser stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fail {
    /// The source is not Lua as this parser reads it, at a line.
    Syntax(u32),
    /// [`MAX_AST_NODES`].
    Nodes,
    /// [`MAX_AST_DEPTH`], at a line.
    Depth(u32),
    Cancelled,
}

/// 2^53: every integer of at most this magnitude is exact as a double.
pub(crate) const EXACT: u64 = 1 << 53;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Eof,
    Name,
    Int(i64),
    /// An integer numeral whose value as written is past [`EXACT`], as
    /// Lua 5.4 reads it.
    Wide(i64),
    /// A float numeral, by its value's bits.
    Float(u64),
    Str(u32),
    // Keywords.
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // Symbols.
    Plus,
    Minus,
    Star,
    Slash,
    SlashSlash,
    Percent,
    Caret,
    Hash,
    Amp,
    Tilde,
    Pipe,
    Shl,
    Shr,
    EqEq,
    Ne,
    Le,
    Ge,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    DColon,
    Semi,
    Colon,
    Comma,
    Dot,
    DotDot,
    Ellipsis,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    tok: Tok,
    start: u32,
    end: u32,
    line: u32,
}

const KEYWORDS: [(&[u8], Tok); 22] = [
    (b"and", Tok::And),
    (b"break", Tok::Break),
    (b"do", Tok::Do),
    (b"else", Tok::Else),
    (b"elseif", Tok::Elseif),
    (b"end", Tok::End),
    (b"false", Tok::False),
    (b"for", Tok::For),
    (b"function", Tok::Function),
    (b"goto", Tok::Goto),
    (b"if", Tok::If),
    (b"in", Tok::In),
    (b"local", Tok::Local),
    (b"nil", Tok::Nil),
    (b"not", Tok::Not),
    (b"or", Tok::Or),
    (b"repeat", Tok::Repeat),
    (b"return", Tok::Return),
    (b"then", Tok::Then),
    (b"true", Tok::True),
    (b"until", Tok::Until),
    (b"while", Tok::While),
];

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Lua's whitespace (`lisspace` in the C locale).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The lexer: a cursor over the source.
#[derive(Clone, Copy)]
struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    /// The cancellation request, seen every [`TICK_EVERY`] bytes scanned.
    cancel: &'a dyn Fn() -> bool,
    steps: u32,
}

/// How many bytes the lexer scans between two looks at the cancellation
/// request, whatever token they are in.
const TICK_EVERY: u32 = 1 << 16;

impl Lexer<'_> {
    /// One byte scanned: the cancellation request seen every
    /// [`TICK_EVERY`].
    fn tick(&mut self) -> Result<(), Fail> {
        self.steps = self.steps.wrapping_add(1);
        if self.steps.is_multiple_of(TICK_EVERY) && (self.cancel)() {
            return Err(Fail::Cancelled);
        }
        Ok(())
    }

    fn cur(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn at(&self, k: usize) -> Option<u8> {
        self.src.get(self.pos + k).copied()
    }

    fn fail(&self) -> Fail {
        Fail::Syntax(self.line)
    }

    /// Past a newline sequence (`inclinenumber`): `\n`, `\r`, `\n\r` or
    /// `\r\n`, one line.
    fn newline(&mut self) {
        let first = self.cur();
        self.pos += 1;
        if matches!(self.cur(), Some(c @ (b'\n' | b'\r')) if Some(c) != first) {
            self.pos += 1;
        }
        self.line = self.line.saturating_add(1);
    }

    /// The level of a long bracket at the cursor (`[`, `=`s, `[`), the
    /// cursor past it; `None`, and the cursor kept, when there is none.
    fn long_open(&mut self) -> Result<Option<usize>, Fail> {
        let mut k = 1;
        while self.at(k) == Some(b'=') {
            self.tick()?;
            k += 1;
        }
        if self.at(k) == Some(b'[') {
            self.pos += k + 1;
            Ok(Some(k - 1))
        } else if k > 1 {
            Err(self.fail())
        } else {
            Ok(None)
        }
    }

    /// A long string's or comment's content after its opening bracket,
    /// newlines as `\n`, into `out` when given.
    fn long_body(&mut self, level: usize, mut out: Option<&mut Vec<u8>>) -> Result<(), Fail> {
        if matches!(self.cur(), Some(b'\n' | b'\r')) {
            self.newline();
        }
        loop {
            self.tick()?;
            match self.cur() {
                None => return Err(self.fail()),
                Some(b']') => {
                    let mut k = 1;
                    while self.at(k) == Some(b'=') {
                        self.tick()?;
                        k += 1;
                    }
                    if k - 1 == level && self.at(k) == Some(b']') {
                        self.pos += k + 1;
                        return Ok(());
                    }
                    if let Some(o) = out.as_deref_mut() {
                        o.push(b']');
                    }
                    self.pos += 1;
                }
                Some(b'\n' | b'\r') => {
                    self.newline();
                    if let Some(o) = out.as_deref_mut() {
                        o.push(b'\n');
                    }
                }
                Some(c) => {
                    if let Some(o) = out.as_deref_mut() {
                        o.push(c);
                    }
                    self.pos += 1;
                }
            }
        }
    }

    /// A quoted string's content, the cursor past its opening quote.
    fn short_string(&mut self, quote: u8, out: &mut Vec<u8>) -> Result<(), Fail> {
        loop {
            self.tick()?;
            let Some(c) = self.cur() else {
                return Err(self.fail());
            };
            match c {
                b'\n' | b'\r' => return Err(self.fail()),
                b'\\' => {
                    self.pos += 1;
                    self.escape(out)?;
                }
                c if c == quote => {
                    self.pos += 1;
                    return Ok(());
                }
                c => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// An escape after its `\` (`llex.c`, `read_string`).
    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), Fail> {
        let Some(c) = self.cur() else {
            return Err(self.fail());
        };
        let simple = match c {
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            b'\'' => Some(b'\''),
            _ => None,
        };
        if let Some(b) = simple {
            out.push(b);
            self.pos += 1;
            return Ok(());
        }
        match c {
            b'\n' | b'\r' => {
                self.newline();
                out.push(b'\n');
            }
            b'x' => {
                self.pos += 1;
                let mut v = 0_u8;
                for _ in 0..2 {
                    let d = self
                        .cur()
                        .and_then(|d| char::from(d).to_digit(16))
                        .ok_or(self.fail())?;
                    v = v * 16 + u8::try_from(d).unwrap_or(0);
                    self.pos += 1;
                }
                out.push(v);
            }
            b'z' => {
                self.pos += 1;
                while let Some(c) = self.cur() {
                    self.tick()?;
                    if c == b'\n' || c == b'\r' {
                        self.newline();
                    } else if is_space(c) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            b'u' => {
                self.pos += 1;
                if self.cur() != Some(b'{') {
                    return Err(self.fail());
                }
                self.pos += 1;
                let mut v: u32 = 0;
                let mut digits = 0;
                while let Some(d) = self.cur().and_then(|d| char::from(d).to_digit(16)) {
                    self.tick()?;
                    v = v
                        .checked_mul(16)
                        .and_then(|v| v.checked_add(d))
                        .filter(|&v| v <= 0x7FFF_FFFF)
                        .ok_or(self.fail())?;
                    digits += 1;
                    self.pos += 1;
                }
                if digits == 0 || self.cur() != Some(b'}') {
                    return Err(self.fail());
                }
                self.pos += 1;
                utf8(v, out);
            }
            b'0'..=b'9' => {
                let mut v: u32 = 0;
                for _ in 0..3 {
                    match self.cur() {
                        Some(d @ b'0'..=b'9') => {
                            v = v * 10 + u32::from(d - b'0');
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                out.push(u8::try_from(v).map_err(|_| self.fail())?);
            }
            _ => return Err(self.fail()),
        }
        Ok(())
    }

    /// A numeral (`read_numeral`): an integer when Lua reads one.
    fn numeral(&mut self) -> Result<Tok, Fail> {
        let start = self.pos;
        let hex = self.cur() == Some(b'0') && matches!(self.at(1), Some(b'x' | b'X'));
        let expo: &[u8] = if hex {
            self.pos += 2;
            b"Pp"
        } else {
            b"Ee"
        };
        loop {
            self.tick()?;
            match self.cur() {
                Some(c) if expo.contains(&c) => {
                    self.pos += 1;
                    if matches!(self.cur(), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                }
                Some(c) if c.is_ascii_hexdigit() || c == b'.' => self.pos += 1,
                _ => break,
            }
        }
        if self.cur().is_some_and(is_alpha) {
            return Err(self.fail());
        }
        let text = self.src.get(start..self.pos).unwrap_or_default();
        let fail = self.fail();
        let mut lex = *self;
        let r = number(text, &mut || lex.tick());
        self.steps = lex.steps;
        r?.ok_or(fail)
    }

    /// The next token, its strings decoded into `bytes`.
    fn next(&mut self, ast: &mut Ast) -> Result<Token, Fail> {
        loop {
            self.tick()?;
            let Some(c) = self.cur() else {
                return Ok(self.token(Tok::Eof, self.pos));
            };
            match c {
                b'\n' | b'\r' => self.newline(),
                c if is_space(c) => self.pos += 1,
                b'-' if self.at(1) == Some(b'-') => {
                    self.pos += 2;
                    // A bracket that opens no long comment starts a short one
                    // (`llex.c`): `--[=` is a comment to the line's end.
                    if self.cur() == Some(b'[')
                        && let Ok(Some(level)) = self.long_open()
                    {
                        self.long_body(level, None)?;
                        continue;
                    }
                    while let Some(c) = self.cur() {
                        self.tick()?;
                        if c == b'\n' || c == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let start = self.pos;
        let line = self.line;
        let Some(c) = self.cur() else {
            return Ok(self.token(Tok::Eof, start));
        };
        let one = |l: &mut Self, t: Tok, n: usize| {
            l.pos += n;
            t
        };
        let tok = match c {
            c if is_alpha(c) => {
                while self.cur().is_some_and(is_alnum) {
                    self.tick()?;
                    self.pos += 1;
                }
                let word = self.src.get(start..self.pos).unwrap_or_default();
                if let Some(n) = word
                    .strip_prefix(TAG_PREFIX)
                    .and_then(|d| std::str::from_utf8(d).ok())
                    .and_then(|d| d.parse::<u64>().ok())
                {
                    ast.tag_names.push(n);
                }
                KEYWORDS
                    .iter()
                    .find(|(k, _)| *k == word)
                    .map_or(Tok::Name, |&(_, t)| t)
            }
            b'0'..=b'9' => self.numeral()?,
            b'.' if self.at(1).is_some_and(|d| d.is_ascii_digit()) => self.numeral()?,
            b'"' | b'\'' => {
                self.pos += 1;
                let from = ast.bytes.len();
                self.short_string(c, &mut ast.bytes)?;
                Tok::Str(push_str(ast, from))
            }
            b'[' => match self.long_open()? {
                Some(level) => {
                    let from = ast.bytes.len();
                    self.long_body(level, Some(&mut ast.bytes))?;
                    Tok::Str(push_str(ast, from))
                }
                None => one(self, Tok::LBracket, 1),
            },
            b'+' => one(self, Tok::Plus, 1),
            b'-' => one(self, Tok::Minus, 1),
            b'*' => one(self, Tok::Star, 1),
            b'/' if self.at(1) == Some(b'/') => one(self, Tok::SlashSlash, 2),
            b'/' => one(self, Tok::Slash, 1),
            b'%' => one(self, Tok::Percent, 1),
            b'^' => one(self, Tok::Caret, 1),
            b'#' => one(self, Tok::Hash, 1),
            b'&' => one(self, Tok::Amp, 1),
            b'~' if self.at(1) == Some(b'=') => one(self, Tok::Ne, 2),
            b'~' => one(self, Tok::Tilde, 1),
            b'|' => one(self, Tok::Pipe, 1),
            b'<' if self.at(1) == Some(b'<') => one(self, Tok::Shl, 2),
            b'<' if self.at(1) == Some(b'=') => one(self, Tok::Le, 2),
            b'<' => one(self, Tok::Lt, 1),
            b'>' if self.at(1) == Some(b'>') => one(self, Tok::Shr, 2),
            b'>' if self.at(1) == Some(b'=') => one(self, Tok::Ge, 2),
            b'>' => one(self, Tok::Gt, 1),
            b'=' if self.at(1) == Some(b'=') => one(self, Tok::EqEq, 2),
            b'=' => one(self, Tok::Assign, 1),
            b'(' => one(self, Tok::LParen, 1),
            b')' => one(self, Tok::RParen, 1),
            b'{' => one(self, Tok::LBrace, 1),
            b'}' => one(self, Tok::RBrace, 1),
            b']' => one(self, Tok::RBracket, 1),
            b':' if self.at(1) == Some(b':') => one(self, Tok::DColon, 2),
            b':' => one(self, Tok::Colon, 1),
            b';' => one(self, Tok::Semi, 1),
            b',' => one(self, Tok::Comma, 1),
            b'.' if self.at(1) == Some(b'.') && self.at(2) == Some(b'.') => {
                one(self, Tok::Ellipsis, 3)
            }
            b'.' if self.at(1) == Some(b'.') => one(self, Tok::DotDot, 2),
            b'.' => one(self, Tok::Dot, 1),
            _ => return Err(self.fail()),
        };
        Ok(Token {
            tok,
            start: u32::try_from(start).unwrap_or(u32::MAX),
            end: u32::try_from(self.pos).unwrap_or(u32::MAX),
            line,
        })
    }

    fn token(&self, tok: Tok, at: usize) -> Token {
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        Token {
            tok,
            start: at,
            end: at,
            line: self.line,
        }
    }
}

/// Records the bytes from `from` as a string.
fn push_str(ast: &mut Ast, from: usize) -> u32 {
    let s = u32::try_from(from).unwrap_or(u32::MAX);
    let e = u32::try_from(ast.bytes.len()).unwrap_or(u32::MAX);
    ast.strs.push((s, e));
    u32::try_from(ast.strs.len() - 1).unwrap_or(u32::MAX)
}

/// `v` as Lua's `\u{}` writes it (`luaO_utf8esc`): up to six bytes.
fn utf8(v: u32, out: &mut Vec<u8>) {
    let low = |x: u32| x.to_le_bytes()[0];
    if v < 0x80 {
        out.push(low(v));
        return;
    }
    let mut buf = [0_u8; 6];
    let mut n = 0;
    let mut x = v;
    let mut mfb: u32 = 0x3f;
    loop {
        buf[5 - n] = low(0x80 | (x & 0x3f));
        n += 1;
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    out.push(low((!mfb << 1) | x));
    out.extend_from_slice(&buf[6 - n..]);
}

/// A numeral's token, as `luaO_str2num` reads it: an integer when it has
/// only decimal digits that fit (an overflowing one is a float) or is
/// hexadecimal without a point or an exponent (wrapping), else a float
/// when it reads as one, its value rounded to nearest as `strtod` rounds
/// it; `None` when it is malformed. An integer whose value as written is
/// past [`EXACT`] is [`Tok::Wide`]: the source's LuaJ reads it as a double
/// (`LexState.strx2number` accumulates a hexadecimal one in a double,
/// which keeps it positive), another value than the integer.
fn number(text: &[u8], tick: &mut dyn FnMut() -> Result<(), Fail>) -> Result<Option<Tok>, Fail> {
    // Each byte visited once per scan, each visit a tick.
    let mut all = |t: &[u8], f: fn(&u8) -> bool| -> Result<bool, Fail> {
        for c in t {
            tick()?;
            if !f(c) {
                return Ok(false);
            }
        }
        Ok(true)
    };
    let hex = text.len() > 1 && text[0] == b'0' && matches!(text[1], b'x' | b'X');
    if hex {
        let digits = &text[2..];
        if !digits.is_empty() && all(digits, u8::is_ascii_hexdigit)? {
            let mut v: u64 = 0;
            // The value as written, while it is within `EXACT`.
            let mut exact = Some(0_u64);
            for &d in digits {
                tick()?;
                let d = char::from(d).to_digit(16).unwrap_or(0);
                v = v.wrapping_mul(16).wrapping_add(u64::from(d));
                exact = exact
                    .and_then(|x| x.checked_mul(16)?.checked_add(u64::from(d)))
                    .filter(|&x| x <= EXACT);
            }
            let v = v.cast_signed();
            return Ok(Some(if exact.is_some() {
                Tok::Int(v)
            } else {
                Tok::Wide(v)
            }));
        }
        if !float(digits, true, tick)? {
            return Ok(None);
        }
        return Ok(Some(Tok::Float(hex_float(digits, tick)?)));
    }
    if !text.is_empty() && all(text, u8::is_ascii_digit)? {
        let mut v: i64 = 0;
        for &d in text {
            tick()?;
            match v
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(d - b'0')))
            {
                Some(n) => v = n,
                None => return Ok(Some(Tok::Float(decimal_float(text)))),
            }
        }
        return Ok(Some(if v.unsigned_abs() <= EXACT {
            Tok::Int(v)
        } else {
            Tok::Wide(v)
        }));
    }
    Ok(float(text, false, tick)?.then(|| Tok::Float(decimal_float(text))))
}

/// The bits of decimal float numeral `t`, already checked: Rust's reading
/// rounds to nearest, as `strtod` does.
fn decimal_float(t: &[u8]) -> u64 {
    std::str::from_utf8(t)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
        .to_bits()
}

/// The most significant hexadecimal digits a float numeral's value keeps
/// exactly: past them, a digit only moves the exponent and, when not
/// zero, marks the value as above the kept ones (which rounding sees).
const HEX_DIGITS: u32 = 30;

/// The bits of hexadecimal float numeral `t` (after its `0x`), already
/// checked: rounded to nearest, ties to even, as `strtod` rounds it; each
/// digit visited a tick. The bits follow the sandbox's VM (Lua 5.4 with a
/// correctly rounding `strtod`), not the source's LuaJ, which can round a
/// long hexadecimal numeral otherwise; nothing reads float bits before
/// the presentation program (docs/stage3.md S7-06d).
fn hex_float(t: &[u8], tick: &mut dyn FnMut() -> Result<(), Fail>) -> Result<u64, Fail> {
    let (mut m, mut e, mut sig, mut sticky, mut dot) = (0_u128, 0_i64, 0_u32, false, false);
    let mut i = 0;
    while let Some(&c) = t.get(i) {
        if c == b'.' {
            dot = true;
        } else if let Some(d) = char::from(c).to_digit(16) {
            tick()?;
            if sig == 0 && d == 0 {
                // A leading zero.
            } else if sig < HEX_DIGITS {
                sig += 1;
                m = m * 16 + u128::from(d);
            } else {
                sticky |= d != 0;
                e += 4;
            }
            if dot {
                e -= 4;
            }
        } else {
            break;
        }
        i += 1;
    }
    if matches!(t.get(i), Some(b'p' | b'P')) {
        i += 1;
        let neg = t.get(i) == Some(&b'-');
        if matches!(t.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let mut x: i64 = 0;
        while let Some(&c) = t.get(i).filter(|c| c.is_ascii_digit()) {
            tick()?;
            x = (x * 10 + i64::from(c - b'0')).min(1 << 24);
            i += 1;
        }
        e += if neg { -x } else { x };
    }
    Ok(scaled(m, e, sticky).to_bits())
}

/// `m · 2^e`, `sticky` when the true value is a little above it, rounded
/// to the nearest `f64`, ties to even.
fn scaled(m: u128, e: i64, sticky: bool) -> f64 {
    if m == 0 {
        return 0.0;
    }
    let n = i64::from(128 - m.leading_zeros());
    // The value's top bit, then the significand bits a normal or a
    // subnormal result keeps.
    let top = n - 1 + e;
    let keep = if top >= -1022 { 53 } else { 53 - (-1022 - top) };
    if keep < 0 {
        return 0.0;
    }
    let shift = n - keep;
    let (mut q, mut low) = (m, e);
    if shift > 0 {
        let s = u32::try_from(shift).unwrap_or(127);
        q = m >> s;
        let rem = m & ((1_u128 << s) - 1);
        let half = 1_u128 << (s - 1);
        if rem > half || (rem == half && (sticky || q & 1 == 1)) {
            q += 1;
        }
        low = e + shift;
    }
    if top + i64::from(q >> keep.max(0) != 0) > 1023 {
        return f64::INFINITY;
    }
    // `q` holds at most 54 bits, its lowest bit's weight `2^low` is at
    // least the least subnormal and at most the value's top bit, and the
    // result is representable: an exact conversion and one exact product.
    let q = q as f64;
    q * pow2(low)
}

/// `2^k` for `k` in `-1074..=1023`, built from its bits.
fn pow2(k: i64) -> f64 {
    if k >= -1022 {
        f64::from_bits(u64::try_from(k + 1023).unwrap_or(0) << 52)
    } else {
        f64::from_bits(1_u64 << u32::try_from(k + 1074).unwrap_or(0))
    }
}

/// Whether `t` is a float numeral's mantissa and exponent: digits with
/// at most one point, at least one digit, and an optional exponent with a
/// sign and at least one decimal digit (`p` for hexadecimal); each byte
/// visited a tick.
fn float(t: &[u8], hex: bool, tick: &mut dyn FnMut() -> Result<(), Fail>) -> Result<bool, Fail> {
    let digit = |c: u8| {
        if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        }
    };
    let mut i = 0;
    let mut digits = 0;
    while i < t.len() && digit(t[i]) {
        tick()?;
        i += 1;
        digits += 1;
    }
    if i < t.len() && t[i] == b'.' {
        i += 1;
        while i < t.len() && digit(t[i]) {
            tick()?;
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return Ok(false);
    }
    if i < t.len() && matches!(t[i], b'e' | b'E' | b'p' | b'P') {
        if hex != matches!(t[i], b'p' | b'P') {
            return Ok(false);
        }
        i += 1;
        if i < t.len() && matches!(t[i], b'+' | b'-') {
            i += 1;
        }
        let from = i;
        while i < t.len() && t[i].is_ascii_digit() {
            tick()?;
            i += 1;
        }
        if i == from {
            return Ok(false);
        }
    }
    Ok(i == t.len())
}

/// A function being parsed: its index and its upvalues so far.
struct FuncState {
    id: u32,
    upvalues: Vec<u32>,
    /// [`Self::upvalues`] as a set, for O(1) membership.
    seen: HashSet<u32>,
    unread: bool,
    /// The length of the scope stack where its locals start.
    base: usize,
    /// Its static access paths so far.
    paths: Vec<Path>,
}

/// Where a suffixed expression stands on a static access path.
#[derive(Clone, Copy)]
enum PathAt {
    /// On none.
    Off,
    /// At an upvalue binding, before any step.
    Root(u32),
    /// After a step, by its index among the function's paths.
    Step(u32),
}

/// The most locals a function has in scope at once: Lua's own
/// (`MAXVARS`), so a chunk past it is one Lua refuses too.
const MAX_LOCALS: usize = 200;

/// The parser of one file.
struct Parser<'a, 'c> {
    lex: Lexer<'a>,
    tok: Token,
    ast: Ast,
    /// Nodes charged so far, across the candidate.
    charged: &'c mut u64,
    cancel: &'c dyn Fn() -> bool,
    depth: u32,
    /// The locals in scope, innermost last, and by name.
    active: Vec<u32>,
    by_name: HashMap<Vec<u8>, Vec<u32>>,
    funcs: Vec<FuncState>,
    /// The shared scratch stack lists are gathered on.
    scratch: Vec<u32>,
    /// Where the last token consumed ends.
    last_end: u32,
    /// The last index by `.name` parsed: its expression and where its
    /// `.` starts.
    last_dot: (u32, u32),
}

/// Parses `src`, charging its nodes to `charged` (the candidate's count
/// so far), looking at `cancel` every [`CHECK_EVERY`] nodes.
pub(crate) fn parse(src: &[u8], charged: &mut u64, cancel: &dyn Fn() -> bool) -> Result<Ast, Fail> {
    let mut lex = Lexer {
        src,
        pos: 0,
        line: 1,
        cancel,
        steps: 0,
    };
    let mut ast = Ast::default();
    let tok = lex.next(&mut ast)?;
    let mut p = Parser {
        lex,
        tok,
        ast,
        charged,
        cancel,
        depth: 0,
        active: Vec::new(),
        by_name: HashMap::new(),
        funcs: Vec::new(),
        scratch: Vec::new(),
        last_end: 0,
        last_dot: (NONE, 0),
    };
    p.chunk()?;
    Ok(p.ast)
}

impl<'a> Parser<'a, '_> {
    fn fail(&self) -> Fail {
        Fail::Syntax(self.tok.line)
    }

    /// One node charged before it is added.
    fn charge(&mut self) -> Result<(), Fail> {
        if *self.charged >= MAX_AST_NODES {
            return Err(Fail::Nodes);
        }
        *self.charged += 1;
        if (*self.charged).is_multiple_of(u64::from(CHECK_EVERY)) && (self.cancel)() {
            return Err(Fail::Cancelled);
        }
        Ok(())
    }

    /// [`Self::charge`] at a named site, which a test can make fail.
    /// For a test, the site records what the real charge added to the
    /// node count, and can be made to meet a spent count, so the real
    /// check refuses.
    fn charge_at(&mut self, site: &'static str) -> Result<(), Fail> {
        #[cfg(test)]
        let before = {
            if super::tests::fail_at(site) {
                *self.charged = MAX_AST_NODES;
            }
            *self.charged
        };
        #[cfg(not(test))]
        let _ = site;
        let r = self.charge();
        #[cfg(test)]
        super::tests::site_charged(site, self.charged.saturating_sub(before));
        r
    }

    fn enter(&mut self) -> Result<(), Fail> {
        if self.depth >= MAX_AST_DEPTH {
            return Err(Fail::Depth(self.tok.line));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn advance(&mut self) -> Result<Token, Fail> {
        let t = self.tok;
        self.tok = self.lex.next(&mut self.ast)?;
        self.last_end = t.end;
        Ok(t)
    }

    fn check(&self, t: Tok) -> bool {
        self.tok.tok == t
    }

    fn accept(&mut self, t: Tok) -> Result<bool, Fail> {
        if self.check(t) {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, t: Tok) -> Result<Token, Fail> {
        if self.check(t) {
            self.advance()
        } else {
            Err(self.fail())
        }
    }

    /// The token after the current one, the lexer left where it was.
    fn peek(&mut self) -> Result<Tok, Fail> {
        let saved = self.lex;
        let (strs, bytes) = (self.ast.strs.len(), self.ast.bytes.len());
        let tags = self.ast.tag_names.len();
        let t = self.lex.next(&mut self.ast)?;
        self.lex = saved;
        self.ast.strs.truncate(strs);
        self.ast.bytes.truncate(bytes);
        self.ast.tag_names.truncate(tags);
        Ok(t.tok)
    }

    fn name_bytes(&self, (s, e): (u32, u32)) -> &'a [u8] {
        self.lex.src.get(s as usize..e as usize).unwrap_or_default()
    }

    fn expr(&mut self, e: Expr, line: u32) -> Result<u32, Fail> {
        self.charge()?;
        self.ast.exprs.push(e);
        self.ast.expr_lines.push(line);
        Ok(u32::try_from(self.ast.exprs.len() - 1).unwrap_or(NONE))
    }

    fn stmt(&mut self, s: Stmt, line: u32) -> Result<u32, Fail> {
        self.charge()?;
        self.ast.stmts.push(s);
        self.ast.stmt_lines.push(line);
        Ok(u32::try_from(self.ast.stmts.len() - 1).unwrap_or(NONE))
    }

    /// The scratch entries from `mark` moved into a list, each charged.
    fn list_from(&mut self, mark: usize) -> Result<List, Fail> {
        let n = self.scratch.len().saturating_sub(mark);
        for _ in 0..n {
            self.charge()?;
        }
        let start = u32::try_from(self.ast.lists.len()).unwrap_or(NONE);
        self.ast.lists.extend(self.scratch.drain(mark..));
        Ok((start, u32::try_from(n).unwrap_or(NONE)))
    }

    fn current_func(&self) -> u32 {
        self.funcs.last().map_or(NONE, |f| f.id)
    }

    /// Declares a local (not yet in scope).
    fn declare(&mut self, name: (u32, u32), kind: DeclKind) -> Result<u32, Fail> {
        self.charge()?;
        let env = self.name_bytes(name) == b"_ENV";
        self.ast.decls.push(Decl {
            name,
            func: self.current_func(),
            kind,
            attrib: Attrib::None,
            init: NONE,
            env,
            assigned: false,
        });
        Ok(u32::try_from(self.ast.decls.len() - 1).unwrap_or(NONE))
    }

    /// The local named `name` in scope, innermost first: a hash of the
    /// name, O(its length).
    fn lookup(&self, name: &[u8]) -> Option<u32> {
        self.by_name.get(name).and_then(|ds| ds.last().copied())
    }

    /// The name declaration `d` is in scope by, borrowed from the source.
    fn decl_name(&self, d: u32) -> &'a [u8] {
        match self.ast.decls.get(d as usize) {
            Some(x) if x.kind == DeclKind::SelfParam => b"self",
            Some(x) => self.name_bytes(x.name),
            None => b"",
        }
    }

    /// Brings declaration `d` into scope; past [`MAX_LOCALS`] in its
    /// function the chunk is refused, as Lua refuses it.
    fn bind(&mut self, d: u32) -> Result<(), Fail> {
        let base = self.funcs.last().map_or(0, |f| f.base);
        if self.active.len() - base >= MAX_LOCALS {
            return Err(self.fail());
        }
        let name = self.decl_name(d);
        self.by_name.entry(name.to_vec()).or_default().push(d);
        self.active.push(d);
        Ok(())
    }

    /// Ends the scopes opened since the stack was `len` long.
    fn unbind_to(&mut self, len: usize) {
        while self.active.len() > len {
            let Some(d) = self.active.pop() else {
                break;
            };
            let name = self.decl_name(d);
            if let Some(ds) = self.by_name.get_mut(name) {
                ds.pop();
                if ds.is_empty() {
                    self.by_name.remove(name);
                }
            }
        }
    }

    /// Records that the current function reads `up` (a declaration or
    /// [`CHUNK_ENV`]) when it belongs to an enclosing function.
    fn use_binding(&mut self, up: u32) {
        let owner = if up == CHUNK_ENV {
            NONE
        } else {
            self.ast.decls.get(up as usize).map_or(NONE, |d| d.func)
        };
        let Some(f) = self.funcs.last_mut() else {
            return;
        };
        if owner == f.id || f.unread || f.seen.contains(&up) {
            return;
        }
        if f.upvalues.len() >= MAX_READER {
            f.unread = true;
            return;
        }
        f.seen.insert(up);
        f.upvalues.push(up);
    }

    /// A name read as a value: a local, the chunk's `_ENV` cell for a bare
    /// `_ENV` no local shadows (Lua's own upvalue of the chunk, never a
    /// global named `_ENV`), or a global through `_ENV`.
    fn name_expr(&mut self, t: Token) -> Result<u32, Fail> {
        let name = self.name_bytes((t.start, t.end));
        if let Some(d) = self.lookup(name) {
            self.use_binding(d);
            return self.expr(Expr::Local(d), t.line);
        }
        let env = self.lookup(b"_ENV").unwrap_or(CHUNK_ENV);
        self.use_binding(env);
        if name == b"_ENV" {
            return self.expr(Expr::Local(CHUNK_ENV), t.line);
        }
        let name = self.field_name(t);
        self.expr(Expr::Global { name, env }, t.line)
    }

    /// Whether binding `d` (a declaration or [`CHUNK_ENV`]) is an upvalue
    /// of the current function, one whose captures are copied: never in
    /// the chunk, which no root is.
    fn is_upvalue(&self, d: u32) -> bool {
        let current = self.current_func();
        if current == 0 || current == NONE {
            return false;
        }
        d == CHUNK_ENV
            || self
                .ast
                .decls
                .get(d as usize)
                .is_some_and(|x| x.func != current)
    }

    /// Where expression `e`, just read, stands on a static access path: a
    /// global read through an upvalue `_ENV` is its first step.
    fn path_root(&mut self, e: u32) -> Result<PathAt, Fail> {
        match self.ast.expr(e) {
            Expr::Local(d) if self.is_upvalue(d) => Ok(PathAt::Root(d)),
            Expr::Global { name, env } if self.is_upvalue(env) => {
                self.path_step(PathAt::Root(env), name)
            }
            _ => Ok(PathAt::Off),
        }
    }

    /// The step reading the constant string `key` after `at`, charged
    /// before it is added.
    fn path_step(&mut self, at: PathAt, key: u32) -> Result<PathAt, Fail> {
        let (parent, root) = match at {
            PathAt::Off => return Ok(PathAt::Off),
            PathAt::Root(r) => (NONE, r),
            PathAt::Step(p) => (p, NONE),
        };
        self.charge()?;
        let Some(f) = self.funcs.last_mut() else {
            return Ok(PathAt::Off);
        };
        let i = u32::try_from(f.paths.len()).unwrap_or(NONE);
        f.paths.push(Path { parent, root, key });
        Ok(PathAt::Step(i))
    }

    /// A field name as a string.
    fn field_name(&mut self, t: Token) -> u32 {
        let from = self.ast.bytes.len();
        let (s, e) = (t.start as usize, t.end as usize);
        let bytes = self.lex.src.get(s..e).unwrap_or_default();
        self.ast.bytes.extend_from_slice(bytes);
        push_str(&mut self.ast, from)
    }

    fn chunk(&mut self) -> Result<(), Fail> {
        // The chunk is function 0, a vararg function with no parameters.
        self.charge()?;
        self.ast.funcs.push(Func {
            form: Form::Expr,
            start: 0,
            open: 0,
            end: u32::try_from(self.lex.src.len()).unwrap_or(NONE),
            line: 0,
            params: (0, 0),
            vararg: true,
            body: (0, 0),
            upvalues: (0, 0),
            unread: false,
            paths: (0, 0),
        });
        self.funcs.push(FuncState {
            id: 0,
            upvalues: Vec::new(),
            seen: HashSet::new(),
            unread: false,
            base: 0,
            paths: Vec::new(),
        });
        let body = self.block()?;
        if !self.check(Tok::Eof) {
            return Err(self.fail());
        }
        let f = self.funcs.pop();
        if let Some(func) = self.ast.funcs.get_mut(0) {
            func.body = body;
            func.unread = f.is_some_and(|f| f.unread);
        }
        Ok(())
    }

    fn block_end(&self) -> bool {
        matches!(
            self.tok.tok,
            Tok::Eof | Tok::End | Tok::Else | Tok::Elseif | Tok::Until
        )
    }

    /// A block, its scope closed after it.
    fn block(&mut self) -> Result<List, Fail> {
        let scope = self.active.len();
        let list = self.block_open()?;
        self.unbind_to(scope);
        Ok(list)
    }

    /// A block's statements; its locals stay in scope (for `until`).
    fn block_open(&mut self) -> Result<List, Fail> {
        let mark = self.scratch.len();
        while !self.block_end() {
            if self.check(Tok::Return) {
                let s = self.retstat()?;
                self.scratch.push(s);
                break;
            }
            let s = self.statement()?;
            self.scratch.push(s);
        }
        self.list_from(mark)
    }

    fn retstat(&mut self) -> Result<u32, Fail> {
        let line = self.advance()?.line;
        let mark = self.scratch.len();
        if !self.block_end() && !self.check(Tok::Semi) {
            self.exprlist()?;
        }
        self.accept(Tok::Semi)?;
        let list = self.list_from(mark)?;
        if !self.block_end() {
            return Err(self.fail());
        }
        self.stmt(Stmt::Return(list), line)
    }

    fn statement(&mut self) -> Result<u32, Fail> {
        self.enter()?;
        let r = self.statement_inner();
        self.leave();
        self.ast.stmt_ends.push(self.last_end);
        r
    }

    fn statement_inner(&mut self) -> Result<u32, Fail> {
        let line = self.tok.line;
        match self.tok.tok {
            Tok::Semi => {
                self.advance()?;
                self.stmt(Stmt::Empty, line)
            }
            Tok::If => {
                self.advance()?;
                let mark = self.scratch.len();
                self.arm(Tok::Then)?;
                while self.check(Tok::Elseif) {
                    self.advance()?;
                    self.arm(Tok::Then)?;
                }
                if self.accept(Tok::Else)? {
                    let (s, n) = self.block()?;
                    for id in [NONE, s, n] {
                        self.push_charged(id)?;
                    }
                }
                self.expect(Tok::End)?;
                let arms = self.list_charged(mark);
                self.stmt(Stmt::If(arms), line)
            }
            Tok::While => {
                self.advance()?;
                let (cond, body) = self.cond_block(Tok::Do)?;
                self.expect(Tok::End)?;
                self.stmt(Stmt::While { cond, body }, line)
            }
            Tok::Do => {
                self.advance()?;
                let body = self.block()?;
                self.expect(Tok::End)?;
                self.stmt(Stmt::Do(body), line)
            }
            Tok::For => self.for_stat(line),
            Tok::Repeat => {
                self.advance()?;
                let scope = self.active.len();
                let body = self.block_open()?;
                self.expect(Tok::Until)?;
                let cond = self.expression()?;
                self.unbind_to(scope);
                self.stmt(Stmt::Repeat { body, cond }, line)
            }
            Tok::Function => self.function_stat(line),
            Tok::Local => {
                let start = self.advance()?.start;
                if self.check(Tok::Function) {
                    self.advance()?;
                    let t = self.expect(Tok::Name)?;
                    let decl = self.declare((t.start, t.end), DeclKind::LocalFunction)?;
                    self.bind(decl)?;
                    let func = self.body(Form::Local { decl }, start, false, line)?;
                    let e = self.expr(Expr::Func(func), line)?;
                    if let Some(d) = self.ast.decls.get_mut(decl as usize) {
                        d.init = e;
                    }
                    self.stmt(Stmt::LocalFunction { decl, func }, line)
                } else {
                    self.local_stat(line)
                }
            }
            Tok::DColon => {
                self.advance()?;
                let t = self.expect(Tok::Name)?;
                self.expect(Tok::DColon)?;
                let name = self.field_name(t);
                self.stmt(Stmt::Label(name), line)
            }
            Tok::Return => Err(self.fail()),
            Tok::Break => {
                self.advance()?;
                self.stmt(Stmt::Break, line)
            }
            Tok::Goto => {
                self.advance()?;
                let t = self.expect(Tok::Name)?;
                let name = self.field_name(t);
                self.stmt(Stmt::Goto(name), line)
            }
            _ => self.expr_stat(line),
        }
    }

    /// A condition, `kw` and a block.
    fn cond_block(&mut self, kw: Tok) -> Result<(u32, List), Fail> {
        let cond = self.expression()?;
        self.expect(kw)?;
        let body = self.block()?;
        Ok((cond, body))
    }

    /// An `if` arm onto the scratch stack: its condition, `kw`, and its
    /// block's list.
    fn arm(&mut self, kw: Tok) -> Result<(), Fail> {
        let (cond, (s, n)) = self.cond_block(kw)?;
        for id in [cond, s, n] {
            self.push_charged(id)?;
        }
        Ok(())
    }

    fn for_stat(&mut self, line: u32) -> Result<u32, Fail> {
        self.advance()?;
        let first = self.expect(Tok::Name)?;
        let scope = self.active.len();
        if self.accept(Tok::Assign)? {
            let start = self.expression()?;
            self.expect(Tok::Comma)?;
            let limit = self.expression()?;
            let step = if self.accept(Tok::Comma)? {
                self.expression()?
            } else {
                NONE
            };
            let var = self.declare((first.start, first.end), DeclKind::ForVar)?;
            self.expect(Tok::Do)?;
            self.bind(var)?;
            let body = self.block()?;
            self.unbind_to(scope);
            self.expect(Tok::End)?;
            return self.stmt(
                Stmt::NumericFor {
                    var,
                    start,
                    limit,
                    step,
                    body,
                },
                line,
            );
        }
        let mut names = vec![(first.start, first.end)];
        while self.accept(Tok::Comma)? {
            let t = self.expect(Tok::Name)?;
            names.push((t.start, t.end));
        }
        self.expect(Tok::In)?;
        let mark = self.scratch.len();
        self.exprlist()?;
        let exprs = self.list_from(mark)?;
        self.expect(Tok::Do)?;
        let mark = self.scratch.len();
        for n in names {
            let d = self.declare(n, DeclKind::ForVar)?;
            self.bind(d)?;
            self.push_charged(d)?;
        }
        let names = self.list_charged(mark);
        let body = self.block()?;
        self.unbind_to(scope);
        self.expect(Tok::End)?;
        self.stmt(Stmt::GenericFor { names, exprs, body }, line)
    }

    fn function_stat(&mut self, line: u32) -> Result<u32, Fail> {
        let start = self.advance()?.start;
        let first = self.expect(Tok::Name)?;
        // The first name is read (a field target) or assigned (a plain
        // name); each name but the last is read, a static access path's
        // step when it is one, and the last is assigned.
        let mark = self.scratch.len();
        self.scratch.push(first.start);
        self.scratch.push(first.end);
        let mut target = self.name_expr(first)?;
        let mut at = PathAt::Off;
        let mut read: Option<u32> = None;
        let mut method = false;
        loop {
            if self.accept(Tok::Colon)? {
                method = true;
            } else if !self.accept(Tok::Dot)? {
                break;
            }
            let t = self.expect(Tok::Name)?;
            self.scratch.push(t.start);
            self.scratch.push(t.end);
            at = match read {
                None => self.path_root(target)?,
                Some(key) => self.path_step(at, key)?,
            };
            let s = self.field_name(t);
            let key = self.expr(Expr::Str(s), t.line)?;
            target = self.expr(Expr::Index { obj: target, key }, t.line)?;
            read = Some(s);
            if method {
                break;
            }
        }
        let names = self.list_from(mark)?;
        if read.is_none() {
            self.target(target)?;
        }
        let func = self.body(Form::Named { names, method }, start, method, line)?;
        self.stmt(Stmt::Function { target, func }, line)
    }

    fn local_stat(&mut self, line: u32) -> Result<u32, Fail> {
        let mark = self.scratch.len();
        loop {
            let t = self.expect(Tok::Name)?;
            let d = self.declare((t.start, t.end), DeclKind::Local)?;
            if self.accept(Tok::Lt)? {
                let a = self.expect(Tok::Name)?;
                let attrib = match self.name_bytes((a.start, a.end)) {
                    b"const" => Attrib::Const,
                    b"close" => Attrib::Close,
                    _ => return Err(Fail::Syntax(a.line)),
                };
                self.expect(Tok::Gt)?;
                if let Some(x) = self.ast.decls.get_mut(d as usize) {
                    x.attrib = attrib;
                }
            }
            self.scratch.push(d);
            if !self.accept(Tok::Comma)? {
                break;
            }
        }
        let names = self.list_from(mark)?;
        let mark = self.scratch.len();
        let start = self.tok.start;
        let assigns = self.accept(Tok::Assign)?;
        let start = if assigns { self.tok.start } else { start };
        if assigns {
            self.exprlist()?;
        }
        let exprs = self.list_from(mark)?;
        // `local d = X.timer_off_value`, the index by `.` the whole
        // expression: a declaration the tagger marks.
        if let (&[decl], &[e]) = (self.ast.list(names), self.ast.list(exprs))
            && let Expr::Index { key, .. } = self.ast.expr(e)
            && matches!(self.ast.expr(key), Expr::Str(s) if self.ast.str(s) == b"timer_off_value")
            && self.last_dot.0 == e
        {
            self.charge_at("mark")?;
            #[cfg(test)]
            super::tests::site_did("mark");
            self.ast.marks.push(Mark {
                decl,
                start,
                dot: self.last_dot.1,
            });
        }
        // Each name its own initializer, a call's first result; the names
        // past the expressions have none.
        let ns = self.ast.list(names).to_vec();
        let es = self.ast.list(exprs).to_vec();
        for (i, &d) in ns.iter().enumerate() {
            let init = es.get(i).copied().unwrap_or(NONE);
            if let Some(x) = self.ast.decls.get_mut(d as usize) {
                x.init = init;
            }
        }
        for d in ns {
            self.bind(d)?;
        }
        self.stmt(Stmt::Local { names, exprs }, line)
    }

    fn expr_stat(&mut self, line: u32) -> Result<u32, Fail> {
        let e = self.suffixed()?;
        if self.check(Tok::Assign) || self.check(Tok::Comma) {
            let mark = self.scratch.len();
            self.target(e)?;
            self.push_charged(e)?;
            while self.accept(Tok::Comma)? {
                let t = self.suffixed()?;
                self.target(t)?;
                self.push_charged(t)?;
            }
            let targets = self.list_charged(mark);
            self.expect(Tok::Assign)?;
            let mark = self.scratch.len();
            self.exprlist()?;
            let exprs = self.list_from(mark)?;
            return self.stmt(Stmt::Assign { targets, exprs }, line);
        }
        if !matches!(self.ast.expr(e), Expr::Call { .. } | Expr::Method { .. }) {
            return Err(self.fail());
        }
        self.stmt(Stmt::Call(e), line)
    }

    /// Checks an assignment's target: a local (counted as assigned), a
    /// global, or a field.
    fn target(&mut self, e: u32) -> Result<(), Fail> {
        match self.ast.expr(e) {
            Expr::Local(d) => {
                if let Some(x) = self.ast.decls.get_mut(d as usize) {
                    x.assigned = true;
                }
                Ok(())
            }
            Expr::Global { .. } | Expr::Index { .. } => Ok(()),
            _ => Err(self.fail()),
        }
    }

    /// Entry `id` onto the scratch stack, charged as a list entry before
    /// it is pushed: a list of such entries is taken by
    /// [`Self::list_charged`].
    fn push_charged(&mut self, id: u32) -> Result<(), Fail> {
        self.charge()?;
        self.scratch.push(id);
        Ok(())
    }

    /// The scratch entries from `mark`, each already charged by
    /// [`Self::push_charged`], moved into a list.
    fn list_charged(&mut self, mark: usize) -> List {
        let n = self.scratch.len().saturating_sub(mark);
        let start = u32::try_from(self.ast.lists.len()).unwrap_or(NONE);
        self.ast.lists.extend(self.scratch.drain(mark..));
        (start, u32::try_from(n).unwrap_or(NONE))
    }

    /// A function body after its name: parameters, block and `end`.
    fn body(&mut self, form: Form, start: u32, method: bool, line: u32) -> Result<u32, Fail> {
        self.charge()?;
        let id = u32::try_from(self.ast.funcs.len()).unwrap_or(NONE);
        self.ast.funcs.push(Func {
            form,
            start,
            open: 0,
            end: 0,
            line,
            params: (0, 0),
            vararg: false,
            body: (0, 0),
            upvalues: (0, 0),
            unread: false,
            paths: (0, 0),
        });
        let base = self.active.len();
        self.funcs.push(FuncState {
            id,
            upvalues: Vec::new(),
            seen: HashSet::new(),
            unread: false,
            base,
            paths: Vec::new(),
        });
        let scope = self.active.len();
        self.expect(Tok::LParen)?;
        let open = self.tok.start;
        let mark = self.scratch.len();
        if method {
            // `self`, implicit: its name is not in the source.
            let d = self.declare((0, 0), DeclKind::SelfParam)?;
            self.bind(d)?;
            self.scratch.push(d);
        }
        let mut vararg = false;
        if !self.check(Tok::RParen) {
            loop {
                if self.accept(Tok::Ellipsis)? {
                    vararg = true;
                    break;
                }
                let t = self.expect(Tok::Name)?;
                let d = self.declare((t.start, t.end), DeclKind::Param)?;
                self.bind(d)?;
                self.scratch.push(d);
                if !self.accept(Tok::Comma)? {
                    break;
                }
            }
        }
        let params = self.list_from(mark)?;
        self.expect(Tok::RParen)?;
        let body = self.block()?;
        let end = self.expect(Tok::End)?;
        self.unbind_to(scope);
        let state = self.funcs.pop();
        let (ups, unread, paths) = state.map_or((Vec::new(), false, Vec::new()), |s| {
            (s.upvalues, s.unread, s.paths)
        });
        let mark = self.scratch.len();
        self.scratch.extend(ups.iter().copied());
        let upvalues = self.list_from(mark)?;
        // Its paths, each charged as it was added, kept together.
        let start = u32::try_from(self.ast.paths.len()).unwrap_or(NONE);
        let count = u32::try_from(paths.len()).unwrap_or(0);
        self.ast.paths.extend(paths);
        if let Some(f) = self.ast.funcs.get_mut(id as usize) {
            f.paths = (start, count);
            f.open = open;
            f.end = end.end;
            f.params = params;
            f.vararg = vararg;
            f.body = body;
            f.upvalues = upvalues;
            f.unread = unread;
        }
        // The enclosing function reads what this one reads from outside
        // it, to build this one's reader.
        for up in ups {
            self.use_binding(up);
        }
        Ok(id)
    }

    /// An expression list onto the scratch stack.
    fn exprlist(&mut self) -> Result<(), Fail> {
        loop {
            let e = self.expression()?;
            self.scratch.push(e);
            if !self.accept(Tok::Comma)? {
                return Ok(());
            }
        }
    }

    fn expression(&mut self) -> Result<u32, Fail> {
        self.subexpr(0)
    }

    fn binop(t: Tok) -> Option<(Bin, u8, u8)> {
        Some(match t {
            Tok::Plus => (Bin::Add, 10, 10),
            Tok::Minus => (Bin::Sub, 10, 10),
            Tok::Star => (Bin::Mul, 11, 11),
            Tok::Percent => (Bin::Mod, 11, 11),
            Tok::Caret => (Bin::Pow, 14, 13),
            Tok::Slash => (Bin::Div, 11, 11),
            Tok::SlashSlash => (Bin::IDiv, 11, 11),
            Tok::Amp => (Bin::BAnd, 6, 6),
            Tok::Pipe => (Bin::BOr, 4, 4),
            Tok::Tilde => (Bin::BXor, 5, 5),
            Tok::Shl => (Bin::Shl, 7, 7),
            Tok::Shr => (Bin::Shr, 7, 7),
            Tok::DotDot => (Bin::Concat, 9, 8),
            Tok::EqEq => (Bin::Eq, 3, 3),
            Tok::Lt => (Bin::Lt, 3, 3),
            Tok::Le => (Bin::Le, 3, 3),
            Tok::Ne => (Bin::Ne, 3, 3),
            Tok::Gt => (Bin::Gt, 3, 3),
            Tok::Ge => (Bin::Ge, 3, 3),
            Tok::And => (Bin::And, 2, 2),
            Tok::Or => (Bin::Or, 1, 1),
            _ => return None,
        })
    }

    /// `subexpr` with Lua's priorities: unary operators at 12.
    fn subexpr(&mut self, limit: u8) -> Result<u32, Fail> {
        self.enter()?;
        let r = self.subexpr_inner(limit);
        self.leave();
        r
    }

    fn subexpr_inner(&mut self, limit: u8) -> Result<u32, Fail> {
        let line = self.tok.line;
        let un = match self.tok.tok {
            Tok::Not => Some(Un::Not),
            Tok::Minus => Some(Un::Neg),
            Tok::Hash => Some(Un::Len),
            Tok::Tilde => Some(Un::BNot),
            _ => None,
        };
        let mut left = if let Some(op) = un {
            self.advance()?;
            let e = self.subexpr(12)?;
            self.expr(Expr::Unary(op, e), line)?
        } else {
            self.simple()?
        };
        while let Some((op, l, r)) = Self::binop(self.tok.tok) {
            if l <= limit {
                break;
            }
            let line = self.advance()?.line;
            let right = self.subexpr(r)?;
            left = self.expr(Expr::Binary(op, left, right), line)?;
        }
        Ok(left)
    }

    fn simple(&mut self) -> Result<u32, Fail> {
        let t = self.tok;
        let e = match t.tok {
            Tok::Int(n) | Tok::Wide(n) => Expr::Int(n),
            Tok::Float(b) => Expr::Float(b),
            Tok::Str(s) => Expr::Str(s),
            Tok::Nil => Expr::Nil,
            Tok::True => Expr::True,
            Tok::False => Expr::False,
            Tok::Ellipsis => Expr::Vararg,
            Tok::LBrace => return self.table(),
            Tok::Function => {
                self.advance()?;
                let func = self.body(Form::Expr, t.start, false, t.line)?;
                return self.expr(Expr::Func(func), t.line);
            }
            _ => return self.suffixed(),
        };
        self.advance()?;
        let id = self.expr(e, t.line)?;
        if matches!(t.tok, Tok::Wide(_)) {
            // One node, charged before it is recorded.
            self.charge_at("wide")?;
            #[cfg(test)]
            super::tests::site_did("wide");
            self.ast.wide_numerals.push(id);
        }
        Ok(id)
    }

    fn primary(&mut self) -> Result<u32, Fail> {
        let t = self.tok;
        match t.tok {
            Tok::Name => {
                self.advance()?;
                self.name_expr(t)
            }
            Tok::LParen => {
                self.advance()?;
                let e = self.expression()?;
                self.expect(Tok::RParen)?;
                self.expr(Expr::Paren(e), t.line)
            }
            _ => Err(self.fail()),
        }
    }

    /// A primary expression and its suffixes, each index by a constant
    /// string on a static access path a step of it.
    fn suffixed(&mut self) -> Result<u32, Fail> {
        let mut e = self.primary()?;
        let mut at = self.path_root(e)?;
        loop {
            let line = self.tok.line;
            match self.tok.tok {
                Tok::Dot => {
                    let dot = self.advance()?.start;
                    let n = self.expect(Tok::Name)?;
                    let s = self.field_name(n);
                    let key = self.expr(Expr::Str(s), n.line)?;
                    e = self.expr(Expr::Index { obj: e, key }, line)?;
                    self.last_dot = (e, dot);
                    at = self.path_step(at, s)?;
                }
                Tok::LBracket => {
                    self.advance()?;
                    let key = self.expression()?;
                    self.expect(Tok::RBracket)?;
                    e = self.expr(Expr::Index { obj: e, key }, line)?;
                    at = match self.ast.expr(key) {
                        Expr::Str(s) => self.path_step(at, s)?,
                        _ => PathAt::Off,
                    };
                }
                Tok::Colon => {
                    self.advance()?;
                    let n = self.expect(Tok::Name)?;
                    let name = self.field_name(n);
                    let args = self.args()?;
                    e = self.expr(Expr::Method { obj: e, name, args }, line)?;
                    at = PathAt::Off;
                }
                Tok::LParen | Tok::Str(_) | Tok::LBrace => {
                    let args = self.args()?;
                    e = self.expr(Expr::Call { callee: e, args }, line)?;
                    at = PathAt::Off;
                }
                _ => return Ok(e),
            }
        }
    }

    fn args(&mut self) -> Result<List, Fail> {
        let mark = self.scratch.len();
        match self.tok.tok {
            Tok::Str(s) => {
                let t = self.advance()?;
                let e = self.expr(Expr::Str(s), t.line)?;
                self.scratch.push(e);
            }
            Tok::LBrace => {
                let e = self.table()?;
                self.scratch.push(e);
            }
            Tok::LParen => {
                self.advance()?;
                if !self.check(Tok::RParen) {
                    self.exprlist()?;
                }
                self.expect(Tok::RParen)?;
            }
            _ => return Err(self.fail()),
        }
        self.list_from(mark)
    }

    fn table(&mut self) -> Result<u32, Fail> {
        let line = self.expect(Tok::LBrace)?.line;
        let mark = self.scratch.len();
        while !self.check(Tok::RBrace) {
            let named = self.check(Tok::Name) && self.peek()? == Tok::Assign;
            match self.tok.tok {
                Tok::LBracket => {
                    self.advance()?;
                    let k = self.expression()?;
                    self.expect(Tok::RBracket)?;
                    self.expect(Tok::Assign)?;
                    let v = self.expression()?;
                    self.scratch.push(k);
                    self.scratch.push(v);
                }
                Tok::Name if named => {
                    let n = self.advance()?;
                    self.advance()?;
                    let s = self.field_name(n);
                    let k = self.expr(Expr::Str(s), n.line)?;
                    let v = self.expression()?;
                    self.scratch.push(k);
                    self.scratch.push(v);
                }
                _ => {
                    let v = self.expression()?;
                    self.scratch.push(NONE);
                    self.scratch.push(v);
                }
            }
            if !self.accept(Tok::Comma)? && !self.accept(Tok::Semi)? {
                break;
            }
        }
        self.expect(Tok::RBrace)?;
        let fields = self.list_from(mark)?;
        self.expr(Expr::Table(fields), line)
    }
}

#[cfg(test)]
mod tests {
    use super::{CHUNK_ENV, Expr, Fail, NONE, Stmt, number, parse};

    fn ok(src: &str) -> super::Ast {
        let mut n = 0;
        parse(src.as_bytes(), &mut n, &|| false).unwrap_or_else(|e| panic!("{src}: {e:?}"))
    }

    #[test]
    fn numerals_take_lua_s_subtypes() {
        use super::Tok;
        let n = |t: &[u8]| number(t, &mut || Ok(())).unwrap();
        assert_eq!(n(b"10"), Some(Tok::Int(10)));
        assert_eq!(n(b"0x10"), Some(Tok::Int(16)));
        assert_eq!(n(b"0xffffffffffffffff"), Some(Tok::Wide(-1)));
        assert_eq!(n(b"9223372036854775807"), Some(Tok::Wide(i64::MAX)));
        // Past 2^53, the source's double holds another value.
        assert_eq!(n(b"9007199254740992"), Some(Tok::Int(1 << 53)));
        assert_eq!(n(b"9007199254740993"), Some(Tok::Wide((1 << 53) + 1)));
        assert_eq!(n(b"0x20000000000000"), Some(Tok::Int(1 << 53)));
        assert_eq!(n(b"0x20000000000001"), Some(Tok::Wide((1 << 53) + 1)));
        let float = |x: f64| Some(Tok::Float(x.to_bits()));
        assert_eq!(
            n(b"9223372036854775808"),
            float(9_223_372_036_854_775_808.0)
        );
        for (f, x) in [
            ("1.0", 1.0),
            ("1e5", 1e5),
            (".5", 0.5),
            ("5.", 5.0),
            ("0x1p4", 16.0),
            ("0x.8", 0.5),
            ("1e+5", 1e5),
            ("3E-2", 3e-2),
            ("0x1.8p-1", 0.75),
            ("0xA.8P1", 21.0),
            ("1e400", f64::INFINITY),
            ("0x1p1024", f64::INFINITY),
            ("0x1p-1074", f64::from_bits(1)),
            ("0x1p-1075", 0.0),
            ("0x1.8p-1075", f64::from_bits(1)),
            ("0x1p-1076", 0.0),
            // 2^53 + 1 and + 3: ties to even, down then up.
            ("0x20000000000001.", 9_007_199_254_740_992.0),
            ("0x20000000000003.", 9_007_199_254_740_996.0),
            // Past the kept digits, a nonzero digit breaks the tie up.
            (
                "0x20000000000001000000000000000001.",
                9_007_199_254_740_994.0 * 2f64.powi(72),
            ),
            ("0x0.0000001p28", 1.0),
            ("0x1.fffffffffffff8p0", 2.0),
        ] {
            assert_eq!(n(f.as_bytes()), float(x), "{f}");
        }
        for bad in ["1e", "0x", "1..2", "0x1e+", "1p4", "."] {
            assert_eq!(n(bad.as_bytes()), None, "{bad}");
        }
    }

    #[test]
    fn the_whole_syntax_parses() {
        ok("local a <const>, b <close> = 1, nil\n\
            goto x ::x:: do end while a do break end repeat local z = 1 until z\n\
            for i = 1, 2, 3 do end for k, v in pairs({}) do end\n\
            if a then elseif b then else end\n\
            function g.h:m(p, ...) return self, p, ... end\n\
            local function f() return f end\n\
            local t = { [1] = 2, x = 3, 4; 5, f\"s\", f{}, g.h:m(1) }\n\
            a = 1 // 2 ^ -3 .. 'x' & 1 | 2 ~ 3 << 4 >> 5 ~= #t and not a or ~b\n\
            local s = [==[\nlong]]x]==] .. \"\\z\n   \\x41\\u{48}\\65\\\n\" -- c\n--[[ c ]] return");
    }

    #[test]
    fn strings_decode_as_lua_does() {
        let a = ok("local s = \"a\\tb\\u{3042}\\x7a\\122\\z  \n c\" local l = [[\nx\r\ny]]");
        let strs: Vec<&[u8]> = (0..a.strs.len())
            .map(|i| a.str(u32::try_from(i).unwrap()))
            .collect();
        assert!(strs.contains(&"a\tb\u{3042}zzc".as_bytes()), "{strs:?}");
        assert!(strs.contains(&&b"x\ny"[..]), "{strs:?}");
    }

    #[test]
    fn names_resolve_to_their_scopes() {
        let a = ok("local x = 1\nlocal function f() return x end\nlocal x = x\ny = x");
        let f = a.func(1).unwrap();
        // `f` reads the first `x`.
        assert_eq!(a.list(f.upvalues), [0]);
        // The second `x`'s initializer reads the first.
        let second = a.decl(2).unwrap();
        assert_eq!(a.expr(second.init), Expr::Local(0));
        // A write in a nested function makes the local its upvalue.
        let a = ok("local m = 1\nm = 2\nlocal function h() m = 3 end");
        assert_eq!(a.list(a.func(1).unwrap().upvalues), [0]);
    }

    #[test]
    fn a_bare_env_is_the_chunk_s_cell() {
        // Read with no local `_ENV` in scope: the chunk's own upvalue, not
        // a global named `_ENV`; `_ENV.x` is the global `x`.
        let a = ok("local function f() return _ENV.x end\n_ENV = {}");
        let f = a.func(1).unwrap();
        assert_eq!(a.list(f.upvalues), [CHUNK_ENV]);
        let &[ret] = a.list(f.body) else { panic!() };
        let Stmt::Return(list) = a.stmt(ret) else {
            panic!()
        };
        let Expr::Index { obj, key } = a.expr(a.list(list)[0]) else {
            panic!()
        };
        assert_eq!(a.expr(obj), Expr::Local(CHUNK_ENV));
        assert!(matches!(a.expr(key), Expr::Str(s) if a.str(s) == b"x"));
        // Its one path step reads `x` from the cell.
        let &[p] = a.paths(f) else { panic!() };
        assert_eq!(
            (p.parent, p.root, a.str(p.key)),
            (NONE, CHUNK_ENV, &b"x"[..])
        );
        // Assigned, it is the cell again.
        let &[_, assign] = a.list(a.func(0).unwrap().body) else {
            panic!()
        };
        let Stmt::Assign { targets, .. } = a.stmt(assign) else {
            panic!()
        };
        assert_eq!(a.expr(a.list(targets)[0]), Expr::Local(CHUNK_ENV));
        // A local `_ENV` shadows it.
        let a = ok("local _ENV = {}\nlocal function g() return _ENV end");
        let g = a.func(1).unwrap();
        assert_eq!(a.list(g.upvalues), [0]);
    }

    #[test]
    fn statements_keep_their_parts() {
        let a = ok("local t = {}\n\
            t.a, t.b = 1, 2\n\
            if t then f() elseif t.a then elseif t.b then else end\n\
            while t do break end\n\
            repeat local z = 1 until z\n\
            for i = 1, 2 do end for i = 1, 2, 3 do end\n\
            for k, v in pairs(t) do end\n\
            do goto x end ::x::\n\
            local function h() end\n\
            function t.c:m() end\n\
            t:m()");
        let top = a.list(a.func(0).unwrap().body);
        let names: Vec<&str> = top.iter().map(|&s| a.stmt(s).name()).collect();
        assert_eq!(
            names,
            [
                "local",
                "assignment",
                "if",
                "while",
                "repeat",
                "numeric for",
                "numeric for",
                "generic for",
                "do",
                "label",
                "function",
                "function",
                "call statement"
            ]
        );
        let Stmt::Assign { targets, exprs } = a.stmt(top[1]) else {
            panic!()
        };
        assert_eq!((targets.1, exprs.1), (2, 2));
        // Four arms, the `else` arm's condition none.
        let Stmt::If(arms) = a.stmt(top[2]) else {
            panic!()
        };
        let arms = a.list(arms);
        assert_eq!(arms.len(), 12);
        assert_eq!(arms[9], NONE);
        assert_eq!(a.list((arms[1], arms[2])).len(), 1);
        let Stmt::NumericFor { step, .. } = a.stmt(top[5]) else {
            panic!()
        };
        assert_eq!(step, NONE);
        let Stmt::NumericFor { step, .. } = a.stmt(top[6]) else {
            panic!()
        };
        assert_ne!(step, NONE);
        let Stmt::GenericFor { names, exprs, .. } = a.stmt(top[7]) else {
            panic!()
        };
        assert_eq!((names.1, exprs.1), (2, 1));
        let Stmt::Do(body) = a.stmt(top[8]) else {
            panic!()
        };
        assert!(matches!(a.stmt(a.list(body)[0]), Stmt::Goto(s) if a.str(s) == b"x"));
        assert!(matches!(
            a.stmt(top[10]),
            Stmt::LocalFunction { func: 1, .. }
        ));
        // `function t.c:m()` targets `t.c.m`.
        let Stmt::Function { target, func: 2 } = a.stmt(top[11]) else {
            panic!()
        };
        let Expr::Index { key, .. } = a.expr(target) else {
            panic!()
        };
        assert!(matches!(a.expr(key), Expr::Str(s) if a.str(s) == b"m"));
    }

    #[test]
    fn static_paths_are_upvalue_and_global_chains_with_constant_keys() {
        let a = ok("local u = {}\n\
            local function f(p)\n\
              local l = {}\n\
              return u.a.b, g.c[\"d\"], p.x, l.y, u[p], u.a:m(), (u).z, h().w, u.a\n\
            end");
        let f = a.func(1).unwrap();
        let steps: Vec<(u32, u32, &[u8])> = a
            .paths(f)
            .iter()
            .map(|p| (p.parent, p.root, a.str(p.key)))
            .collect();
        // `u.a.b`, the global `g`, `.c`, `["d"]`, `u.a` before `:m`, and
        // `u.a` again; not through a parameter, a local of its own, a
        // dynamic key, parentheses or a call.
        assert_eq!(
            steps,
            [
                (NONE, 0, &b"a"[..]),
                (0, NONE, b"b"),
                (NONE, CHUNK_ENV, b"g"),
                (2, NONE, b"c"),
                (3, NONE, b"d"),
                (NONE, 0, b"a"),
                (NONE, CHUNK_ENV, b"h"),
                (NONE, 0, b"a"),
            ]
        );
        // The chunk, which no root is, records none.
        assert!(a.paths(a.func(0).unwrap()).is_empty());
        // A function statement reads every name of its target but the
        // last, which it assigns.
        let a = ok("local t = {}\nlocal function f() function t.c.d:m() end function g() end end");
        let steps: Vec<(u32, u32, &[u8])> = a
            .paths(a.func(1).unwrap())
            .iter()
            .map(|p| (p.parent, p.root, a.str(p.key)))
            .collect();
        assert_eq!(steps, [(NONE, 0, &b"c"[..]), (0, NONE, b"d")]);
    }

    #[test]
    fn lines_are_counted_as_lua_counts_them() {
        let a = ok("local a = 1\r\nlocal b = 2\n\rlocal c = 3\rlocal d = [[\n\n]] local e = 4");
        let lines: Vec<u32> = a.stmt_lines.clone();
        assert_eq!(lines, [1, 2, 3, 4, 6]);
    }

    #[test]
    fn depth_and_nodes_are_capped() {
        let deep = format!("return {}1{}", "(".repeat(300), ")".repeat(300));
        let mut n = 0;
        assert!(matches!(
            parse(deep.as_bytes(), &mut n, &|| false),
            Err(Fail::Depth(1))
        ));
        let mut n = super::MAX_AST_NODES - 3;
        assert_eq!(
            parse(b"local a = 1 + 2 + 3", &mut n, &|| false).err(),
            Some(Fail::Nodes)
        );
    }

    #[test]
    fn the_lexer_sees_cancellation_inside_a_long_token() {
        // One token of a mebibyte each, with almost no nodes: only the
        // lexer's byte count can see the request.
        let mib = 1 << 20;
        let zeros = "0".repeat(mib);
        let eqs = "=".repeat(mib);
        let spaces = " ".repeat(mib);
        let letters = "a".repeat(mib);
        for src in [
            format!("local s = \"\\u{{{zeros}41}}\""),
            format!("local s = \"{letters}\""),
            format!("local s = [[{letters}]]"),
            format!("--[[{letters}]]"),
            format!("-- {letters}"),
            format!("--[{eqs}"),
            format!("local s = [{eqs}[x]{eqs}]"),
            format!("{spaces}return"),
            format!("local n = {zeros}"),
            format!("local {letters} = 1"),
        ] {
            let mut n = 0;
            let r = parse(src.as_bytes(), &mut n, &|| true);
            assert_eq!(r.err(), Some(Fail::Cancelled), "{}", &src[..12]);
            let mut n = 0;
            assert!(
                parse(src.as_bytes(), &mut n, &|| false).is_ok(),
                "{}",
                &src[..12]
            );
        }
    }

    #[test]
    fn names_resolve_in_constant_time_and_locals_are_capped() {
        // 80 nested functions of 199 locals each, about 16 000 in scope,
        // then 100 000 assignments of a name none declares: each read is
        // a hash of the name, not a scan of the scope.
        let locals: String = (0..199).map(|i| format!("local a{i} = 0 ")).collect();
        let mut src = String::new();
        for _ in 0..80 {
            src.push_str(&locals);
            src.push_str("local function f() ");
        }
        src.push_str(&"b = z ".repeat(100_000));
        src.push_str(&"end ".repeat(80));
        let start = std::time::Instant::now();
        ok(&src);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        // Lua's own cap on a function's locals.
        let over: String = (0..201).map(|i| format!("local a{i} = 0 ")).collect();
        let mut n = 0;
        assert!(matches!(
            parse(over.as_bytes(), &mut n, &|| false),
            Err(Fail::Syntax(_))
        ));
        // A nested function counts its own.
        let nested = format!(
            "{} local function f() {} end",
            "local x = 0 ".repeat(150),
            "local y = 0 ".repeat(150)
        );
        ok(&nested);
    }

    #[test]
    fn a_bracket_that_opens_no_long_comment_is_a_short_one() {
        let a = ok("--[= short comment\nlocal x = 1 --[==x\nlocal y = 2 --[\n");
        assert_eq!(a.decls.len(), 2);
    }

    #[test]
    fn malformed_sources_are_refused() {
        // A bracket that opens no long string is an error.
        for bad in [
            "local s = [=x",
            "local",
            "x =",
            "f() = 1",
            "return 1 return 2",
            "local s = 'a\nb'",
            "local s = [=[x]]",
            "a.b:c = 1",
            "local x <other> = 1",
            "x = 1e",
            "x = 3a",
            "@",
        ] {
            let mut n = 0;
            assert!(
                matches!(
                    parse(bad.as_bytes(), &mut n, &|| false),
                    Err(Fail::Syntax(_))
                ),
                "{bad}"
            );
        }
    }
}
