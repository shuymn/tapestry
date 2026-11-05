//! Lua callbacks where the import consumes them (design.md "beatoraja Lua
//! Skins", per-frame closures; docs/stage3.md §3, per-frame functions):
//! an element's `op` functions and `draw` after its role and its numeric
//! ops are decided, and a number's `value` where a `value` field is
//! resolved, before `ref`. A function at a field whose consumer takes
//! none (a `timer`, a text's or an image set's `value`) follows its
//! element's role ([`Cx::unconsumed`]).
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
//! - `skin_config.option["name"] == k`, in any callback: its captured
//!   value, folded under the capture proof (the raw path and the final
//!   integer entry checked at the read, noninterference by the effect
//!   walk, [`crate::lua::callback`]), so lowering takes its analysis only
//!   through the [`Admitted`] witness.
//!
//! While the candidate is open ([`Analysis::open`]), no fold of a
//! captured value (an upvalue, `skin_config.option`, `timer_off_value`)
//! and no host module or helper binding is proven: a callback that needs
//! one is unsupported for [`Unsupported::Interference`], and a critical
//! one refuses the candidate at the open site. A root whose reach writes
//! never lowers ([`Unsupported::Writes`]). Each lowering is a phase of
//! the analysis's time budget, located at its callable.
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
//! `value`'s role is its reads' ([`Lower::scan`] over its whole tree and
//! its helpers): one that reads the EX score or the max combo is
//! critical, whether or not the rest lowers, and so is one with an id or
//! callee the scan cannot resolve. A cap, cancellation or the time budget
//! refuses the candidate whatever the role.

use std::collections::{BTreeMap, HashMap, HashSet};

use skin_model::{MAX_SOURCE_BYTES, PLAY};

use super::ops::{self, Cond};
use super::policy::{CriticalPart, Role};
use super::property::{self, IntProp};
use super::report::{NoteKind, Span};
use super::timer::{Custom, Timer};
use super::{Cx, Prop};
use crate::error::ImportError;
use crate::lua::callback::{
    Admitted, Analysis, Callee, Captured, Disposition, Found, Site, Ty, fatal as sentinel, numeral,
    resolve_callee, tree,
};
use crate::lua::limits::{MAX_AST_DEPTH, MAX_EXPANDED_NODES};
use crate::lua::syntax::{Ast, Bin, DeclKind, Expr, List, Stmt, Un};
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
type Body<'a> = (&'a Ast, Vec<(u32, u32)>, u32);

struct Lower<'a> {
    an: &'a Analysis,
    custom: &'a Custom,
    cancelled: &'a dyn Fn() -> bool,
    out: Lowered,
    /// Each instance's expanded size, once computed, for the instances
    /// this lowering reaches.
    sizes: std::collections::HashMap<u32, u64>,
    /// How deep the current walk is: every expression level, an inlined
    /// local's and a helper's included.
    depth: u32,
}

impl<'a> Lower<'a> {
    /// A lowering of `an`'s callbacks with `custom`'s timers, seeing
    /// `cancelled`.
    fn new(an: &'a Analysis, custom: &'a Custom, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            an,
            custom,
            cancelled,
            out: Lowered {
                src: String::new(),
                guards: Vec::new(),
                part: None,
                deviates: false,
            },
            sizes: std::collections::HashMap::new(),
            depth: 0,
        }
    }
}

impl Lower<'_> {
    /// A generated string kept: past [`MAX_SOURCE_BYTES`] fatal, its
    /// bytes charged as analysis work (each level copies its operands').
    fn bytes(&self, src: String) -> R<String> {
        if src.len() > MAX_SOURCE_BYTES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Generated)));
        }
        self.an
            .charge(u64::try_from(src.len()).unwrap_or(u64::MAX))
            .map_err(stop)?;
        Ok(src)
    }

    fn int(&self, src: String, lo: i64, hi: i64) -> R<V> {
        if lo < i64::from(i32::MIN) || hi > i64::from(i32::MAX) {
            return Err(Stop::Unsupported(Unsupported::Range));
        }
        Ok(V::Int {
            src: self.bytes(src)?,
            lo,
            hi,
        })
    }

    fn boolean(&self, src: String) -> R<V> {
        Ok(V::Bool(self.bytes(src)?))
    }

    fn guard(&mut self, g: &'static str) {
        if !self.out.guards.contains(&g) {
            self.out.guards.push(g);
        }
    }

    /// The body of instance `i`: its tree, its locals (each declaration
    /// and its initializer) and its return expression.
    fn body(an: &Analysis, i: u32) -> R<Body<'_>> {
        let unsupported = Stop::Unsupported(Unsupported::Capture("an untagged closure"));
        let (ast, func) = tree(an, i).ok_or(unsupported)?;
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
                        return Ok((ast, locals, e));
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
        if let Some(&n) = self.sizes.get(&i) {
            return Ok(n);
        }
        if (self.cancelled)() {
            return Err(Stop::Fatal(LuaWhy::Cancelled));
        }
        let (ast, locals, ret) = Self::body(self.an, i)?;
        let mut sized: Vec<(u32, u64)> = Vec::with_capacity(locals.len());
        for &(d, e) in &locals {
            let n = self.size_expr(ast, i, &sized, e)?;
            sized.push((d, n));
        }
        let n = self.size_expr(ast, i, &sized, ret)?;
        // Sparse: an entry for each instance this lowering sized.
        self.sizes.insert(i, n);
        Ok(n)
    }

    fn size_expr(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u64)], e: u32) -> R<u64> {
        self.deeper(|l| l.size_expr_inner(ast, inst, locals, e))
    }

    fn size_expr_inner(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u64)], e: u32) -> R<u64> {
        Ok(match ast.expr(e) {
            Expr::Local(d) => {
                self.charge_scan(locals.len())?;
                locals
                    .iter()
                    .rev()
                    .find(|(l, _)| *l == d)
                    .map_or(1, |&(_, n)| n)
            }
            Expr::Paren(x) | Expr::Unary(_, x) => {
                self.size_expr(ast, inst, locals, x)?.saturating_add(1)
            }
            Expr::Binary(_, a, b) => self
                .size_expr(ast, inst, locals, a)?
                .saturating_add(self.size_expr(ast, inst, locals, b)?)
                .saturating_add(1),
            Expr::Call { callee, .. } => {
                self.bound()?;
                match resolve_callee(self.an, ast, inst, callee).map_err(Stop::Unsupported)? {
                    Callee::Helper(j) => self.size(j)?.saturating_add(1),
                    Callee::Host(_) => 1,
                }
            }
            _ => 1,
        })
    }

    /// Instance `i` sized, its expanded size charged against
    /// [`MAX_EXPANDED_NODES`], then lowered.
    fn expand(&mut self, i: u32) -> R<V> {
        let n = self.size(i)?;
        let total = self.an.expanded.get().saturating_add(n);
        if total > MAX_EXPANDED_NODES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
        }
        self.an.expanded.set(total);
        self.instance(i)
    }

    /// Instance `i` lowered.
    fn instance(&mut self, i: u32) -> R<V> {
        #[cfg(test)]
        crate::lua::tests::expansion_witness();
        if (self.cancelled)() {
            return Err(Stop::Fatal(LuaWhy::Cancelled));
        }
        let (ast, locals, ret) = Self::body(self.an, i)?;
        self.expr(ast, i, &locals, ret)
    }

    fn expr(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u32)], e: u32) -> R<V> {
        self.deeper(|l| l.expr_inner(ast, inst, locals, e))
    }

    fn expr_inner(&mut self, ast: &Ast, inst: u32, locals: &[(u32, u32)], e: u32) -> R<V> {
        let unsupported = |w| Err(Stop::Unsupported(w));
        match ast.expr(e) {
            Expr::Int(n) => self.literal(n),
            Expr::True => self.boolean("true".into()),
            Expr::False => self.boolean("false".into()),
            Expr::Paren(x) => self.expr(ast, inst, locals, x),
            Expr::Local(d) => {
                self.charge_scan(locals.len())?;
                if let Some((k, &(_, init))) =
                    locals.iter().enumerate().rev().find(|(_, (l, _))| *l == d)
                {
                    // The locals before it, as its initializer saw them.
                    let before = locals.get(..k).unwrap_or_default();
                    return self.expr(ast, inst, before, init);
                }
                self.bound()?;
                match self.an.upvalue(inst, d) {
                    Some(Captured::Int(n)) => self.literal(n),
                    Some(Captured::Bool(b)) => self.boolean(b.to_string()),
                    _ => unsupported(Unsupported::Capture("an upvalue")),
                }
            }
            Expr::Index { obj, key } => {
                self.bound()?;
                if let Expr::Str(s) = ast.expr(key)
                    && ast.str(s) == b"timer_off_value"
                {
                    return Ok(V::TimerOff);
                }
                self.config(ast, inst, obj, key)
            }
            Expr::Call { callee, args } => {
                self.bound()?;
                match resolve_callee(self.an, ast, inst, callee).map_err(Stop::Unsupported)? {
                    Callee::Helper(j) => self.expand(j),
                    Callee::Host(api) => {
                        let arg = ast.list(args).first().and_then(|&a| numeral(ast, a));
                        self.read(api, arg)
                    }
                }
            }
            Expr::Unary(op, x) => match (op, self.expr(ast, inst, locals, x)?) {
                (Un::Neg, V::Int { src, lo, hi }) => self.int(
                    format!("(-{src})"),
                    hi.checked_neg().unwrap_or(i64::MAX),
                    lo.checked_neg().unwrap_or(i64::MAX),
                ),
                (Un::Not, V::Bool(s)) => self.boolean(format!("(!{s})")),
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

    fn literal(&self, n: i64) -> R<V> {
        let src = if n < 0 {
            format!("(-{})", n.unsigned_abs())
        } else {
            n.to_string()
        };
        self.int(src, n, n)
    }

    /// `skin_config.option["name"]`: its captured value, which admission
    /// proved no callback writes.
    fn config(&mut self, ast: &Ast, inst: u32, obj: u32, key: u32) -> R<V> {
        let (Expr::Index { obj: cfg, .. }, Expr::Str(k)) = (ast.expr(obj), ast.expr(key)) else {
            return Err(Stop::Unsupported(Unsupported::Type));
        };
        let Expr::Global { env, .. } = ast.expr(cfg) else {
            return Err(Stop::Unsupported(Unsupported::Type));
        };
        self.an
            .option_entry(inst, env, ast.str(k))
            .map(V::Config)
            .map_err(stop)
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
                self.int(format!("({})", v.expr()), lo, hi)
            }
            "option" => match ops::numeric(id.ok_or_else(unmapped)?, &BTreeMap::new()) {
                Cond::Expr {
                    src,
                    display_deviation,
                } => {
                    self.out.deviates |= display_deviation;
                    self.boolean(format!("({src})"))
                }
                Cond::Fold(b) => self.boolean(b.to_string()),
                Cond::Undefined(_) => self.boolean("false".into()),
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
                        self.int(src, lo, hi)
                    }
                    Bin::Sub => {
                        let (lo, hi) = corners(|p, q| p - q);
                        self.int(src, lo, hi)
                    }
                    Bin::Mul => {
                        let (lo, hi) = corners(|p, q| p * q);
                        self.int(src, lo, hi)
                    }
                    Bin::Eq | Bin::Ne | Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge => self.boolean(src),
                    _ => Err(Stop::Unsupported(Unsupported::Truthiness)),
                }
            }
            (V::Bool(x), V::Bool(y)) if matches!(op, Bin::And | Bin::Or) => {
                self.boolean(format!("({x} {sym} {y})"))
            }
            (V::Gauge, V::Int { lo, hi, .. }) | (V::Int { lo, hi, .. }, V::Gauge)
                if lo == hi && matches!(op, Bin::Eq | Bin::Ne) =>
            {
                self.guard("gauge.valid");
                self.out.deviates = true;
                let look = usize::try_from(lo).ok().and_then(|k| LOOKS.get(k));
                self.boolean(match look {
                    Some(name) => format!("(gauge.kind {sym} {name})"),
                    None => (!eq).to_string(),
                })
            }
            (V::Timer(t), V::TimerOff) | (V::TimerOff, V::Timer(t))
                if matches!(op, Bin::Eq | Bin::Ne) =>
            {
                self.boolean(match (t, eq) {
                    (Some(name), true) => format!("(!on({name}))"),
                    (Some(name), false) => format!("on({name})"),
                    (None, on) => on.to_string(),
                })
            }
            (V::Config(v), V::Int { lo, hi, .. }) | (V::Int { lo, hi, .. }, V::Config(v))
                if lo == hi && matches!(op, Bin::Eq | Bin::Ne) =>
            {
                self.boolean(((v == lo) == eq).to_string())
            }
            _ => Err(Stop::Unsupported(Unsupported::Type)),
        }
    }

    /// The role scan of instance `i` (design.md "beatoraja Lua Skins",
    /// roles): every expression of its whole tree and of the helpers it
    /// calls, each id a `number` read takes resolved to the critical part
    /// it shows. An id resolves from a numeral, a local of the same
    /// function never assigned again and initialized by a numeral, a
    /// captured integer, or a helper's parameter, the join of the ids its
    /// call sites pass; a callee, to a host API, a helper or a library
    /// function by its captured binding. While the candidate is open no
    /// captured binding or value holds, so none resolves. A worklist: a
    /// helper is scanned again only when a parameter's join grows, which
    /// it does at most twice a parameter.
    fn scan(&self, i: u32) -> R<Scan> {
        let open = self.an.open.get().is_some();
        let mut out = Scan::default();
        // Each scanned instance's parameters' joins: one hash a check.
        let mut params: HashMap<u32, Vec<Scan>> = HashMap::from([(i, Vec::new())]);
        let mut pending = vec![i];
        let mut blocks: Vec<List> = Vec::new();
        let mut stack: Vec<u32> = Vec::new();
        let mut calls: Vec<u32> = Vec::new();
        let mut reassigned: HashSet<u32> = HashSet::new();
        while let Some(i) = pending.pop() {
            let Some((ast, func)) = tree(self.an, i) else {
                out.unresolved = out.unresolved.or(Some("callee"));
                continue;
            };
            let Some(f) = ast.func(func) else {
                continue;
            };
            // Its parameters' joins copied, one unit each.
            self.an.charge(u64::from(f.params.1)).map_err(stop)?;
            let own = params.get(&i).cloned().unwrap_or_default();
            // The whole tree, function constructors' bodies included: the
            // locals assigned again and the calls collected first, each
            // statement and expression one unit.
            blocks.clear();
            stack.clear();
            calls.clear();
            reassigned.clear();
            blocks.push(f.body);
            loop {
                if let Some(l) = blocks.pop() {
                    for &s in ast.list(l) {
                        // The statement and each item its lists hold.
                        self.an
                            .charge(stmt_width(ast, s).saturating_add(1))
                            .map_err(stop)?;
                        stmt_parts(ast, s, &mut blocks, &mut stack, &mut reassigned);
                    }
                    continue;
                }
                let Some(e) = stack.pop() else {
                    break;
                };
                self.an.charge(1).map_err(stop)?;
                match ast.expr(e) {
                    Expr::Paren(x) | Expr::Unary(_, x) => stack.push(x),
                    Expr::Binary(_, a, b) | Expr::Index { obj: a, key: b } => {
                        stack.push(a);
                        stack.push(b);
                    }
                    Expr::Table(items) => {
                        self.an.charge(u64::from(items.1)).map_err(stop)?;
                        stack.extend(ast.list(items).iter().copied().filter(|&x| x != NONE))
                    }
                    Expr::Func(g) => {
                        if let Some(g) = ast.func(g) {
                            blocks.push(g.body);
                        }
                    }
                    Expr::Method { obj, args, .. } => {
                        out.unresolved = out.unresolved.or(Some("callee"));
                        stack.push(obj);
                        self.an.charge(u64::from(args.1)).map_err(stop)?;
                        stack.extend_from_slice(ast.list(args));
                    }
                    Expr::Call { callee, args } => {
                        stack.push(callee);
                        self.an.charge(u64::from(args.1)).map_err(stop)?;
                        stack.extend_from_slice(ast.list(args));
                        calls.push(e);
                    }
                    _ => {}
                }
            }
            let ids = Ids {
                an: self.an,
                ast,
                inst: i,
                func,
                own: &own,
                reassigned: &reassigned,
                open,
            };
            for &e in &calls {
                let Expr::Call { callee, args } = ast.expr(e) else {
                    continue;
                };
                let args = ast.list(args);
                // The call resolved and each argument read as an id, one
                // unit each.
                self.an
                    .charge(
                        u64::try_from(args.len())
                            .unwrap_or(u64::MAX)
                            .saturating_add(1),
                    )
                    .map_err(stop)?;
                let resolved = if open {
                    None
                } else {
                    match resolve_callee(self.an, ast, i, callee) {
                        Ok(c) => Some(Some(c)),
                        Err(_) => lib_callee(self.an, ast, i, callee)
                            .map_err(stop)?
                            .then_some(None),
                    }
                };
                match resolved {
                    Some(Some(Callee::Host(api))) => {
                        let name = HOST_APIS.get(usize::from(api)).map_or("", |&(_, n)| n);
                        let read = match name {
                            "number" => args.get(..1).unwrap_or_default(),
                            "numbers" => args,
                            _ => &[],
                        };
                        if name == "number" && args.is_empty() {
                            out.unresolved = out.unresolved.or(Some("id"));
                        }
                        for &a in read {
                            out = out.join(ids.id(a)?);
                        }
                    }
                    Some(Some(Callee::Helper(j))) => {
                        let n = tree(self.an, j)
                            .and_then(|(t, g)| t.func(g))
                            .map_or(0, |g| g.params.1 as usize);
                        // Each parameter's join, one unit.
                        self.an
                            .charge(u64::try_from(n).unwrap_or(u64::MAX))
                            .map_err(stop)?;
                        let mut joined = Vec::with_capacity(n);
                        for k in 0..n {
                            // A missing argument is nil: no id.
                            joined.push(match args.get(k) {
                                Some(&a) => ids.id(a)?,
                                None => Scan::unresolved("id"),
                            });
                        }
                        let grew = match params.get_mut(&j) {
                            None => {
                                params.insert(j, joined);
                                true
                            }
                            Some(slots) => {
                                let mut grew = false;
                                for (slot, s) in slots.iter_mut().zip(joined) {
                                    let next = slot.join(s);
                                    grew |= next != *slot;
                                    *slot = next;
                                }
                                grew
                            }
                        };
                        if grew {
                            pending.push(j);
                        }
                    }
                    Some(None) => {}
                    None => out.unresolved = out.unresolved.or(Some("callee")),
                }
            }
        }
        Ok(out)
    }

    /// What a proof resting on captured state needs: a fold of a captured
    /// value (an upvalue, `skin_config.option`, `timer_off_value`) or a
    /// host module or helper binding, each proven by its captured final
    /// binding. While the analysis is open, code it does not see may
    /// change that state, so none holds.
    fn bound(&self) -> R<()> {
        if self.an.open.get().is_some() {
            return Err(Stop::Unsupported(Unsupported::Interference));
        }
        Ok(())
    }

    /// A scan of `n` locals (at most Lua's 200) charged before it is made.
    fn charge_scan(&self, n: usize) -> R<()> {
        self.an
            .charge(u64::try_from(n).unwrap_or(u64::MAX))
            .map_err(stop)
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

/// What the role scan of a callback found: the critical part a read
/// shows, and the first thing it could not resolve (an `id` or a
/// `callee`), which makes the role unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) part: Option<CriticalPart>,
    pub(crate) unresolved: Option<&'static str>,
}

impl Scan {
    fn unresolved(what: &'static str) -> Self {
        Self {
            part: None,
            unresolved: Some(what),
        }
    }

    fn join(self, o: Self) -> Self {
        Self {
            part: self.part.or(o.part),
            unresolved: self.unresolved.or(o.unresolved),
        }
    }
}

/// The id resolution of one scanned function.
struct Ids<'a> {
    an: &'a Analysis,
    ast: &'a Ast,
    inst: u32,
    func: u32,
    /// Its parameters' joins over its call sites.
    own: &'a [Scan],
    /// Its locals assigned after their declaration.
    reassigned: &'a HashSet<u32>,
    open: bool,
}

impl Ids<'_> {
    /// What expression `e`, read as a `number` id, shows.
    fn id(&self, e: u32) -> R<Scan> {
        if let Some(n) = numeral(self.ast, e) {
            return Ok(number_part(n));
        }
        let mut e = e;
        // Parentheses, each one unit.
        while let Expr::Paren(x) = self.ast.expr(e) {
            self.an.charge(1).map_err(stop)?;
            e = x;
        }
        let Expr::Local(d) = self.ast.expr(e) else {
            return Ok(Scan::unresolved("id"));
        };
        let decl = self.ast.decl(d).filter(|x| x.func == self.func);
        match decl {
            // A parameter assigned again holds what the body assigns, not
            // what its callers pass.
            Some(x) if x.kind == DeclKind::Param && !self.reassigned.contains(&d) => {
                let params = self.ast.func(self.func).map_or((0, 0), |f| f.params);
                // Its position, each parameter one unit.
                self.an.charge(u64::from(params.1)).map_err(stop)?;
                let k = self.ast.list(params).iter().position(|&p| p == d);
                Ok(k.and_then(|k| self.own.get(k))
                    .copied()
                    .unwrap_or(Scan::unresolved("id")))
            }
            Some(x)
                if x.kind == DeclKind::Local && x.init != NONE && !self.reassigned.contains(&d) =>
            {
                Ok(numeral(self.ast, x.init).map_or(Scan::unresolved("id"), number_part))
            }
            Some(_) => Ok(Scan::unresolved("id")),
            None if self.open => Ok(Scan::unresolved("id")),
            None => Ok(match self.an.upvalue(self.inst, d) {
                Some(Captured::Int(n)) => number_part(n),
                _ => Scan::unresolved("id"),
            }),
        }
    }
}

/// The critical part `number(n)` shows.
fn number_part(n: i64) -> Scan {
    let part = i32::try_from(n)
        .ok()
        .and_then(|id| match property::int_by_id(id) {
            IntProp::Shown(v) => v.part(),
            _ => None,
        });
    Scan {
        part,
        unresolved: None,
    }
}

/// How many items statement `s`'s lists hold, an upper bound of what
/// [`stmt_parts`] visits beside the statement.
fn stmt_width(ast: &Ast, s: u32) -> u64 {
    let n = |l: List| u64::from(l.1);
    match ast.stmt(s) {
        Stmt::Local { names, exprs } | Stmt::GenericFor { names, exprs, .. } => n(names) + n(exprs),
        Stmt::Assign { targets, exprs } => n(targets) + n(exprs),
        Stmt::Return(l) | Stmt::If(l) => n(l),
        _ => 4,
    }
}

/// Statement `s`'s blocks, expressions and locals it assigns again.
fn stmt_parts(
    ast: &Ast,
    s: u32,
    blocks: &mut Vec<List>,
    exprs: &mut Vec<u32>,
    reassigned: &mut HashSet<u32>,
) {
    let mut expr = |e: u32| {
        if e != NONE {
            exprs.push(e);
        }
    };
    match ast.stmt(s) {
        Stmt::Local { exprs: l, .. } | Stmt::Return(l) => {
            ast.list(l).iter().for_each(|&e| expr(e));
        }
        Stmt::Assign { targets, exprs: l } => {
            for &t in ast.list(targets) {
                match ast.expr(t) {
                    Expr::Local(d) => {
                        reassigned.insert(d);
                    }
                    _ => expr(t),
                }
            }
            ast.list(l).iter().for_each(|&e| expr(e));
        }
        Stmt::Call(e) => expr(e),
        Stmt::If(arms) => {
            for arm in ast.list(arms).chunks(3) {
                if let &[c, s, n] = arm {
                    expr(c);
                    blocks.push((s, n));
                }
            }
        }
        Stmt::While { cond, body } | Stmt::Repeat { body, cond } => {
            expr(cond);
            blocks.push(body);
        }
        Stmt::NumericFor {
            start,
            limit,
            step,
            body,
            ..
        } => {
            expr(start);
            expr(limit);
            expr(step);
            blocks.push(body);
        }
        Stmt::GenericFor { exprs: l, body, .. } => {
            ast.list(l).iter().for_each(|&e| expr(e));
            blocks.push(body);
        }
        Stmt::Do(body) => blocks.push(body),
        Stmt::LocalFunction { func, .. } => {
            if let Some(f) = ast.func(func) {
                blocks.push(f.body);
            }
        }
        Stmt::Function { target, func } => {
            match ast.expr(target) {
                Expr::Local(d) => {
                    reassigned.insert(d);
                }
                _ => expr(target),
            }
            if let Some(f) = ast.func(func) {
                blocks.push(f.body);
            }
        }
        Stmt::Empty | Stmt::Break | Stmt::Goto(_) | Stmt::Label(_) => {}
    }
}

/// Whether `callee` in instance `inst`'s body is a library function by
/// its captured binding: an upvalue, a global, or an entry of one along
/// its static access path; a protected call is not one.
fn lib_callee(an: &Analysis, ast: &Ast, inst: u32, callee: u32) -> Result<bool, Unsupported> {
    let c = match ast.expr(callee) {
        Expr::Index { obj, key } => match (an.binding(ast, inst, obj)?, ast.expr(key)) {
            (Some(Captured::Table { id, meta: false }), Expr::Str(k)) => {
                an.entry(id, ast.str(k))?
            }
            _ => None,
        },
        _ => an.binding(ast, inst, callee)?,
    };
    // A protected call calls its target, which the scan does not follow.
    Ok(match c {
        Some(Captured::Lib(l)) => !matches!(
            crate::lua::LIBS.get(usize::from(l)).copied(),
            Some("pcall" | "xpcall")
        ),
        _ => false,
    })
}

/// What lowering a callback gave.
enum Lowering {
    Lowered(Lowered, Ty),
    Unsupported(Unsupported, Scan),
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
            #[cfg(feature = "dev-tools")]
            self.unused(f, crate::lua::callback::Unused::Dropped);
            return Ok(Prop::Callable(f));
        }
        self.prop(field, name)
    }

    /// The callable at node `id`: its index and its instance.
    pub(super) fn callable_at(&self, id: u32) -> Option<(u32, u32)> {
        let lua = self.def.arena.lua.as_deref()?;
        let n = self.def.arena.nodes.get(id as usize)?;
        let LuaScalar::Callable(c) = *lua.values.get(n.end as usize)? else {
            return None;
        };
        Some((c, lua.callables.get(c as usize)?.instance))
    }

    /// Records what became of callable `c` at node `id`, consumed in
    /// `field` with `role`: its disposition `d`, then, for the report, a
    /// record with `why` it did not lower.
    fn outcome(
        &self,
        (c, id): (u32, u32),
        field: &'static str,
        role: Role,
        (d, why): (Disposition, Option<Unsupported>),
    ) {
        let Some(an) = self.analysis() else {
            return;
        };
        an.set_outcome(c, d);
        #[cfg(feature = "dev-tools")]
        an.consumed
            .borrow_mut()
            .push(crate::lua::callback::Consumed {
                node: id,
                field,
                part: match role {
                    Role::Critical(p) => Some(p),
                    Role::Decorative => None,
                },
                disposition: d,
                why,
            });
        #[cfg(not(feature = "dev-tools"))]
        let _ = (id, field, role, why);
    }

    /// Records why the callback at node `id`, read, was not consumed:
    /// its element dropped before it, or a condition before it folded
    /// false. A consumed callback's record supersedes it.
    #[cfg(feature = "dev-tools")]
    pub(super) fn unused(&self, id: u32, why: crate::lua::callback::Unused) {
        if let (Some(an), Some((c, _))) = (self.analysis(), self.callable_at(id)) {
            an.unused(c, why);
        }
    }

    /// The definition's callback analysis.
    fn analysis(&self) -> Option<&Analysis> {
        self.def
            .arena
            .lua
            .as_deref()
            .and_then(|l| l.analysis.as_deref())
    }

    /// Lowers the callable at node `id`, a phase of the analysis's time
    /// budget located at it.
    fn lower(&mut self, id: u32) -> Result<Lowering, ImportError> {
        let at = self.def.loc(id);
        let fatal = |why| ImportError::Lua {
            at: Some(at.clone()),
            why,
        };
        let Some(an) = self.analysis() else {
            return self.lower_in(id);
        };
        let phase = crate::lua::callback::Phase::Lower;
        an.clock
            .enter(phase, Some(id))
            .map_err(|w| fatal(sentinel(w).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisTime))))?;
        let r = self.lower_in(id);
        let left = self
            .analysis()
            .map_or(Ok(()), |an| an.clock.leave())
            .map_err(|w| fatal(sentinel(w).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisTime))));
        let r = r?;
        left.map(|()| r)
    }

    /// [`Self::lower`]'s work.
    fn lower_in(&mut self, id: u32) -> Result<Lowering, ImportError> {
        let at = self.def.loc(id);
        let fatal = |why| ImportError::Lua {
            at: Some(at.clone()),
            why,
        };
        let untagged =
            Lowering::Unsupported(Unsupported::Capture("an untagged closure"), Scan::default());
        let Some((_, inst)) = self.callable_at(id).filter(|&(_, i)| i != NONE) else {
            return Ok(untagged);
        };
        let def: &crate::resolve::Definition = self.def;
        let Some(an) = def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref()) else {
            return Ok(untagged);
        };
        // Every import runs after admission, which runs the effect walk:
        // an analysis it did not admit lowers nothing.
        let Some(an) = an.admitted().map(Admitted::analysis) else {
            return Ok(Lowering::Unsupported(
                Unsupported::Capture("an unadmitted callback"),
                Scan::default(),
            ));
        };
        let cancelled = || def.cancelled(|| None).is_err();
        let mut l = Lower::new(an, &self.custom, &cancelled);
        let scan = |l: &Lower<'_>| match l.scan(inst) {
            Ok(s) => Ok(s),
            Err(Stop::Fatal(why)) => Err(fatal(why)),
            Err(Stop::Unsupported(w)) => Ok(Scan::unresolved(match w {
                Unsupported::Construct(c) => c,
                _ => "callee",
            })),
        };
        // A root whose reach writes never lowers.
        if an.writes(inst) {
            return Ok(Lowering::Unsupported(Unsupported::Writes, scan(&l)?));
        }
        let ty = match an.classify(inst) {
            Ok(ty) => ty,
            Err(f) => {
                if let Some(why) = sentinel(f.why) {
                    return Err(fatal(why));
                }
                return Ok(Lowering::Unsupported(f.why, scan(&l)?));
            }
        };
        let r = l.expand(inst);
        #[cfg(test)]
        crate::lua::tests::memo_witness(l.sizes.len());
        match r {
            Ok(V::Int { src, .. }) | Ok(V::Bool(src)) => {
                l.out.src = src;
                Ok(Lowering::Lowered(l.out, ty))
            }
            Ok(_) => Ok(Lowering::Unsupported(Unsupported::Type, scan(&l)?)),
            Err(Stop::Unsupported(why)) => {
                let mut s = scan(&l)?;
                s.part = l.out.part.or(s.part);
                Ok(Lowering::Unsupported(why, s))
            }
            Err(Stop::Fatal(why)) => Err(fatal(why)),
        }
    }

    /// The refusal of critical callback `id` in `field`: located at its
    /// definition, or, for [`Unsupported::Interference`], at the open
    /// site that voids its proof.
    fn critical(&self, id: u32, field: &'static str, why: Unsupported) -> ImportError {
        let open = match why {
            Unsupported::Interference => self.analysis().and_then(|an| an.open.get()),
            _ => None,
        };
        let at = match open.map(|o| o.site) {
            Some(Site::Node(n)) => self.def.loc(n),
            Some(Site::Code { file, line }) => self
                .analysis()
                .map_or_else(|| self.def.loc(id), |an| an.loc(&Found { why, file, line })),
            None => self.def.loc(id),
        };
        ImportError::Lua {
            at: Some(at),
            why: LuaWhy::Critical { field, why },
        }
    }

    /// The callable at node `id`, read in `field` whose consumer takes
    /// none ([`Unsupported::Consumer`] `consumer`), for an element of
    /// `role`: a critical one refuses the candidate, a decorative one is
    /// replaced, the note to drop its element with returned.
    pub(super) fn unconsumed(
        &self,
        role: Role,
        id: u32,
        field: &'static str,
        consumer: &'static str,
    ) -> Result<NoteKind, ImportError> {
        let (c, inst) = self.callable_at(id).unwrap_or((NONE, NONE));
        // A writer's reason is its write, whatever consumes it.
        let writes = inst != NONE && self.analysis().is_some_and(|an| an.writes(inst));
        let why = if writes {
            Unsupported::Writes
        } else {
            Unsupported::Consumer(consumer)
        };
        if role != Role::Decorative {
            self.outcome((c, id), field, role, (Disposition::Refused, Some(why)));
            return Err(self.critical(id, field, why));
        }
        self.outcome(
            (c, id),
            field,
            role,
            (Disposition::Replaced(why), Some(why)),
        );
        Ok(NoteKind::Callback { field, why })
    }

    /// The role of the callable at node `id`, read as an element's value
    /// for an element of `role`: critical when the role scan finds a
    /// critical part (a cap or cancellation fatal); a decorative one whose
    /// scan resolves not every id or callee refuses the candidate
    /// ([`Unsupported::Unresolved`]).
    pub(super) fn callback_role(
        &self,
        id: u32,
        field: &'static str,
        role: Role,
    ) -> Result<Role, ImportError> {
        let (Some(an), Some((c, inst))) = (self.analysis(), self.callable_at(id)) else {
            return Ok(role);
        };
        if inst == NONE {
            return Ok(role);
        }
        let fatal = |why| ImportError::Lua {
            at: Some(self.def.loc(id)),
            why,
        };
        let time = |w| fatal(sentinel(w).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisTime)));
        // A phase of the analysis's time budget, located at the value.
        an.clock
            .enter(crate::lua::callback::Phase::Scan, Some(id))
            .map_err(time)?;
        let cancelled = || self.def.cancelled(|| None).is_err();
        let r = Lower::new(an, &self.custom, &cancelled).scan(inst);
        let left = an.clock.leave().map_err(time);
        let scan = match r {
            Ok(s) => s,
            Err(Stop::Fatal(why)) => return Err(fatal(why)),
            Err(Stop::Unsupported(_)) => Scan::unresolved("callee"),
        };
        left?;
        let role = scan.part.map_or(role, Role::Critical);
        if role == Role::Decorative
            && let Some(what) = scan.unresolved
        {
            let why = Unsupported::Unresolved(what);
            self.outcome((c, id), field, role, (Disposition::Refused, Some(why)));
            return Err(self.critical(id, field, why));
        }
        Ok(role)
    }

    /// An element's `draw` callback, or one of its `op` items, at node
    /// `id` in `field`, for an element of `role` at `at`: its condition
    /// joined to `conds` (its guards first), or `Ok(false)` when it is
    /// replaced by `false` and the element is dropped with the reason.
    pub(super) fn draw_callback(
        &mut self,
        role: Role,
        at: Span,
        (id, field): (u32, &'static str),
        conds: &mut Vec<String>,
        deviates: &mut bool,
    ) -> Result<bool, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        let critical = role != Role::Decorative;
        let why = match self.lower(id) {
            Ok(Lowering::Lowered(l, Ty::Bool)) => {
                self.outcome((c, id), field, role, (Disposition::Lowered, None));
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
                self.outcome((c, id), field, role, (Disposition::Refused, None));
                return Err(e);
            }
        };
        if critical {
            self.outcome((c, id), field, role, (Disposition::Refused, Some(why)));
            return Err(self.critical(id, field, why));
        }
        self.outcome(
            (c, id),
            field,
            role,
            (Disposition::Replaced(why), Some(why)),
        );
        self.drop_el(Role::Decorative, at, NoteKind::Callback { field, why })?;
        Ok(false)
    }

    /// A number's `value` callback at node `id`, for the number at `at`
    /// in `base` unless what it reads has a part of its own: its value,
    /// or `None` when it is replaced by the omitted number, reported; a
    /// critical one refuses instead. One that does not lower and whose
    /// role scan resolves not every id or callee is critical
    /// ([`Unsupported::Unresolved`], unless the candidate is open or the
    /// callback writes, which give their own reason).
    pub(super) fn value_callback(
        &mut self,
        at: Span,
        id: u32,
        base: Role,
    ) -> Result<Option<NumberValue>, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        let (why, scan) = match self.lower(id) {
            Ok(Lowering::Lowered(l, Ty::Int)) => {
                self.outcome(
                    (c, id),
                    "value",
                    l.part.map_or(base, Role::Critical),
                    (Disposition::Lowered, None),
                );
                #[cfg(test)]
                LOWERED.with(|w| w.borrow_mut().push(l.src.clone()));
                let guard = (!l.guards.is_empty()).then(|| l.guards.join(" && "));
                return Ok(Some(NumberValue {
                    expr: l.src,
                    guard,
                    role: l.part.map_or(base, Role::Critical),
                    deviates: l.deviates,
                }));
            }
            Ok(Lowering::Lowered(l, _)) => (
                Unsupported::Type,
                Scan {
                    part: l.part,
                    unresolved: None,
                },
            ),
            Ok(Lowering::Unsupported(why, scan)) => (why, scan),
            Err(e) => {
                self.outcome((c, id), "value", base, (Disposition::Refused, None));
                return Err(e);
            }
        };
        let role = scan.part.map_or(base, Role::Critical);
        if role != Role::Decorative || scan.unresolved.is_some() {
            let why = match (role, scan.unresolved) {
                (Role::Decorative, Some(what))
                    if !matches!(why, Unsupported::Interference | Unsupported::Writes) =>
                {
                    Unsupported::Unresolved(what)
                }
                _ => why,
            };
            self.outcome((c, id), "value", role, (Disposition::Refused, Some(why)));
            return Err(self.critical(id, "value", why));
        }
        self.outcome(
            (c, id),
            "value",
            Role::Decorative,
            (Disposition::Replaced(why), Some(why)),
        );
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
