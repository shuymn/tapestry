//! Lua callbacks where the import consumes them (design.md "beatoraja Lua
//! Skins", per-frame closures; docs/stage3.md §3, per-frame functions):
//! an element's `draw` after its role and its ops are decided, and a
//! number's `value` where a `value` field is resolved, before `ref`.
//!
//! A write-free callback ([`crate::lua::callback`]) is lowered into an
//! expression's source, compiled through the importer's compile path and
//! its budget like any condition or value: each read takes its JSON
//! mapping, with that mapping's availability guard (joined into the
//! element's visibility, so a value that can be unknown draws nothing
//! while it is), its D4 deviation and its critical part (`number(101)`
//! the EX score, as 101 in JSON):
//!
//! - `number(n)`: the value property `n` ([`property::int_by_id`]), its
//!   range that of its fields;
//! - `option(n)`: the built-in boolean property `n` ([`ops::numeric`] with
//!   no option items, as the source's function reads only the factory),
//!   one it does not know `false`, as the source's is;
//! - `timer(n) == timer_off_value` and `~=`: `!on(t)` and `on(t)` of the
//!   JSON mapping's timer `t`, a timer never set off;
//! - `gauge_type() == k` and `~=`: `gauge.kind` against the look `k`
//!   selects (0 to 5, `assist_easy` to `hazard`; any other `k` matches no
//!   look), under `gauge.valid`, as JSON's gauge conditions;
//! - `skin_config.option["name"] == k`, decorative callbacks only: its
//!   captured value, folded.
//!
//! Helpers are inlined, each call charged its expanded size before it is
//! expanded ([`MAX_EXPANDED_NODES`]), and locals inlined where they are
//! read, so a read nothing uses is dropped. Every integer intermediate is
//! bounded from the ranges of what it reads, and one that could leave
//! `i32` is unsupported: within that range Lua's integers and the
//! expressions' agree. The generated source is bounded by
//! [`MAX_SOURCE_BYTES`].
//!
//! A callback that does not lower is unsupported: a decorative one is
//! replaced by its typed constant, `false` for a `draw` (the element is
//! never drawn) and the omitted number for a `value`, reported with the
//! reason; a critical one refuses the candidate at its definition. A
//! `value`'s role is its reads': one that reads the EX score or the max
//! combo is critical, whether or not the rest lowers. A cap or
//! cancellation refuses the candidate whatever the role.

use std::collections::BTreeMap;

use skin_model::{MAX_SOURCE_BYTES, PLAY};

use super::ops::{self, Cond};
use super::policy::{CriticalPart, Role};
use super::property::{self, IntProp};
use super::report::{NoteKind, Span};
use super::timer::{Custom, Timer};
use super::{Cx, Prop};
use crate::error::ImportError;
use crate::lua::callback::{
    Analysis, Callee, Captured, Disposition, Ty, fatal as sentinel, numeral, resolve_callee, tree,
};
use crate::lua::limits::{MAX_AST_DEPTH, MAX_EXPANDED_NODES};
use crate::lua::syntax::{Ast, Bin, Expr, Stmt, Un};
use crate::lua::{HOST_APIS, LuaLimit, LuaWhy, Unsupported};
use crate::tree::{Kind, LuaScalar, NONE};

/// The gauge looks `gauge_type()` selects, by index.
const LOOKS: [&str; 6] = ["assist_easy", "easy", "normal", "hard", "ex_hard", "hazard"];

/// A lowered callback.
#[derive(Debug)]
pub(crate) struct Lowered {
    pub(crate) src: String,
    /// The availability guards of its reads, in order.
    pub(crate) guards: Vec<&'static str>,
    /// The critical part a read shows.
    pub(crate) part: Option<CriticalPart>,
    pub(crate) deviates: bool,
    /// Whether it folds a configuration read.
    pub(crate) config: bool,
}

/// A number's resolved value: a property's or a callback's.
pub(crate) struct NumberValue {
    pub(crate) expr: String,
    pub(crate) guard: Option<String>,
    pub(crate) role: Role,
    pub(crate) deviates: bool,
}

impl NumberValue {
    pub(crate) fn of(v: property::Value) -> Self {
        Self {
            expr: v.expr().to_owned(),
            guard: v.availability().map(str::to_owned),
            role: v.part().map_or(Role::Decorative, Role::Critical),
            deviates: v.deviates(),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The callbacks this thread lowered, in order: a `draw`'s whole
    /// condition (its guards joined first) or a `value`'s expression.
    pub(crate) static LOWERED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Why lowering stopped.
enum Stop {
    Unsupported(Unsupported),
    Fatal(LuaWhy),
}

/// A walk's stop for `why`: fatal for a cap or cancellation.
fn stop(why: Unsupported) -> Stop {
    sentinel(why).map_or(Stop::Unsupported(why), Stop::Fatal)
}

type R<T> = Result<T, Stop>;

/// A lowered subexpression.
enum V {
    Int {
        src: String,
        lo: i64,
        hi: i64,
    },
    Bool(String),
    Gauge,
    /// A mapped timer's name, or `None` for one never set.
    Timer(Option<&'static str>),
    TimerOff,
    Config(i64),
}

/// The JSON mapping's timer name of model slot `slot`.
fn timer_name(slot: u16) -> Option<&'static str> {
    PLAY.timers.get(usize::from(slot)).map(|t| t.name)
}

/// An instance's body as [`Lower::body`] reads it.
type Body<'a> = (&'a Ast, u32, Vec<(u32, u32)>, u32);

struct Lower<'a> {
    an: &'a Analysis,
    custom: &'a Custom,
    cancelled: &'a dyn Fn() -> bool,
    critical: bool,
    out: Lowered,
    /// Each instance's expanded size, once computed.
    sizes: Vec<Option<u64>>,
    /// How deep the current walk is: every expression level, an inlined
    /// local's and a helper's included.
    depth: u32,
}

impl Lower<'_> {
    fn bytes(src: String) -> R<String> {
        if src.len() > MAX_SOURCE_BYTES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Generated)));
        }
        Ok(src)
    }

    fn int(src: String, lo: i64, hi: i64) -> R<V> {
        if lo < i64::from(i32::MIN) || hi > i64::from(i32::MAX) {
            return Err(Stop::Unsupported(Unsupported::Range));
        }
        Ok(V::Int {
            src: Self::bytes(src)?,
            lo,
            hi,
        })
    }

    fn boolean(src: String) -> R<V> {
        Ok(V::Bool(Self::bytes(src)?))
    }

    fn guard(&mut self, g: &'static str) {
        if !self.out.guards.contains(&g) {
            self.out.guards.push(g);
        }
    }

    /// The body of instance `i`: its tree, the instance, its locals (each
    /// declaration and its initializer) and its return expression.
    fn body(an: &Analysis, i: u32) -> R<Body<'_>> {
        let unsupported = Stop::Unsupported(Unsupported::Capture("an untagged closure"));
        let (ast, func, _) = tree(an, i).ok_or(unsupported)?;
        let f = ast
            .func(func)
            .ok_or(Stop::Unsupported(Unsupported::Capture(
                "an untagged closure",
            )))?;
        let mut locals = Vec::new();
        for &s in ast.list(f.body) {
            an.charge(1).map_err(stop)?;
            match ast.stmt(s) {
                Stmt::Local { names, exprs } => {
                    if let (&[d], &[e]) = (ast.list(names), ast.list(exprs)) {
                        locals.push((d, e));
                    }
                }
                Stmt::Return(list) => {
                    if let &[e] = ast.list(list) {
                        return Ok((ast, i, locals, e));
                    }
                }
                _ => {}
            }
        }
        Err(Stop::Unsupported(Unsupported::Construct("no return")))
    }

    /// Instance `i`'s expanded size: its return expression's nodes, each
    /// local and helper read counted at its full size.
    fn size(&mut self, i: u32) -> R<u64> {
        if let Some(Some(n)) = self.sizes.get(i as usize) {
            return Ok(*n);
        }
        if (self.cancelled)() {
            return Err(Stop::Fatal(LuaWhy::Cancelled));
        }
        let (ast, inst, locals, ret) = Self::body(self.an, i)?;
        let mut sized: Vec<(u32, u64)> = Vec::with_capacity(locals.len());
        for &(d, e) in &locals {
            let n = self.size_expr(ast, inst, &sized, e)?;
            sized.push((d, n));
        }
        let n = self.size_expr(ast, inst, &sized, ret)?;
        if self.sizes.len() <= i as usize {
            self.sizes.resize(i as usize + 1, None);
        }
        if let Some(s) = self.sizes.get_mut(i as usize) {
            *s = Some(n);
        }
        Ok(n)
    }

    fn size_expr(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u64)], e: u32) -> R<u64> {
        self.deeper(|l| l.size_expr_inner(ast, inst, locals, e))
    }

    fn size_expr_inner(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u64)], e: u32) -> R<u64> {
        Ok(match ast.expr(e) {
            Expr::Local(d) => match locals.iter().rev().find(|(l, _)| *l == d) {
                Some(&(_, n)) => n,
                None => 1,
            },
            Expr::Paren(x) | Expr::Unary(_, x) => {
                self.size_expr(ast, inst, locals, x)?.saturating_add(1)
            }
            Expr::Binary(_, a, b) => self
                .size_expr(ast, inst, locals, a)?
                .saturating_add(self.size_expr(ast, inst, locals, b)?)
                .saturating_add(1),
            Expr::Call { callee, .. } => {
                match resolve_callee(self.an, ast, inst, callee).map_err(Stop::Unsupported)? {
                    Callee::Helper(j) => self.size(j)?.saturating_add(1),
                    Callee::Host(_) => 1,
                }
            }
            _ => 1,
        })
    }

    /// Instance `i` lowered.
    fn instance(&mut self, i: u32) -> R<V> {
        #[cfg(test)]
        crate::lua::tests::expansion_witness();
        if (self.cancelled)() {
            return Err(Stop::Fatal(LuaWhy::Cancelled));
        }
        let (ast, inst, locals, ret) = Self::body(self.an, i)?;
        self.expr(ast, inst, &locals, ret)
    }

    fn expr(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u32)], e: u32) -> R<V> {
        self.deeper(|l| l.expr_inner(ast, inst, locals, e))
    }

    fn expr_inner(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u32)], e: u32) -> R<V> {
        let unsupported = |w| Err(Stop::Unsupported(w));
        match ast.expr(e) {
            Expr::Int(n) => Self::literal(n),
            Expr::True => Self::boolean("true".into()),
            Expr::False => Self::boolean("false".into()),
            Expr::Paren(x) => self.expr(ast, inst, locals, x),
            Expr::Local(d) => {
                if let Some(&(_, init)) = locals.iter().rev().find(|(l, _)| *l == d) {
                    // The locals before it, as its initializer saw them.
                    let k = locals.iter().position(|&(l, _)| l == d).unwrap_or(0);
                    let before = locals.get(..k).unwrap_or_default();
                    return self.expr(ast, inst, before, init);
                }
                match self.an.upvalue(inst, d) {
                    Some(Captured::Int(n)) => Self::literal(n),
                    Some(Captured::Bool(b)) => Self::boolean(b.to_string()),
                    _ => unsupported(Unsupported::Capture("an upvalue")),
                }
            }
            Expr::Index { obj, key } => {
                if let Expr::Str(s) = ast.expr(key)
                    && ast.str(s) == b"timer_off_value"
                {
                    return Ok(V::TimerOff);
                }
                self.config(ast, inst, obj, key)
            }
            Expr::Call { callee, args } => {
                match resolve_callee(self.an, ast, inst, callee).map_err(Stop::Unsupported)? {
                    Callee::Helper(j) => {
                        let n = self.size(j)?;
                        let total = self.an.expanded.get().saturating_add(n);
                        if total > MAX_EXPANDED_NODES {
                            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
                        }
                        self.an.expanded.set(total);
                        self.instance(j)
                    }
                    Callee::Host(api) => {
                        let arg = ast.list(args).first().and_then(|&a| numeral(ast, a));
                        self.read(api, arg)
                    }
                }
            }
            Expr::Unary(op, x) => match (op, self.expr(ast, inst, locals, x)?) {
                (Un::Neg, V::Int { src, lo, hi }) => Self::int(
                    format!("(-{src})"),
                    hi.checked_neg().unwrap_or(i64::MAX),
                    lo.checked_neg().unwrap_or(i64::MAX),
                ),
                (Un::Not, V::Bool(s)) => Self::boolean(format!("(!{s})")),
                _ => unsupported(Unsupported::Type),
            },
            Expr::Binary(op, a, b) => {
                let (va, vb) = (
                    self.expr(ast, inst, locals, a)?,
                    self.expr(ast, inst, locals, b)?,
                );
                self.binary(op, va, vb)
            }
            _ => unsupported(Unsupported::Type),
        }
    }

    fn literal(n: i64) -> R<V> {
        let src = if n < 0 {
            format!("(-{})", n.unsigned_abs())
        } else {
            n.to_string()
        };
        Self::int(src, n, n)
    }

    /// `skin_config.option["name"]`: its captured value.
    fn config(&mut self, ast: &Ast, inst: u32, obj: u32, key: u32) -> R<V> {
        if self.critical {
            return Err(Stop::Unsupported(Unsupported::Config));
        }
        let (Expr::Index { obj: cfg, .. }, Expr::Str(k)) = (ast.expr(obj), ast.expr(key)) else {
            return Err(Stop::Unsupported(Unsupported::Type));
        };
        let Expr::Global { env, .. } = ast.expr(cfg) else {
            return Err(Stop::Unsupported(Unsupported::Type));
        };
        let Some(Captured::Env(i)) = self.an.upvalue(inst, env) else {
            return Err(Stop::Unsupported(Unsupported::Capture("_ENV")));
        };
        let k = ast.str(k);
        self.out.config = true;
        self.an
            .envs
            .get(i as usize)
            .and_then(|c| c.options.iter().find(|(n, _)| n == k))
            .map(|&(_, v)| V::Config(v))
            .ok_or(Stop::Unsupported(Unsupported::Capture("an option entry")))
    }

    /// A host read of API `api` with the numeral `arg`.
    fn read(&mut self, api: u16, arg: Option<i64>) -> R<V> {
        let name = HOST_APIS.get(usize::from(api)).map_or("", |&(_, n)| n);
        let unmapped = || {
            Stop::Unsupported(Unsupported::Unmapped {
                api,
                id: arg.unwrap_or(0),
            })
        };
        let id = arg.and_then(|a| i32::try_from(a).ok());
        match name {
            "gauge_type" => Ok(V::Gauge),
            "number" => {
                let IntProp::Shown(v) = property::int_by_id(id.ok_or_else(unmapped)?) else {
                    return Err(unmapped());
                };
                if let Some(g) = v.availability() {
                    self.guard(g);
                }
                self.out.deviates |= v.deviates();
                self.out.part = self.out.part.or(v.part());
                let (lo, hi) = v.range();
                Self::int(format!("({})", v.expr()), lo, hi)
            }
            "option" => match ops::numeric(id.ok_or_else(unmapped)?, &BTreeMap::new()) {
                Cond::Expr {
                    src,
                    display_deviation,
                } => {
                    self.out.deviates |= display_deviation;
                    Self::boolean(format!("({src})"))
                }
                Cond::Fold(b) => Self::boolean(b.to_string()),
                Cond::Undefined(_) => Self::boolean("false".into()),
                Cond::Unsupported(_) => Err(unmapped()),
            },
            "timer" => {
                let id = id.filter(|&n| n >= 0).ok_or_else(unmapped)?;
                match self.custom.resolve(id) {
                    Timer::Builtin(slot) => {
                        Ok(V::Timer(Some(timer_name(slot).ok_or_else(unmapped)?)))
                    }
                    Timer::Unset => Ok(V::Timer(None)),
                    Timer::Rank(_) | Timer::Unsupported => Err(unmapped()),
                }
            }
            _ => Err(Stop::Unsupported(Unsupported::HostApi(api))),
        }
    }

    fn binary(&mut self, op: Bin, a: V, b: V) -> R<V> {
        let sym = match op {
            Bin::Add => "+",
            Bin::Sub => "-",
            Bin::Mul => "*",
            Bin::Eq => "==",
            Bin::Ne => "!=",
            Bin::Lt => "<",
            Bin::Le => "<=",
            Bin::Gt => ">",
            Bin::Ge => ">=",
            Bin::And => "&&",
            Bin::Or => "||",
            _ => return Err(Stop::Unsupported(Unsupported::Type)),
        };
        let eq = op == Bin::Eq;
        match (a, b) {
            (
                V::Int {
                    src: x,
                    lo: al,
                    hi: ah,
                },
                V::Int {
                    src: y,
                    lo: bl,
                    hi: bh,
                },
            ) => {
                let src = format!("({x} {sym} {y})");
                let corners = |f: fn(i128, i128) -> i128| {
                    let c = [
                        f(al.into(), bl.into()),
                        f(al.into(), bh.into()),
                        f(ah.into(), bl.into()),
                        f(ah.into(), bh.into()),
                    ];
                    let lo = c.iter().copied().min().unwrap_or(0);
                    let hi = c.iter().copied().max().unwrap_or(0);
                    (
                        i64::try_from(lo).unwrap_or(i64::MIN),
                        i64::try_from(hi).unwrap_or(i64::MAX),
                    )
                };
                match op {
                    Bin::Add => {
                        let (lo, hi) = corners(|p, q| p + q);
                        Self::int(src, lo, hi)
                    }
                    Bin::Sub => {
                        let (lo, hi) = corners(|p, q| p - q);
                        Self::int(src, lo, hi)
                    }
                    Bin::Mul => {
                        let (lo, hi) = corners(|p, q| p * q);
                        Self::int(src, lo, hi)
                    }
                    Bin::Eq | Bin::Ne | Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge => Self::boolean(src),
                    _ => Err(Stop::Unsupported(Unsupported::Truthiness)),
                }
            }
            (V::Bool(x), V::Bool(y)) if matches!(op, Bin::And | Bin::Or) => {
                Self::boolean(format!("({x} {sym} {y})"))
            }
            (V::Gauge, V::Int { lo, hi, .. }) | (V::Int { lo, hi, .. }, V::Gauge)
                if lo == hi && matches!(op, Bin::Eq | Bin::Ne) =>
            {
                self.guard("gauge.valid");
                self.out.deviates = true;
                let look = usize::try_from(lo).ok().and_then(|k| LOOKS.get(k));
                Self::boolean(match look {
                    Some(name) => format!("(gauge.kind {sym} {name})"),
                    None => (!eq).to_string(),
                })
            }
            (V::Timer(t), V::TimerOff) | (V::TimerOff, V::Timer(t))
                if matches!(op, Bin::Eq | Bin::Ne) =>
            {
                Self::boolean(match (t, eq) {
                    (Some(name), true) => format!("(!on({name}))"),
                    (Some(name), false) => format!("on({name})"),
                    (None, on) => on.to_string(),
                })
            }
            (V::Config(v), V::Int { lo, hi, .. }) | (V::Int { lo, hi, .. }, V::Config(v))
                if lo == hi && matches!(op, Bin::Eq | Bin::Ne) =>
            {
                Self::boolean(((v == lo) == eq).to_string())
            }
            _ => Err(Stop::Unsupported(Unsupported::Type)),
        }
    }

    /// The critical part a read reachable from instance `i` shows, its
    /// helpers included: what a callback's role depends on. A worklist,
    /// each instance visited once, so no nesting deepens it.
    fn part(&self, i: u32) -> R<Option<CriticalPart>> {
        let mut seen = vec![i];
        let mut pending = vec![i];
        let mut stack: Vec<u32> = Vec::new();
        while let Some(i) = pending.pop() {
            let Some((ast, func, _)) = tree(self.an, i) else {
                continue;
            };
            let Some(f) = ast.func(func) else {
                continue;
            };
            stack.clear();
            for &s in ast.list(f.body) {
                self.an.charge(1).map_err(stop)?;
                match ast.stmt(s) {
                    Stmt::Local { exprs, .. } | Stmt::Return(exprs) => {
                        stack.extend_from_slice(ast.list(exprs));
                    }
                    _ => {}
                }
            }
            while let Some(e) = stack.pop() {
                self.an.charge(1).map_err(stop)?;
                match ast.expr(e) {
                    Expr::Paren(x) | Expr::Unary(_, x) => stack.push(x),
                    Expr::Binary(_, a, b) => {
                        stack.push(a);
                        stack.push(b);
                    }
                    Expr::Call { callee, args } => match resolve_callee(self.an, ast, i, callee) {
                        Ok(Callee::Helper(j)) => {
                            if !seen.contains(&j) {
                                seen.push(j);
                                pending.push(j);
                            }
                        }
                        Ok(Callee::Host(api)) => {
                            let is_number = HOST_APIS
                                .get(usize::from(api))
                                .is_some_and(|&(_, n)| n == "number");
                            let id = ast
                                .list(args)
                                .first()
                                .and_then(|&a| numeral(ast, a))
                                .and_then(|n| i32::try_from(n).ok());
                            if is_number
                                && let Some(IntProp::Shown(v)) = id.map(property::int_by_id)
                                && let Some(part) = v.part()
                            {
                                return Ok(Some(part));
                            }
                        }
                        Err(_) => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Runs `f` one level deeper into a walk: past [`MAX_AST_DEPTH`] the
    /// lowering stops at the depth cap, fatal.
    fn deeper<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        if self.depth >= MAX_AST_DEPTH {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::AstDepth)));
        }
        self.an.charge(1).map_err(stop)?;
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }
}

/// What lowering a callback gave.
enum Lowering {
    Lowered(Lowered, Ty),
    Unsupported(Unsupported, Option<CriticalPart>),
}

impl Cx<'_> {
    /// Field `f` as a property, a callable kept as its node.
    pub(super) fn callback_prop(
        &mut self,
        field: Option<u32>,
        name: &'static str,
    ) -> Result<Prop, ImportError> {
        if let Some(f) = field
            && let Some(lua) = self.def.arena.lua.as_deref()
            && let Some(n) = self.def.arena.nodes.get(f as usize)
            && n.kind == Kind::Lua
            && matches!(lua.values.get(n.end as usize), Some(LuaScalar::Callable(_)))
        {
            return Ok(Prop::Callable(f));
        }
        self.prop(field, name)
    }

    /// The callable at node `id`: its index and its instance.
    fn callable_at(&self, id: u32) -> Option<(u32, u32)> {
        let lua = self.def.arena.lua.as_deref()?;
        let n = self.def.arena.nodes.get(id as usize)?;
        let LuaScalar::Callable(c) = *lua.values.get(n.end as usize)? else {
            return None;
        };
        Some((c, lua.callables.get(c as usize)?.instance))
    }

    fn set_outcome(&self, c: u32, d: Disposition) {
        if let Some(an) = self
            .def
            .arena
            .lua
            .as_deref()
            .and_then(|l| l.analysis.as_deref())
        {
            an.set_outcome(c, d);
        }
    }

    /// Lowers the callable at node `id`, as a critical callback when
    /// `critical`.
    fn lower(&mut self, id: u32, critical: bool) -> Result<Lowering, ImportError> {
        let at = self.def.loc(id);
        let fatal = |why| ImportError::Lua {
            at: Some(at.clone()),
            why,
        };
        let Some((_, inst)) = self.callable_at(id).filter(|&(_, i)| i != NONE) else {
            return Ok(Lowering::Unsupported(
                Unsupported::Capture("an untagged closure"),
                None,
            ));
        };
        let def: &crate::resolve::Definition = self.def;
        let Some(an) = def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref()) else {
            return Ok(Lowering::Unsupported(
                Unsupported::Capture("an untagged closure"),
                None,
            ));
        };
        let cancelled = || def.cancelled(|| None).is_err();
        let mut l = Lower {
            an,
            custom: &self.custom,
            cancelled: &cancelled,
            critical,
            out: Lowered {
                src: String::new(),
                guards: Vec::new(),
                part: None,
                deviates: false,
                config: false,
            },
            sizes: Vec::new(),
            depth: 0,
        };
        let part = |l: &Lower<'_>| match l.part(inst) {
            Ok(p) => Ok(p),
            Err(Stop::Fatal(why)) => Err(fatal(why)),
            Err(Stop::Unsupported(_)) => Ok(None),
        };
        let ty = match an.classify(inst) {
            Ok(ty) => ty,
            Err(f) => {
                if let Some(why) = sentinel(f.why) {
                    return Err(fatal(why));
                }
                return Ok(Lowering::Unsupported(f.why, part(&l)?));
            }
        };
        let r = l.size(inst).and_then(|n| {
            let total = an.expanded.get().saturating_add(n);
            if total > MAX_EXPANDED_NODES {
                return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
            }
            an.expanded.set(total);
            l.instance(inst)
        });
        match r {
            Ok(V::Int { src, .. }) | Ok(V::Bool(src)) => {
                l.out.src = src;
                Ok(Lowering::Lowered(l.out, ty))
            }
            Ok(_) => Ok(Lowering::Unsupported(Unsupported::Type, part(&l)?)),
            Err(Stop::Unsupported(why)) => {
                let found = match l.out.part {
                    Some(p) => Some(p),
                    None => part(&l)?,
                };
                Ok(Lowering::Unsupported(why, found))
            }
            Err(Stop::Fatal(why)) => Err(fatal(why)),
        }
    }

    /// The refusal of critical callback `id` in `field`.
    fn critical(&self, id: u32, field: &'static str, why: Unsupported) -> ImportError {
        ImportError::Lua {
            at: Some(self.def.loc(id)),
            why: LuaWhy::Critical { field, why },
        }
    }

    /// An element's `draw` callback at node `id`, for an element of
    /// `role` at `at`: its condition joined to `conds` (its guards first),
    /// or `Ok(false)` when it is replaced by `false` and the element is
    /// dropped with the reason.
    pub(super) fn draw_callback(
        &mut self,
        role: Role,
        at: Span,
        id: u32,
        conds: &mut Vec<String>,
        deviates: &mut bool,
    ) -> Result<bool, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        let critical = role != Role::Decorative;
        let why = match self.lower(id, critical) {
            Ok(Lowering::Lowered(l, Ty::Bool)) => {
                self.set_outcome(c, Disposition::Lowered);
                #[cfg(test)]
                LOWERED.with(|w| {
                    let mut all: Vec<String> = l.guards.iter().map(|g| (*g).to_owned()).collect();
                    all.push(l.src.clone());
                    w.borrow_mut().push(all.join(" && "));
                });
                for g in l.guards {
                    if !conds.iter().any(|c| c == g) {
                        conds.push(g.to_owned());
                    }
                }
                *deviates |= l.deviates;
                if !conds.contains(&l.src) {
                    conds.push(l.src);
                }
                return Ok(true);
            }
            Ok(Lowering::Lowered(..)) => Unsupported::Type,
            Ok(Lowering::Unsupported(why, _)) => why,
            Err(e) => {
                self.set_outcome(c, Disposition::Refused);
                return Err(e);
            }
        };
        if critical {
            self.set_outcome(c, Disposition::Refused);
            return Err(self.critical(id, "draw", why));
        }
        self.set_outcome(c, Disposition::Replaced(why));
        self.drop_el(
            Role::Decorative,
            at,
            NoteKind::Callback { field: "draw", why },
        )?;
        Ok(false)
    }

    /// A number's `value` callback at node `id`, for the number at `at`:
    /// its value, or `None` when it is replaced by the omitted number,
    /// reported.
    pub(super) fn value_callback(
        &mut self,
        at: Span,
        id: u32,
    ) -> Result<Option<NumberValue>, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        let (why, part) = match self.lower(id, false) {
            Ok(Lowering::Lowered(l, _)) if l.config && l.part.is_some() => {
                (Unsupported::Config, l.part)
            }
            Ok(Lowering::Lowered(l, Ty::Int)) => {
                self.set_outcome(c, Disposition::Lowered);
                #[cfg(test)]
                LOWERED.with(|w| w.borrow_mut().push(l.src.clone()));
                let guard = (!l.guards.is_empty()).then(|| l.guards.join(" && "));
                return Ok(Some(NumberValue {
                    expr: l.src,
                    guard,
                    role: l.part.map_or(Role::Decorative, Role::Critical),
                    deviates: l.deviates,
                }));
            }
            Ok(Lowering::Lowered(l, _)) => (Unsupported::Type, l.part),
            Ok(Lowering::Unsupported(why, part)) => (why, part),
            Err(e) => {
                self.set_outcome(c, Disposition::Refused);
                return Err(e);
            }
        };
        if part.is_some() {
            self.set_outcome(c, Disposition::Refused);
            return Err(self.critical(id, "value", why));
        }
        self.set_outcome(c, Disposition::Replaced(why));
        self.drop_el(
            Role::Decorative,
            at,
            NoteKind::Callback {
                field: "value",
                why,
            },
        )?;
        Ok(None)
    }
}
