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
//! [`super::num::to_int`], [`super::num::to_float`], a float stored as
//! its bits): a Lua error gives the source's caught value (false, 0, `0f`)
//! with `ok` 1; an unavailable read gives `ok` 0;
//! an integer result of `i32::MIN` or `i32::MAX` gives `ok` 0, as the
//! source's number draws nothing for those (`SkinNumber.java:139-143` at
//! `0.8.9`). No heap, no recursion, no indexing.
//!
//! A timer callback ([`Conv::Timer`]) runs once a frame, its result
//! `tolong()` (`SkinLuaAccessor.java:736-747` at `0.8.9`): off, a Lua
//! error or no value, and its timer is off; any other value is its start
//! on the state clock, in µs. Its age is `state_us - start` (saturating);
//! negative, the element is not drawn; otherwise it is capped at
//! [`MAX_KEY_US`] and the timer's start placed that far before the frame's
//! chart time, so the element's track ages by it. Its outputs are whether
//! it was on, and 1 once it ran.
//!
//! Slots are loaded and stored in the bank in place: the compiler emits a
//! callback's stores together at its end, after every value they store is
//! in a register, so an unavailable read or a Lua error before them ends
//! the invocation with none of them.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::num;
use super::{Bank, CallbackDef, Clock, Conv, Fail, Insn, MAX_REGISTERS, Present, ReadDef, Val};
use crate::MAX_KEY_US;
use crate::expr::State;
use crate::schema::Ty;
use bms_model::TimeUs;

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

/// What one frame's run reads and writes besides the fields.
pub struct Io<'a, 'b> {
    pub state: &'a State<'b>,
    pub clock: Clock,
    pub bank: &'a mut Bank,
    /// Callback `k`'s converted result at `2k`, whether it gave one at
    /// `2k + 1`.
    pub outs: &'a mut [i32],
    /// The timer callbacks' starts, in order, `None` while off.
    pub timers: &'a mut [Option<TimeUs>],
}

impl Present {
    /// The number of timer callbacks: the timer outputs it gives. Read each
    /// frame, when the play's timers are checked against the skin's.
    #[must_use]
    pub fn timers(&self) -> usize {
        self.def
            .callbacks
            .iter()
            .filter(|c| c.conv == Conv::Timer)
            .count()
    }

    /// Runs every callback in order over `io`: its state, clock and slots,
    /// writing callback `k`'s converted result to `outs[2k]` and whether it
    /// gave one to `outs[2k + 1]`, and each timer callback's start to its
    /// timer. Allocates nothing and never panics; a slot `outs` or
    /// `timers` lacks is not written.
    pub fn run(&self, io: &mut Io<'_, '_>) {
        let mut timer = 0_usize;
        for (k, cb) in self.def.callbacks.iter().enumerate() {
            let held = self.held(cb, io.state, io.outs);
            let go = held.is_none();
            #[cfg(feature = "dev-tools")]
            let id = u16::try_from(k).unwrap_or(u16::MAX);
            #[cfg(feature = "dev-tools")]
            if let Some(h) = held {
                super::trace::record(|| super::trace::Event::Skipped {
                    k: id,
                    why: h.skip(),
                });
            }
            let (value, ok) = if cb.conv == Conv::Timer {
                let start = if go {
                    self.timer(
                        cb,
                        io,
                        #[cfg(feature = "dev-tools")]
                        id,
                    )
                } else {
                    None
                };
                if let Some(t) = io.timers.get_mut(timer) {
                    *t = start.and_then(|s| s);
                }
                timer = timer.saturating_add(1);
                (i32::from(start.is_some()), go)
            } else if go {
                let r = self.invoke(cb, io.state, io.clock, io.bank);
                let out = convert(cb.conv, r);
                #[cfg(feature = "dev-tools")]
                super::trace::record(|| super::trace::Event::Ran {
                    k: id,
                    result: r,
                    conv: if cb.conv == Conv::Truthy {
                        super::trace::Converted::Bool(out.0 != 0)
                    } else {
                        super::trace::Converted::Int(out.0, out.1)
                    },
                });
                out
            } else {
                (0, false)
            };
            let at = k.saturating_mul(2);
            if let Some(slot) = io.outs.get_mut(at) {
                *slot = value;
            }
            if let Some(slot) = io.outs.get_mut(at.saturating_add(1)) {
                *slot = i32::from(ok);
            }
        }
    }

    /// Timer callback `cb`'s one invocation: `None` when it gave off, a
    /// Lua error or no value; otherwise `Some` of its start against the
    /// frame, `None` within while that start is after the state clock.
    fn timer(
        &self,
        cb: &CallbackDef,
        io: &mut Io<'_, '_>,
        #[cfg(feature = "dev-tools")] k: u16,
    ) -> Option<Option<TimeUs>> {
        let r = self.invoke(cb, io.state, io.clock, io.bank);
        let v = start_of(r);
        #[cfg(feature = "dev-tools")]
        super::trace::record(|| super::trace::Event::Ran {
            k,
            result: r,
            conv: super::trace::Converted::Long(v),
        });
        let start = v.filter(|&s| s != i64::MIN);
        let elapsed = start.map(|s| io.clock.state_us.saturating_sub(s));
        let installed = elapsed
            .filter(|&e| e >= 0)
            .map(|e| TimeUs(io.clock.now.0.saturating_sub(e.min(MAX_KEY_US))));
        #[cfg(feature = "dev-tools")]
        super::trace::record(|| super::trace::Event::Timer {
            k,
            on: start.is_some(),
            start,
            elapsed_us: elapsed,
            installed,
        });
        start.map(|_| installed)
    }

    /// Why `cb` does not run this frame, if it does not: the callback it
    /// follows does not allow it (a number's value given, a draw true, a
    /// timer on), or its gate does not hold.
    fn held(&self, cb: &CallbackDef, state: &State<'_>, outs: &[i32]) -> Option<Held> {
        if let Some(a) = cb.after {
            let at = usize::from(a).saturating_mul(2);
            let ok = outs.get(at.saturating_add(1)) == Some(&1);
            let value = outs.get(at).is_some_and(|&v| v != 0);
            let allows = match self.def.callbacks.get(usize::from(a)).map(|c| c.conv) {
                Some(Conv::Int) => ok,
                Some(Conv::Truthy | Conv::Timer) => ok && value,
                // Never an `after` target (V4).
                Some(Conv::Float) | None => false,
            };
            if !allows {
                return Some(Held::After);
            }
        }
        let open = match cb.gate {
            None => true,
            Some(g) => self
                .def
                .reads
                .get(usize::from(g))
                .is_some_and(|r| read(r, state) == Ok(Val::Bool(true))),
        };
        (!open).then_some(Held::Gate)
    }

    /// One invocation of `cb`: its result or why it gave none.
    fn invoke(
        &self,
        cb: &CallbackDef,
        state: &State<'_>,
        clock: Clock,
        bank: &mut Bank,
    ) -> Result<Val, Fail> {
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
                Insn::StateUs { dst } => (dst, num::clock(clock.state_us)),
                Insn::LoadSlot { dst, slot } => (
                    dst,
                    bank.vals
                        .get(usize::from(slot))
                        .copied()
                        .unwrap_or(Val::Nil),
                ),
                Insn::StoreSlot { slot, src, .. } => {
                    let v = get(src);
                    if let Some(s) = bank.vals.get_mut(usize::from(slot)) {
                        *s = v;
                    }
                    pc = next;
                    continue;
                }
            };
            if let Some(slot) = regs.get_mut(usize::from(dst)) {
                *slot = v;
            }
            pc = next;
        }
        Ok(Val::Nil)
    }
}

/// Why a callback does not run this frame.
#[derive(Clone, Copy)]
enum Held {
    After,
    Gate,
}

#[cfg(feature = "dev-tools")]
impl Held {
    const fn skip(self) -> super::trace::Skip {
        match self {
            Self::After => super::trace::Skip::After,
            Self::Gate => super::trace::Skip::Gate,
        }
    }
}

/// A timer invocation's result as `tolong()` gives it, a Lua error the
/// off value (`SkinLuaAccessor.java:740-745` at `0.8.9`); `None` for no
/// value.
fn start_of(r: Result<Val, Fail>) -> Option<i64> {
    match r {
        Ok(v) => Some(num::to_long(v)),
        Err(Fail::Error) => Some(i64::MIN),
        Err(Fail::Unavailable) => None,
    }
}

/// The bits of `f`, as an output slot holds a float.
#[expect(
    clippy::cast_possible_wrap,
    reason = "the slot holds the bits, read back by `f32::from_bits`"
)]
fn f32_bits(f: f32) -> i32 {
    f.to_bits() as i32
}

/// Result `r` as conversion `conv` gives it, and whether it gave a value:
/// a float as its bits, a Lua error the source's caught `0f` (+0's bits).
fn convert(conv: Conv, r: Result<Val, Fail>) -> (i32, bool) {
    match (conv, r) {
        (_, Err(Fail::Unavailable)) => (0, false),
        (_, Err(Fail::Error)) => (0, true),
        (Conv::Truthy, Ok(v)) => (i32::from(num::to_bool(v)), true),
        (Conv::Int, Ok(v)) => {
            let n = num::to_int(v);
            (n, n != i32::MIN && n != i32::MAX)
        }
        (Conv::Float, Ok(v)) => (f32_bits(num::to_float(v)), true),
        // Run through [`Present::timer`] instead.
        (Conv::Timer, Ok(_)) => (0, false),
    }
}
