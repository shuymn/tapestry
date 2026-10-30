//! Name resolution, type checking and range checking, in one bottom-up pass
//! over the unfolded tree.
//!
//! Every branch is checked, including the ones options will fold away, so
//! an expression that compiles is valid under every option configuration.
//! Every integer node's interval must lie in `i32`.

use super::error::{CompileError, ErrorKind};
use super::interval::Iv;
use super::parse::{Ast, BinOp, Func, Id, Kind, UnOp};
use super::program::{Binary, Op, Unary};
use super::{MAX_ELAPSED_MS, OptionDef, Scope, TIMER_OFF};
use crate::schema::{FieldTy, SchemaError, Ty};

/// A checked tree, ready to fold and emit.
#[derive(Clone, Debug)]
pub(super) enum T {
    Const(i32),
    /// A load, elapsed or on.
    Read(Op),
    Un(Unary, Box<T>),
    Bin(Binary, Box<T>, Box<T>),
    /// Condition, then, else.
    Select(Box<T>, Box<T>, Box<T>),
}

/// A checked node: its tree, type and interval (for a bool or an enum, the
/// range of its encoding).
struct Typed {
    t: T,
    ty: Ty,
    iv: Iv,
}

pub(super) fn check(ast: &Ast<'_>, scope: &Scope<'_>, want: Ty) -> Result<T, CompileError> {
    Ok(Checker { ast, scope }.want(ast.root(), want)?.t)
}

struct Checker<'a, 's> {
    ast: &'a Ast<'s>,
    scope: &'a Scope<'a>,
}

impl Checker<'_, '_> {
    fn err(&self, id: Id, kind: ErrorKind) -> CompileError {
        CompileError::new(self.ast.span(id), kind)
    }

    /// `iv` as the interval of integer node `id`, refused outside `i32`.
    fn int(&self, id: Id, t: T, iv: Iv) -> Result<Typed, CompileError> {
        if !iv.in_i32() {
            return Err(self.err(
                id,
                ErrorKind::OutOfRange {
                    lo: iv.lo,
                    hi: iv.hi,
                },
            ));
        }
        Ok(Typed { t, ty: Ty::Int, iv })
    }

    fn boolean(t: T) -> Typed {
        Typed {
            t,
            ty: Ty::Bool,
            iv: Iv::new(0, 1),
        }
    }

    /// Checks node `id` and requires type `ty`.
    fn want(&self, id: Id, ty: Ty) -> Result<Typed, CompileError> {
        let n = self.node(id)?;
        if n.ty != ty {
            return Err(self.err(
                id,
                ErrorKind::Type {
                    expected: ty,
                    found: n.ty,
                },
            ));
        }
        Ok(n)
    }

    /// Checks node `id` and requires an integer.
    fn ordered(&self, id: Id) -> Result<Typed, CompileError> {
        let n = self.node(id)?;
        if n.ty != Ty::Int {
            return Err(self.err(id, ErrorKind::NotOrdered { found: n.ty }));
        }
        Ok(n)
    }

    fn node(&self, id: Id) -> Result<Typed, CompileError> {
        match self.ast.node(id).kind {
            Kind::Int(v) => self.int(id, T::Const(v), Iv::point(v)),
            Kind::Bool(b) => Ok(Self::boolean(T::Const(i32::from(b)))),
            Kind::Name(_) => self.field(id),
            Kind::Un(UnOp::Neg, a) => {
                let a = self.ordered(a)?;
                self.int(id, T::Un(Unary::Neg, Box::new(a.t)), a.iv.neg())
            }
            Kind::Un(UnOp::Not, a) => {
                let a = self.want(a, Ty::Bool)?;
                Ok(Self::boolean(T::Un(Unary::Not, Box::new(a.t))))
            }
            Kind::Bin(op, a, b) => self.binary(id, op, a, b),
            Kind::If(c, a, b) => {
                let c = self.want(c, Ty::Bool)?;
                let a = self.node(a)?;
                let b_typed = self.want(b, a.ty)?;
                let t = T::Select(Box::new(c.t), Box::new(a.t), Box::new(b_typed.t));
                let iv = a.iv.hull(b_typed.iv);
                if a.ty == Ty::Int {
                    self.int(id, t, iv)
                } else {
                    Ok(Typed { t, ty: a.ty, iv })
                }
            }
            Kind::Call(func, args, n) => self.call(id, func, args, n),
        }
    }

    /// A field read.
    fn field(&self, id: Id) -> Result<Typed, CompileError> {
        let name = self.ast.text(id);
        let Some((slot, field)) = self.scope.schema.field(name) else {
            return Err(self.err(id, ErrorKind::UnknownName { name: name.into() }));
        };
        let load = |lo, hi| T::Read(Op::Load { slot, lo, hi });
        match field.ty {
            FieldTy::Bool => Ok(Self::boolean(load(0, 1))),
            FieldTy::Int { lo, hi } => {
                if lo > hi {
                    return Err(self.err(id, SchemaError::EmptyRange(field.name).into()));
                }
                self.int(id, load(lo, hi), Iv::new(lo, hi))
            }
            FieldTy::Enum(e) => {
                let last = self
                    .scope
                    .schema
                    .variants(e)
                    .and_then(|v| v.len().checked_sub(1))
                    .ok_or_else(|| self.err(id, SchemaError::BadEnum(field.name).into()))?;
                let last =
                    i32::try_from(last).map_err(|_| self.err(id, SchemaError::TooMany.into()))?;
                Ok(Typed {
                    t: load(0, last),
                    ty: Ty::Enum(e),
                    iv: Iv::new(0, last),
                })
            }
        }
    }

    /// Whether node `id` can be an enum variant literal: a bare
    /// single-segment name that is not a field.
    fn variant_candidate(&self, id: Id) -> bool {
        let name = self.ast.text(id);
        matches!(self.ast.node(id).kind, Kind::Name(_))
            && !name.contains('.')
            && self.scope.schema.field(name).is_none()
    }

    /// The operands of `==` or `!=`: a variant literal on one side takes
    /// the enum of the other.
    fn equality_operands(&self, a: Id, b: Id) -> Result<(Typed, Typed), CompileError> {
        let literal_a = self.variant_candidate(a);
        let literal_b = self.variant_candidate(b);
        if literal_a && !literal_b {
            let tb = self.node(b)?;
            return Ok((self.variant(a, tb.ty)?, tb));
        }
        let ta = self.node(a)?;
        if literal_b {
            let tb = self.variant(b, ta.ty)?;
            return Ok((ta, tb));
        }
        let tb = self.node(b)?;
        Ok((ta, tb))
    }

    /// Name `id` as a variant of `ty`, which must be an enum.
    fn variant(&self, id: Id, ty: Ty) -> Result<Typed, CompileError> {
        let name = self.ast.text(id);
        let Ty::Enum(e) = ty else {
            return Err(self.err(id, ErrorKind::UnknownName { name: name.into() }));
        };
        let index = self
            .scope
            .schema
            .variants(e)
            .and_then(|v| v.iter().position(|&v| v == name))
            .and_then(|i| i32::try_from(i).ok())
            .ok_or_else(|| self.err(id, ErrorKind::UnknownVariant { name: name.into() }))?;
        Ok(Typed {
            t: T::Const(index),
            ty,
            iv: Iv::point(index),
        })
    }

    fn binary(&self, id: Id, op: BinOp, a: Id, b: Id) -> Result<Typed, CompileError> {
        let arith = |op: Binary, f: fn(Iv, Iv) -> Iv| -> Result<Typed, CompileError> {
            let ta = self.ordered(a)?;
            let tb = self.ordered(b)?;
            self.int(
                id,
                T::Bin(op, Box::new(ta.t), Box::new(tb.t)),
                f(ta.iv, tb.iv),
            )
        };
        let compare = |op: Binary, swap: bool| -> Result<Typed, CompileError> {
            let ta = self.ordered(a)?;
            let tb = self.ordered(b)?;
            let (l, r) = if swap { (tb, ta) } else { (ta, tb) };
            Ok(Self::boolean(T::Bin(op, Box::new(l.t), Box::new(r.t))))
        };
        let logic = |op: Binary| -> Result<Typed, CompileError> {
            let ta = self.want(a, Ty::Bool)?;
            let tb = self.want(b, Ty::Bool)?;
            Ok(Self::boolean(T::Bin(op, Box::new(ta.t), Box::new(tb.t))))
        };
        match op {
            BinOp::Add => arith(Binary::Add, Iv::add),
            BinOp::Sub => arith(Binary::Sub, Iv::sub),
            BinOp::Mul => arith(Binary::Mul, Iv::mul),
            BinOp::Div => arith(Binary::Div, Iv::div),
            BinOp::Rem => arith(Binary::Rem, Iv::rem),
            BinOp::Lt => compare(Binary::Lt, false),
            BinOp::Le => compare(Binary::Le, false),
            BinOp::Gt => compare(Binary::Lt, true),
            BinOp::Ge => compare(Binary::Le, true),
            BinOp::And => logic(Binary::And),
            BinOp::Or => logic(Binary::Or),
            BinOp::Eq | BinOp::Ne => {
                let (ta, tb) = self.equality_operands(a, b)?;
                if ta.ty != tb.ty {
                    return Err(self.err(
                        b,
                        ErrorKind::Type {
                            expected: ta.ty,
                            found: tb.ty,
                        },
                    ));
                }
                let op = if op == BinOp::Eq {
                    Binary::Eq
                } else {
                    Binary::Ne
                };
                Ok(Self::boolean(T::Bin(op, Box::new(ta.t), Box::new(tb.t))))
            }
        }
    }

    fn call(&self, id: Id, func: Func, args: [Id; 3], n: u8) -> Result<Typed, CompileError> {
        let [x, y, z] = args;
        match func {
            Func::Abs => {
                let tx = self.ordered(x)?;
                self.int(id, T::Un(Unary::Abs, Box::new(tx.t)), tx.iv.abs())
            }
            Func::Min | Func::Max => {
                let tx = self.ordered(x)?;
                let ty = self.ordered(y)?;
                let (op, iv) = if func == Func::Min {
                    (Binary::Min, tx.iv.min(ty.iv))
                } else {
                    (Binary::Max, tx.iv.max(ty.iv))
                };
                self.int(id, T::Bin(op, Box::new(tx.t), Box::new(ty.t)), iv)
            }
            // `clamp(x, l, h)` is `min(max(x, l), h)`: total when `l > h`.
            Func::Clamp => {
                let tx = self.ordered(x)?;
                let tl = self.ordered(y)?;
                let th = self.ordered(z)?;
                let iv = tx.iv.max(tl.iv).min(th.iv);
                let inner = T::Bin(Binary::Max, Box::new(tx.t), Box::new(tl.t));
                self.int(id, T::Bin(Binary::Min, Box::new(inner), Box::new(th.t)), iv)
            }
            Func::On | Func::Elapsed => {
                let slot = self.timer(x)?;
                if func == Func::On {
                    Ok(Self::boolean(T::Read(Op::On(slot))))
                } else {
                    self.int(
                        id,
                        T::Read(Op::Elapsed(slot)),
                        Iv::new(TIMER_OFF, MAX_ELAPSED_MS),
                    )
                }
            }
            Func::Option => self.option(id, n, x, y),
        }
    }

    /// The slot of the timer named by argument `id`.
    fn timer(&self, id: Id) -> Result<u16, CompileError> {
        let name = self.name_arg(id, "a timer name")?;
        if let Some((slot, _)) = self.scope.schema.timer(name) {
            return Ok(slot);
        }
        let kind = if self.scope.schema.field(name).is_some() {
            ErrorKind::NotATimer { name: name.into() }
        } else {
            ErrorKind::UnknownTimer { name: name.into() }
        };
        Err(self.err(id, kind))
    }

    /// The text of argument `id`, which must be a bare name.
    fn name_arg(&self, id: Id, what: &'static str) -> Result<&str, CompileError> {
        if matches!(self.ast.node(id).kind, Kind::Name(_)) {
            Ok(self.ast.text(id))
        } else {
            Err(self.err(id, ErrorKind::Expected { what }))
        }
    }

    /// `option(name)` or `option(name, choice)`, as a bool constant.
    fn option(&self, id: Id, n: u8, name_id: Id, choice_id: Id) -> Result<Typed, CompileError> {
        let name = self.name_arg(name_id, "an option name")?;
        let def = self
            .scope
            .options
            .iter()
            .find(|o| match o {
                OptionDef::Toggle { name: n, .. } | OptionDef::OneOf { name: n, .. } => *n == name,
            })
            .ok_or_else(|| self.err(name_id, ErrorKind::UnknownOption { name: name.into() }))?;
        let arity = |expected| {
            self.err(
                id,
                ErrorKind::Arity {
                    func: "option",
                    expected,
                    found: usize::from(n),
                },
            )
        };
        let value = match (*def, n) {
            (OptionDef::Toggle { on, .. }, 1) => on,
            (
                OptionDef::OneOf {
                    choices, chosen, ..
                },
                2,
            ) => {
                let choice = self.name_arg(choice_id, "a choice name")?;
                let index = choices.iter().position(|&c| c == choice).ok_or_else(|| {
                    self.err(
                        choice_id,
                        ErrorKind::UnknownChoice {
                            option: name.into(),
                            choice: choice.into(),
                        },
                    )
                })?;
                index == chosen
            }
            (OptionDef::Toggle { .. }, _) => return Err(arity(1)),
            (OptionDef::OneOf { .. }, _) => return Err(arity(2)),
        };
        Ok(Self::boolean(T::Const(i32::from(value))))
    }
}
