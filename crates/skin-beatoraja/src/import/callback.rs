//! Lua callbacks where the import consumes them (design.md "beatoraja Lua
//! Skins", per-frame closures; docs/stage3.md §3, per-frame functions):
//! an element's `op` functions and `draw` after its role and its numeric
//! ops are decided, and a number's `value` where a `value` field is
//! or an image set's `value` where a `value` field is resolved, before
//! `ref`. A function at a field whose consumer takes none (a `timer`)
//! follows its element's role ([`Cx::unconsumed`]). A text's `value` folds
//! into a prepared value ([`Cx::text_callback`]), or follows the element's
//! role.
//!
//! A write-free root is partially evaluated into its residual
//! ([`crate::lua::residual`], taking the analysis only through the
//! [`crate::lua::callback::Admitted`] witness), converted as its consumer
//! converts it, and emitted here as an expression's source, compiled
//! through the importer's compile path and its budget like any condition
//! or value. The conversion comes first, so a result of a kind its
//! consumer does not take is refused as that (a `text(1)` number is a
//! type the number does not take) before its reads are looked at; the
//! reads the residual recorded are kept apart from the result, so a
//! conversion or a fold never drops one. Every recorded read is then
//! attached, before anything is emitted, whether or not the result still
//! uses it: its JSON
//! mapping, that mapping's availability guard (joined into the element's
//! visibility, so a value that can be unknown draws nothing while it is),
//! its D4 deviation and its critical part (`number(101)` the EX score, as
//! 101 in JSON); one with no mapping refuses the callback:
//!
//! - `number(n)`: the value property `n` ([`property::int_by_id`]), its
//!   range that of its fields;
//! - `judge(n)`: the count of judge index `n` ([`property::judge_count`]),
//!   0 for any other known integer, as the source's is;
//! - `option(n)`: the built-in boolean property `n` ([`ops::numeric`] with
//!   no option items, as the source's function reads only the factory),
//!   one it does not know `false`, as the source's is;
//! - `timer(n)` against the off value, `==` and `~=`: `!on(t)` and
//!   `on(t)` of the JSON mapping's timer `t`, a timer never set off; any
//!   other use of a timer is a type it does not take;
//! - `gauge_type()` against a known `k`, `==` and `~=`: `gauge.kind`
//!   against the look `k` selects (0 to 5, `assist_easy` to `hazard`; any
//!   other `k` matches no look), under `gauge.valid`, as JSON's gauge
//!   conditions; and an image set's whole value, by
//!   [`property::IndexProp::GaugeKind`]'s selector.
//!
//! The rest maps as it reads: integers, booleans, `-`, `!`, `+ - *` and
//! comparisons, every integer node bounded from the ranges of what it
//! reads, one that could leave `i32` unsupported (within that range Lua's
//! integers and the expressions' agree). A select becomes `(if C then A
//! else B)` only when the result and every arm are one kind, integer or
//! boolean, after conversion (otherwise [`Unsupported::Type`]); the
//! residual's own refusals give totality, the closed candidate and its
//! node kinds purity, and the ranges, the residual's height and a trial
//! compile of the source within R4's limits ([`LIMITS`]) the rest. A
//! boolean select with a constant arm, or an arm that is its condition,
//! is `&&`, `||` or `!` of it. Each node is emitted once a lowering, a
//! shared one read from the memo; every generated string has its length
//! checked against [`MAX_SOURCE_BYTES`] (fatal past it) and charged
//! before it is built. Each lowering is a phase of the analysis's time
//! budget, located at its callable.
//!
//! A callback that does not lower is unsupported: a decorative one is
//! replaced by its typed constant, `false` for a `draw` (the element is
//! never drawn) and the omitted number for a `value`, reported with the
//! reason; a critical one refuses the candidate at its definition, or,
//! for [`Unsupported::Interference`] (a capture read while the candidate
//! is open), at the open site. A root whose reach writes never lowers
//! ([`Unsupported::Writes`]). A `value`'s role is its reads' ([`Lower::scan`]
//! over its whole tree and its helpers): one that reads the EX score or
//! the max combo is critical, whether or not the rest lowers, and so is
//! one with an id or callee the scan cannot resolve. A cap, cancellation
//! or the time budget refuses the candidate whatever the role.

use std::collections::{BTreeMap, HashMap};

use skin_model::{MAX_SOURCE_BYTES, PLAY, Scope, Ty};

use super::ops::{self, Cond};
use super::policy::{CriticalPart, Role};
use super::property::{self, IndexProp, IntProp};
use super::report::{NoteKind, Span};
use super::timer::{Custom, Timer};
use super::{Cx, Prop};
use crate::error::ImportError;
use crate::lua::callback::{
    Analysis, Callee, Captured, Disposition, Found, Site, fatal as sentinel, numeral,
    resolve_callee, tree,
};
use crate::lua::residual::{
    self, Conv, LIMITS, Node, R, Residual, Stop, Val, charge, did, stop, units,
};
use crate::lua::syntax::{Ast, Bin, DeclKind, Expr, List, Stmt};
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
    /// Whether its residual folded to a known value, for the conversion
    /// report.
    #[cfg(feature = "dev-tools")]
    pub(crate) constant: bool,
}

/// A text's value as a callback folded it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextFold {
    /// A string.
    Literal(String),
    /// Chart field `id`, 10 to 16.
    Field(i32),
    /// Chart field `id`, or the string where it is empty.
    FieldOr(i32, String),
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

/// A lowered callback's guards, critical part and deviation.
#[cfg(test)]
pub(crate) type Meta = (Vec<&'static str>, Option<CriticalPart>, bool);

#[cfg(test)]
thread_local! {
    /// Beside [`LOWERED`], each lowered callback's guards, critical part
    /// and deviation.
    pub(crate) static LOWERED_META: std::cell::RefCell<Vec<Meta>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The last trial compile: its source, and its error's kind.
    pub(crate) static TRIED: std::cell::RefCell<Option<(String, Option<skin_model::ErrorKind>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// The instances this thread's role scans visited, each visit counted.
    pub(crate) static SCANNED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A read as its mapping gives it, attached before anything is emitted.
#[derive(Debug)]
enum Mapped {
    /// `number(n)`: its field's expression and range.
    Int { src: String, lo: i64, hi: i64 },
    /// `option(n)`: its condition.
    Bool(String),
    /// `gauge_type()`, read only against a look.
    Gauge,
    /// `timer(n)`: its JSON mapping's timer name, or `None` for one never
    /// set, read only against the off value.
    Timer(Option<&'static str>),
}

/// An emitted subexpression.
#[derive(Debug)]
enum V {
    Int { src: String, lo: i64, hi: i64 },
    Bool(String),
}

impl V {
    fn src(&self) -> &str {
        match self {
            Self::Int { src, .. } | Self::Bool(src) => src,
        }
    }
}

/// The JSON mapping's timer name of model slot `slot`.
fn timer_name(slot: u16) -> Option<&'static str> {
    PLAY.timers.get(usize::from(slot)).map(|t| t.name)
}

/// The R4 symbol of comparison or arithmetic operator `op`.
fn symbol(op: Bin) -> &'static str {
    match op {
        Bin::Add => "+",
        Bin::Sub => "-",
        Bin::Mul => "*",
        Bin::Eq => "==",
        Bin::Ne => "!=",
        Bin::Lt => "<",
        Bin::Le => "<=",
        Bin::Gt => ">",
        _ => ">=",
    }
}

/// The emitter of a callback's residual onto the expressions, and the
/// role scan.
struct Lower<'a> {
    an: &'a Analysis,
    custom: &'a Custom,
    out: Lowered,
    /// Each read's mapping, by its node.
    mapped: HashMap<u32, Mapped>,
    /// Each node emitted, by its node: a shared node is emitted once.
    memo: HashMap<u32, V>,
}

impl<'a> Lower<'a> {
    /// A lowering of `an`'s callbacks with `custom`'s timers.
    fn new(an: &'a Analysis, custom: &'a Custom) -> Self {
        Self {
            an,
            custom,
            out: Lowered {
                src: String::new(),
                guards: Vec::new(),
                part: None,
                deviates: false,
                #[cfg(feature = "dev-tools")]
                constant: false,
            },
            mapped: HashMap::new(),
            memo: HashMap::new(),
        }
    }
}

impl Lower<'_> {
    /// `parts` joined into one generated string: their length summed and
    /// checked against [`MAX_SOURCE_BYTES`] (fatal past it), charged, then
    /// allocated.
    fn text(an: &Analysis, parts: &[&str]) -> R<String> {
        let n = parts.iter().fold(0_usize, |n, p| n.saturating_add(p.len()));
        if n > MAX_SOURCE_BYTES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Generated)));
        }
        charge(an, "text", units(n))?;
        did("text");
        let mut s = String::with_capacity(n);
        for p in parts {
            s.push_str(p);
        }
        Ok(s)
    }

    /// Integer `n`'s literal, `(-m)` when negative: its digits counted,
    /// then checked, charged and written as [`Self::text`] does.
    fn int_text(an: &Analysis, n: i64) -> R<String> {
        let mut m = n.unsigned_abs();
        let mut digits = 1_usize;
        while m >= 10 {
            m /= 10;
            digits += 1;
        }
        let len = digits + if n < 0 { 3 } else { 0 };
        if len > MAX_SOURCE_BYTES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Generated)));
        }
        charge(an, "text", units(len))?;
        did("text");
        #[cfg(test)]
        did("work:int");
        let mut s = String::with_capacity(len);
        if n < 0 {
            s.push_str("(-");
        }
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{}", n.unsigned_abs()));
        if n < 0 {
            s.push(')');
        }
        Ok(s)
    }

    fn guard(&mut self, g: &'static str) {
        if !self.out.guards.contains(&g) {
            self.out.guards.push(g);
        }
    }

    /// Residual `res` emitted as the source of an expression of the
    /// consumer's type: every read attached first (its mapping, guard,
    /// deviation and part, an unmapped one refused), then the nodes the
    /// root reaches, then a trial compile.
    fn emit_root(&mut self, res: &Residual, conv: Conv) -> R<String> {
        for &r in &res.reads {
            charge(self.an, "attach", 1)?;
            did("attach");
            if let Node::Read { api, arg } = res.node(r) {
                let m = self.map(api, arg)?;
                self.mapped.insert(r, m);
            }
        }
        #[cfg(feature = "dev-tools")]
        {
            self.out.constant = matches!(res.node(res.root), Node::Known(_));
        }
        let gauge =
            conv == Conv::Index && matches!(self.mapped.get(&res.root), Some(Mapped::Gauge));
        let src = if gauge {
            IndexProp::GaugeKind
                .selector()
                .ok_or(Stop::Unsupported(Unsupported::Type))?
                .to_owned()
        } else {
            self.emit(res, res.root)?;
            let src = match (self.memo.get(&res.root), conv) {
                (Some(V::Bool(s)), Conv::Truthy) => s,
                (Some(V::Int { src, .. }), Conv::Int | Conv::Index) => src,
                _ => return Err(Stop::Unsupported(Unsupported::Type)),
            };
            charge(self.an, "memo", units(src.len()))?;
            did("memo");
            src.clone()
        };
        let ty = if conv == Conv::Truthy {
            Ty::Bool
        } else {
            Ty::Int
        };
        charge(self.an, "compile", units(src.len()))?;
        did("compile");
        let tried = skin_model::compile(&src, &Scope::new(&PLAY), ty);
        #[cfg(test)]
        TRIED.with(|t| {
            *t.borrow_mut() = Some((src.clone(), tried.as_ref().err().map(|e| e.kind.clone())));
        });
        if tried.is_err() {
            return Err(Stop::Unsupported(LIMITS));
        }
        Ok(src)
    }

    /// Host read `api` of `arg` mapped: `number(n)` and `option(n)`
    /// through the JSON mappings, `timer(n)` through the custom timers,
    /// `gauge_type()` under `gauge.valid`; any other a refusal.
    fn map(&mut self, api: u16, arg: Option<i64>) -> R<Mapped> {
        let name = HOST_APIS.get(usize::from(api)).map_or("", |&(_, n)| n);
        let unmapped = || {
            Stop::Unsupported(Unsupported::Unmapped {
                api,
                id: arg.unwrap_or(0),
            })
        };
        let id = arg.and_then(|a| i32::try_from(a).ok());
        match name {
            "gauge_type" => {
                self.guard("gauge.valid");
                self.out.deviates = true;
                Ok(Mapped::Gauge)
            }
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
                let src = Self::text(self.an, &["(", v.expr(), ")"])?;
                Ok(Mapped::Int { src, lo, hi })
            }
            "judge" => {
                let n = arg.ok_or_else(unmapped)?;
                let Some(v) = property::judge_count(n) else {
                    return Ok(Mapped::Int {
                        src: Self::text(self.an, &["0"])?,
                        lo: 0,
                        hi: 0,
                    });
                };
                let (lo, hi) = v.range();
                let src = Self::text(self.an, &["(", v.expr(), ")"])?;
                Ok(Mapped::Int { src, lo, hi })
            }
            "option" => {
                let id = id.ok_or_else(unmapped)?;
                // The mapping builds its condition: its bound charged first.
                charge(self.an, "option", units(ops::MAX_COND_BYTES))?;
                #[cfg(test)]
                did("work:option");
                self.option(ops::numeric(id, &BTreeMap::new()), unmapped)
            }
            "timer" => {
                let id = id.filter(|&n| n >= 0).ok_or_else(unmapped)?;
                match self.custom.resolve(id) {
                    Timer::Builtin(slot) => {
                        Ok(Mapped::Timer(Some(timer_name(slot).ok_or_else(unmapped)?)))
                    }
                    Timer::Unset => Ok(Mapped::Timer(None)),
                    Timer::Rank(_) | Timer::Unsupported => Err(unmapped()),
                }
            }
            _ => Err(Stop::Unsupported(Unsupported::HostApi(api))),
        }
    }

    /// An option read's mapped condition `c`, built within the bound
    /// already charged.
    fn option(&mut self, c: Cond, unmapped: impl FnOnce() -> Stop) -> R<Mapped> {
        match c {
            Cond::Expr {
                src,
                display_deviation,
            } => {
                self.out.deviates |= display_deviation;
                Ok(Mapped::Bool(Self::text(self.an, &["(", &src, ")"])?))
            }
            Cond::Fold(b) => Ok(Mapped::Bool(Self::text(
                self.an,
                &[if b { "true" } else { "false" }],
            )?)),
            Cond::Undefined(_) => Ok(Mapped::Bool(Self::text(self.an, &["false"])?)),
            Cond::Unsupported(_) => Err(unmapped()),
        }
    }

    /// An integer subexpression, its range within `i32` (where Lua's
    /// integers and the expressions' agree).
    fn int(src: String, lo: i64, hi: i64) -> R<V> {
        if lo < i64::from(i32::MIN) || hi > i64::from(i32::MAX) {
            return Err(Stop::Unsupported(Unsupported::Range));
        }
        Ok(V::Int { src, lo, hi })
    }

    /// Node `r` emitted into the memo, once: each visit charged.
    fn emit(&mut self, res: &Residual, r: u32) -> R<()> {
        charge(self.an, "emit", 1)?;
        did("emit");
        if self.memo.contains_key(&r) {
            return Ok(());
        }
        let v = self.emit_node(res, r)?;
        self.memo.insert(r, v);
        Ok(())
    }

    /// The emitted operand `r`.
    fn get(&self, r: u32) -> R<&V> {
        self.memo
            .get(&r)
            .ok_or(Stop::Unsupported(Unsupported::Type))
    }

    fn emit_node(&mut self, res: &Residual, r: u32) -> R<V> {
        let an = self.an;
        let ty = || Err(Stop::Unsupported(Unsupported::Type));
        match res.node(r) {
            Node::Known(Val::Int(n)) => Self::int(Self::int_text(an, n)?, n, n),
            Node::Known(Val::Bool(b)) => Ok(V::Bool(Self::text(
                an,
                &[if b { "true" } else { "false" }],
            )?)),
            Node::Known(_) | Node::Unreachable => ty(),
            Node::Read { .. } => match self.mapped.get(&r) {
                Some(Mapped::Int { src, lo, hi }) => Self::int(Self::text(an, &[src])?, *lo, *hi),
                Some(Mapped::Bool(src)) => Ok(V::Bool(Self::text(an, &[src])?)),
                _ => ty(),
            },
            Node::Not(x) => {
                self.emit(res, x)?;
                match self.get(x)? {
                    V::Bool(s) => Ok(V::Bool(Self::text(an, &["(!", s, ")"])?)),
                    V::Int { .. } => ty(),
                }
            }
            Node::Neg(x) => {
                self.emit(res, x)?;
                match self.get(x)? {
                    // Every operand is within `i32` ([`Self::int`]): its
                    // negation, sums, differences and products fit `i64`.
                    V::Int { src, lo, hi } => {
                        Self::int(Self::text(an, &["(-", src, ")"])?, -*hi, -*lo)
                    }
                    V::Bool(_) => ty(),
                }
            }
            Node::Arith(op, a, b) => {
                self.emit(res, a)?;
                self.emit(res, b)?;
                let (
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
                ) = (self.get(a)?, self.get(b)?)
                else {
                    return ty();
                };
                let f = |p: i64, q: i64| match op {
                    Bin::Add => p + q,
                    Bin::Sub => p - q,
                    _ => p * q,
                };
                let c = [f(*al, *bl), f(*al, *bh), f(*ah, *bl), f(*ah, *bh)];
                let lo = c.iter().copied().min().unwrap_or(0);
                let hi = c.iter().copied().max().unwrap_or(0);
                let src = Self::text(an, &["(", x, " ", symbol(op), " ", y, ")"])?;
                Self::int(src, lo, hi)
            }
            Node::Cmp(op, a, b) => self.compare(res, op, a, b),
            Node::Select { cond, then, other } => {
                self.emit(res, cond)?;
                self.emit(res, then)?;
                self.emit(res, other)?;
                let V::Bool(c) = self.get(cond)? else {
                    return ty();
                };
                match (self.get(then)?, self.get(other)?) {
                    (V::Bool(t), V::Bool(o)) => {
                        // An arm that is the condition itself is the
                        // value the condition gives there.
                        let arm = |x: u32, b: bool| {
                            if x == cond {
                                Some(Val::Bool(b))
                            } else {
                                res.val(x)
                            }
                        };
                        let src = match (arm(then, true), arm(other, false)) {
                            (Some(Val::Bool(true)), Some(Val::Bool(false))) => {
                                Self::text(an, &[c])?
                            }
                            (Some(Val::Bool(false)), Some(Val::Bool(true))) => {
                                Self::text(an, &["(!", c, ")"])?
                            }
                            (_, Some(Val::Bool(false))) => {
                                Self::text(an, &["(", c, " && ", t, ")"])?
                            }
                            (Some(Val::Bool(true)), _) => {
                                Self::text(an, &["(", c, " || ", o, ")"])?
                            }
                            _ => Self::text(an, &["(if ", c, " then ", t, " else ", o, ")"])?,
                        };
                        Ok(V::Bool(src))
                    }
                    (
                        V::Int {
                            src: t,
                            lo: tl,
                            hi: th,
                        },
                        V::Int {
                            src: o,
                            lo: ol,
                            hi: oh,
                        },
                    ) => {
                        let (lo, hi) = ((*tl).min(*ol), (*th).max(*oh));
                        let src = Self::text(an, &["(if ", c, " then ", t, " else ", o, ")"])?;
                        Self::int(src, lo, hi)
                    }
                    _ => ty(),
                }
            }
        }
    }

    /// A comparison: `gauge_type()` against a known look, a timer against
    /// the off value, or integers or booleans.
    fn compare(&mut self, res: &Residual, op: Bin, a: u32, b: u32) -> R<V> {
        let an = self.an;
        let eq = op == Bin::Eq;
        let read = |r: u32| match res.node(r) {
            Node::Read { .. } => self.mapped.get(&r),
            _ => None,
        };
        let sides = [(a, b), (b, a)];
        for (x, y) in sides {
            match (read(x), res.val(y)) {
                (Some(Mapped::Gauge), Some(Val::Int(k))) if matches!(op, Bin::Eq | Bin::Ne) => {
                    let look = usize::try_from(k).ok().and_then(|k| LOOKS.get(k));
                    return Ok(V::Bool(match look {
                        Some(name) => {
                            Self::text(an, &["(gauge.kind ", symbol(op), " ", name, ")"])?
                        }
                        None => Self::text(an, &[if eq { "false" } else { "true" }])?,
                    }));
                }
                (Some(Mapped::Timer(t)), Some(Val::OffValue)) => {
                    let t = *t;
                    return Ok(V::Bool(match (t, eq) {
                        (Some(name), true) => Self::text(an, &["(!on(", name, "))"])?,
                        (Some(name), false) => Self::text(an, &["on(", name, ")"])?,
                        (None, on) => Self::text(an, &[if on { "true" } else { "false" }])?,
                    }));
                }
                _ => {}
            }
        }
        self.emit(res, a)?;
        self.emit(res, b)?;
        let (x, y) = (self.get(a)?, self.get(b)?);
        let both = matches!((x, y), (V::Int { .. }, V::Int { .. }))
            || matches!((x, y), (V::Bool(_), V::Bool(_))) && matches!(op, Bin::Eq | Bin::Ne);
        if !both {
            return Err(Stop::Unsupported(Unsupported::Type));
        }
        Ok(V::Bool(Self::text(
            an,
            &["(", x.src(), " ", symbol(op), " ", y.src(), ")"],
        )?))
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
        while let Some(i) = pending.pop() {
            let Some((ast, func)) = tree(self.an, i) else {
                out.unresolved = out.unresolved.or(Some("callee"));
                continue;
            };
            let Some(f) = ast.func(func) else {
                continue;
            };
            #[cfg(test)]
            SCANNED.with(|n| n.set(n.get() + 1));
            // Its parameters' joins copied, one unit each.
            self.an.charge(u64::from(f.params.1)).map_err(stop)?;
            let own = params.get(&i).cloned().unwrap_or_default();
            // The whole tree, function constructors' bodies included: the
            // calls collected first, each statement and expression one
            // unit. A local assigned again is the parser's
            // ([`crate::lua::syntax::Decl::assigned`]).
            blocks.clear();
            stack.clear();
            calls.clear();
            blocks.push(f.body);
            loop {
                if let Some(l) = blocks.pop() {
                    for &s in ast.list(l) {
                        // The statement and each item its lists hold.
                        self.an
                            .charge(stmt_width(ast, s).saturating_add(1))
                            .map_err(stop)?;
                        stmt_parts(ast, s, &mut blocks, &mut stack);
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
                        self.charge_scan(n)?;
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

    /// `n` units of work charged before they are done: a scan of `n`
    /// locals (at most Lua's 200), or `n` parameters' joins.
    fn charge_scan(&self, n: usize) -> R<()> {
        self.an
            .charge(u64::try_from(n).unwrap_or(u64::MAX))
            .map_err(stop)
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
            Some(x) if x.kind == DeclKind::Param && !x.assigned => {
                let params = self.ast.func(self.func).map_or((0, 0), |f| f.params);
                // Its position, each parameter one unit.
                self.an.charge(u64::from(params.1)).map_err(stop)?;
                let k = self.ast.list(params).iter().position(|&p| p == d);
                Ok(k.and_then(|k| self.own.get(k))
                    .copied()
                    .unwrap_or(Scan::unresolved("id")))
            }
            Some(x) if x.kind == DeclKind::Local && x.init != NONE && !x.assigned => {
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

/// Statement `s`'s blocks and expressions; a local it assigns is no
/// expression read.
fn stmt_parts(ast: &Ast, s: u32, blocks: &mut Vec<List>, exprs: &mut Vec<u32>) {
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
                    Expr::Local(_) => {}
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
                Expr::Local(_) => {}
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
    Lowered(Lowered),
    Unsupported(Unsupported, Scan),
}

/// The string `v` when it is one: valid UTF-8, its bytes charged.
fn known_string(an: &Analysis, v: Val) -> R<Option<String>> {
    let Val::Str(s) = v else {
        return Ok(None);
    };
    let bytes: Option<&[u8]> = match s {
        residual::S::Empty => Some(b""),
        residual::S::Lit { file, id } => an.ast(file).map(|a| a.str(id)),
        residual::S::Name(n) => {
            charge(an, "str", units(an.names.len()))?;
            did("str");
            an.names.iter().find_map(|(b, &i)| (i == n).then_some(&**b))
        }
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    charge(an, "str", units(bytes.len()))?;
    did("str");
    Ok(std::str::from_utf8(bytes).ok().map(str::to_owned))
}

/// What residual `res` folds to as a text's value (docs/stage3.md §3): a
/// known string, a read of a chart field, or a select on that read against
/// `""` with the read and a known string as its arms. Every read must be
/// of a chart field, as a text has nowhere to put a guard; `None` otherwise.
fn text_fold(an: &Analysis, res: &Residual) -> R<Option<TextFold>> {
    let field = |r: u32| match res.node(r) {
        Node::Read { api, arg: Some(n) } if residual::is_text(api) && (10..=16).contains(&n) => {
            i32::try_from(n).ok()
        }
        _ => None,
    };
    if res.reads.iter().any(|&r| field(r).is_none()) {
        return Ok(None);
    }
    if let Some(id) = field(res.root) {
        return Ok(Some(TextFold::Field(id)));
    }
    Ok(match res.node(res.root) {
        Node::Known(v) => known_string(an, v)?.map(TextFold::Literal),
        Node::Select { cond, then, other } => {
            let Node::Cmp(op, read, _) = res.node(cond) else {
                return Ok(None);
            };
            let (read_arm, known_arm) = match op {
                Bin::Eq => (other, then),
                Bin::Ne => (then, other),
                _ => return Ok(None),
            };
            let (Some(id), Node::Known(v)) = (field(read), res.node(known_arm)) else {
                return Ok(None);
            };
            if field(read_arm) != Some(id) {
                return Ok(None);
            }
            known_string(an, v)?.map(|s| TextFold::FieldOr(id, s))
        }
        _ => None,
    })
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
        at: (u32, u32),
        field: &'static str,
        role: Role,
        d: (Disposition, Option<Unsupported>),
    ) {
        self.record(at, field, role, d, None);
    }

    /// [`Self::outcome`] of callable `c` at node `id`, lowered to `l`.
    fn lowered_outcome(&self, at: (u32, u32), field: &'static str, role: Role, l: &Lowered) {
        self.record(at, field, role, (Disposition::Lowered, None), Some(l));
    }

    /// [`Self::outcome`]'s work, with what the callback lowered to, if it
    /// did, for the report.
    fn record(
        &self,
        (c, id): (u32, u32),
        field: &'static str,
        role: Role,
        (d, why): (Disposition, Option<Unsupported>),
        lowered: Option<&Lowered>,
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
                element: self.element.clone(),
                lowered: lowered.map(|l| crate::lua::survey::Lowered {
                    src: l.src.clone(),
                    guards: l.guards.clone(),
                    constant: l.constant,
                }),
            });
        #[cfg(not(feature = "dev-tools"))]
        let _ = (id, field, role, why, lowered);
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

    /// Lowers the callable at node `id`, converted by `conv`, a phase of
    /// the analysis's time budget located at it.
    fn lower(&mut self, id: u32, conv: Conv) -> Result<Lowering, ImportError> {
        self.lower_phase(id, |cx| cx.lower_in(id, conv))
    }

    /// `work` on the callable at node `id`, a phase of the analysis's time
    /// budget located at it.
    fn lower_phase<T>(
        &mut self,
        id: u32,
        work: impl FnOnce(&mut Self) -> Result<T, ImportError>,
    ) -> Result<T, ImportError> {
        let at = self.def.loc(id);
        let fatal = |why| ImportError::Lua {
            at: Some(at.clone()),
            why,
        };
        let Some(an) = self.analysis() else {
            return work(self);
        };
        let phase = crate::lua::callback::Phase::Lower;
        an.clock
            .enter(phase, Some(id))
            .map_err(|w| fatal(sentinel(w).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisTime))))?;
        let r = work(self);
        let left = self
            .analysis()
            .map_or(Ok(()), |an| an.clock.leave())
            .map_err(|w| fatal(sentinel(w).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisTime))));
        let r = r?;
        left.map(|()| r)
    }

    /// [`Self::lower`]'s work.
    fn lower_in(&mut self, id: u32, conv: Conv) -> Result<Lowering, ImportError> {
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
        let Some(adm) = an.admitted() else {
            return Ok(Lowering::Unsupported(
                Unsupported::Capture("an unadmitted callback"),
                Scan::default(),
            ));
        };
        let an = adm.analysis();
        let cancelled = || def.cancelled(|| None).is_err();
        let mut l = Lower::new(an, &self.custom);
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
        let r = residual::evaluate(adm, inst, conv, &cancelled)
            .and_then(|res: Residual| l.emit_root(&res, conv));
        #[cfg(test)]
        crate::lua::tests::memo_witness(l.memo.len());
        match r {
            Ok(src) => {
                l.out.src = src;
                Ok(Lowering::Lowered(l.out))
            }
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
        self.follows_role(role, (c, id), field, why)
    }

    /// Callable `c` at node `id`, for `why` not consumed in `field`, for an
    /// element of `role`: a critical one refuses the candidate, a decorative
    /// one is replaced, the note to drop its element with returned.
    fn follows_role(
        &self,
        role: Role,
        (c, id): (u32, u32),
        field: &'static str,
        why: Unsupported,
    ) -> Result<NoteKind, ImportError> {
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

    /// A text's `value` callback at node `id`, for an element of `role`:
    /// what it folded to, or the note to drop the element with. A result
    /// with no shape a text takes is unconsumed; an evaluation that stopped
    /// carries its own reason.
    pub(super) fn text_callback(
        &mut self,
        role: Role,
        id: u32,
    ) -> Result<Result<TextFold, NoteKind>, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        // Its reads decide its role before it folds.
        let role = self.callback_role(id, "value", role)?;
        match self.lower_phase(id, |cx| cx.fold_text(id))? {
            Ok(Some(fold)) => {
                self.record(
                    (c, id),
                    "value",
                    role,
                    (Disposition::Lowered, None),
                    Some(&Lowered {
                        src: format!("{fold:?}"),
                        guards: Vec::new(),
                        part: None,
                        deviates: false,
                        #[cfg(feature = "dev-tools")]
                        constant: matches!(fold, TextFold::Literal(_)),
                    }),
                );
                Ok(Ok(fold))
            }
            Ok(None) => self.unconsumed(role, id, "value", "text value").map(Err),
            Err(why) => self.follows_role(role, (c, id), "value", why).map(Err),
        }
    }

    /// What the callable at node `id` folds to as a text's value (`None`
    /// for a result with no shape a text takes), or why evaluation stopped.
    fn fold_text(&self, id: u32) -> Result<Result<Option<TextFold>, Unsupported>, ImportError> {
        let def: &crate::resolve::Definition = self.def;
        let (Some((_, inst)), Some(an)) = (
            self.callable_at(id).filter(|&(_, i)| i != NONE),
            def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref()),
        ) else {
            return Ok(Err(Unsupported::Capture("an untagged closure")));
        };
        let Some(adm) = an.admitted() else {
            return Ok(Err(Unsupported::Capture("an unadmitted callback")));
        };
        let an = adm.analysis();
        // A root whose reach writes never lowers.
        if an.writes(inst) {
            return Ok(Err(Unsupported::Writes));
        }
        let cancelled = || def.cancelled(|| None).is_err();
        match residual::evaluate(adm, inst, Conv::Text, &cancelled)
            .and_then(|res: Residual| text_fold(an, &res))
        {
            Ok(fold) => Ok(Ok(fold)),
            Err(Stop::Unsupported(why)) => Ok(Err(why)),
            Err(Stop::Fatal(why)) => Err(ImportError::Lua {
                at: Some(self.def.loc(id)),
                why,
            }),
        }
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
        let r = Lower::new(an, &self.custom).scan(inst);
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
        let why = match self.lower(id, Conv::Truthy) {
            Ok(Lowering::Lowered(l)) => {
                self.lowered_outcome((c, id), field, role, &l);
                #[cfg(test)]
                LOWERED.with(|w| {
                    let mut all: Vec<String> = l.guards.iter().map(|g| (*g).to_owned()).collect();
                    all.push(l.src.clone());
                    w.borrow_mut().push(all.join(" && "));
                });
                #[cfg(test)]
                LOWERED_META.with(|w| w.borrow_mut().push((l.guards.clone(), l.part, l.deviates)));
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
        conv: Conv,
    ) -> Result<Option<NumberValue>, ImportError> {
        let c = self.callable_at(id).map_or(NONE, |(c, _)| c);
        let (why, scan) = match self.lower(id, conv) {
            Ok(Lowering::Lowered(l)) => {
                self.lowered_outcome((c, id), "value", l.part.map_or(base, Role::Critical), &l);
                #[cfg(test)]
                LOWERED.with(|w| w.borrow_mut().push(l.src.clone()));
                #[cfg(test)]
                LOWERED_META.with(|w| w.borrow_mut().push((l.guards.clone(), l.part, l.deviates)));
                let guard = (!l.guards.is_empty()).then(|| l.guards.join(" && "));
                return Ok(Some(NumberValue {
                    expr: l.src,
                    guard,
                    role: l.part.map_or(base, Role::Critical),
                    deviates: l.deviates,
                }));
            }
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
