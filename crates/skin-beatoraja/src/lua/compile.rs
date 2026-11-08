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

use skin_model::Ty;
use skin_model::present::{self, Cmp, Insn, MAX_CALLBACK_OPS, MAX_REGISTERS, MathFn};

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
}

/// An integer part's bounds, `None` for a node that is never an integer.
type Iv = Option<(i128, i128)>;

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
            Node::Neg(x) => exact(get(x).map(|(lo, hi)| (-hi, -lo)))?,
            Node::Arith(op, a, b) => match (get(a), get(b), ints(a) && ints(b)) {
                (Some((al, ah)), Some((bl, bh)), true) => {
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
                MathOp::Floor | MathOp::Ceil if res.kind(a) & FLT != 0 => hull(get(a), FULL),
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

/// `res`, converted by `conv` (a `draw` or a number's value), compiled into
/// one callback.
pub(crate) fn compile<L: Leaves>(
    an: &Analysis,
    leaves: &mut L,
    res: &Residual,
    conv: Conv,
) -> R<Compiled> {
    let conv = match conv {
        Conv::Truthy => present::Conv::Truthy,
        Conv::Int => present::Conv::Int,
        Conv::Text | Conv::Index => return Err(Stop::Unsupported(Unsupported::Type)),
    };
    let ivs = intervals(an, leaves, res)?;
    if conv == present::Conv::Int {
        exact(ivs.get(res.root as usize).copied().flatten())?;
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
    c.emit(res, res.root, 0)?;
    c.push(Insn::Ret { src: 0 })?;
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
            Node::Known(v) => {
                let pv = match v {
                    Val::Nil => present::Val::Nil,
                    Val::Bool(b) => present::Val::Bool(b),
                    Val::Int(n) => present::Val::Int(n),
                    Val::Float(f) => present::Val::Float(f64::from_bits(f)),
                    _ => return ty(),
                };
                let k = self.constant(pv)?;
                self.push(Insn::Const { dst, k })?;
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
