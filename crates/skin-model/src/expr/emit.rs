//! Postfix emission, with no jumps: every operation is pure and total, so
//! both sides of `&&`, `||` and `if` are evaluated, and the per-frame cost
//! is exactly the operation count.

use super::check::T;
use super::error::{CompileError, ErrorKind, Span};
use super::program::{Op, Program};
use super::{MAX_OPS, MAX_STACK};
use crate::schema::Ty;

/// Emits `t`, then refuses it past [`MAX_OPS`] operations or
/// [`MAX_STACK`] stack slots; both errors carry the root span.
pub(super) fn emit(t: &T, ty: Ty, root: Span) -> Result<Program, CompileError> {
    let mut e = Emitter {
        ops: Vec::new(),
        depth: 0,
        max: 0,
    };
    e.node(t);
    if e.ops.len() > MAX_OPS {
        return Err(CompileError::new(
            root,
            ErrorKind::TooManyOps { ops: e.ops.len() },
        ));
    }
    let stack = u8::try_from(e.max)
        .ok()
        .filter(|_| e.max <= MAX_STACK)
        .ok_or_else(|| CompileError::new(root, ErrorKind::StackTooDeep { depth: e.max }))?;
    Ok(Program::new(e.ops.into_boxed_slice(), ty, stack))
}

struct Emitter {
    ops: Vec<Op>,
    depth: usize,
    max: usize,
}

impl Emitter {
    /// Pushes `op`, which pops `arity` values and pushes one.
    fn op(&mut self, op: Op, arity: usize) {
        self.ops.push(op);
        self.depth = self.depth + 1 - arity;
        self.max = self.max.max(self.depth);
    }

    fn node(&mut self, t: &T) {
        match t {
            T::Const(c) => self.op(Op::Const(*c), 0),
            T::Read(op) => self.op(*op, 0),
            T::Un(u, a) => {
                self.node(a);
                self.op(Op::Un(*u), 1);
            }
            T::Bin(b, x, y) => {
                self.node(x);
                self.node(y);
                self.op(Op::Bin(*b), 2);
            }
            T::Select(c, a, b) => {
                self.node(c);
                self.node(a);
                self.node(b);
                self.op(Op::Select, 3);
            }
        }
    }
}
