//! A consumed callback partially evaluated into its residual (design.md
//! "beatoraja Lua Skins", per-frame closures; docs/stage3.md §3,
//! per-frame functions): what is known at import folded, the rest kept as
//! typed nodes a backend maps (`import::callback` onto the expressions),
//! never by running the callback.
//!
//! [`evaluate`] reads an admitted analysis ([`Admitted`]) and walks the
//! callback's tree and the helpers it calls:
//!
//! - **Values known at import**: nil, booleans, integers of at most 2^53
//!   in magnitude, strings (a captured one by its interned id, a literal
//!   by its bytes, compared by bytes), tables without a metatable,
//!   closures, host modules, host functions and library functions; and
//!   the host's off value ([`Val::OffValue`]).
//! - **What folds**: captures (upvalues, and globals through an `_ENV`
//!   without a metatable), table entries along the static paths
//!   ([`Analysis::entry`]), `skin_config` included, so the configuration
//!   folds whatever its kind but a float; module members; locals; helper
//!   calls, inlined with their arguments; operators on known operands;
//!   `if`, `elseif` and `else` with a known condition. Each capture read
//!   holds only while the candidate is closed ([`Unsupported::Interference`]
//!   otherwise).
//! - **Integers follow the source's doubles**: a numeral the parser
//!   records as wide ([`super::syntax::Ast::wide_numerals`]) and a
//!   captured integer past 2^53 in magnitude do not fold
//!   ([`Unsupported::Range`]), but a captured `i64::MIN` read from a
//!   never-assigned local whose every initialization the provenance marker
//!   recorded as the host module's `timer_off_value`
//!   ([`super::instrument`]), with the module's proxy as the pass built it,
//!   which is the off value. Every folded operand and result stays within
//!   2^53. Values construction computed are taken as the source's, under
//!   the construction premise, whatever arithmetic gave them.
//! - **What stays residual**: host reads with a known integer argument
//!   ([`Node::Read`], each recorded in [`Residual::reads`] as it is
//!   created on a reached path), `-`, `not`, integer `+ - *`,
//!   comparisons, and [`Node::Select`]. `A and B` is `Select(A, B, A)`
//!   and `A or B` `Select(A, A, B)` on `A`'s truthiness, only nil and
//!   false being false; a known `A` short-circuits, and `B` is never
//!   evaluated then. An operator on a [`Node::Select`] of mixed kinds
//!   distributes into its arms; `==` and `~=` of different kinds fold.
//! - **Statements**: `local` (each declaration its own binding), an
//!   assignment to the function's own locals (every value evaluated
//!   before any is stored; a statement assigning one twice is refused),
//!   call statements, `do`, `return` of one value or none, and `if`
//!   joined: a residual condition runs both arms from one state and
//!   merges their locals with [`Node::Select`], and a `return` that may
//!   have happened travels as a condition and a value, so paths are never
//!   split.
//! - **Refused**, each [`Unsupported`] following the callback's role:
//!   loops, `goto`, function constructors, floats, `/ // % ^`, bitwise
//!   operators, `..`, `#`, library calls, method calls, table
//!   constructors, varargs, `return` lists, recursion, a host argument that
//!   is not a known integer, a definite Lua error on a reached path, and
//!   string arithmetic or ordering.
//!
//! The callback's result is then converted as its consumer converts it
//! ([`Conv`]): a `draw` or `op` by truthiness, a number's `value` as an
//! integer (nil and booleans 0), distributed into a [`Node::Select`]'s
//! arms.
//!
//! The residual is a DAG in one arena a lowering, dropped with it. Every
//! node is charged against [`MAX_EXPANDED_NODES`] before it is pushed
//! (fatal), and one past R4's nesting is refused by role as it is made;
//! every statement, expression level, inlined call, inline-stack scan,
//! parameter binding, environment copy and merge, string comparison and
//! conversion or distribution visit is charged before its work, the
//! depth bounded by [`MAX_AST_DEPTH`].

use super::callback::{Admitted, Analysis, Captured, MAIN_STATE, fatal};
use super::limits::{MAX_AST_DEPTH, MAX_EXPANDED_NODES};
use super::syntax::{Ast, Attrib, Bin, CHUNK_ENV, EXACT, Expr, List, NONE, Stmt, Un};
use super::{HOST_APIS, LuaLimit, LuaWhy, Unsupported};

/// A residual's refusal for a shape the expressions cannot hold: nesting
/// past [`skin_model::MAX_NESTING`], or a source the trial compile
/// refuses.
pub(crate) const LIMITS: Unsupported = Unsupported::Construct("past the expression's limits");

/// Value kinds, a node's set of them.
pub(crate) const NIL: u8 = 1;
pub(crate) const BOOL: u8 = 2;
pub(crate) const INT: u8 = 4;
pub(crate) const OFF: u8 = 8;
pub(crate) const STR: u8 = 16;
pub(crate) const TABLE: u8 = 32;
pub(crate) const FUNC: u8 = 64;
/// The kinds whose every value is true.
const TRUTHY: u8 = INT | OFF | STR | TABLE | FUNC;

/// A string known at import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum S {
    /// A captured one, by its index among [`Analysis::names`].
    Name(u32),
    /// A literal: its file and its index in that file's strings.
    Lit { file: u16, id: u32 },
}

/// A value known at import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Val {
    Nil,
    Bool(bool),
    /// At most [`EXACT`] in magnitude.
    Int(i64),
    /// The host's timer off value, only from a recorded provenance.
    OffValue,
    Str(S),
    /// A captured table without a metatable.
    Table(u32),
    Closure(u32),
    Module(u8),
    Host(u16),
    Lib(u16),
}

impl Val {
    fn kind(self) -> u8 {
        match self {
            Self::Nil => NIL,
            Self::Bool(_) => BOOL,
            Self::Int(_) => INT,
            Self::OffValue => OFF,
            Self::Str(_) => STR,
            Self::Table(_) => TABLE,
            Self::Closure(_) | Self::Module(_) | Self::Host(_) | Self::Lib(_) => FUNC,
        }
    }
}

/// A residual node, by its index in [`Residual`]'s arena for operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Known(Val),
    /// A host reader with one result, by its index in [`HOST_APIS`], and
    /// its known integer argument.
    Read {
        api: u16,
        arg: Option<i64>,
    },
    /// `not` of a boolean.
    Not(u32),
    /// `-` of an integer.
    Neg(u32),
    /// `+ - *` of integers.
    Arith(Bin, u32, u32),
    /// A comparison: integers, booleans (`==` and `~=`), or a timer read
    /// against the off value (`==` and `~=`).
    Cmp(Bin, u32, u32),
    /// `then` where the boolean `cond` holds, else `other`.
    Select {
        cond: u32,
        then: u32,
        other: u32,
    },
    /// A value no path gives: a return that did not happen.
    Unreachable,
}

/// How the consumer converts the callback's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Conv {
    /// A `draw` or an `op` item: Lua's truthiness.
    Truthy,
    /// A number's `value`: an integer, nil and booleans 0.
    Int,
}

/// Why evaluation stopped.
pub(crate) enum Stop {
    Unsupported(Unsupported),
    Fatal(LuaWhy),
}

/// A walk's stop for `why`: fatal for a cap or cancellation.
pub(crate) fn stop(why: Unsupported) -> Stop {
    fatal(why).map_or(Stop::Unsupported(why), Stop::Fatal)
}

pub(crate) type R<T> = Result<T, Stop>;

fn refuse<T>(why: Unsupported) -> R<T> {
    Err(Stop::Unsupported(why))
}

fn construct<T>(what: &'static str) -> R<T> {
    refuse(Unsupported::Construct(what))
}

/// `n` units of analysis work charged at the named `site` before its
/// work: a test can make it fail with [`TOO_MUCH`](super::callback::TOO_MUCH).
pub(crate) fn charge(an: &Analysis, site: &'static str, n: u64) -> R<()> {
    #[cfg(test)]
    {
        if super::tests::fail_at(site) {
            return Err(stop(super::callback::TOO_MUCH));
        }
        super::tests::site_charged(site, n);
    }
    #[cfg(not(test))]
    let _ = site;
    an.charge(n).map_err(stop)
}

/// Records, for a test, that the work charged at `site` started.
pub(crate) fn did(site: &'static str) {
    #[cfg(test)]
    super::tests::site_did(site);
    #[cfg(not(test))]
    let _ = site;
}

/// A length as work units.
pub(crate) fn units(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A callback's residual: its nodes, each with its kinds and height, the
/// reads created on a reached path, and its converted result.
#[derive(Debug, Default)]
pub(crate) struct Residual {
    nodes: Vec<Node>,
    kinds: Vec<u8>,
    heights: Vec<u16>,
    /// Every [`Node::Read`] created, in order.
    pub(crate) reads: Vec<u32>,
    pub(crate) root: u32,
}

impl Residual {
    pub(crate) fn node(&self, r: u32) -> Node {
        self.nodes
            .get(r as usize)
            .copied()
            .unwrap_or(Node::Unreachable)
    }

    pub(crate) fn kind(&self, r: u32) -> u8 {
        self.kinds.get(r as usize).copied().unwrap_or(0)
    }

    /// The value of node `r` when it is known.
    pub(crate) fn val(&self, r: u32) -> Option<Val> {
        match self.node(r) {
            Node::Known(v) => Some(v),
            _ => None,
        }
    }
}

/// The kind of host API `api`'s read, when it is a reader with one result
/// the residual keeps, and its arity.
fn reader(api: u16) -> Option<(usize, u8)> {
    match HOST_APIS.get(usize::from(api)).copied()? {
        ("main_state", "number" | "timer" | "event_index") => Some((1, INT)),
        ("main_state", "option") => Some((1, BOOL)),
        ("main_state", "text") => Some((1, STR)),
        ("main_state", "gauge_type") => Some((0, INT)),
        _ => None,
    }
}

/// Whether host API `api` is `main_state.timer`.
pub(crate) fn is_timer(api: u16) -> bool {
    HOST_APIS.get(usize::from(api)) == Some(&("main_state", "timer"))
}

/// What a function body left: no `return` yet, one that happened, or one
/// that happened where the condition holds, with its value.
#[derive(Clone, Copy, Debug)]
enum Done {
    No,
    Yes(u32),
    Maybe(u32, u32),
}

/// Locals in scope, innermost last: each declaration to its node.
type Env = Vec<(u32, u32)>;

/// One function being evaluated: its tree, instance and file, and its
/// locals in scope.
struct Frame<'a> {
    ast: &'a Ast,
    inst: u32,
    file: u16,
    env: Env,
}

struct Eval<'a> {
    an: &'a Analysis,
    cancelled: &'a dyn Fn() -> bool,
    res: Residual,
    depth: u32,
    /// The instances being inlined, outermost first.
    stack: Vec<u32>,
}

/// Instance `inst` of admitted analysis `adm` evaluated into its
/// residual, converted by `conv`; `cancelled` seen at each inlined
/// function.
pub(crate) fn evaluate(
    adm: Admitted<'_>,
    inst: u32,
    conv: Conv,
    cancelled: &dyn Fn() -> bool,
) -> R<Residual> {
    let mut ev = Eval {
        an: adm.analysis(),
        cancelled,
        res: Residual::default(),
        depth: 0,
        stack: Vec::new(),
    };
    let v = ev.function(inst, &[])?;
    let root = match conv {
        Conv::Truthy => ev.truthy(v)?,
        Conv::Int => ev.integer(v)?,
    };
    ev.res.root = root;
    Ok(ev.res)
}

impl<'a> Eval<'a> {
    fn push(&mut self, node: Node) -> R<u32> {
        let h = |r: u32| self.res.heights.get(r as usize).copied().unwrap_or(0);
        let k = |r: u32| self.res.kind(r);
        let (kind, height) = match node {
            Node::Known(v) => (v.kind(), 1),
            Node::Read { api, .. } => (reader(api).map_or(0, |(_, k)| k), 1),
            Node::Unreachable => (0, 1),
            Node::Not(x) => (BOOL, h(x)),
            Node::Neg(x) => (INT, h(x)),
            Node::Arith(_, a, b) => (INT, h(a).max(h(b))),
            Node::Cmp(_, a, b) => (BOOL, h(a).max(h(b))),
            Node::Select { cond, then, other } => {
                (k(then) | k(other), h(cond).max(h(then)).max(h(other)))
            }
        };
        let height = match node {
            Node::Known(_) | Node::Read { .. } | Node::Unreachable => height,
            _ => height.saturating_add(1),
        };
        // Past R4's nesting, refused as it is made.
        if usize::from(height) > skin_model::MAX_NESTING {
            return refuse(LIMITS);
        }
        #[cfg(test)]
        {
            if super::tests::fail_at("node") {
                return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
            }
            super::tests::site_charged("node", 1);
        }
        let total = self.an.expanded.get().saturating_add(1);
        if total > MAX_EXPANDED_NODES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
        }
        self.an.expanded.set(total);
        did("node");
        let id = u32::try_from(self.res.nodes.len())
            .map_err(|_| Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)))?;
        self.res.nodes.push(node);
        self.res.kinds.push(kind);
        self.res.heights.push(height);
        Ok(id)
    }

    fn known(&mut self, v: Val) -> R<u32> {
        self.push(Node::Known(v))
    }

    fn val(&self, r: u32) -> Option<Val> {
        self.res.val(r)
    }

    fn kind(&self, r: u32) -> u8 {
        self.res.kind(r)
    }

    /// Whether node `r` may have more than one kind: a [`Node::Select`]
    /// operators distribute into.
    fn mixed(&self, r: u32) -> Option<(u32, u32, u32)> {
        let k = self.kind(r);
        match self.res.node(r) {
            Node::Select { cond, then, other } if k & k.wrapping_sub(1) != 0 => {
                Some((cond, then, other))
            }
            _ => None,
        }
    }

    /// What a proof resting on captured state needs: the candidate closed.
    fn bound(&self) -> R<()> {
        if self.an.open.get().is_some() {
            return refuse(Unsupported::Interference);
        }
        Ok(())
    }

    /// Runs `f` one level deeper: past [`MAX_AST_DEPTH`] fatal.
    fn deeper<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        if self.depth >= MAX_AST_DEPTH {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::AstDepth)));
        }
        #[cfg(test)]
        {
            if super::tests::fail_at("depth") {
                return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::AstDepth)));
            }
            super::tests::site_charged("depth", 1);
        }
        self.an.charge(1).map_err(stop)?;
        did("depth");
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// `then` where `cond` holds, else `other`, simplified: a known
    /// condition, equal arms, an unreachable arm, an arm selecting on the
    /// same condition, and `Select(c, true, false)`.
    fn select(&mut self, cond: u32, then: u32, other: u32) -> R<u32> {
        match self.val(cond) {
            Some(Val::Bool(true)) => return Ok(then),
            Some(Val::Bool(false)) => return Ok(other),
            _ => {}
        }
        let then = match self.res.node(then) {
            Node::Select {
                cond: c, then: t, ..
            } if c == cond => t,
            _ => then,
        };
        let other = match self.res.node(other) {
            Node::Select {
                cond: c, other: o, ..
            } if c == cond => o,
            _ => other,
        };
        if then == other || self.res.node(other) == Node::Unreachable {
            return Ok(then);
        }
        if self.res.node(then) == Node::Unreachable {
            return Ok(other);
        }
        match (self.val(then), self.val(other)) {
            (Some(a), Some(b)) if a == b && !matches!(a, Val::Str(_)) => return Ok(then),
            (Some(Val::Bool(true)), Some(Val::Bool(false))) => return Ok(cond),
            _ => {}
        }
        self.push(Node::Select { cond, then, other })
    }

    /// Instance `j` called with `args`: the inline stack scanned, the
    /// parameters bound, then its body.
    fn function(&mut self, j: u32, args: &[u32]) -> R<u32> {
        #[cfg(test)]
        super::tests::expansion_witness();
        if (self.cancelled)() {
            return Err(Stop::Fatal(LuaWhy::Cancelled));
        }
        charge(self.an, "scan", units(self.stack.len()))?;
        did("scan");
        if self.stack.contains(&j) {
            return refuse(Unsupported::Recursion);
        }
        let untagged = || Stop::Unsupported(Unsupported::Capture("an untagged closure"));
        let inst = *self.an.instance(j).ok_or_else(untagged)?;
        let ast = self.an.ast(inst.file).ok_or_else(untagged)?;
        let f = ast.func(inst.func).ok_or_else(untagged)?;
        if f.vararg {
            return construct("vararg");
        }
        if inst.values.is_none() && f.upvalues.1 > 0 {
            return refuse(Unsupported::Capture("no reader"));
        }
        let params = ast.list(f.params);
        charge(self.an, "bind", units(params.len().max(args.len())))?;
        did("bind");
        let mut env = Vec::with_capacity(params.len());
        for (k, &p) in params.iter().enumerate() {
            let v = match args.get(k) {
                Some(&a) => a,
                None => self.known(Val::Nil)?,
            };
            env.push((p, v));
        }
        let mut frame = Frame {
            ast,
            inst: j,
            file: inst.file,
            env,
        };
        self.stack.push(j);
        let mut done = Done::No;
        let r = self.block(&mut frame, f.body, &mut done);
        self.stack.pop();
        r?;
        match done {
            Done::No => self.known(Val::Nil),
            Done::Yes(v) => Ok(v),
            Done::Maybe(c, v) => {
                let nil = self.known(Val::Nil)?;
                self.select(c, v, nil)
            }
        }
    }

    fn block(&mut self, f: &mut Frame<'a>, body: List, done: &mut Done) -> R<()> {
        let mark = f.env.len();
        for &s in f.ast.list(body) {
            if matches!(done, Done::Yes(_)) {
                break;
            }
            charge(self.an, "stmt", 1)?;
            did("stmt");
            self.stmt(f, s, done)?;
        }
        f.env.truncate(mark);
        Ok(())
    }

    /// The slot of local `d` in `f`'s environment, the scan charged.
    fn slot(&self, f: &Frame<'a>, d: u32) -> R<Option<usize>> {
        self.an.charge(units(f.env.len())).map_err(stop)?;
        Ok(f.env.iter().rposition(|&(x, _)| x == d))
    }

    fn stmt(&mut self, f: &mut Frame<'a>, s: u32, done: &mut Done) -> R<()> {
        let ast = f.ast;
        match ast.stmt(s) {
            Stmt::Empty => Ok(()),
            Stmt::Local { names, exprs } => {
                let (ns, es) = (ast.list(names), ast.list(exprs));
                for &d in ns {
                    if ast.decl(d).is_some_and(|x| x.attrib == Attrib::Close) {
                        return construct("<close>");
                    }
                }
                let mut vals = Vec::with_capacity(ns.len().min(es.len()));
                for (k, &e) in es.iter().enumerate() {
                    let v = self.expr(f, e)?;
                    if k < ns.len() {
                        vals.push(v);
                    }
                }
                for (k, &d) in ns.iter().enumerate() {
                    let v = match vals.get(k) {
                        Some(&v) => v,
                        None => self.known(Val::Nil)?,
                    };
                    f.env.push((d, v));
                }
                Ok(())
            }
            Stmt::Assign { targets, exprs } => {
                let ts = ast.list(targets);
                let mut slots = Vec::with_capacity(ts.len());
                for &t in ts {
                    let Expr::Local(d) = ast.expr(t) else {
                        return construct("assignment");
                    };
                    let Some(k) = self.slot(f, d)? else {
                        return construct("assignment");
                    };
                    slots.push(k);
                }
                // A target assigned twice: the order the source stores
                // in decides it.
                self.an.charge(units(slots.len())).map_err(stop)?;
                let mut sorted = slots.clone();
                sorted.sort_unstable();
                if sorted.windows(2).any(|w| w.first() == w.get(1)) {
                    return construct("a target assigned twice");
                }
                let mut vals = Vec::with_capacity(slots.len());
                for (k, &e) in ast.list(exprs).iter().enumerate() {
                    let v = self.expr(f, e)?;
                    if k < slots.len() {
                        vals.push(v);
                    }
                }
                for (k, &slot) in slots.iter().enumerate() {
                    let v = match vals.get(k) {
                        Some(&v) => v,
                        None => self.known(Val::Nil)?,
                    };
                    if let Some(entry) = f.env.get_mut(slot) {
                        entry.1 = v;
                    }
                }
                Ok(())
            }
            Stmt::Call(e) => self.expr(f, e).map(|_| ()),
            Stmt::Return(list) => {
                let v = match ast.list(list) {
                    [] => self.known(Val::Nil)?,
                    &[e] => self.expr(f, e)?,
                    _ => return construct("return list"),
                };
                *done = match *done {
                    Done::No => Done::Yes(v),
                    Done::Maybe(c, w) => Done::Yes(self.select(c, w, v)?),
                    Done::Yes(w) => Done::Yes(w),
                };
                Ok(())
            }
            Stmt::If(arms) => self.branches(f, ast.list(arms), done),
            Stmt::Do(body) => self.block(f, body, done),
            other => construct(other.name()),
        }
    }

    /// An `if`'s arms (condition, block start, block length; the `else`
    /// arm's condition [`NONE`]): known conditions take their arm, each
    /// residual one runs its arm from the state before, and the states
    /// are merged from the last.
    fn branches(&mut self, f: &mut Frame<'a>, arms: &[u32], done: &mut Done) -> R<()> {
        // The residual arms: condition, locals and return state after.
        let mut taken: Vec<(u32, Env, Done)> = Vec::new();
        for arm in arms.chunks(3) {
            let &[c, s, n] = arm else {
                break;
            };
            let cond = if c == NONE {
                self.known(Val::Bool(true))?
            } else {
                let x = self.expr(f, c)?;
                self.truthy(x)?
            };
            match self.val(cond) {
                Some(Val::Bool(true)) => {
                    self.block(f, (s, n), done)?;
                    break;
                }
                Some(Val::Bool(false)) => {}
                _ => {
                    charge(self.an, "env", units(f.env.len()))?;
                    did("env");
                    let before = (f.env.clone(), *done);
                    self.block(f, (s, n), done)?;
                    let env = std::mem::replace(&mut f.env, before.0);
                    let after = std::mem::replace(done, before.1);
                    taken.push((cond, env, after));
                }
            }
        }
        while let Some((cond, env, after)) = taken.pop() {
            charge(self.an, "env", units(f.env.len()))?;
            did("env");
            match (after, *done) {
                // The arm returned: the paths past the `if` took the rest.
                (Done::Yes(_), _) => {}
                (_, Done::Yes(_)) => f.env = env,
                _ => {
                    for (k, &(_, t)) in env.iter().enumerate() {
                        let Some(&(_, o)) = f.env.get(k) else {
                            continue;
                        };
                        if t != o {
                            let v = self.select(cond, t, o)?;
                            if let Some(slot) = f.env.get_mut(k) {
                                slot.1 = v;
                            }
                        }
                    }
                }
            }
            *done = self.merge(cond, after, *done)?;
        }
        Ok(())
    }

    /// The return states `a` where `cond` holds and `b` elsewhere, joined.
    fn merge(&mut self, cond: u32, a: Done, b: Done) -> R<Done> {
        Ok(match (a, b) {
            (Done::No, Done::No) => Done::No,
            (Done::Yes(x), Done::Yes(y)) => Done::Yes(self.select(cond, x, y)?),
            _ => {
                let (ra, va) = self.parts(a)?;
                let (rb, vb) = self.parts(b)?;
                let r = self.select(cond, ra, rb)?;
                let v = self.select(cond, va, vb)?;
                Done::Maybe(r, v)
            }
        })
    }

    /// A return state as whether it returned and its value.
    fn parts(&mut self, d: Done) -> R<(u32, u32)> {
        Ok(match d {
            Done::No => (self.known(Val::Bool(false))?, self.push(Node::Unreachable)?),
            Done::Yes(v) => (self.known(Val::Bool(true))?, v),
            Done::Maybe(r, v) => (r, v),
        })
    }

    fn expr(&mut self, f: &mut Frame<'a>, e: u32) -> R<u32> {
        self.deeper(|ev| ev.expr_inner(f, e))
    }

    fn expr_inner(&mut self, f: &mut Frame<'a>, e: u32) -> R<u32> {
        let ast = f.ast;
        match ast.expr(e) {
            Expr::Nil => self.known(Val::Nil),
            Expr::True => self.known(Val::Bool(true)),
            Expr::False => self.known(Val::Bool(false)),
            Expr::Int(n) => {
                if ast.wide(e) || n.unsigned_abs() > EXACT {
                    return refuse(Unsupported::Range);
                }
                self.known(Val::Int(n))
            }
            Expr::Float(_) => construct("float"),
            Expr::Str(s) => self.known(Val::Str(S::Lit {
                file: f.file,
                id: s,
            })),
            Expr::Vararg => construct("vararg"),
            Expr::Func(_) => construct("function"),
            Expr::Table(_) => construct("table"),
            Expr::Method { .. } => construct("method call"),
            Expr::Paren(x) => self.expr(f, x),
            Expr::Local(d) => self.local(f, d),
            Expr::Global { name, env } => self.global(f, name, env),
            Expr::Index { obj, key } => {
                let o = self.expr(f, obj)?;
                let k = self.expr(f, key)?;
                self.index(o, k)
            }
            Expr::Call { callee, args } => self.call(f, callee, args),
            Expr::Unary(Un::Len, _) => construct("#"),
            Expr::Unary(Un::BNot, _) => construct("bitwise"),
            Expr::Unary(Un::Neg, x) => {
                let v = self.expr(f, x)?;
                self.neg(v)
            }
            Expr::Unary(Un::Not, x) => {
                let v = self.expr(f, x)?;
                let t = self.truthy(v)?;
                self.not(t)
            }
            Expr::Binary(op, a, b) => self.binary(f, op, a, b),
        }
    }

    /// Local or upvalue `d`.
    fn local(&mut self, f: &mut Frame<'a>, d: u32) -> R<u32> {
        if let Some(k) = self.slot(f, d)?
            && let Some(&(_, v)) = f.env.get(k)
        {
            return Ok(v);
        }
        self.bound()?;
        match self.an.upvalue(f.inst, d) {
            Some(c) => self.captured(c, Some((f.ast, f.file, d))),
            None => refuse(Unsupported::Capture("an upvalue")),
        }
    }

    /// Global `name` read through `env`.
    fn global(&mut self, f: &mut Frame<'a>, name: u32, env: u32) -> R<u32> {
        if env != CHUNK_ENV && self.slot(f, env)?.is_some() {
            return construct("_ENV");
        }
        self.bound()?;
        match self.an.global(f.inst, env, f.ast.str(name)).map_err(stop)? {
            Some(c) => self.captured(c, None),
            None => refuse(Unsupported::Capture("an entry no path copied")),
        }
    }

    /// A captured value, from upvalue `decl` of its tree and file when it
    /// is one.
    fn captured(&mut self, c: Captured, decl: Option<(&Ast, u16, u32)>) -> R<u32> {
        let v = match c {
            Captured::Nil => Val::Nil,
            Captured::Bool(b) => Val::Bool(b),
            Captured::Int(n) if n.unsigned_abs() <= EXACT => Val::Int(n),
            Captured::Int(i64::MIN) if self.off_provenance(decl) => Val::OffValue,
            Captured::Int(_) => return refuse(Unsupported::Range),
            Captured::Float(_) => return construct("float"),
            Captured::Str(id) => Val::Str(S::Name(id)),
            Captured::Host(api) => Val::Host(api),
            Captured::Module(m) => Val::Module(m),
            Captured::Closure(j) => Val::Closure(j),
            Captured::Table { id, meta: false } => Val::Table(id),
            Captured::Table { id, meta: true } => {
                return refuse(Unsupported::Capture(self.metatable(id)));
            }
            Captured::Lib(l) => Val::Lib(l),
            Captured::Other => return refuse(Unsupported::Capture("a captured value")),
        };
        self.known(v)
    }

    /// Whether upvalue `decl` holds the host's off value: it is never
    /// assigned after its declaration, the marker recorded every value its
    /// initializer read `timer_off_value` from as the host module, and the
    /// module's proxy is as the pass built it.
    fn off_provenance(&self, decl: Option<(&Ast, u16, u32)>) -> bool {
        let Some((ast, file, d)) = decl else {
            return false;
        };
        let key = (u64::from(file) << 32) | u64::from(d);
        self.an.module_intact
            && self.an.marks.get(&key) == Some(&true)
            && ast.decl(d).is_some_and(|x| !x.assigned)
    }

    /// What a captured table with a metatable is called in a refusal.
    fn metatable(&self, id: u32) -> &'static str {
        let option = self.an.config.and_then(|c| {
            let k = self.an.names.get(&b"option"[..])?;
            match self.an.tables.get(c as usize)?.get(k)? {
                Captured::Table { id, .. } => Some(*id),
                _ => None,
            }
        });
        if self.an.config == Some(id) {
            "skin_config's metatable"
        } else if option == Some(id) {
            "skin_config.option's metatable"
        } else {
            "a table's metatable"
        }
    }

    /// The bytes of literal `s`.
    fn bytes(&self, file: u16, id: u32) -> &'a [u8] {
        self.an.ast(file).map_or(&[][..], |a| a.str(id))
    }

    fn index(&mut self, o: u32, k: u32) -> R<u32> {
        match (self.val(o), self.val(k)) {
            (Some(Val::Module(m)), Some(Val::Str(S::Lit { file, id }))) => {
                let name = self.bytes(file, id);
                if name == b"timer_off_value" {
                    return if m == MAIN_STATE {
                        self.known(Val::OffValue)
                    } else {
                        construct("module member")
                    };
                }
                let module = super::host::MODULES
                    .get(usize::from(m))
                    .copied()
                    .unwrap_or("");
                let api = HOST_APIS
                    .iter()
                    .position(|&(x, n)| x == module && n.as_bytes() == name)
                    .and_then(|i| u16::try_from(i).ok());
                match api {
                    Some(api) => self.known(Val::Host(api)),
                    None => construct("module member"),
                }
            }
            (Some(Val::Table(t)), Some(Val::Str(s))) => {
                let c = match s {
                    S::Lit { file, id } => self.an.entry(t, self.bytes(file, id)).map_err(stop)?,
                    S::Name(n) => {
                        self.an.charge(1).map_err(stop)?;
                        self.an
                            .tables
                            .get(t as usize)
                            .and_then(|m| m.get(&n))
                            .copied()
                    }
                };
                match c {
                    Some(c) => self.captured(c, None),
                    None => refuse(Unsupported::Capture("an entry no path copied")),
                }
            }
            (Some(Val::Nil | Val::Bool(_) | Val::Int(_) | Val::OffValue), _) => {
                construct("a Lua error")
            }
            _ => construct("index"),
        }
    }

    fn call(&mut self, f: &mut Frame<'a>, callee: u32, args: List) -> R<u32> {
        let c = self.expr(f, callee)?;
        let args = f.ast.list(args);
        match self.val(c) {
            Some(Val::Closure(j)) => {
                self.an.charge(1).map_err(stop)?;
                let mut xs = Vec::with_capacity(args.len());
                for &a in args {
                    xs.push(self.expr(f, a)?);
                }
                self.deeper(|ev| ev.function(j, &xs))
            }
            Some(Val::Host(api)) => {
                let Some((arity, _)) = reader(api) else {
                    return refuse(Unsupported::HostApi(api));
                };
                let mut xs = Vec::with_capacity(args.len());
                for &a in args {
                    xs.push(self.expr(f, a)?);
                }
                if xs.len() != arity {
                    return construct("host arguments");
                }
                let arg = match xs.first() {
                    None => None,
                    Some(&x) => match self.val(x) {
                        Some(Val::Int(n)) => Some(n),
                        _ => return construct("host arguments"),
                    },
                };
                let r = self.push(Node::Read { api, arg })?;
                charge(self.an, "read", 1)?;
                did("read");
                self.res.reads.push(r);
                Ok(r)
            }
            Some(Val::Lib(_)) => construct("library call"),
            _ => refuse(Unsupported::Callee),
        }
    }

    /// One visit of a conversion or an operator's distribution, charged.
    fn visit(&self) -> R<()> {
        charge(self.an, "visit", 1)?;
        did("visit");
        Ok(())
    }

    /// Node `v`'s truthiness: a boolean node.
    fn truthy(&mut self, v: u32) -> R<u32> {
        self.visit()?;
        let k = self.kind(v);
        if k == 0 || k == BOOL {
            return Ok(v);
        }
        if k & !TRUTHY == 0 {
            return self.known(Val::Bool(true));
        }
        if k == NIL {
            return self.known(Val::Bool(false));
        }
        let Some((c, a, b)) = self.mixed(v) else {
            return refuse(Unsupported::Type);
        };
        let a = self.truthy(a)?;
        let b = self.truthy(b)?;
        self.select(c, a, b)
    }

    /// What `v`'s truthiness is known to be.
    fn truth_of(&self, v: u32) -> Option<bool> {
        let k = self.kind(v);
        if k == BOOL {
            return match self.val(v) {
                Some(Val::Bool(b)) => Some(b),
                _ => None,
            };
        }
        if k != 0 && k & !TRUTHY == 0 {
            return Some(true);
        }
        (k == NIL).then_some(false)
    }

    /// Node `v` as a number's value: integers kept, nil and booleans 0.
    fn integer(&mut self, v: u32) -> R<u32> {
        self.visit()?;
        let k = self.kind(v);
        if k == 0 || k == INT {
            return Ok(v);
        }
        if k & !(NIL | BOOL) == 0 {
            return self.known(Val::Int(0));
        }
        if let Some((c, a, b)) = self.mixed(v) {
            let a = self.integer(a)?;
            let b = self.integer(b)?;
            return self.select(c, a, b);
        }
        if k == OFF {
            return refuse(Unsupported::Range);
        }
        refuse(Unsupported::Type)
    }

    fn not(&mut self, t: u32) -> R<u32> {
        match self.val(t) {
            Some(Val::Bool(b)) => self.known(Val::Bool(!b)),
            _ if self.kind(t) == 0 => Ok(t),
            _ => self.push(Node::Not(t)),
        }
    }

    fn neg(&mut self, v: u32) -> R<u32> {
        self.visit()?;
        if let Some((c, a, b)) = self.mixed(v) {
            let a = self.neg(a)?;
            let b = self.neg(b)?;
            return self.select(c, a, b);
        }
        match self.kind(v) {
            0 => Ok(v),
            INT => match self.val(v) {
                Some(Val::Int(n)) => self.known(Val::Int(-n)),
                _ => self.push(Node::Neg(v)),
            },
            OFF => refuse(Unsupported::Range),
            STR => refuse(Unsupported::Type),
            _ => construct("a Lua error"),
        }
    }

    fn binary(&mut self, f: &mut Frame<'a>, op: Bin, a: u32, b: u32) -> R<u32> {
        match op {
            Bin::Div => return construct("/"),
            Bin::IDiv => return construct("//"),
            Bin::Mod => return construct("%"),
            Bin::Pow => return construct("^"),
            Bin::Concat => return construct(".."),
            Bin::BAnd | Bin::BOr | Bin::BXor | Bin::Shl | Bin::Shr => return construct("bitwise"),
            _ => {}
        }
        match op {
            Bin::And | Bin::Or => {
                let x = self.expr(f, a)?;
                match (self.truth_of(x), op == Bin::And) {
                    (Some(true), true) | (Some(false), false) => self.expr(f, b),
                    (Some(_), _) => Ok(x),
                    (None, and) => {
                        let y = self.expr(f, b)?;
                        let t = self.truthy(x)?;
                        if and {
                            self.select(t, y, x)
                        } else {
                            self.select(t, x, y)
                        }
                    }
                }
            }
            _ => {
                let x = self.expr(f, a)?;
                let y = self.expr(f, b)?;
                self.op(op, x, y)
            }
        }
    }

    /// Binary operator `op` (arithmetic or a comparison) of `x` and `y`,
    /// distributed into a mixed operand's arms.
    fn op(&mut self, op: Bin, x: u32, y: u32) -> R<u32> {
        self.visit()?;
        if let Some((c, a, b)) = self.mixed(x) {
            let a = self.op(op, a, y)?;
            let b = self.op(op, b, y)?;
            return self.select(c, a, b);
        }
        if let Some((c, a, b)) = self.mixed(y) {
            let a = self.op(op, x, a)?;
            let b = self.op(op, x, b)?;
            return self.select(c, a, b);
        }
        let (kx, ky) = (self.kind(x), self.kind(y));
        if kx == 0 {
            return Ok(x);
        }
        if ky == 0 {
            return Ok(y);
        }
        let ints = match (self.val(x), self.val(y)) {
            (Some(Val::Int(p)), Some(Val::Int(q))) => Some((p, q)),
            _ => None,
        };
        let exact = |n: i64| -> R<i64> {
            if n.unsigned_abs() <= EXACT {
                Ok(n)
            } else {
                refuse(Unsupported::Range)
            }
        };
        match op {
            Bin::Add | Bin::Sub | Bin::Mul => {
                if (kx | ky) & (NIL | BOOL | TABLE | FUNC) != 0 {
                    return construct("a Lua error");
                }
                if (kx | ky) & OFF != 0 {
                    return refuse(Unsupported::Range);
                }
                if (kx | ky) & STR != 0 {
                    return refuse(Unsupported::Type);
                }
                if let Some((p, q)) = ints {
                    let r = match op {
                        Bin::Add => p.checked_add(q),
                        Bin::Sub => p.checked_sub(q),
                        _ => p.checked_mul(q),
                    };
                    let r = exact(r.ok_or(Stop::Unsupported(Unsupported::Range))?)?;
                    return self.known(Val::Int(r));
                }
                self.push(Node::Arith(op, x, y))
            }
            Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge => {
                if (kx | ky) & OFF != 0 {
                    return refuse(Unsupported::Range);
                }
                if kx == STR && ky == STR {
                    return refuse(Unsupported::Type);
                }
                if kx != INT || ky != INT {
                    return construct("a Lua error");
                }
                if let Some((p, q)) = ints {
                    let r = match op {
                        Bin::Lt => p < q,
                        Bin::Le => p <= q,
                        Bin::Gt => p > q,
                        _ => p >= q,
                    };
                    return self.known(Val::Bool(r));
                }
                self.push(Node::Cmp(op, x, y))
            }
            Bin::Eq | Bin::Ne => {
                let eq = op == Bin::Eq;
                if kx == OFF || ky == OFF {
                    let (r, off) = if kx == OFF { (y, x) } else { (x, y) };
                    let timer = matches!(self.res.node(r), Node::Read { api, .. } if is_timer(api));
                    if timer && self.val(off) == Some(Val::OffValue) {
                        return self.push(Node::Cmp(op, r, off));
                    }
                    return refuse(Unsupported::Range);
                }
                if kx != ky {
                    return self.known(Val::Bool(!eq));
                }
                if let (Some(a), Some(b)) = (self.val(x), self.val(y)) {
                    let same = self.equal(a, b)?;
                    return self.known(Val::Bool(same == eq));
                }
                if kx == INT || kx == BOOL {
                    return self.push(Node::Cmp(op, x, y));
                }
                refuse(Unsupported::Type)
            }
            _ => refuse(Unsupported::Type),
        }
    }

    /// Whether known values `a` and `b` are equal in Lua; strings by their
    /// bytes, charged.
    fn equal(&self, a: Val, b: Val) -> R<bool> {
        let (Val::Str(p), Val::Str(q)) = (a, b) else {
            return Ok(a == b);
        };
        match (p, q) {
            (S::Name(m), S::Name(n)) => Ok(m == n),
            (S::Lit { file, id }, S::Name(n)) | (S::Name(n), S::Lit { file, id }) => {
                let s = self.bytes(file, id);
                charge(self.an, "str", units(s.len()))?;
                did("str");
                Ok(self.an.names.get(s) == Some(&n))
            }
            (S::Lit { file: f, id: i }, S::Lit { file: g, id: j }) => {
                let (s, t) = (self.bytes(f, i), self.bytes(g, j));
                charge(self.an, "str", units(s.len().max(t.len())))?;
                did("str");
                Ok(s == t)
            }
        }
    }
}
