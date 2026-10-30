//! Compiled programs and their evaluator, which runs every frame.
//!
//! A program is a postfix list of pure, total operations on a fixed array
//! stack. Compilation bounds its stack depth and proves that no integer
//! leaves `i32` while every read lies in its declared range; reads are
//! clamped into that range, so the proof holds whatever the caller passes.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::arith::{floor_div, floor_mod};
use super::{MAX_ELAPSED_MS, MAX_STACK, TIMER_OFF};
use crate::schema::Ty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unary {
    Neg,
    Abs,
    /// On a bool (0 or 1).
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Binary {
    Add,
    Sub,
    Mul,
    /// Floor division; 0 for a zero divisor.
    Div,
    /// Floor modulo; 0 for a zero divisor.
    Rem,
    Min,
    Max,
    Eq,
    Ne,
    Lt,
    Le,
    /// On bools (0 or 1).
    And,
    /// On bools (0 or 1).
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Const(i32),
    /// Reads field `slot`, clamped into `[lo, hi]`; a missing slot reads
    /// `lo`.
    Load {
        slot: u16,
        lo: i32,
        hi: i32,
    },
    /// Timer `slot`'s elapsed ms, clamped into `[-1, MAX_ELAPSED_MS]`;
    /// negative or missing reads as off (-1).
    Elapsed(u16),
    /// Whether timer `slot` is on.
    On(u16),
    Un(Unary),
    Bin(Binary),
    /// Pops `b`, `a`, `c`; pushes `a` if `c != 0`, else `b`.
    Select,
}

/// One operator applied to a value; the evaluator and constant folding
/// share it, so folding cannot diverge from evaluation.
pub(crate) fn unary(op: Unary, a: i32) -> i32 {
    match op {
        Unary::Neg => a.wrapping_neg(),
        Unary::Abs => a.wrapping_abs(),
        Unary::Not => a ^ 1,
    }
}

/// One operator applied to two values; see [`unary`]. The wrapping forms
/// never wrap on a compiled program; they keep this function total.
pub(crate) fn binary(op: Binary, a: i32, b: i32) -> i32 {
    match op {
        Binary::Add => a.wrapping_add(b),
        Binary::Sub => a.wrapping_sub(b),
        Binary::Mul => a.wrapping_mul(b),
        Binary::Div => floor_div(a, b),
        Binary::Rem => floor_mod(a, b),
        Binary::Min => a.min(b),
        Binary::Max => a.max(b),
        Binary::Eq => i32::from(a == b),
        Binary::Ne => i32::from(a != b),
        Binary::Lt => i32::from(a < b),
        Binary::Le => i32::from(a <= b),
        Binary::And => a & b,
        Binary::Or => a | b,
    }
}

/// A compiled expression. Only [`compile`](super::compile) makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    ops: Box<[Op]>,
    ty: Ty,
    stack: u8,
}

/// The state a program reads, indexed by slot.
#[derive(Clone, Copy, Debug)]
pub struct State<'a> {
    /// Per field slot: 0 or 1 for a bool, the variant index for an enum.
    pub fields: &'a [i32],
    /// Per timer slot: elapsed ms (`>= 0`) while on, [`TIMER_OFF`] while
    /// off.
    pub timers: &'a [i32],
}

impl Program {
    pub(crate) fn new(ops: Box<[Op]>, ty: Ty, stack: u8) -> Self {
        Self { ops, ty, stack }
    }

    /// The result type.
    #[must_use]
    pub fn ty(&self) -> Ty {
        self.ty
    }

    /// The operations run per evaluation: the per-frame cost.
    #[must_use]
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// The value of a program folded to one constant at compile time.
    pub(crate) fn constant(&self) -> Option<i32> {
        match *self.ops {
            [Op::Const(c)] => Some(c),
            _ => None,
        }
    }

    /// The stack slots evaluation uses.
    #[must_use]
    pub fn stack_depth(&self) -> usize {
        usize::from(self.stack)
    }

    /// Evaluates the program: a bool is 0 or 1, an enum its variant index.
    /// Allocates nothing and never panics.
    #[must_use]
    pub fn eval(&self, state: &State<'_>) -> i32 {
        let mut st = Stack {
            v: [0; MAX_STACK],
            sp: 0,
        };
        for op in &self.ops {
            match *op {
                Op::Const(c) => st.push(c),
                Op::Load { slot, lo, hi } => {
                    let v = state.fields.get(usize::from(slot)).copied();
                    st.push(v.map_or(lo, |v| v.max(lo).min(hi)));
                }
                Op::Elapsed(t) => st.push(timer(state, t)),
                Op::On(t) => st.push(i32::from(timer(state, t) >= 0)),
                Op::Un(u) => {
                    let a = st.pop();
                    st.push(unary(u, a));
                }
                Op::Bin(b) => {
                    let rhs = st.pop();
                    let lhs = st.pop();
                    st.push(binary(b, lhs, rhs));
                }
                Op::Select => {
                    let b = st.pop();
                    let a = st.pop();
                    let c = st.pop();
                    st.push(if c != 0 { a } else { b });
                }
            }
        }
        st.pop()
    }

    /// [`eval`](Self::eval) as a bool: whether it is not 0.
    #[must_use]
    pub fn eval_bool(&self, state: &State<'_>) -> bool {
        self.eval(state) != 0
    }
}

/// Timer `t`'s reading: clamped elapsed ms, or [`TIMER_OFF`].
fn timer(state: &State<'_>, t: u16) -> i32 {
    match state.timers.get(usize::from(t)) {
        Some(&v) if v >= 0 => v.min(MAX_ELAPSED_MS),
        _ => TIMER_OFF,
    }
}

/// The evaluation stack. Compilation bounds the depth by [`MAX_STACK`] and
/// balances every pop with a push, so neither access misses; a miss would
/// drop the push or read 0.
struct Stack {
    v: [i32; MAX_STACK],
    sp: usize,
}

impl Stack {
    fn push(&mut self, x: i32) {
        debug_assert!(self.sp < MAX_STACK, "the compiled stack depth is exceeded");
        if let Some(slot) = self.v.get_mut(self.sp) {
            *slot = x;
            self.sp += 1;
        }
    }

    fn pop(&mut self) -> i32 {
        debug_assert!(self.sp > 0, "a pop without a push");
        let Some(top) = self.sp.checked_sub(1) else {
            return 0;
        };
        self.sp = top;
        self.v.get(top).copied().unwrap_or(0)
    }
}
