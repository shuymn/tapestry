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
//! [`crate::Scope`] with [`OutTy`] outputs. A timer callback
//! ([`Conv::Timer`]) gives a timer instead, which an element names as
//! [`crate::TimerRef::Output`]: its start, `None` while off.
//!
//! The program's state is its slots ([`SlotDef`]): scalar values a
//! callback loads and stores, each within its domain ([`Domain`]), that
//! live across frames in the attempt's bank ([`Bank`]) and that no
//! expression reads. `main_state.time()` is [`Insn::StateUs`], the
//! frame's state clock.
//!
//! [`Present::verify`] checks a definition against the caps (rules V1 to
//! V8 in [`verify`]; V6, the consumers' reads, and V8's element timers are
//! checked by [`crate::Skin::new`] over the skin's expressions), and
//! [`Present::run`] runs it ([`exec`]); its numerics are the source's
//! interpreter's, LuaJ's doubles ([`num`]), shared with the importer's
//! folding. These floats are
//! presentation only and never reach the evaluator.

mod exec;
pub mod num;
#[cfg(feature = "dev-tools")]
pub mod trace;
mod verify;

use crate::expr::Program;

pub use crate::output::OutTy;
#[cfg(feature = "dev-tools")]
pub use exec::invocations;
pub use exec::{Io, elapsed_ms};
pub use verify::{PresentError, callback_cost};

/// The most instructions in one callback ([`crate::MAX_OPS`]).
pub const MAX_CALLBACK_OPS: usize = crate::expr::MAX_OPS;
/// The most registers a callback uses ([`crate::MAX_STACK`]).
pub const MAX_REGISTERS: usize = crate::expr::MAX_STACK;
/// The most callbacks in one program ([`crate::MAX_ELEMENTS`]): every
/// output slot, two a callback past the play schema, fits `u16`.
pub const MAX_CALLBACKS: usize = crate::MAX_ELEMENTS;
/// The most slots in one program.
pub const MAX_SLOTS: usize = 256;
/// The source's timer off value, `Long.MIN_VALUE`, as the double its
/// interpreter holds: a slot's off state.
pub const OFF: Val = Val::Float(-9_223_372_036_854_775_808.0);

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
    /// The frame's state clock, `state_us`: the source's
    /// `main_state.time()`.
    StateUs {
        dst: u8,
    },
    /// Slot `slot`'s value.
    LoadSlot {
        dst: u8,
        slot: u16,
    },
    /// Stores register `src` into slot `slot`; `domain` is the value's
    /// domain as the compiler proved it, an index of
    /// [`PresentDef::domains`].
    StoreSlot {
        slot: u16,
        src: u8,
        domain: u16,
    },
}

/// The values a slot or a stored value may hold: the off value ([`OFF`])
/// and an integer interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    pub off: bool,
    /// Inclusive bounds, each within ±2^53.
    pub ints: Option<(i64, i64)>,
}

impl Domain {
    /// Whether `v` is in the domain.
    #[must_use]
    pub fn holds(self, v: Val) -> bool {
        match v {
            Val::Int(n) => self.ints.is_some_and(|(lo, hi)| lo <= n && n <= hi),
            Val::Float(f) => self.off && f.to_bits() == off_bits(),
            Val::Nil | Val::Bool(_) => false,
        }
    }

    /// Whether every value of `other` is in the domain.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        (!other.off || self.off)
            && other
                .ints
                .is_none_or(|(lo, hi)| self.ints.is_some_and(|(a, b)| a <= lo && hi <= b))
    }
}

/// The bits of [`OFF`].
fn off_bits() -> u64 {
    (-9_223_372_036_854_775_808.0_f64).to_bits()
}

/// One slot: its value when the attempt is prepared, and its domain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotDef {
    pub init: Val,
    pub domain: Domain,
}

/// The slots of one attempt, prepared with the attempt and kept by a
/// rearm.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bank {
    vals: Box<[Val]>,
}

impl Bank {
    /// The values the slots hold now.
    #[must_use]
    pub fn values(&self) -> &[Val] {
        &self.vals
    }

    /// A bank holding `vals`, which a program takes only when it
    /// [`fits`](Present::fits).
    #[must_use]
    pub fn of(vals: Vec<Val>) -> Self {
        Self {
            vals: vals.into_boxed_slice(),
        }
    }

    /// Takes `other`'s values, reusing this bank's storage when the
    /// lengths match.
    pub fn copy_from(&mut self, other: &Self) {
        if self.vals.len() == other.vals.len() {
            self.vals.copy_from_slice(&other.vals);
        } else {
            self.vals.clone_from(&other.vals);
        }
    }
}

/// What a frame gives the program beside the fields and timers: the state
/// clock and the chart time the play's timers start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    /// µs since the play state began (`SkinFrame::state_us`).
    pub state_us: i64,
    /// The frame's chart time, which a timer output's start is placed
    /// against.
    pub now: bms_model::TimeUs,
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
    /// A destination's `timer`: the source's `tolong()`, run once to see
    /// whether it is on and again for its start.
    Timer,
    /// A slider's `value`: the source's `tofloat()` ([`num::to_float`]),
    /// stored as the `f32`'s bits.
    Float,
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
    /// An earlier callback this one follows: a number's value (`Int`),
    /// when it gave one; a `draw` (`Truthy`), when it gave true; a timer,
    /// when it was on.
    pub after: Option<u16>,
}

impl OutTy {
    /// The output type a conversion gives.
    #[must_use]
    pub const fn of(conv: Conv) -> Self {
        match conv {
            Conv::Truthy => Self::Bool,
            Conv::Int => Self::Int,
            Conv::Timer => Self::Timer,
            Conv::Float => Self::Float,
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
    pub slots: Vec<SlotDef>,
    /// The stores' domains ([`Insn::StoreSlot`]).
    pub domains: Vec<Domain>,
}

/// A verified program, ready to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Present {
    def: PresentDef,
    /// Per callback, whether it loads or stores a slot.
    slotted: Box<[bool]>,
    /// Whether its slotted callbacks may run on a carried state: each has
    /// no gate and no `after`, and every read it makes (value and guard)
    /// loads only the point ([`Present::carry_safe`]).
    carry_safe: bool,
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

    /// Whether its slotted callbacks may run on a state carried from an
    /// earlier attempt of the play state: each has no gate and no `after`,
    /// and every read it makes, value and availability guard, loads only
    /// the point (`field::POINT`), the one field whose cached value the
    /// carry reproduces. The clock (`Insn::StateUs`) is no read. Any other
    /// field, an output or a timer refuses it.
    #[must_use]
    pub fn carry_safe(&self) -> bool {
        self.carry_safe
    }

    /// Per callback, whether it loads or stores a slot (dev-tools).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub fn slotted(&self) -> &[bool] {
        &self.slotted
    }

    /// Whether `bank` fits the program's slots: one value a slot, each in
    /// its slot's domain (V7's rule for initial values).
    #[must_use]
    pub fn fits(&self, bank: &Bank) -> bool {
        bank.vals.len() == self.def.slots.len()
            && self
                .def
                .slots
                .iter()
                .zip(&bank.vals)
                .all(|(s, &v)| s.domain.holds(v))
    }

    /// The slots at their initial values: a fresh attempt's.
    #[must_use]
    pub fn bank(&self) -> Bank {
        Bank {
            vals: self.def.slots.iter().map(|s| s.init).collect(),
        }
    }
}
