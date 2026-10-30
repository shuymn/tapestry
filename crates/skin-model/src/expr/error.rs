//! Compile errors and their source spans.

use super::{MAX_NAME_BYTES, MAX_NESTING, MAX_OPS, MAX_SKIN_OPS, MAX_SOURCE_BYTES, MAX_STACK};
use crate::schema::{SchemaError, Ty};

/// Byte offsets into an expression's source, `start..end`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// The span `start..end`; offsets beyond `u32` saturate (sources are
    /// bounded far below that).
    pub(crate) fn new(start: usize, end: usize) -> Self {
        Self {
            start: u32::try_from(start).unwrap_or(u32::MAX),
            end: u32::try_from(end).unwrap_or(u32::MAX),
        }
    }

    /// The source text this span covers.
    pub(crate) fn text(self, src: &str) -> &str {
        let start = usize::try_from(self.start).unwrap_or(usize::MAX);
        let end = usize::try_from(self.end).unwrap_or(usize::MAX);
        src.get(start..end).unwrap_or("")
    }

    /// The smallest span covering both.
    pub(crate) fn to(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// The first error found while compiling an expression, and where.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind}")]
pub struct CompileError {
    pub span: Span,
    pub kind: ErrorKind,
}

impl CompileError {
    pub(crate) fn new(span: Span, kind: ErrorKind) -> Self {
        Self { span, kind }
    }
}

/// What went wrong. Messages name the offending identifier but quote no
/// other source; rendering the source around a span is the caller's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("the expression is {len} bytes long; the limit is {MAX_SOURCE_BYTES}")]
    TooLong { len: usize },
    #[error("the expression nests deeper than {MAX_NESTING} levels")]
    TooDeep,
    #[error("the expression compiles to {ops} operations; the limit is {MAX_OPS}")]
    TooManyOps { ops: usize },
    #[error("the expression needs {depth} stack slots; the limit is {MAX_STACK}")]
    StackTooDeep { depth: usize },
    #[error("the skin's expressions exceed {MAX_SKIN_OPS} operations in total")]
    TooManySkinOps,
    #[error("unexpected character {0:?}")]
    UnexpectedChar(char),
    #[error("expected {what}")]
    Expected { what: &'static str },
    #[error("comparisons do not chain; add parentheses")]
    NonAssociative,
    #[error("the integer is too large")]
    IntTooLarge,
    #[error("a duration is whole milliseconds (`120ms`) or seconds with up to 3 decimals (`1.5s`)")]
    BadDuration,
    #[error("the name is longer than {MAX_NAME_BYTES} bytes")]
    NameTooLong,
    #[error("unknown name `{name}`")]
    UnknownName { name: Box<str> },
    #[error("unknown function `{name}`")]
    UnknownFunction { name: Box<str> },
    #[error("unknown timer `{name}`")]
    UnknownTimer { name: Box<str> },
    #[error("`{name}` is a field, not a timer")]
    NotATimer { name: Box<str> },
    #[error("unknown option `{name}`")]
    UnknownOption { name: Box<str> },
    #[error("option `{option}` has no choice `{choice}`")]
    UnknownChoice { option: Box<str>, choice: Box<str> },
    #[error("`{name}` is not a variant of the compared enum")]
    UnknownVariant { name: Box<str> },
    #[error("`{func}` takes {expected} argument(s), found {found}")]
    Arity {
        func: &'static str,
        expected: u8,
        found: usize,
    },
    #[error("expected {expected:?}, found {found:?}")]
    Type { expected: Ty, found: Ty },
    #[error("expected an integer, found {found:?}; only integers are ordered")]
    NotOrdered { found: Ty },
    #[error("the value can reach [{lo}, {hi}], outside the 32-bit integer range")]
    OutOfRange { lo: i64, hi: i64 },
    #[error("the schema is invalid: {0}")]
    BadSchema(#[from] SchemaError),
}
