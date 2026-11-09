//! The program backend (design.md "beatoraja Lua Skins", the program):
//! a callback's residual ([`super::residual`]) the expressions refuse,
//! compiled into one callback of `skin_model::present`'s register code.
//!
//! Each node is emitted into a destination register, its operands into the
//! registers above it, so a callback uses as many registers as its
//! residual is deep; a node shared by two parents is emitted at each, every
//! instruction counted against [`MAX_CALLBACK_OPS`]. A select becomes a
//! test and a jump, so only the arm taken runs, as in Lua. A host read
//! becomes one read of its mapped expression with its availability guard
//! (the lowering's [`Leaves`], which also takes a gauge or timer
//! comparison whole).
//!
//! Every node the evaluation reached is checked before anything is
//! emitted, whether or not its value reaches the result, since Lua runs it
//! all the same: every integer is bounded from the ranges of what it reads,
//! and an integer arithmetic result, and an integer a number's value
//! converts, must lie within ±2^53, where the source's doubles hold it
//! exactly, so nothing wraps and the source's double path agrees; an
//! integer `//` whose divisor can be 0 is refused, since Lua 5.4 raises
//! there (the source has no `//`; its `%` by 0 is NaN, which the program
//! keeps). A `math.floor` or `ceil` of a float is an integer of no known
//! bound. And every read a reached path performs keeps its availability:
//! the guards of reads whose values are discarded are read first, so an
//! unavailable one gives no value, as a lowered expression's joined guards
//! do. Each refusal is [`Unsupported`] and follows the callback's role.
//!
//! A timer root's slots ([`slots`]) are checked before it compiles,
//! design.md "beatoraja Lua Skins", the program's state:
//!
//! - **Domain**: each slot's every write is a constant, a mapped integer
//!   read, the state clock or the off value, never a value computed from
//!   state, so its domain is their join with its initial value.
//! - **One way**: no write is off, and each is reached only under the
//!   slot's own off test, so once set no guard holds again.
//! - **Frozen guard**: every condition selecting a write reads only
//!   constants, the slot itself and the fields of an allowlist
//!   ([`GUARD_FIELDS`]: the point), each its source read's, fixed by the
//!   tally and so unchanged outside play frames; a guard reading another
//!   slot, the clock or any other field is refused. So a guard false on
//!   the last play frame stays false.
//! - **Loading**: every condition selecting a write is false before the
//!   first play frame, evaluated with the slot at its initial value and
//!   each allowed field at the value the source reads then (established by
//!   running the source), so the program, which runs in play frames only,
//!   omits no write the source makes while loading.
//!
//! No operation it keeps can raise: the residual keeps arithmetic, `math`
//! and order only of numbers, and the intervals refuse an integer `//` by
//! a possible 0, so its stores are kept whatever it returns. Its stores
//! are emitted together after its result: every value into its own
//! register, then each [`Insn::StoreSlot`] with its domain.

use skin_model::Ty;
use skin_model::present::{self, Cmp, Domain, Insn, MAX_CALLBACK_OPS, MAX_REGISTERS, MathFn};

use super::Unsupported;
use super::callback::Analysis;
use super::residual::{
    self, Conv, FLT, INT, LIMITS, MathOp, Node, R, Residual, Stop, Val, charge, did,
};
use super::syntax::{Bin, EXACT};

/// A read the program evaluates: its expression's source over the play
/// schema, its type and its availability guards.
#[derive(Debug)]
pub(crate) struct Leaf {
    pub(crate) src: String,
    pub(crate) ty: Ty,
    pub(crate) guards: Vec<&'static str>,
}

/// How residual nodes map to reads: a host read, or a comparison the
/// mapping takes whole (a gauge's look, a timer against the off value).
pub(crate) trait Leaves {
    /// Node `r` of `res` as a read, `None` when it is not one.
    fn leaf(&mut self, res: &Residual, r: u32) -> R<Option<Leaf>>;
    /// Read node `r`'s integer range, when it is an integer read.
    fn range(&self, r: u32) -> Option<(i64, i64)>;
    /// The availability guards of every read `res` recorded, each set
    /// once, empty sets left out.
    fn guards(&self, res: &Residual) -> Vec<Vec<&'static str>>;
}

/// A compiled callback, its constant and read indices its own from 0.
#[derive(Debug)]
pub(crate) struct Compiled {
    pub(crate) code: Vec<Insn>,
    pub(crate) consts: Vec<present::Val>,
    pub(crate) reads: Vec<Leaf>,
    pub(crate) regs: u8,
    pub(crate) conv: present::Conv,
    /// Its stores' domains, which its [`Insn::StoreSlot`]s index from 0.
    pub(crate) domains: Vec<Domain>,
    /// The slots it loads and stores, proven ([`slots`]).
    pub(crate) slots: Vec<Slot>,
}

/// A slot of the root being compiled: its index, its initial value and
/// its domain, which [`slots`] proves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slot {
    pub(crate) slot: u16,
    pub(crate) init: present::Val,
    pub(crate) domain: Domain,
}

/// The fields a slot write's guard may read, each with the value the
/// source reads before the first play frame (an allowlist: any other read
/// in a guard refuses the slot). Each entry is established by running the
/// source's read at `0.8.9`, not modelled:
///
/// - `point`, `main_state.number(100)` (`IntegerPropertyFactory.java:174`:
///   `ScoreDataProperty.getNowScore()`): 0 before the first judgment, the
///   play state not yet having called `ScoreDataProperty.update`
///   (`BMSPlayer.java:1038`), so the field's default
///   (`ScoreDataProperty.java:13`); always available; the tally's alone, so
///   unchanged outside play frames.
///
/// Left out: the EX score (`number(71)`), `Integer.MIN_VALUE` before the
/// first judgment (`IntegerPropertyFactory.java:607-615`: no score data
/// yet), where tapestry's field reads 0; every other field, unverified.
const GUARD_FIELDS: [(&str, i64); 1] = [("point", 0)];

/// The value guard field `src` (a mapped read's source) has before the
/// first play frame, when it is one of [`GUARD_FIELDS`].
fn guard_field(src: &str) -> Option<i64> {
    let name = src
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(src);
    GUARD_FIELDS
        .iter()
        .find(|&&(f, _)| f == name)
        .map(|&(_, v)| v)
}

/// The largest value of the state clock: 2^53 µs.
const CLOCK_MAX: i64 = 1 << 53;

fn refusal<T>(what: &'static str) -> R<T> {
    Err(Stop::Unsupported(super::Unsupported::Construct(what)))
}

/// The domain of `v` as a value slot `k` (whose domain so far is `own`)
/// takes: a constant, a mapped integer read, the state clock, the off
/// value, the slot itself, or a select of those; anything else is a
/// value computed from state, which a slot does not take.
fn value_domain<L: Leaves>(
    an: &Analysis,
    leaves: &L,
    res: &Residual,
    v: u32,
    k: u16,
    own: Domain,
) -> R<Domain> {
    charge(an, "compile:slot", 1)?;
    let join = |a: Domain, b: Domain| Domain {
        off: a.off || b.off,
        ints: match (a.ints, b.ints) {
            (Some((p, q)), Some((r, s))) => Some((p.min(r), q.max(s))),
            (x, None) | (None, x) => x,
        },
    };
    let ints = |lo: i64, hi: i64| Domain {
        off: false,
        ints: Some((lo, hi)),
    };
    Ok(match res.node(v) {
        Node::Slot(j) if j == k => own,
        Node::Known(Val::Int(n)) => ints(n, n),
        Node::Known(Val::OffValue) => Domain {
            off: true,
            ints: None,
        },
        Node::StateUs => ints(0, CLOCK_MAX),
        Node::Read { .. } => match leaves.range(v) {
            Some((lo, hi)) if res.kind(v) == INT => ints(lo, hi),
            _ => return refusal("a slot written a read it cannot bound"),
        },
        Node::Unreachable => Domain {
            off: false,
            ints: None,
        },
        Node::Select { then, other, .. } => join(
            value_domain(an, leaves, res, then, k, own)?,
            value_domain(an, leaves, res, other, k, own)?,
        ),
        _ => return refusal("a slot written a value computed from state"),
    })
}

/// A value known under the start assumption, or unknown.
#[derive(Clone, Copy, PartialEq)]
enum At0 {
    Off,
    Int(i64),
    Bool(bool),
}

/// Node `v` as it is before the first play frame, as far as that is known:
/// slot `k` at its initial value `init`, every field of [`GUARD_FIELDS`]
/// at the source's value then.
fn at_start<L: Leaves>(
    an: &Analysis,
    leaves: &mut L,
    res: &Residual,
    v: u32,
    (k, init): (u16, present::Val),
) -> R<Option<At0>> {
    charge(an, "compile:slot", 1)?;
    Ok(match res.node(v) {
        Node::Slot(j) if j == k => match init {
            present::Val::Int(n) => Some(At0::Int(n)),
            v if v == present::OFF => Some(At0::Off),
            _ => None,
        },
        Node::Known(Val::OffValue) => Some(At0::Off),
        Node::Known(Val::Int(n)) => Some(At0::Int(n)),
        Node::Known(Val::Bool(b)) => Some(At0::Bool(b)),
        Node::Read { .. } => match leaves.leaf(res, v)? {
            Some(l) if l.ty == Ty::Int => guard_field(&l.src).map(At0::Int),
            _ => None,
        },
        Node::Not(x) => match at_start(an, leaves, res, x, (k, init))? {
            Some(At0::Bool(b)) => Some(At0::Bool(!b)),
            _ => None,
        },
        Node::Cmp(op, a, b) => {
            let (x, y) = (
                at_start(an, leaves, res, a, (k, init))?,
                at_start(an, leaves, res, b, (k, init))?,
            );
            match (op, x, y) {
                (Bin::Eq | Bin::Ne, Some(x), Some(y)) => {
                    Some(At0::Bool((x == y) == (op == Bin::Eq)))
                }
                (_, Some(At0::Int(p)), Some(At0::Int(q))) => Some(At0::Bool(match op {
                    Bin::Lt => p < q,
                    Bin::Le => p <= q,
                    Bin::Gt => p > q,
                    _ => p >= q,
                })),
                _ => None,
            }
        }
        Node::Select { cond, then, other } => match at_start(an, leaves, res, cond, (k, init))? {
            Some(At0::Bool(true)) => at_start(an, leaves, res, then, (k, init))?,
            Some(At0::Bool(false)) => at_start(an, leaves, res, other, (k, init))?,
            _ => None,
        },
        _ => None,
    })
}

/// Refuses a condition `c` selecting slot `k`'s write that reads anything
/// but constants, slot `k` and the fields of [`GUARD_FIELDS`]: another
/// slot, whose change can enable it after the last play frame; the clock;
/// any other field.
fn frozen<L: Leaves>(an: &Analysis, leaves: &mut L, res: &Residual, c: u32, k: u16) -> R<()> {
    charge(an, "compile:slot", 1)?;
    match res.node(c) {
        Node::Known(_) | Node::Unreachable => Ok(()),
        Node::Slot(j) if j == k => Ok(()),
        Node::Slot(_) => refusal("a slot write guarded by another slot"),
        Node::Read { .. } => match leaves.leaf(res, c)? {
            Some(l) if l.ty == Ty::Int && guard_field(&l.src).is_some() => Ok(()),
            _ => refusal("a slot write guarded by a read off its allowlist"),
        },
        Node::StateUs => refusal("a slot write guarded by state that changes outside play"),
        Node::Not(x) | Node::Neg(x) => frozen(an, leaves, res, x, k),
        Node::Arith(_, a, b) | Node::Math(_, a, b) | Node::Cmp(_, a, b) => {
            frozen(an, leaves, res, a, k)?;
            frozen(an, leaves, res, b, k)
        }
        Node::Select { cond, then, other } => {
            frozen(an, leaves, res, cond, k)?;
            frozen(an, leaves, res, then, k)?;
            frozen(an, leaves, res, other, k)
        }
    }
}

/// Whether `c` having truthiness `pol` implies slot `k` is off.
fn implies_off(an: &Analysis, res: &Residual, c: u32, pol: bool, k: u16) -> R<bool> {
    charge(an, "compile:slot", 1)?;
    let off_test = |x: u32, y: u32| {
        matches!(res.node(x), Node::Slot(j) if j == k) && res.val(y) == Some(Val::OffValue)
    };
    Ok(match res.node(c) {
        Node::Known(Val::Bool(b)) => b != pol,
        Node::Cmp(op @ (Bin::Eq | Bin::Ne), a, b) if off_test(a, b) || off_test(b, a) => {
            (op == Bin::Eq) == pol
        }
        Node::Not(x) => implies_off(an, res, x, !pol, k)?,
        Node::Select { cond, then, other } => {
            let when = implies_off(an, res, cond, true, k)? || implies_off(an, res, then, pol, k)?;
            let unless = implies_off(an, res, cond, false, k)?
                || if other == cond {
                    // There the value is the condition's: false.
                    pol
                } else {
                    implies_off(an, res, other, pol, k)?
                };
            when && unless
        }
        _ => false,
    })
}

/// The writes in `v`, slot `k`'s final value, each checked under the
/// conditions `path` that select it: not off, under the slot's off test,
/// and under a condition false before the first play frame.
fn check_writes<L: Leaves>(
    an: &Analysis,
    leaves: &mut L,
    res: &Residual,
    v: u32,
    (k, init): (u16, present::Val),
    path: &mut Vec<(u32, bool)>,
) -> R<()> {
    charge(an, "compile:slot", 1)?;
    match res.node(v) {
        Node::Slot(j) if j == k => Ok(()),
        Node::Unreachable => Ok(()),
        Node::Select { cond, then, other } => {
            path.push((cond, true));
            let r = check_writes(an, leaves, res, then, (k, init), path);
            path.pop();
            r?;
            path.push((cond, false));
            let r = check_writes(an, leaves, res, other, (k, init), path);
            path.pop();
            r
        }
        _ => {
            if res.val(v) == Some(Val::OffValue) {
                return refusal("a slot written the off value");
            }
            let mut guarded = false;
            let mut before = false;
            for &(c, pol) in path.iter() {
                frozen(an, leaves, res, c, k)?;
                guarded = guarded || implies_off(an, res, c, pol, k)?;
                before = before
                    || match at_start(an, leaves, res, c, (k, init))? {
                        Some(At0::Bool(b)) => b != pol,
                        _ => false,
                    };
            }
            if !guarded {
                return refusal("a slot written again once set");
            }
            if !before {
                return refusal("a slot write that can happen outside play frames");
            }
            Ok(())
        }
    }
}

/// The root's slots `slots`, at their initial values and domains so far,
/// proven for `res` ([`Residual::stores`]): each slot's domain joined with
/// its writes', each write checked against the rules above.
pub(crate) fn slots<L: Leaves>(
    an: &Analysis,
    leaves: &mut L,
    res: &Residual,
    slots: &mut [Slot],
) -> R<()> {
    for &(k, v) in &res.stores {
        let Some(s) = slots.iter_mut().find(|s| s.slot == k) else {
            return refusal("a slot it does not own");
        };
        s.domain = value_domain(an, leaves, res, v, k, s.domain)?;
        let init = s.init;
        check_writes(an, leaves, res, v, (k, init), &mut Vec::new())?;
    }
    Ok(())
}

/// An integer part's bounds, `None` for a node that is never an integer.
pub(crate) type Iv = Option<(i128, i128)>;

const FULL: Iv = Some((i64::MIN as i128, i64::MAX as i128));

fn exact(iv: Iv) -> R<Iv> {
    let e = i128::from(EXACT);
    match iv {
        Some((lo, hi)) if lo < -e || hi > e => Err(Stop::Unsupported(Unsupported::Range)),
        _ => Ok(iv),
    }
}

fn hull(a: Iv, b: Iv) -> Iv {
    match (a, b) {
        (Some((p, q)), Some((r, s))) => Some((p.min(r), q.max(s))),
        (x, None) | (None, x) => x,
    }
}

/// Floor division (`//` of integers), the divisor not 0.
fn floor_div(a: i128, b: i128) -> i128 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Every node of `res` checked, operands before the nodes that use them,
/// each node that may be an integer given bounds (the whole of `i64` when
/// nothing narrower is known), each that never is none,
/// before either backend is chosen (a lowered expression too runs every
/// reached operation in the source):
/// its integer part's bounds, each arithmetic result's within ±2^53 and
/// each integer `//`'s divisor proven nonzero.
pub(crate) fn intervals<L: Leaves>(an: &Analysis, leaves: &L, res: &Residual) -> R<Vec<Iv>> {
    let n = res.len();
    let mut ivs: Vec<Iv> = Vec::with_capacity(n as usize);
    for r in 0..n {
        charge(an, "compile:interval", 1)?;
        did("compile:interval");
        let get = |x: u32| ivs.get(x as usize).copied().flatten();
        let ints = |x: u32| res.kind(x) & INT != 0;
        let iv = match res.node(r) {
            Node::Known(Val::Int(k)) => Some((i128::from(k), i128::from(k))),
            Node::Read { .. } => leaves
                .range(r)
                .map(|(lo, hi)| (i128::from(lo), i128::from(hi))),
            Node::StateUs => Some((0, i128::from(CLOCK_MAX))),
            Node::Neg(x) => exact(get(x).map(|(lo, hi)| (-hi, -lo)))?,
            // Both operands bounded: both may be integers (bounds are
            // given exactly to the nodes that may be).
            Node::Arith(op, a, b) => match (get(a), get(b)) {
                (Some((al, ah)), Some((bl, bh))) => {
                    let corners = |f: &dyn Fn(i128, i128) -> i128| {
                        let c = [f(al, bl), f(al, bh), f(ah, bl), f(ah, bh)];
                        let lo = c.iter().copied().min().unwrap_or(0);
                        let hi = c.iter().copied().max().unwrap_or(0);
                        Some((lo, hi))
                    };
                    match op {
                        Bin::Add => exact(corners(&|p, q| p + q))?,
                        Bin::Sub => exact(corners(&|p, q| p - q))?,
                        Bin::Mul => exact(corners(&|p, q| p.saturating_mul(q)))?,
                        Bin::Div => None,
                        Bin::IDiv if bl <= 0 && bh >= 0 => {
                            return Err(Stop::Unsupported(Unsupported::Construct(
                                "an integer // whose divisor can be 0",
                            )));
                        }
                        Bin::IDiv => exact(corners(&floor_div))?,
                        // `%`, the source's `a - b * floor(a / b)` in
                        // doubles: exact, so smaller than the divisor in
                        // magnitude (NaN, a float, for a zero divisor),
                        // while |a| + |b| < 2^53: `a / b` then rounds to no
                        // integer it is not, and `b * floor(a / b)` and the
                        // difference are exact. Past that it rounds, and
                        // its integer is of no known bound.
                        _ => {
                            let a_max = al.abs().max(ah.abs());
                            let m = bl.abs().max(bh.abs());
                            if a_max + m < i128::from(EXACT) {
                                (m > 0).then_some((1 - m, m - 1))
                            } else {
                                FULL
                            }
                        }
                    }
                }
                _ => None,
            },
            Node::Math(f, a, b) => match f {
                MathOp::Floor | MathOp::Ceil if res.kind(a) & FLT != 0 => FULL,
                MathOp::Floor | MathOp::Ceil => get(a),
                MathOp::Abs => exact(get(a).map(|(lo, hi)| {
                    let low = if lo <= 0 && hi >= 0 {
                        0
                    } else {
                        lo.abs().min(hi.abs())
                    };
                    (low, lo.abs().max(hi.abs()))
                }))?,
                MathOp::Max | MathOp::Min => hull(get(a), get(b)),
            },
            Node::Select { then, other, .. } => hull(get(then), get(other)),
            _ => None,
        };
        // "Of no known bound" is not "never an integer": a node that may
        // be an integer and that no rule above bounds (a timer read, whose
        // start or off value is any `long`) gets the whole of `i64`, so
        // arithmetic on it meets the ±2^53 rule.
        let iv = if ints(r) { iv.or(FULL) } else { None };
        ivs.push(iv);
    }
    Ok(ivs)
}

struct C<'a, L> {
    an: &'a Analysis,
    leaves: &'a mut L,
    out: Compiled,
    top: u8,
}

/// `res`, converted by `conv` (a `draw`, a number's value or a timer),
/// compiled into one callback, its slots `slots` proven ([`slots`]).
///
/// `root` is the root's integer bounds from [`intervals`], run before the
/// backend was chosen; each node is charged again here, as the walk it
/// replaces was.
pub(crate) fn compile<L: Leaves>(
    an: &Analysis,
    leaves: &mut L,
    res: &Residual,
    conv: Conv,
    root: Iv,
    slots: &[Slot],
) -> R<Compiled> {
    let conv = match conv {
        Conv::Truthy => present::Conv::Truthy,
        Conv::Int => present::Conv::Int,
        Conv::Timer => present::Conv::Timer,
        Conv::Text | Conv::Index => return Err(Stop::Unsupported(Unsupported::Type)),
    };
    for _ in 0..res.len() {
        charge(an, "compile:interval", 1)?;
        did("compile:interval");
    }
    if conv == present::Conv::Int {
        exact(root)?;
    }
    let guards = leaves.guards(res);
    let mut c = C {
        an,
        leaves,
        out: Compiled {
            code: Vec::new(),
            consts: Vec::new(),
            reads: Vec::new(),
            regs: 1,
            conv,
            domains: Vec::new(),
            slots: slots.to_vec(),
        },
        top: 0,
    };
    // Every read's availability first, its value aside.
    for guards in guards {
        c.read(
            Leaf {
                src: "true".to_owned(),
                ty: Ty::Bool,
                guards,
            },
            0,
        )?;
    }
    // Every store's value into its own register, then the result, then
    // the stores: a parallel assignment, after which nothing can fail.
    let n = u8::try_from(res.stores.len()).map_err(|_| Stop::Unsupported(LIMITS))?;
    for (i, &(_, v)) in (0_u8..).zip(&res.stores) {
        c.emit(res, v, i)?;
    }
    c.emit(res, res.root, n)?;
    for (i, &(k, v)) in (0_u8..).zip(&res.stores) {
        let own = slots
            .iter()
            .find(|s| s.slot == k)
            .map(|s| s.domain)
            .ok_or(Stop::Unsupported(LIMITS))?;
        let d = value_domain(an, &*c.leaves, res, v, k, own)?;
        let domain = u16::try_from(c.out.domains.len()).map_err(|_| Stop::Unsupported(LIMITS))?;
        c.out.domains.push(d);
        c.push(Insn::StoreSlot {
            slot: k,
            src: i,
            domain,
        })?;
    }
    c.push(Insn::Ret { src: n })?;
    c.out.regs = c.top.saturating_add(1);
    Ok(c.out)
}

impl<L: Leaves> C<'_, L> {
    fn push(&mut self, insn: Insn) -> R<usize> {
        if self.out.code.len() >= MAX_CALLBACK_OPS {
            return Err(Stop::Unsupported(LIMITS));
        }
        self.out.code.push(insn);
        Ok(self.out.code.len() - 1)
    }

    /// The current instruction count as a jump target.
    fn here(&self) -> R<u16> {
        u16::try_from(self.out.code.len()).map_err(|_| Stop::Unsupported(LIMITS))
    }

    fn patch(&mut self, at: usize, to: u16) {
        if let Some(Insn::Jump { to: t } | Insn::Test { to: t, .. }) = self.out.code.get_mut(at) {
            *t = to;
        }
    }

    fn reg(&mut self, r: u8) -> R<u8> {
        if usize::from(r) >= MAX_REGISTERS {
            return Err(Stop::Unsupported(LIMITS));
        }
        self.top = self.top.max(r);
        Ok(r)
    }

    fn constant(&mut self, v: present::Val) -> R<u16> {
        let same = |a: &present::Val| match (*a, v) {
            (present::Val::Float(x), present::Val::Float(y)) => x.to_bits() == y.to_bits(),
            (a, b) => a == b,
        };
        if let Some(k) = self.out.consts.iter().position(same) {
            return u16::try_from(k).map_err(|_| Stop::Unsupported(LIMITS));
        }
        let k = u16::try_from(self.out.consts.len()).map_err(|_| Stop::Unsupported(LIMITS))?;
        self.out.consts.push(v);
        Ok(k)
    }

    /// Read `leaf` into register `dst`.
    fn read(&mut self, leaf: Leaf, dst: u8) -> R<()> {
        let dst = self.reg(dst)?;
        let read = u16::try_from(self.out.reads.len()).map_err(|_| Stop::Unsupported(LIMITS))?;
        self.out.reads.push(leaf);
        self.push(Insn::Read { dst, read })?;
        Ok(())
    }

    /// Node `r` emitted into register `dst`, its operands into the ones
    /// above it.
    fn emit(&mut self, res: &Residual, r: u32, dst: u8) -> R<()> {
        charge(self.an, "compile:node", 1)?;
        did("compile:node");
        let dst = self.reg(dst)?;
        let up = dst.saturating_add(1);
        if let Some(leaf) = self.leaves.leaf(res, r)? {
            return self.read(leaf, dst);
        }
        let ty = || Err(Stop::Unsupported(Unsupported::Type));
        match res.node(r) {
            // The off value is a value only for a timer, its result or
            // its slots'.
            Node::Known(Val::OffValue) if self.out.conv == present::Conv::Timer => {
                let k = self.constant(present::OFF)?;
                self.push(Insn::Const { dst, k })?;
            }
            Node::Known(v) => {
                let pv = residual::num_val(v).ok_or(Stop::Unsupported(Unsupported::Type))?;
                let k = self.constant(pv)?;
                self.push(Insn::Const { dst, k })?;
            }
            Node::StateUs => {
                self.push(Insn::StateUs { dst })?;
            }
            Node::Slot(slot) => {
                self.push(Insn::LoadSlot { dst, slot })?;
            }
            Node::Unreachable => {
                let k = self.constant(present::Val::Nil)?;
                self.push(Insn::Const { dst, k })?;
            }
            // A read the mapping does not take as a value (a gauge or a
            // timer outside its comparison).
            Node::Read { .. } => return ty(),
            Node::Not(x) => {
                self.emit(res, x, dst)?;
                self.push(Insn::Not { dst, a: dst })?;
            }
            Node::Neg(x) => {
                self.emit(res, x, dst)?;
                self.push(Insn::Neg { dst, a: dst })?;
            }
            Node::Arith(op, a, b) => {
                self.emit(res, a, dst)?;
                self.emit(res, b, up)?;
                self.push(Insn::Arith {
                    op: residual::arith(op),
                    dst,
                    a: dst,
                    b: up,
                })?;
            }
            Node::Math(f, a, b) => {
                self.emit(res, a, dst)?;
                let one = match f {
                    MathOp::Floor => Some(MathFn::Floor),
                    MathOp::Ceil => Some(MathFn::Ceil),
                    MathOp::Abs => Some(MathFn::Abs),
                    MathOp::Max | MathOp::Min => None,
                };
                match one {
                    Some(f) => {
                        self.push(Insn::Math { f, dst, a: dst })?;
                    }
                    None => {
                        self.emit(res, b, up)?;
                        self.push(Insn::Extreme {
                            max: f == MathOp::Max,
                            dst,
                            a: dst,
                            b: up,
                        })?;
                    }
                }
            }
            Node::Cmp(op, a, b) => {
                self.emit(res, a, dst)?;
                self.emit(res, b, up)?;
                let (op, a, b) = match op {
                    Bin::Eq => (Cmp::Eq, dst, up),
                    Bin::Ne => (Cmp::Ne, dst, up),
                    Bin::Lt => (Cmp::Lt, dst, up),
                    Bin::Le => (Cmp::Le, dst, up),
                    Bin::Gt => (Cmp::Lt, up, dst),
                    _ => (Cmp::Le, up, dst),
                };
                self.push(Insn::Cmp { op, dst, a, b })?;
            }
            Node::Select { cond, then, other } => {
                self.emit(res, cond, dst)?;
                let test = self.push(Insn::Test {
                    cond: dst,
                    when: false,
                    to: 0,
                })?;
                self.emit(res, then, dst)?;
                let jump = self.push(Insn::Jump { to: 0 })?;
                let at = self.here()?;
                self.patch(test, at);
                self.emit(res, other, dst)?;
                let end = self.here()?;
                self.patch(jump, end);
            }
        }
        Ok(())
    }
}
