//! The play presentation program (design.md "beatoraja Lua Skins", the
//! program; docs/stage3.md S7-06): a skin's per-frame callbacks that no
//! expression represents, compiled at import into typed register code and
//! run each play frame without Lua.
//!
//! A [`PresentDef`] lists its callbacks in the source's object order, each
//! a span of [`Insn`]s over at most [`MAX_REGISTERS`] registers holding a
//! [`Val`] (nil, a boolean, a Lua integer or a Lua float), with the
//! expressions it reads ([`ReadDef`]), its consumer's conversion
//! ([`Conv`]) and its gating: a `gate`, the source's draw conditions
//! evaluated before it, and `after`, the number value it follows. Callback
//! `k`'s result is output `lua.<k>` and whether it gave one `lua.<k>.ok`,
//! two slots after the schema's fields that consumers read through a
//! [`crate::Scope`] with [`OutTy`] outputs.
//!
//! [`Present::verify`] checks a definition against the caps (rules V1 to
//! V5 in [`verify`]; V6, the consumers' reads, is checked by
//! [`crate::Skin::new`] over the skin's expressions), and
//! [`Present::run`] runs it ([`exec`]); its numerics are the source's
//! interpreter's, LuaJ's doubles ([`num`]), shared with the importer's
//! folding. These floats are
//! presentation only and never reach the evaluator.

mod exec;
pub mod num;
mod verify;

use crate::expr::Program;

pub use crate::output::OutTy;
#[cfg(feature = "dev-tools")]
pub use exec::invocations;
pub use verify::{PresentError, callback_cost};

/// The most instructions in one callback ([`crate::MAX_OPS`]).
pub const MAX_CALLBACK_OPS: usize = crate::expr::MAX_OPS;
/// The most registers a callback uses ([`crate::MAX_STACK`]).
pub const MAX_REGISTERS: usize = crate::expr::MAX_STACK;
/// The most callbacks in one program ([`crate::MAX_ELEMENTS`]): every
/// output slot, two a callback past the play schema, fits `u16`.
pub const MAX_CALLBACKS: usize = crate::MAX_ELEMENTS;

/// A value in a register.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Val {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
}

/// Why an operation gave no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// A Lua runtime error the source raises too.
    Error,
    /// Integer arithmetic that would wrap; the compiler proves accepted
    /// code never reaches it.
    Overflow,
    /// A read whose availability guard is false: unknown state.
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    /// `/`, always a float.
    Div,
    /// `//`.
    IDiv,
    /// `%`.
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathFn {
    Floor,
    Ceil,
    Abs,
}

/// One instruction; registers are `u8`, constants and reads `u16`
/// indices, and jump targets offsets within the callback's span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insn {
    Const {
        dst: u8,
        k: u16,
    },
    /// Read `read` evaluated: its guard, then its value.
    Read {
        dst: u8,
        read: u16,
    },
    Move {
        dst: u8,
        src: u8,
    },
    Arith {
        op: Arith,
        dst: u8,
        a: u8,
        b: u8,
    },
    Neg {
        dst: u8,
        a: u8,
    },
    Cmp {
        op: Cmp,
        dst: u8,
        a: u8,
        b: u8,
    },
    /// Lua's `not`.
    Not {
        dst: u8,
        a: u8,
    },
    Math {
        f: MathFn,
        dst: u8,
        a: u8,
    },
    /// `math.max(a, b)` (`max`) or `math.min(a, b)`.
    Extreme {
        max: bool,
        dst: u8,
        a: u8,
        b: u8,
    },
    Jump {
        to: u16,
    },
    /// Jumps to `to` when `cond`'s truthiness is `when`.
    Test {
        cond: u8,
        when: bool,
        to: u16,
    },
    Ret {
        src: u8,
    },
}

/// An expression the program reads, over the play schema: its value, an
/// integer or a boolean, and its availability guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadDef {
    pub value: Program,
    pub guard: Option<Program>,
}

/// How a callback's consumer converts its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conv {
    /// A `draw`: Lua's truthiness.
    Truthy,
    /// A number's `value`: the source's `toint()`.
    Int,
}

/// One callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallbackDef {
    /// Its span in [`PresentDef::code`].
    pub start: u32,
    pub len: u16,
    /// The registers it uses.
    pub regs: u8,
    pub conv: Conv,
    /// A boolean read: the source's conditions evaluated before it; false
    /// skips it.
    pub gate: Option<u16>,
    /// An earlier `Int` callback: this one runs only when that number gave
    /// a value.
    pub after: Option<u16>,
}

impl OutTy {
    /// The output type a conversion gives.
    #[must_use]
    pub const fn of(conv: Conv) -> Self {
        match conv {
            Conv::Truthy => Self::Bool,
            Conv::Int => Self::Int,
        }
    }
}

/// A program as the importer builds it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresentDef {
    pub code: Vec<Insn>,
    pub consts: Vec<Val>,
    pub reads: Vec<ReadDef>,
    /// In the source's order.
    pub callbacks: Vec<CallbackDef>,
    /// Per callback, its output's type ([`OutTy::of`] its conversion).
    pub signature: Vec<OutTy>,
}

/// A verified program, ready to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Present {
    def: PresentDef,
}

impl Present {
    /// The number of callbacks: the program writes `2 * len` output slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.def.callbacks.len()
    }

    /// Whether it has no callback.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.def.callbacks.is_empty()
    }

    /// Its output signature.
    #[must_use]
    pub fn signature(&self) -> &[OutTy] {
        &self.def.signature
    }
}
