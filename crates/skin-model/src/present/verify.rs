//! The verifier: a [`PresentDef`] checked exactly before the skin is
//! installed (design.md "Threat Model", T08), so the executor's bounds hold
//! for any definition it accepts.
//!
//! | Rule | Checks |
//! |---|---|
//! | V1 | at most [`MAX_CALLBACKS`] callbacks, one signature entry each matching its conversion; constants and reads fit `u16` indices; at most [`MAX_SKIN_OPS`] instructions |
//! | V2 | each span inside the code, 1 to [`MAX_CALLBACK_OPS`] long, with at most [`MAX_REGISTERS`] registers |
//! | V3 | every register below the callback's count, every constant and read index valid, every integer a `Const` loads within ±2^53, every jump forward and within the span (`pc < to <= len`; the end returns nil) |
//! | V4 | each read's value an integer or a boolean, each guard and gate a boolean, every read set within the schema's fields; `after` names an earlier `Int` callback, for a timer callback an earlier `draw` or timer, or for a slider's `Float` value an earlier `draw` |
//! | V5 | the per-frame cost (every instruction, every read's and guard's operations at each `Read`, each gate's, and every slot the callback loads or stores) charged to the skin's [`SkinOps`] |
//! | V7 | at most [`MAX_SLOTS`] slots, each domain's bounds ordered and within ±2^53, each initial value within its slot's domain, every slot and domain index valid, and each store's domain within its slot's |
//!
//! V6, that each consumer reads an output inside the signature at its type,
//! and V8, that an element's program timer names a timer callback and that
//! no expression reads a timer output, are [`crate::Skin::new`]'s, which
//! holds the consumers.

use super::{
    CallbackDef, Conv, Domain, Insn, MAX_CALLBACK_OPS, MAX_CALLBACKS, MAX_REGISTERS, MAX_SLOTS,
    OutTy, Present, PresentDef, ReadDef, Val,
};
use crate::expr::{MAX_SKIN_OPS, Program, SkinOps};
use crate::schema::Ty;

/// Why a program was refused; `callback` is an index.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresentError {
    #[error("the program has {n} callbacks, more than {MAX_CALLBACKS}")]
    TooManyCallbacks { n: usize },
    #[error("the program's signature does not match its callbacks")]
    Signature,
    #[error("the program has too many constants or reads for its indices")]
    TooManyEntries,
    #[error("the program has {n} instructions, more than {MAX_SKIN_OPS}")]
    TooLong { n: usize },
    #[error("callback {callback}'s span is outside the code or not 1 to {MAX_CALLBACK_OPS} long")]
    Span { callback: usize },
    #[error("callback {callback} uses more than {MAX_REGISTERS} registers")]
    Registers { callback: usize },
    #[error("callback {callback}'s instruction {pc} names a register, constant or read it lacks")]
    Operand { callback: usize, pc: usize },
    #[error("callback {callback}'s instruction {pc} loads an integer past ±2^53")]
    Constant { callback: usize, pc: usize },
    #[error("callback {callback}'s instruction {pc} jumps backward or past its end")]
    Jump { callback: usize, pc: usize },
    #[error("read {read} is not an integer or boolean, or its guard is not a boolean")]
    ReadType { read: usize },
    #[error("read {read} reads past the schema's fields")]
    ReadSet { read: usize },
    #[error("callback {callback}'s gate is not a boolean read")]
    Gate { callback: usize },
    #[error(
        "callback {callback} follows a callback that is not an earlier number value, for a timer an earlier draw or timer, or for a slider value an earlier draw"
    )]
    After { callback: usize },
    #[error("the program and the skin's expressions exceed {MAX_SKIN_OPS} operations a frame")]
    Cost,
    #[error("the program has {n} slots, more than {MAX_SLOTS}")]
    TooManySlots { n: usize },
    #[error("slot {slot}'s domain or initial value is out of range")]
    Slot { slot: usize },
    #[error("callback {callback}'s instruction {pc} stores a value outside its slot's domain")]
    Store { callback: usize, pc: usize },
}

impl Present {
    /// `def` checked against V1 to V5 and V7, its per-frame cost charged
    /// to `ops`;
    /// `base` is the schema's field count, the first output slot.
    ///
    /// # Errors
    ///
    /// The first rule `def` breaks.
    pub fn verify(def: PresentDef, base: usize, ops: &mut SkinOps) -> Result<Self, PresentError> {
        v1(&def)?;
        v7_slots(&def)?;
        for (i, r) in def.reads.iter().enumerate() {
            v4_read(r, i, base)?;
        }
        let mut cost = 0_usize;
        for (k, cb) in def.callbacks.iter().enumerate() {
            let code = v2(&def, cb, k)?;
            v3(&def, cb, code, k)?;
            v4_gating(&def, cb, k)?;
            v7_stores(&def, code, k)?;
            cost = cost.saturating_add(cost_of(&def, cb, code));
        }
        ops.charge_ops(cost).map_err(|_| PresentError::Cost)?;
        #[cfg(feature = "dev-tools")]
        let slotted = def
            .callbacks
            .iter()
            .map(|cb| {
                let span = usize::try_from(cb.start)
                    .ok()
                    .and_then(|s| def.code.get(s..s.saturating_add(usize::from(cb.len))));
                span.is_some_and(|c| {
                    c.iter()
                        .any(|i| matches!(i, Insn::LoadSlot { .. } | Insn::StoreSlot { .. }))
                })
            })
            .collect::<Box<[bool]>>();
        Ok(Self {
            def,
            #[cfg(feature = "dev-tools")]
            slotted,
        })
    }
}

fn v1(def: &PresentDef) -> Result<(), PresentError> {
    let n = def.callbacks.len();
    if n > MAX_CALLBACKS {
        return Err(PresentError::TooManyCallbacks { n });
    }
    if def.signature.len() != n
        || def
            .callbacks
            .iter()
            .zip(&def.signature)
            .any(|(c, &t)| OutTy::of(c.conv) != t)
    {
        return Err(PresentError::Signature);
    }
    let fits = |n: usize| n <= usize::from(u16::MAX) + 1;
    if !fits(def.consts.len()) || !fits(def.reads.len()) || !fits(def.domains.len()) {
        return Err(PresentError::TooManyEntries);
    }
    if def.code.len() > MAX_SKIN_OPS {
        return Err(PresentError::TooLong { n: def.code.len() });
    }
    Ok(())
}

/// Callback `k`'s span, checked.
fn v2<'a>(def: &'a PresentDef, cb: &CallbackDef, k: usize) -> Result<&'a [Insn], PresentError> {
    let len = usize::from(cb.len);
    let start = usize::try_from(cb.start).map_err(|_| PresentError::Span { callback: k })?;
    if len == 0 || len > MAX_CALLBACK_OPS {
        return Err(PresentError::Span { callback: k });
    }
    let code = start
        .checked_add(len)
        .and_then(|end| def.code.get(start..end))
        .ok_or(PresentError::Span { callback: k })?;
    if usize::from(cb.regs) > MAX_REGISTERS {
        return Err(PresentError::Registers { callback: k });
    }
    Ok(code)
}

fn v3(def: &PresentDef, cb: &CallbackDef, code: &[Insn], k: usize) -> Result<(), PresentError> {
    let len = code.len();
    for (pc, insn) in code.iter().enumerate() {
        let operand = PresentError::Operand { callback: k, pc };
        let reg = |r: u8| {
            if r < cb.regs {
                Ok(())
            } else {
                Err(operand.clone())
            }
        };
        let target = |to: u16| {
            if pc < usize::from(to) && usize::from(to) <= len {
                Ok(())
            } else {
                Err(PresentError::Jump { callback: k, pc })
            }
        };
        match *insn {
            Insn::Const { dst, k: c } => {
                reg(dst)?;
                match def.consts.get(usize::from(c)) {
                    None => return Err(operand),
                    // Every integer the program holds is an exact double.
                    Some(&Val::Int(n)) if n.unsigned_abs() > EXACT.unsigned_abs() => {
                        return Err(PresentError::Constant { callback: k, pc });
                    }
                    Some(_) => {}
                }
            }
            Insn::Read { dst, read } => {
                reg(dst)?;
                if usize::from(read) >= def.reads.len() {
                    return Err(operand);
                }
            }
            Insn::Move { dst, src: a }
            | Insn::Neg { dst, a }
            | Insn::Not { dst, a }
            | Insn::Math { dst, a, .. } => {
                reg(dst)?;
                reg(a)?;
            }
            Insn::Arith { dst, a, b, .. }
            | Insn::Cmp { dst, a, b, .. }
            | Insn::Extreme { dst, a, b, .. } => {
                reg(dst)?;
                reg(a)?;
                reg(b)?;
            }
            Insn::Jump { to } => target(to)?,
            Insn::Test { cond, to, .. } => {
                reg(cond)?;
                target(to)?;
            }
            Insn::Ret { src } | Insn::StoreSlot { src, .. } => reg(src)?,
            Insn::StateUs { dst } | Insn::LoadSlot { dst, .. } => reg(dst)?,
        }
        if let Insn::LoadSlot { slot, .. } | Insn::StoreSlot { slot, .. } = *insn
            && usize::from(slot) >= def.slots.len()
        {
            return Err(operand);
        }
        if let Insn::StoreSlot { domain, .. } = *insn
            && usize::from(domain) >= def.domains.len()
        {
            return Err(operand);
        }
    }
    Ok(())
}

/// The largest integer the source's doubles hold exactly.
const EXACT: i64 = 1 << 53;

/// Whether domain `d`'s bounds are ordered and exact.
fn well_formed(d: Domain) -> bool {
    d.ints
        .is_none_or(|(lo, hi)| lo <= hi && -EXACT <= lo && hi <= EXACT)
}

/// V7's slots: their count, domains and initial values, and every store
/// domain's bounds.
fn v7_slots(def: &PresentDef) -> Result<(), PresentError> {
    let n = def.slots.len();
    if n > MAX_SLOTS {
        return Err(PresentError::TooManySlots { n });
    }
    for (slot, s) in def.slots.iter().enumerate() {
        if !well_formed(s.domain) || !s.domain.holds(s.init) {
            return Err(PresentError::Slot { slot });
        }
    }
    Ok(())
}

/// V7's stores in `code`: each store's domain within its slot's.
fn v7_stores(def: &PresentDef, code: &[Insn], k: usize) -> Result<(), PresentError> {
    for (pc, insn) in code.iter().enumerate() {
        if let Insn::StoreSlot { slot, domain, .. } = *insn {
            let within = match (
                def.slots.get(usize::from(slot)),
                def.domains.get(usize::from(domain)),
            ) {
                (Some(s), Some(&d)) => well_formed(d) && s.domain.contains(d),
                _ => false,
            };
            if !within {
                return Err(PresentError::Store { callback: k, pc });
            }
        }
    }
    Ok(())
}

/// Whether `p` reads only the schema's `base` fields.
fn within(p: &Program, base: usize) -> bool {
    p.fields().all(|s| usize::from(s) < base)
}

fn v4_read(r: &ReadDef, i: usize, base: usize) -> Result<(), PresentError> {
    let typed = matches!(r.value.ty(), Ty::Int | Ty::Bool)
        && r.guard.as_ref().is_none_or(|g| g.ty() == Ty::Bool);
    if !typed {
        return Err(PresentError::ReadType { read: i });
    }
    if !within(&r.value, base) || r.guard.as_ref().is_some_and(|g| !within(g, base)) {
        return Err(PresentError::ReadSet { read: i });
    }
    Ok(())
}

fn v4_gating(def: &PresentDef, cb: &CallbackDef, k: usize) -> Result<(), PresentError> {
    if let Some(g) = cb.gate
        && def
            .reads
            .get(usize::from(g))
            .is_none_or(|r| r.value.ty() != Ty::Bool)
    {
        return Err(PresentError::Gate { callback: k });
    }
    if let Some(a) = cb.after {
        let earlier = usize::from(a) < k
            && def.callbacks.get(usize::from(a)).is_some_and(|c| {
                c.conv == Conv::Int
                    || (cb.conv == Conv::Timer && matches!(c.conv, Conv::Truthy | Conv::Timer))
                    // A slider reads its value only when it draws
                    // (`SkinSlider.java:105-113` at `0.8.9`).
                    || (cb.conv == Conv::Float && c.conv == Conv::Truthy)
            });
        if !earlier {
            return Err(PresentError::After { callback: k });
        }
    }
    Ok(())
}

/// A read's operations: its value's and its guard's.
fn read_cost(r: Option<&ReadDef>) -> usize {
    r.map_or(0, |r| {
        r.value
            .op_count()
            .saturating_add(r.guard.as_ref().map_or(0, Program::op_count))
    })
}

/// The most work a frame's run of `cb` does, as [`callback_cost`].
fn cost_of(def: &PresentDef, cb: &CallbackDef, code: &[Insn]) -> usize {
    let gate = cb.gate.and_then(|g| def.reads.get(usize::from(g)));
    callback_cost(code, &def.reads, gate)
}

/// The most work a frame's run of a callback does, both arms of every
/// branch counted: its instructions `code`, each `Read`'s operations (the
/// read it names in `reads`, its value's and its guard's), each slot it
/// loads or stores, and its `gate` read's; every callback, a timer
/// callback too, runs once a frame. V5
/// charges this; an importer charges the same before it keeps the
/// callback, so its count of the skin's operations covers the program's.
#[must_use]
pub fn callback_cost(code: &[Insn], reads: &[ReadDef], gate: Option<&ReadDef>) -> usize {
    let at_reads = code.iter().fold(0_usize, |n, insn| match *insn {
        Insn::Read { read, .. } => n.saturating_add(read_cost(reads.get(usize::from(read)))),
        _ => n,
    });
    let touched = code
        .iter()
        .enumerate()
        .filter(|&(i, insn)| match *insn {
            Insn::LoadSlot { slot, .. } | Insn::StoreSlot { slot, .. } => !code
                .iter()
                .take(i)
                .any(|e| matches!(*e, Insn::LoadSlot { slot: s, .. } | Insn::StoreSlot { slot: s, .. } if s == slot)),
            _ => false,
        })
        .count();
    code.len()
        .saturating_add(at_reads)
        .saturating_add(touched)
        .saturating_add(read_cost(gate))
}
