//! The executor, in-play code (design.md "Scope of No Allocation"): each
//! frame, after the fields and timers, every callback in order.
//!
//! A callback whose `after` number gave no value, or whose gate is false
//! or unavailable, is skipped: its `ok` is 0. Otherwise its registers are
//! reset to nil and its span is stepped with checked fetches; a jump or a
//! taken test moves forward (the verifier proves it), any other
//! instruction steps once, so an invocation ends within its length. A
//! `Ret`, or the end of the span with nil, gives the result, which is
//! converted as its consumer converts it ([`super::num::to_bool`],
//! [`super::num::to_int`]): a Lua error gives the source's caught value
//! (false, 0) with `ok` 1; an unavailable read or an overflow gives `ok` 0;
//! an integer result of `i32::MIN` or `i32::MAX` gives `ok` 0, as the
//! source's number draws nothing for those (`SkinNumber.java:139-143` at
//! `0.8.9`). No heap, no recursion, no indexing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::num;
use super::{CallbackDef, Conv, Fail, Insn, MAX_REGISTERS, Present, ReadDef, Val};
use crate::expr::State;
use crate::schema::Ty;

#[cfg(feature = "dev-tools")]
thread_local! {
    static INVOCATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many callbacks this thread's programs have invoked, a skipped one
/// not counted (dev-tools): what a test reads to see the schedule.
#[cfg(feature = "dev-tools")]
#[must_use]
pub fn invocations() -> u64 {
    INVOCATIONS.with(std::cell::Cell::get)
}

/// Read `r` on `state`: unavailable while its guard is false.
fn read(r: &ReadDef, state: &State<'_>) -> Result<Val, Fail> {
    if let Some(g) = &r.guard
        && !g.eval_bool(state)
    {
        return Err(Fail::Unavailable);
    }
    let v = r.value.eval(state);
    Ok(if r.value.ty() == Ty::Bool {
        Val::Bool(v != 0)
    } else {
        Val::Int(i64::from(v))
    })
}

impl Present {
    /// Runs every callback on `state`, writing callback `k`'s converted
    /// result to `outs[2k]` and whether it gave one to `outs[2k + 1]`.
    /// Allocates nothing and never panics; a slot `outs` lacks is not
    /// written.
    pub fn run(&self, state: &State<'_>, outs: &mut [i32]) {
        for (k, cb) in self.def.callbacks.iter().enumerate() {
            let ran = self.gated(cb, state, outs).then(|| self.invoke(cb, state));
            let (value, ok) = match ran {
                None => (0, false),
                Some(r) => convert(cb.conv, r),
            };
            let at = k.saturating_mul(2);
            if let Some(slot) = outs.get_mut(at) {
                *slot = value;
            }
            if let Some(slot) = outs.get_mut(at.saturating_add(1)) {
                *slot = i32::from(ok);
            }
        }
    }

    /// Whether `cb` runs this frame: its `after` number gave a value and
    /// its gate holds.
    fn gated(&self, cb: &CallbackDef, state: &State<'_>, outs: &[i32]) -> bool {
        if let Some(a) = cb.after {
            let ok = usize::from(a)
                .checked_mul(2)
                .and_then(|i| outs.get(i.saturating_add(1)));
            if ok != Some(&1) {
                return false;
            }
        }
        match cb.gate {
            None => true,
            Some(g) => self
                .def
                .reads
                .get(usize::from(g))
                .is_some_and(|r| read(r, state) == Ok(Val::Bool(true))),
        }
    }

    /// One invocation of `cb`: its result or why it gave none.
    fn invoke(&self, cb: &CallbackDef, state: &State<'_>) -> Result<Val, Fail> {
        #[cfg(feature = "dev-tools")]
        INVOCATIONS.with(|n| n.set(n.get().wrapping_add(1)));
        let mut regs = [Val::Nil; MAX_REGISTERS];
        let start = usize::try_from(cb.start).unwrap_or(usize::MAX);
        let len = usize::from(cb.len);
        let mut pc = 0_usize;
        while pc < len {
            let Some(&insn) = start.checked_add(pc).and_then(|i| self.def.code.get(i)) else {
                return Ok(Val::Nil);
            };
            let get = |r: u8| regs.get(usize::from(r)).copied().unwrap_or(Val::Nil);
            let mut next = pc.saturating_add(1);
            let (dst, v) = match insn {
                Insn::Const { dst, k } => (
                    dst,
                    self.def
                        .consts
                        .get(usize::from(k))
                        .copied()
                        .unwrap_or(Val::Nil),
                ),
                Insn::Read { dst, read: r } => {
                    let def = self.def.reads.get(usize::from(r)).ok_or(Fail::Error)?;
                    (dst, read(def, state)?)
                }
                Insn::Move { dst, src } => (dst, get(src)),
                Insn::Arith { op, dst, a, b } => (dst, num::arith(op, get(a), get(b))?),
                Insn::Neg { dst, a } => (dst, num::neg(get(a))?),
                Insn::Cmp { op, dst, a, b } => (dst, Val::Bool(num::compare(op, get(a), get(b))?)),
                Insn::Not { dst, a } => (dst, Val::Bool(!num::truthy(get(a)))),
                Insn::Math { f, dst, a } => (dst, num::math(f, get(a))?),
                Insn::Extreme { max, dst, a, b } => (dst, num::extreme(max, get(a), get(b))?),
                Insn::Jump { to } => {
                    pc = usize::from(to).max(next);
                    continue;
                }
                Insn::Test { cond, when, to } => {
                    if num::truthy(get(cond)) == when {
                        next = usize::from(to).max(next);
                    }
                    pc = next;
                    continue;
                }
                Insn::Ret { src } => return Ok(get(src)),
            };
            if let Some(slot) = regs.get_mut(usize::from(dst)) {
                *slot = v;
            }
            pc = next;
        }
        Ok(Val::Nil)
    }
}

/// Result `r` as conversion `conv` gives it, and whether it gave a value.
fn convert(conv: Conv, r: Result<Val, Fail>) -> (i32, bool) {
    match (conv, r) {
        (_, Err(Fail::Unavailable | Fail::Overflow)) => (0, false),
        (_, Err(Fail::Error)) => (0, true),
        (Conv::Truthy, Ok(v)) => (i32::from(num::to_bool(v)), true),
        (Conv::Int, Ok(v)) => {
            let n = num::to_int(v);
            (n, n != i32::MIN && n != i32::MAX)
        }
    }
}
