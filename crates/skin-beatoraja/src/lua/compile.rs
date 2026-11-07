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
//! Every integer is bounded from the ranges of what it reads: an integer
//! arithmetic result, and an integer a number's value converts, must lie
//! within ±2^53, where the source's doubles hold it exactly, so nothing
//! wraps and the source's double path agrees; an integer `//` or `%` whose
//! divisor can be 0 is refused, since Lua 5.4 raises there and the
//! source's doubles give NaN. A `math.floor` or `ceil` of a float is an
//! integer of no known bound. Each refusal is [`Unsupported`] and follows
//! the callback's role.

use skin_model::Ty;
use skin_model::present::{self, Cmp, Insn, MAX_CALLBACK_OPS, MAX_REGISTERS, MathFn};

use super::Unsupported;
use super::callback::Analysis;
use super::residual::{
    self, Conv, FLT, INT, LIMITS, MathOp, Node, R, Residual, Stop, Val, charge, did,
};
use super::syntax::{Bin, EXACT};

/// A read the program evaluates: its expression's source over the play
/// schema, its type, its availability guards and, for an integer, its
/// range.
#[derive(Debug)]
pub(crate) struct Leaf {
    pub(crate) src: String,
    pub(crate) ty: Ty,
    pub(crate) guards: Vec<&'static str>,
    pub(crate) range: Option<(i64, i64)>,
}

/// How residual nodes map to reads: a host read, or a comparison the
/// mapping takes whole (a gauge's look, a timer against the off value).
pub(crate) trait Leaves {
    /// Node `r` of `res` as a read, `None` when it is not one.
    fn leaf(&mut self, res: &Residual, r: u32) -> R<Option<Leaf>>;
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
    let iv = c.emit(res, res.root, 0)?;
    if conv == present::Conv::Int {
        exact(iv)?;
    }
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

    /// Node `r` emitted into register `dst`: its integer part's bounds.
    fn emit(&mut self, res: &Residual, r: u32, dst: u8) -> R<Iv> {
        charge(self.an, "compile:node", 1)?;
        did("compile:node");
        let dst = self.reg(dst)?;
        let up = dst.saturating_add(1);
        if let Some(leaf) = self.leaves.leaf(res, r)? {
            let iv = leaf.range.map(|(lo, hi)| (i128::from(lo), i128::from(hi)));
            let read =
                u16::try_from(self.out.reads.len()).map_err(|_| Stop::Unsupported(LIMITS))?;
            self.out.reads.push(leaf);
            self.push(Insn::Read { dst, read })?;
            return Ok(iv);
        }
        let ty = || Err(Stop::Unsupported(Unsupported::Type));
        match res.node(r) {
            Node::Known(v) => {
                let (pv, iv) = match v {
                    Val::Nil => (present::Val::Nil, None),
                    Val::Bool(b) => (present::Val::Bool(b), None),
                    Val::Int(n) => (present::Val::Int(n), Some((i128::from(n), i128::from(n)))),
                    Val::Float(f) => (present::Val::Float(f64::from_bits(f)), None),
                    _ => return ty(),
                };
                let k = self.constant(pv)?;
                self.push(Insn::Const { dst, k })?;
                Ok(iv)
            }
            Node::Unreachable => {
                let k = self.constant(present::Val::Nil)?;
                self.push(Insn::Const { dst, k })?;
                Ok(None)
            }
            // A read the mapping does not take as a value (a gauge or a
            // timer outside its comparison).
            Node::Read { .. } => ty(),
            Node::Not(x) => {
                self.emit(res, x, dst)?;
                self.push(Insn::Not { dst, a: dst })?;
                Ok(None)
            }
            Node::Neg(x) => {
                let iv = self.emit(res, x, dst)?;
                self.push(Insn::Neg { dst, a: dst })?;
                exact(iv.map(|(lo, hi)| (-hi, -lo)))
            }
            Node::Arith(op, a, b) => self.arith(res, op, (a, b), dst, up),
            Node::Math(f, a, b) => {
                let ia = self.emit(res, a, dst)?;
                let flt = res.kind(a) & FLT != 0;
                match f {
                    MathOp::Floor | MathOp::Ceil | MathOp::Abs => {
                        let f = match f {
                            MathOp::Floor => MathFn::Floor,
                            MathOp::Ceil => MathFn::Ceil,
                            _ => MathFn::Abs,
                        };
                        self.push(Insn::Math { f, dst, a: dst })?;
                        if f == MathFn::Abs {
                            let m = ia.map(|(lo, hi)| {
                                let top = lo.abs().max(hi.abs());
                                (
                                    if lo <= 0 && hi >= 0 {
                                        0
                                    } else {
                                        lo.abs().min(hi.abs())
                                    },
                                    top,
                                )
                            });
                            return exact(m);
                        }
                        Ok(if flt { hull(ia, FULL) } else { ia })
                    }
                    MathOp::Max | MathOp::Min => {
                        let ib = self.emit(res, b, up)?;
                        self.push(Insn::Extreme {
                            max: f == MathOp::Max,
                            dst,
                            a: dst,
                            b: up,
                        })?;
                        Ok(hull(ia, ib))
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
                Ok(None)
            }
            Node::Select { cond, then, other } => {
                self.emit(res, cond, dst)?;
                let test = self.push(Insn::Test {
                    cond: dst,
                    when: false,
                    to: 0,
                })?;
                let it = self.emit(res, then, dst)?;
                let jump = self.push(Insn::Jump { to: 0 })?;
                let at = self.here()?;
                self.patch(test, at);
                let io = self.emit(res, other, dst)?;
                let end = self.here()?;
                self.patch(jump, end);
                Ok(hull(it, io))
            }
        }
    }

    /// Arithmetic `op` of `a` and `b` into `dst`, `b` through `up`.
    fn arith(&mut self, res: &Residual, op: Bin, (a, b): (u32, u32), dst: u8, up: u8) -> R<Iv> {
        let ia = self.emit(res, a, dst)?;
        let ib = self.emit(res, b, up)?;
        self.push(Insn::Arith {
            op: residual::arith(op),
            dst,
            a: dst,
            b: up,
        })?;
        // The integer part: only where both operands may be integers.
        let ints = res.kind(a) & INT != 0 && res.kind(b) & INT != 0;
        let (Some((al, ah)), Some((bl, bh)), true) = (ia, ib, ints) else {
            return Ok(None);
        };
        let corners = |f: &dyn Fn(i128, i128) -> i128| {
            let c = [f(al, bl), f(al, bh), f(ah, bl), f(ah, bh)];
            let lo = c.iter().copied().min().unwrap_or(0);
            let hi = c.iter().copied().max().unwrap_or(0);
            Some((lo, hi))
        };
        let iv = match op {
            Bin::Add => corners(&|p, q| p + q),
            Bin::Sub => corners(&|p, q| p - q),
            Bin::Mul => corners(&|p, q| p.saturating_mul(q)),
            Bin::Div => None,
            _ if bl <= 0 && bh >= 0 => {
                return Err(Stop::Unsupported(Unsupported::Construct(
                    "an integer // or % whose divisor can be 0",
                )));
            }
            Bin::IDiv => corners(&floor_div),
            _ if bl > 0 => Some((0, bh - 1)),
            _ => Some((bl + 1, 0)),
        };
        exact(iv)
    }
}
