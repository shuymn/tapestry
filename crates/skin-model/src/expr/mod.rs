//! The expression language: conditions and values a skin computes from
//! state.
//!
//! Types are `bool`, `int` and enums. Every integer subexpression is
//! range-checked at compile time against the declared ranges of what it
//! reads, so no value leaves `i32` at run time; `/` and `%` floor and give 0
//! for a zero divisor. [`compile`] runs a hand-written lexer and Pratt
//! parser, resolves names against a [`Scope`], checks types and ranges on
//! the unfolded tree (so every option configuration is valid), folds
//! options and constants, and emits a postfix [`Program`] whose length and
//! stack depth are bounded.

#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod arith;
mod check;
mod emit;
mod error;
mod fold;
mod interval;
mod lex;
mod parse;
mod program;

pub use error::{CompileError, ErrorKind, Span};
pub(crate) use lex::is_tail;
pub use program::{Program, State};

use crate::schema::{Schema, Ty};

/// The longest expression source, in bytes, checked before lexing.
pub const MAX_SOURCE_BYTES: usize = 1024;
/// The longest name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;
/// The deepest nesting. It bounds two things:
/// - The operator height of the syntax tree: the operators on its longest
///   root-to-leaf path, with a call counting one and `clamp` counting two
///   above its first two arguments (it is checked as `min(max(x, l), h)`).
///   So 64 nested unary operators or calls, 32 nested `clamp`s, or a
///   65-term chain of binary operators.
/// - The parser's recursion: each binary operator's right operand,
///   parenthesis, unary operand, call argument list and `if` counts one
///   level while it is parsed. So 64 nested parentheses, or 32 of
///   `1 + (…)`.
pub const MAX_NESTING: usize = 64;
/// The most postfix operations per expression, after folding.
pub const MAX_OPS: usize = 256;
/// The deepest evaluation stack, in slots.
pub const MAX_STACK: usize = 32;
/// The most operations across one skin's expressions; see [`SkinOps`].
pub const MAX_SKIN_OPS: usize = 1 << 16;
/// The largest elapsed time a timer reads, in ms (4 h): a chart's time
/// range plus its READY state fits, and `elapsed(t) * 100` stays in `i32`.
pub const MAX_ELAPSED_MS: i32 = 14_400_000;
/// A timer's value while it is off; any negative value reads as off.
pub const TIMER_OFF: i32 = -1;

/// Names a schema may not use: the built-ins and keywords.
pub(crate) const RESERVED: &[&str] = &[
    "min", "max", "clamp", "abs", "on", "elapsed", "option", "if", "then", "else", "true", "false",
];

/// A skin option, as configured for the skin being loaded. An option test
/// folds to a constant.
#[derive(Clone, Copy, Debug)]
pub enum OptionDef<'a> {
    /// `option(name)` is `on`.
    Toggle { name: &'a str, on: bool },
    /// `option(name, choice)` is whether `choice` is `choices[chosen]`.
    OneOf {
        name: &'a str,
        choices: &'a [&'a str],
        chosen: usize,
    },
}

/// What names an expression may use: a schema's fields and timers, and the
/// skin's options.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    schema: &'a Schema,
    options: &'a [OptionDef<'a>],
}

impl<'a> Scope<'a> {
    /// A scope over `schema` with no options.
    #[must_use]
    pub fn new(schema: &'a Schema) -> Self {
        Self {
            schema,
            options: &[],
        }
    }

    /// This scope with `options`; the first option of a name wins.
    #[must_use]
    pub fn options(self, options: &'a [OptionDef<'a>]) -> Self {
        Self { options, ..self }
    }
}

/// Compiles one expression whose result must have type `want`. Reports
/// the first error.
pub fn compile(src: &str, scope: &Scope<'_>, want: Ty) -> Result<Program, CompileError> {
    if src.len() > MAX_SOURCE_BYTES {
        return Err(CompileError::new(
            Span::new(0, src.len()),
            ErrorKind::TooLong { len: src.len() },
        ));
    }
    let tokens = lex::lex(src)?;
    let ast = parse::parse(src, &tokens)?;
    let root = ast.span(ast.root());
    let typed = check::check(&ast, scope, want)?;
    emit::emit(&fold::fold(typed), want, root)
}

/// The running operation total of one skin's programs, bounded by
/// [`MAX_SKIN_OPS`]. The container that holds a skin's programs charges
/// each one as it is compiled.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkinOps(usize);

impl SkinOps {
    /// Adds `program`'s operations, or refuses them with
    /// [`ErrorKind::TooManySkinOps`] (adding nothing) when the total would
    /// pass [`MAX_SKIN_OPS`]. The error's span is empty: the program's
    /// source is the caller's.
    pub fn charge(&mut self, program: &Program) -> Result<(), CompileError> {
        let total = self.0.saturating_add(program.op_count());
        if total > MAX_SKIN_OPS {
            return Err(CompileError::new(
                Span::default(),
                ErrorKind::TooManySkinOps,
            ));
        }
        self.0 = total;
        Ok(())
    }

    /// The operations charged so far.
    #[must_use]
    pub fn total(self) -> usize {
        self.0
    }
}
