//! Option and constant folding, after the check (so it never hides a range
//! error).
//!
//! A node whose operands are all constants becomes the constant the
//! evaluator would compute, through the evaluator's own operator functions.
//! `if` on a constant becomes its chosen branch, and `&&` and `||` with a
//! constant operand reduce, either side (operations are pure). Nothing else
//! is rewritten.

use super::check::T;
use super::program::{Binary, binary, unary};

pub(super) fn fold(t: T) -> T {
    match t {
        T::Const(_) | T::Read(_) => t,
        T::Un(op, a) => match fold(*a) {
            T::Const(x) => T::Const(unary(op, x)),
            a => T::Un(op, Box::new(a)),
        },
        T::Bin(op, a, b) => match (op, fold(*a), fold(*b)) {
            (op, T::Const(x), T::Const(y)) => T::Const(binary(op, x, y)),
            (Binary::And, T::Const(0), _) | (Binary::And, _, T::Const(0)) => T::Const(0),
            (Binary::Or, T::Const(1), _) | (Binary::Or, _, T::Const(1)) => T::Const(1),
            (Binary::And, T::Const(_), x)
            | (Binary::And, x, T::Const(_))
            | (Binary::Or, T::Const(_), x)
            | (Binary::Or, x, T::Const(_)) => x,
            (op, a, b) => T::Bin(op, Box::new(a), Box::new(b)),
        },
        T::Select(c, a, b) => match fold(*c) {
            T::Const(c) => fold(if c != 0 { *a } else { *b }),
            c => T::Select(Box::new(c), Box::new(fold(*a)), Box::new(fold(*b))),
        },
    }
}
