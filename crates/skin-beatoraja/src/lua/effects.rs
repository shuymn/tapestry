//! The effect walk (design.md "beatoraja Lua Skins", effects by role):
//! whether every root the configured definition holds stays inside the
//! modeled subset (`Closed`), and which roots write.
//!
//! The walk visits the roots in conversion order, then the closures they
//! call, each function prototype (its file, function and captured values)
//! once. It decides each value's abstract kind, a set over nil, booleans,
//! numbers, strings, tables, functions and unknown values, refined by a
//! captured or fresh table without a metatable, a host module, a host API,
//! a library function or a closure instance, and checks every construct
//! against the subset by every kind a value may have:
//!
//! - statements: `local` without attributes, assignment to a local of the
//!   same function, `return`, call statements, `if`, `while`, `repeat`, a
//!   numeric `for` over numbers, a generic `for` over `pairs(t)`,
//!   `ipairs(t)` or `next, t` with `t` a table without a metatable, `do`,
//!   `break`, `goto` and labels;
//! - expressions: literals, local, upvalue and global reads, indexes of a
//!   table without a metatable or of a host module, table constructors,
//!   parentheses, and calls of a walked closure, a host reader or a pure
//!   library function meeting its condition;
//! - operators: comparisons of numbers or of strings, arithmetic of
//!   numbers, `..` of strings and numbers, `#` of strings and tables
//!   without a metatable, `==` and `~=` unless both operands may be
//!   unknown, and `not`, `and` and `or` of anything; no bitwise operator
//!   and no `//`, which the source's grammar does not have.
//!
//! Anything else opens the analysis at the first place the walk meets it
//! ([`Analysis::open`]), after any place the conversion opened it at. An
//! assignment to anything but a local of the same function, a writer
//! host API, `table.insert`, `table.remove`, `table.sort` and
//! `setmetatable` also make the roots that reach them writers, which
//! never lower. The walk goes on past an open place, so every root's
//! writer bit is known; a cap, cancellation or the time budget stops it,
//! fatal.

use std::collections::{HashMap, HashSet};

use super::callback::{Analysis, Captured, Found, Open, ProtoKey, Site, TOO_DEEP, callable_nodes};
use super::clock::Phase;
use super::host::MODULES;
use super::limits::MAX_AST_DEPTH;
use super::sandbox::LIBS;
use super::syntax::{Ast, Attrib, Bin, CHUNK_ENV, Expr, List, NONE, Stmt, Un};
use super::{HOST_APIS, Unsupported};
use crate::tree::{Arena, CallableKind};

const NIL: u8 = 1;
const BOOL: u8 = 2;
const NUM: u8 = 4;
const STR: u8 = 8;
const TABLE: u8 = 16;
const FN: u8 = 32;
const UNKNOWN: u8 = 64;

/// What a value of a kind is known to be beyond its set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ref {
    None,
    /// A table without a metatable: a captured one by its index, or a
    /// fresh one (`None`).
    Table(Option<u32>),
    Module(u8),
    Host(u16),
    Lib(u16),
    Closure(u32),
}

/// An abstract kind: the set of what a value may be, and a refinement
/// that holds for every member of a one-member set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct K {
    bits: u8,
    r: Ref,
}

impl K {
    const NIL: Self = Self::of(NIL);
    const BOOL: Self = Self::of(BOOL);
    const NUM: Self = Self::of(NUM);
    const STR: Self = Self::of(STR);
    const UNKNOWN: Self = Self::of(UNKNOWN);
    const FRESH: Self = Self {
        bits: TABLE,
        r: Ref::Table(None),
    };

    const fn of(bits: u8) -> Self {
        Self { bits, r: Ref::None }
    }

    fn join(self, o: Self) -> Self {
        let r = if self.r == o.r { self.r } else { Ref::None };
        Self {
            bits: self.bits | o.bits,
            r,
        }
    }

    /// Whether every member is one of `bits`.
    fn only(self, bits: u8) -> bool {
        self.bits != 0 && self.bits & !bits == 0
    }

    fn may_be_unknown(self) -> bool {
        self.bits & UNKNOWN != 0
    }

    /// A table without a metatable, every member.
    fn plain_table(self) -> bool {
        self.bits == TABLE && matches!(self.r, Ref::Table(_))
    }
}

/// A call's results: kinds by position, then the kind of each position
/// past them when their number is not fixed (each may be absent, so nil
/// too).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Tuple {
    fixed: Vec<K>,
    tail: Option<K>,
}

impl Tuple {
    fn one(k: K) -> Self {
        Self {
            fixed: vec![k],
            tail: None,
        }
    }

    fn unknown() -> Self {
        Self {
            fixed: Vec::new(),
            tail: Some(K::UNKNOWN),
        }
    }

    /// The kind at position `n`, nil past the end.
    fn at(&self, n: usize) -> K {
        match self.fixed.get(n) {
            Some(&k) => k,
            None => self.tail.map_or(K::NIL, |t| t.join(K::NIL)),
        }
    }

    fn first(&self) -> K {
        self.at(0)
    }

    /// The join of two result lists, position by position.
    fn join(&self, o: &Self) -> Self {
        let n = self.fixed.len().max(o.fixed.len());
        let fixed = (0..n).map(|k| self.at(k).join(o.at(k))).collect();
        let tail = match (self.tail, o.tail) {
            (Some(a), Some(b)) => Some(a.join(b)),
            (Some(a), None) | (None, Some(a)) => Some(a.join(K::NIL)),
            (None, None) => None,
        };
        Self { fixed, tail }
    }

    /// Every position nil as well: a body that may fall through.
    fn or_nil(&self) -> Self {
        Self {
            fixed: self.fixed.iter().map(|k| k.join(K::NIL)).collect(),
            tail: self.tail.map(|t| t.join(K::NIL)),
        }
    }
}

/// A prototype's walk.
#[derive(Clone, Debug)]
enum State {
    InProgress,
    Done(Tuple),
}

/// The kind of captured value `c`, a table with a metatable `None`.
fn captured(c: Captured) -> Option<K> {
    Some(match c {
        Captured::Nil => K::NIL,
        Captured::Bool(_) => K::BOOL,
        Captured::Int(_) | Captured::Float(_) => K::NUM,
        Captured::Str(_) => K::STR,
        Captured::Host(api) => K {
            bits: FN,
            r: Ref::Host(api),
        },
        Captured::Module(m) => K {
            bits: TABLE,
            r: Ref::Module(m),
        },
        Captured::Closure(j) => K {
            bits: FN,
            r: Ref::Closure(j),
        },
        Captured::Table { meta: true, .. } => return None,
        Captured::Table { id, meta: false } => K {
            bits: TABLE,
            r: Ref::Table(Some(id)),
        },
        Captured::Lib(l) => K {
            bits: FN,
            r: Ref::Lib(l),
        },
        Captured::Other => K::UNKNOWN,
    })
}

/// A host API's class: its results when it reads, `None` when it is
/// outside the subset, with whether it writes.
enum HostClass {
    Reads(Tuple),
    Writes,
    Outside,
}

fn host(api: u16, args: &Tuple) -> HostClass {
    let name = HOST_APIS.get(usize::from(api)).map_or("", |&(_, n)| n);
    match name {
        "option" | "timer_is_on" | "timer_is_off" | "key_pressed" | "is_timer_on"
        | "is_timer_off" => HostClass::Reads(Tuple::one(K::BOOL)),
        "number"
        | "float_number"
        | "timer"
        | "timer_elapsed"
        | "timer_elapsed_ms"
        | "timer_elapsed_seconds"
        | "time"
        | "event_index"
        | "screen_width"
        | "screen_height"
        | "rate"
        | "exscore"
        | "rate_best"
        | "exscore_best"
        | "rate_rival"
        | "exscore_rival"
        | "volume_sys"
        | "volume_key"
        | "volume_bg"
        | "gauge"
        | "gauge_type"
        | "judge"
        | "now_timer" => HostClass::Reads(Tuple::one(K::NUM)),
        // One number an argument.
        "numbers" => HostClass::Reads(Tuple {
            fixed: vec![K::NUM; args.fixed.len()],
            tail: args.tail.map(|_| K::NUM),
        }),
        "text" => HostClass::Reads(Tuple::one(K::STR)),
        "offset" => HostClass::Reads(Tuple::one(K::FRESH)),
        "set_timer" | "event_exec" | "set_volume_sys" | "set_volume_key" | "set_volume_bg"
        | "file_delete" | "file_mkdir" | "file_write" | "file_append" | "file_clear"
        | "open_url" | "http_get_async" | "http_cancel" | "http_get" | "http_get_lines"
        | "audio_play" | "audio_loop" | "audio_preload" | "audio_stop" | "audio_dispose" => {
            HostClass::Writes
        }
        _ => HostClass::Outside,
    }
}

/// The library functions that write: a table's entries or metatable.
const LIB_WRITERS: [&str; 4] = ["table.insert", "table.remove", "table.sort", "setmetatable"];

/// Whether `s`, a `string.format` format, uses only `%s` (strings and
/// numbers), `%d` and `%f` (numbers) and `%%`, with flags, widths and
/// precisions: the kinds each conversion takes, in order, or `None`.
fn format_kinds(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut k = 0;
    while let Some(&c) = s.get(k) {
        k += 1;
        if c != b'%' {
            continue;
        }
        while s
            .get(k)
            .is_some_and(|c| b"-+ #0".contains(c) || c.is_ascii_digit() || *c == b'.')
        {
            k += 1;
        }
        let conv = *s.get(k)?;
        k += 1;
        out.push(match conv {
            b'%' => continue,
            b's' => STR | NUM,
            b'd' | b'f' => NUM,
            _ => return None,
        });
    }
    Some(out)
}

/// One function's walk: its locals' kinds and its results so far.
struct Body<'a> {
    ast: &'a Ast,
    file: u16,
    func: u32,
    inst: u32,
    proto: u32,
    /// The kinds of its locals declared so far.
    locals: HashMap<u32, K>,
    /// Its locals assigned after their declaration.
    reassigned: HashSet<u32>,
    /// The join of its `return` lists.
    ret: Option<Tuple>,
}

/// The walk over every root of an analysis.
pub(crate) struct Walk<'a> {
    an: &'a Analysis,
    /// Each prototype's index, and each instance's.
    protos: HashMap<ProtoKey, u32>,
    proto_of: HashMap<u32, u32>,
    states: Vec<State>,
    /// Whether each prototype's own body writes.
    writes: Vec<bool>,
    /// Calls between prototypes: caller, callee.
    edges: Vec<(u32, u32)>,
    depth: u32,
}

type R<T> = Result<T, Found>;

impl<'a> Walk<'a> {
    pub(crate) fn new(an: &'a Analysis) -> Self {
        Self {
            an,
            protos: HashMap::new(),
            proto_of: HashMap::new(),
            states: Vec::new(),
            writes: Vec::new(),
            edges: Vec::new(),
            depth: 0,
        }
    }

    /// Records the open place `why` at `site`, unless one came first.
    fn open(&self, site: Site, why: Unsupported) {
        if self.an.open.get().is_none() {
            self.an.open.set(Some(Open { site, why }));
        }
    }

    /// Opens at expression or statement line `line` of `b`: the construct
    /// `why` is outside the subset; its value is unknown.
    fn outside(&self, b: &Body<'_>, line: u32, why: Unsupported) -> K {
        self.open(Site::Code { file: b.file, line }, why);
        K::UNKNOWN
    }

    /// [`Self::outside`] for a write: `b`'s prototype writes.
    fn write(&mut self, b: &Body<'_>, line: u32, why: Unsupported) {
        if let Some(w) = self.writes.get_mut(b.proto as usize) {
            *w = true;
        }
        self.outside(b, line, why);
    }

    fn fatal(&self, b: &Body<'_>, line: u32, why: Unsupported) -> Found {
        Found {
            why,
            file: b.file,
            line,
        }
    }

    /// One unit of work and one level deeper: past [`MAX_AST_DEPTH`] the
    /// walk stops, fatal.
    fn enter(&mut self, b: &Body<'_>, line: u32) -> R<()> {
        self.an.charge(1).map_err(|w| self.fatal(b, line, w))?;
        if self.depth >= MAX_AST_DEPTH {
            return Err(self.fatal(b, line, TOO_DEEP));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Walks every root of `arena` in conversion order, each callable
    /// once, then gives each root instance whose reach writes to the
    /// analysis's writers.
    pub(crate) fn roots(mut self, arena: &Arena) -> Result<(), (Option<u32>, Found)> {
        let lua = match arena.lua.as_deref() {
            Some(l) => l,
            None => return Ok(()),
        };
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        for (id, c) in callable_nodes(arena) {
            let unit = |w| Found {
                why: w,
                file: 0,
                line: 0,
            };
            self.an.charge(1).map_err(|w| (Some(id), unit(w)))?;
            let Some(callable) = lua.callables.get(c as usize) else {
                continue;
            };
            if !callable.root || !seen.insert(c) {
                continue;
            }
            // The time budget, checked at each root.
            self.an.clock.root(id).map_err(|w| (Some(id), unit(w)))?;
            if callable.kind != CallableKind::Lua || callable.instance == NONE {
                self.open(Site::Node(id), Unsupported::Capture("not a skin closure"));
                continue;
            }
            self.depth = 0;
            self.instance(callable.instance)
                .map_err(|f| (Some(id), f))?;
            roots.push(callable.instance);
        }
        self.an.clock.clear();
        // Writers: every prototype that calls one, through any chain.
        // Each prototype and call edge one unit, before the index is
        // built.
        let unit = |w| Found {
            why: w,
            file: 0,
            line: 0,
        };
        let edges = u64::try_from(self.edges.len()).unwrap_or(u64::MAX);
        let n = self.writes.len();
        self.an
            .charge(edges.saturating_add(u64::try_from(n).unwrap_or(u64::MAX)))
            .map_err(|w| (None, unit(w)))?;
        let mut callers: Vec<Vec<u32>> = vec![Vec::new(); n];
        for &(from, to) in &self.edges {
            if let Some(c) = callers.get_mut(to as usize) {
                c.push(from);
            }
        }
        let mut pending: Vec<u32> = (0_u32..)
            .zip(&self.writes)
            .filter_map(|(p, &w)| w.then_some(p))
            .collect();
        while let Some(p) = pending.pop() {
            self.an.charge(1).map_err(|w| (None, unit(w)))?;
            for &q in callers
                .get(p as usize)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                // Each call edge one unit, at most once a callee.
                self.an.charge(1).map_err(|w| (None, unit(w)))?;
                if let Some(w) = self.writes.get_mut(q as usize)
                    && !*w
                {
                    *w = true;
                    pending.push(q);
                }
            }
        }
        let n = u64::try_from(roots.len()).unwrap_or(u64::MAX);
        self.an.charge(n).map_err(|w| (None, unit(w)))?;
        let mut writers = self.an.writers.borrow_mut();
        for i in roots {
            if self
                .proto_of
                .get(&i)
                .and_then(|&p| self.writes.get(p as usize))
                .copied()
                .unwrap_or(false)
            {
                writers.insert(i);
            }
        }
        Ok(())
    }

    /// Instance `i` walked, once a prototype: its results, unknown while
    /// it is being walked (a recursion), and its prototype.
    fn instance(&mut self, i: u32) -> R<(Tuple, Option<u32>)> {
        let untagged = Found {
            why: Unsupported::Capture("an untagged closure"),
            file: 0,
            line: 0,
        };
        let p = match self.proto_of.get(&i) {
            Some(&p) => p,
            None => {
                let key = self
                    .an
                    .proto_key(i)
                    .map_err(|why| Found { why, ..untagged })?;
                let Some(key) = key else {
                    // No instance: nothing the walk can read.
                    return Ok((Tuple::unknown(), None));
                };
                let next = u32::try_from(self.states.len()).unwrap_or(u32::MAX);
                let p = *self.protos.entry(key).or_insert(next);
                if p == next {
                    self.states.push(State::InProgress);
                    self.writes.push(false);
                    self.proto_of.insert(i, p);
                    let r = self.body(i, p)?;
                    if let Some(s) = self.states.get_mut(p as usize) {
                        *s = State::Done(r.clone());
                    }
                    return Ok((r, Some(p)));
                }
                self.proto_of.insert(i, p);
                p
            }
        };
        match self.states.get(p as usize) {
            Some(State::Done(t)) => {
                // Its results copied, one unit each.
                let n = u64::try_from(t.fixed.len()).unwrap_or(u64::MAX);
                self.an.charge(n).map_err(|why| Found { why, ..untagged })?;
                Ok((t.clone(), Some(p)))
            }
            _ => Ok((Tuple::unknown(), Some(p))),
        }
    }

    /// The results of instance `i`'s body, prototype `p`.
    fn body(&mut self, i: u32, p: u32) -> R<Tuple> {
        let an = self.an;
        let Some(inst) = an.instance(i).copied() else {
            return Ok(Tuple::unknown());
        };
        let Some(ast) = an.ast(inst.file) else {
            return Ok(Tuple::unknown());
        };
        let Some(f) = ast.func(inst.func) else {
            return Ok(Tuple::unknown());
        };
        let mut b = Body {
            ast,
            file: inst.file,
            func: inst.func,
            inst: i,
            proto: p,
            locals: HashMap::new(),
            reassigned: HashSet::new(),
            ret: None,
        };
        if inst.values.is_none() && f.upvalues.1 > 0 {
            self.outside(&b, f.line, Unsupported::Capture("no reader"));
        }
        self.an
            .charge(u64::from(f.params.1))
            .map_err(|w| self.fatal(&b, f.line, w))?;
        for &d in ast.list(f.params) {
            b.locals.insert(d, K::UNKNOWN);
        }
        self.reassigned(&mut b, f.body)?;
        self.block(&mut b, f.body)?;
        let ret = b.ret.take().unwrap_or_default();
        let always = returns(an, ast, f.body).map_err(|w| self.fatal(&b, f.line, w))?;
        Ok(if always { ret } else { ret.or_nil() })
    }

    /// Collects the locals of `b`'s function that block `l` assigns.
    fn reassigned(&mut self, b: &mut Body<'_>, l: List) -> R<()> {
        let ast = b.ast;
        for &s in ast.list(l) {
            let line = ast.stmt_line(s);
            self.enter(b, line)?;
            let r = self.reassigned_stmt(b, s);
            self.leave();
            r?;
        }
        Ok(())
    }

    fn reassigned_stmt(&mut self, b: &mut Body<'_>, s: u32) -> R<()> {
        let ast = b.ast;
        match ast.stmt(s) {
            Stmt::Assign { targets, .. } => {
                for &t in ast.list(targets) {
                    if let Expr::Local(d) = ast.expr(t) {
                        b.reassigned.insert(d);
                    }
                }
            }
            Stmt::Function { target, .. } => {
                if let Expr::Local(d) = ast.expr(target) {
                    b.reassigned.insert(d);
                }
            }
            Stmt::If(arms) => {
                for arm in ast.list(arms).chunks(3) {
                    if let &[_, s, n] = arm {
                        self.reassigned(b, (s, n))?;
                    }
                }
            }
            Stmt::While { body, .. }
            | Stmt::Repeat { body, .. }
            | Stmt::NumericFor { body, .. }
            | Stmt::GenericFor { body, .. }
            | Stmt::Do(body) => self.reassigned(b, body)?,
            _ => {}
        }
        Ok(())
    }

    fn block(&mut self, b: &mut Body<'_>, l: List) -> R<()> {
        let ast = b.ast;
        for &s in ast.list(l) {
            let line = ast.stmt_line(s);
            self.enter(b, line)?;
            let r = self.stmt(b, s, line);
            self.leave();
            r?;
        }
        Ok(())
    }

    /// Whether `d` is a local of `b`'s function.
    fn own(b: &Body<'_>, d: u32) -> bool {
        d != CHUNK_ENV && b.ast.decl(d).is_some_and(|x| x.func == b.func)
    }

    /// Declares local `d` with kind `k`: unknown when it is assigned again.
    fn declare(b: &mut Body<'_>, d: u32, k: K) {
        let k = if b.reassigned.contains(&d) {
            K::UNKNOWN
        } else {
            k
        };
        b.locals.insert(d, k);
    }

    fn stmt(&mut self, b: &mut Body<'_>, s: u32, line: u32) -> R<()> {
        let ast = b.ast;
        match ast.stmt(s) {
            Stmt::Empty | Stmt::Break | Stmt::Goto(_) | Stmt::Label(_) => {}
            Stmt::Local { names, exprs } => {
                let t = self.list(b, exprs)?;
                for (k, &d) in ast.list(names).iter().enumerate() {
                    if ast.decl(d).is_some_and(|x| x.attrib != Attrib::None) {
                        self.outside(b, line, Unsupported::Construct("local attribute"));
                    }
                    Self::declare(b, d, t.at(k));
                }
            }
            Stmt::Return(list) => {
                let t = self.list(b, list)?;
                // The join of the results so far, one unit a position.
                let n = b
                    .ret
                    .as_ref()
                    .map_or(0, |r| r.fixed.len())
                    .max(t.fixed.len());
                self.an
                    .charge(u64::try_from(n).unwrap_or(u64::MAX))
                    .map_err(|w| self.fatal(b, line, w))?;
                b.ret = Some(match b.ret.take() {
                    Some(r) => r.join(&t),
                    None => t,
                });
            }
            Stmt::Assign { targets, exprs } => {
                for &t in ast.list(targets) {
                    match ast.expr(t) {
                        Expr::Local(d) if Self::own(b, d) => {}
                        Expr::Index { obj, key } => {
                            self.one(b, obj)?;
                            self.one(b, key)?;
                            self.write(b, line, Unsupported::Construct("assignment"));
                        }
                        _ => self.write(b, line, Unsupported::Construct("assignment")),
                    }
                }
                self.list(b, exprs)?;
            }
            Stmt::Call(e) => {
                self.multi(b, e)?;
            }
            Stmt::If(arms) => {
                for arm in ast.list(arms).chunks(3) {
                    if let &[c, s, n] = arm {
                        if c != NONE {
                            self.one(b, c)?;
                        }
                        self.block(b, (s, n))?;
                    }
                }
            }
            Stmt::While { cond, body } => {
                self.one(b, cond)?;
                self.block(b, body)?;
            }
            Stmt::Repeat { body, cond } => {
                self.block(b, body)?;
                self.one(b, cond)?;
            }
            Stmt::NumericFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let mut num = self.one(b, start)?.only(NUM) & self.one(b, limit)?.only(NUM);
                if step != NONE {
                    num &= self.one(b, step)?.only(NUM);
                }
                if !num {
                    self.outside(b, line, Unsupported::Construct("numeric for bounds"));
                }
                Self::declare(b, var, K::NUM);
                self.block(b, body)?;
            }
            Stmt::GenericFor { names, exprs, body } => {
                let modeled = self.header(b, exprs)?;
                if modeled.is_none() {
                    self.outside(b, line, Unsupported::Construct("generic for header"));
                }
                for (k, &d) in ast.list(names).iter().enumerate() {
                    let num = k == 0 && modeled == Some(true);
                    Self::declare(b, d, if num { K::NUM } else { K::UNKNOWN });
                }
                self.block(b, body)?;
            }
            Stmt::Do(body) => self.block(b, body)?,
            Stmt::LocalFunction { decl, .. } => {
                self.outside(b, line, Unsupported::Construct("function"));
                Self::declare(b, decl, K::UNKNOWN);
            }
            Stmt::Function { target, .. } => match ast.expr(target) {
                Expr::Local(d) if Self::own(b, d) => {
                    self.outside(b, line, Unsupported::Construct("function"));
                }
                _ => self.write(b, line, Unsupported::Construct("function")),
            },
        }
        Ok(())
    }

    /// A generic `for`'s header: `Some(true)` for `ipairs(t)`, whose first
    /// variable is a number, `Some(false)` for `pairs(t)` or `next, t[,
    /// nil]`, with `t` a table without a metatable; `None` for any other.
    fn header(&mut self, b: &mut Body<'_>, exprs: List) -> R<Option<bool>> {
        let ast = b.ast;
        let lib = |k: K| match k {
            K {
                bits: FN,
                r: Ref::Lib(l),
            } => LIBS.get(usize::from(l)).copied(),
            _ => None,
        };
        match *ast.list(exprs) {
            [e] => {
                let Expr::Call { callee, args } = ast.expr(e) else {
                    self.multi(b, e)?;
                    return Ok(None);
                };
                let f = self.one(b, callee)?;
                let args = ast.list(args);
                let mut t = Vec::with_capacity(args.len());
                for &a in args {
                    t.push(self.one(b, a)?);
                }
                Ok(match (lib(f), t.as_slice()) {
                    (Some("ipairs"), &[t]) if t.plain_table() => Some(true),
                    (Some("pairs"), &[t]) if t.plain_table() => Some(false),
                    _ => None,
                })
            }
            [n, t] => {
                let (n, t) = (self.one(b, n)?, self.one(b, t)?);
                Ok((lib(n) == Some("next") && t.plain_table()).then_some(false))
            }
            [n, t, z] => {
                let (n, t, z) = (self.one(b, n)?, self.one(b, t)?, ast.expr(z));
                Ok((lib(n) == Some("next") && t.plain_table() && z == Expr::Nil).then_some(false))
            }
            _ => {
                self.list(b, exprs)?;
                Ok(None)
            }
        }
    }

    /// An expression list's results: each truncated to its first value
    /// but the last, which expands.
    fn list(&mut self, b: &mut Body<'_>, l: List) -> R<Tuple> {
        let ids = b.ast.list(l);
        let mut out = Tuple::default();
        for (k, &e) in ids.iter().enumerate() {
            if k + 1 == ids.len() {
                let t = self.multi(b, e)?;
                out.fixed.extend(t.fixed);
                out.tail = t.tail;
            } else {
                out.fixed.push(self.one(b, e)?);
            }
        }
        Ok(out)
    }

    fn one(&mut self, b: &mut Body<'_>, e: u32) -> R<K> {
        Ok(self.multi(b, e)?.first())
    }

    fn multi(&mut self, b: &mut Body<'_>, e: u32) -> R<Tuple> {
        let line = b.ast.expr_line(e);
        self.enter(b, line)?;
        let r = self.expr(b, e, line);
        self.leave();
        r
    }

    fn expr(&mut self, b: &mut Body<'_>, e: u32, line: u32) -> R<Tuple> {
        let ast = b.ast;
        let one = |k| Ok(Tuple::one(k));
        match ast.expr(e) {
            Expr::Nil => one(K::NIL),
            Expr::True | Expr::False => one(K::BOOL),
            Expr::Int(_) | Expr::Float(_) => one(K::NUM),
            Expr::Str(_) => one(K::STR),
            Expr::Vararg => Ok(Tuple::unknown()),
            Expr::Func(_) => one(self.outside(b, line, Unsupported::Construct("function"))),
            Expr::Local(d) => one(self.local(b, d, line)),
            Expr::Global { name, env } => {
                let t = self.local(b, env, line);
                let k = self.index(b, t, Some(ast.str(name)), line)?;
                one(k)
            }
            Expr::Index { obj, key } => {
                let t = self.one(b, obj)?;
                self.one(b, key)?;
                let k = match ast.expr(key) {
                    Expr::Str(s) => Some(ast.str(s)),
                    _ => None,
                };
                let k = self.index(b, t, k, line)?;
                one(k)
            }
            Expr::Call { callee, args } => {
                let f = self.one(b, callee)?;
                let t = self.list(b, args)?;
                self.call(b, f, &t, ast.list(args), line)
            }
            Expr::Method { obj, args, .. } => {
                self.one(b, obj)?;
                self.list(b, args)?;
                self.outside(b, line, Unsupported::Construct("method call"));
                Ok(Tuple::unknown())
            }
            Expr::Paren(x) => one(self.one(b, x)?),
            Expr::Table(items) => {
                for pair in ast.list(items).chunks(2) {
                    if let &[k, v] = pair {
                        if k != NONE {
                            self.one(b, k)?;
                        }
                        self.multi(b, v)?;
                    }
                }
                one(K::FRESH)
            }
            Expr::Unary(op, x) => {
                let k = self.one(b, x)?;
                one(match op {
                    Un::Not => K::BOOL,
                    Un::Neg if k.only(NUM) => K::NUM,
                    Un::Neg => self.outside(b, line, Unsupported::Type),
                    Un::Len
                        if k.only(STR | TABLE)
                            && (k.bits & TABLE == 0 || matches!(k.r, Ref::Table(_))) =>
                    {
                        K::NUM
                    }
                    Un::Len => self.outside(b, line, Unsupported::Construct("#")),
                    Un::BNot => self.outside(b, line, Unsupported::Construct("bitwise")),
                })
            }
            Expr::Binary(op, x, y) => {
                let (p, q) = (self.one(b, x)?, self.one(b, y)?);
                one(self.binary(b, op, p, q, line))
            }
        }
    }

    fn binary(&mut self, b: &Body<'_>, op: Bin, p: K, q: K, line: u32) -> K {
        match op {
            Bin::And | Bin::Or => p.join(q),
            Bin::Eq | Bin::Ne => {
                if p.may_be_unknown() && q.may_be_unknown() {
                    return self.outside(b, line, Unsupported::Type);
                }
                K::BOOL
            }
            Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge => {
                if (p.only(NUM) && q.only(NUM)) || (p.only(STR) && q.only(STR)) {
                    return K::BOOL;
                }
                self.outside(b, line, Unsupported::Type)
            }
            Bin::Add | Bin::Sub | Bin::Mul | Bin::Div | Bin::Mod | Bin::Pow => {
                if p.only(NUM) && q.only(NUM) {
                    return K::NUM;
                }
                self.outside(b, line, Unsupported::Type)
            }
            Bin::Concat => {
                if p.only(STR | NUM) && q.only(STR | NUM) {
                    return K::STR;
                }
                self.outside(b, line, Unsupported::Type)
            }
            Bin::IDiv => self.outside(b, line, Unsupported::Construct("//")),
            Bin::BAnd | Bin::BOr | Bin::BXor | Bin::Shl | Bin::Shr => {
                self.outside(b, line, Unsupported::Construct("bitwise"))
            }
        }
    }

    /// The kind of local `d` read in `b`: its own, or the captured value
    /// of an upvalue.
    fn local(&mut self, b: &Body<'_>, d: u32, line: u32) -> K {
        if Self::own(b, d) {
            return b.locals.get(&d).copied().unwrap_or(K::UNKNOWN);
        }
        match self.an.upvalue(b.inst, d) {
            Some(c) => match captured(c) {
                Some(k) => k,
                None => self.outside(b, line, Unsupported::Capture("a table's metatable")),
            },
            None => K::UNKNOWN,
        }
    }

    /// The kind of `t[k]`, `k` a constant string key or `None`.
    fn index(&mut self, b: &Body<'_>, t: K, k: Option<&[u8]>, line: u32) -> R<K> {
        if t.bits != TABLE {
            return Ok(self.outside(b, line, Unsupported::Construct("index")));
        }
        match (t.r, k) {
            (Ref::Table(Some(id)), Some(k)) => {
                let c = self.an.entry(id, k).map_err(|w| self.fatal(b, line, w))?;
                Ok(match c.map(captured) {
                    Some(Some(k)) => k,
                    Some(None) => {
                        self.outside(b, line, Unsupported::Capture("a table's metatable"))
                    }
                    None => K::UNKNOWN,
                })
            }
            (Ref::Table(_), _) => Ok(K::UNKNOWN),
            (Ref::Module(m), Some(k)) => {
                let module = MODULES.get(usize::from(m)).copied().unwrap_or("");
                if module == "main_state" && k == b"timer_off_value" {
                    return Ok(K::NUM);
                }
                Ok(HOST_APIS
                    .iter()
                    .position(|&(mm, n)| mm == module && n.as_bytes() == k)
                    .and_then(|a| u16::try_from(a).ok())
                    .map_or(K::UNKNOWN, |api| K {
                        bits: FN,
                        r: Ref::Host(api),
                    }))
            }
            (Ref::Module(_), None) => Ok(K::UNKNOWN),
            _ => Ok(self.outside(b, line, Unsupported::Construct("index"))),
        }
    }

    /// A call of `f` with results `args` of the expressions `ids`.
    fn call(&mut self, b: &mut Body<'_>, f: K, args: &Tuple, ids: &[u32], line: u32) -> R<Tuple> {
        if f.bits != FN {
            self.outside(b, line, Unsupported::Callee);
            return Ok(Tuple::unknown());
        }
        match f.r {
            Ref::Closure(j) => {
                self.depth += 1;
                let r = if self.depth > MAX_AST_DEPTH {
                    Err(self.fatal(b, line, TOO_DEEP))
                } else {
                    self.instance(j)
                };
                self.depth -= 1;
                let (t, p) = r?;
                if let Some(p) = p {
                    self.edges.push((b.proto, p));
                }
                Ok(t)
            }
            Ref::Host(api) => match host(api, args) {
                HostClass::Reads(t) => Ok(t),
                HostClass::Writes => {
                    self.write(b, line, Unsupported::HostApi(api));
                    Ok(Tuple::unknown())
                }
                HostClass::Outside => {
                    self.outside(b, line, Unsupported::HostApi(api));
                    Ok(Tuple::unknown())
                }
            },
            Ref::Lib(l) => {
                // Its condition reads each argument and a format's bytes,
                // one unit each.
                let format = match ids.first().map(|&e| b.ast.expr(e)) {
                    Some(Expr::Str(s)) => b.ast.str(s).len(),
                    _ => 0,
                };
                let n = args.fixed.len().saturating_add(format);
                self.an
                    .charge(u64::try_from(n).unwrap_or(u64::MAX))
                    .map_err(|w| self.fatal(b, line, w))?;
                self.lib(b, l, args, ids, line)
            }
            _ => {
                self.outside(b, line, Unsupported::Callee);
                Ok(Tuple::unknown())
            }
        }
    }

    /// A call of library function `l` with results `args` of the
    /// expressions `ids`: its results when it is pure and its condition
    /// holds; a protected call's target walked as a call.
    fn lib(&mut self, b: &mut Body<'_>, l: u16, args: &Tuple, ids: &[u32], line: u32) -> R<Tuple> {
        let name = LIBS.get(usize::from(l)).copied().unwrap_or("");
        if name == "pcall" || name == "xpcall" {
            let targets = if name == "pcall" { 1 } else { 2 };
            let mut ok = args.fixed.len() >= targets;
            for k in 0..targets.min(args.fixed.len()) {
                // `f` gets the arguments after the targets; `xpcall`'s
                // handler, the error.
                let rest = if k == 0 {
                    Tuple {
                        fixed: args.fixed.iter().skip(targets).copied().collect(),
                        tail: args.tail,
                    }
                } else {
                    Tuple::one(K::UNKNOWN)
                };
                let after = if k == 0 {
                    ids.get(targets..).unwrap_or_default()
                } else {
                    &[]
                };
                ok &= self.protected(b, args.at(k), &rest, after, line)?;
            }
            if !ok {
                self.outside(b, line, Unsupported::Construct(name));
                return Ok(Tuple::unknown());
            }
            return Ok(Tuple {
                fixed: vec![K::BOOL],
                tail: Some(K::UNKNOWN),
            });
        }
        match pure(b.ast, name, args, ids) {
            Some(t) => Ok(t),
            None => {
                let why = Unsupported::Construct(name);
                if LIB_WRITERS.contains(&name) {
                    self.write(b, line, why);
                } else {
                    self.outside(b, line, why);
                }
                Ok(Tuple::unknown())
            }
        }
    }

    /// Whether `f`, the target of a protected call with `args`, is a
    /// walked closure, a host reader or a pure library function meeting
    /// its condition; a closure is walked.
    fn protected(
        &mut self,
        b: &mut Body<'_>,
        f: K,
        args: &Tuple,
        ids: &[u32],
        line: u32,
    ) -> R<bool> {
        if f.bits != FN {
            return Ok(false);
        }
        Ok(match f.r {
            Ref::Closure(_) => {
                self.call(b, f, args, ids, line)?;
                true
            }
            Ref::Host(api) => matches!(host(api, args), HostClass::Reads(_)),
            Ref::Lib(l) => {
                let name = LIBS.get(usize::from(l)).copied().unwrap_or("");
                name != "pcall" && name != "xpcall" && pure(b.ast, name, args, ids).is_some()
            }
            _ => false,
        })
    }
}

/// The results of a call of pure library function `name` with results
/// `args` of the expressions `ids`, when its condition holds (every kind
/// each argument may have); `None` for any other call.
fn pure(ast: &Ast, name: &str, args: &Tuple, ids: &[u32]) -> Option<Tuple> {
    let n = args.fixed.len();
    let fixed = args.tail.is_none();
    let arg = |k: usize| args.at(k);
    let all = |bits: u8| {
        args.fixed.iter().all(|k| k.only(bits)) && args.tail.is_none_or(|t| t.only(bits))
    };
    let rest = |from: usize, bits: u8| {
        args.fixed.iter().skip(from).all(|k| k.only(bits)) && args.tail.is_none_or(|t| t.only(bits))
    };
    let literal = |k: usize| match ids.get(k).map(|&e| ast.expr(e)) {
        Some(Expr::Str(s)) => Some(ast.str(s)),
        _ => None,
    };
    let ok = match name {
        "math.floor" | "math.ceil" | "math.abs" => n == 1 && fixed && arg(0).only(NUM),
        "math.max" | "math.min" => n >= 1 && all(NUM),
        "string.len" | "string.upper" | "string.lower" => n == 1 && fixed && arg(0).only(STR),
        "string.sub" => (2..=3).contains(&n) && fixed && arg(0).only(STR) && rest(1, NUM),
        "string.byte" => n >= 1 && arg(0).only(STR) && rest(1, NUM),
        "string.char" => all(NUM),
        "string.find" | "string.match" => {
            (2..=3).contains(&n) && fixed && arg(0).only(STR) && arg(1).only(STR) && rest(2, NUM)
        }
        // A string replacement only: the source's matcher reads a table
        // replacement through its metamethods, the sandbox's raw, and a
        // function replacement runs skin code.
        "string.gsub" => {
            (3..=4).contains(&n)
                && fixed
                && arg(0).only(STR)
                && arg(1).only(STR)
                && arg(2).only(STR)
                && rest(3, NUM)
        }
        "string.format" => literal(0).and_then(format_kinds).is_some_and(|kinds| {
            fixed
                && kinds.len() < n
                && kinds
                    .iter()
                    .enumerate()
                    .all(|(k, &bits)| arg(k + 1).only(bits))
        }),
        "tonumber" => n == 1 && fixed && arg(0).only(STR | NUM),
        "type" => n >= 1,
        "select" => n >= 1 && (arg(0).only(NUM) || literal(0) == Some(b"#")),
        "next" => (1..=2).contains(&n) && fixed && arg(0).plain_table(),
        "error" | "assert" => all(NIL | BOOL | NUM | STR),
        "table.concat" => (1..=2).contains(&n) && fixed && arg(0).plain_table() && rest(1, STR),
        _ => false,
    };
    if !ok {
        return None;
    }
    Some(match name {
        "math.floor" | "math.ceil" | "math.abs" | "math.max" | "math.min" | "string.len" => {
            Tuple::one(K::NUM)
        }
        "string.upper" | "string.lower" | "string.sub" | "string.char" | "string.format"
        | "type" | "table.concat" => Tuple::one(K::STR),
        // Zero or more numbers.
        "string.byte" => Tuple {
            fixed: Vec::new(),
            tail: Some(K::NUM),
        },
        "string.gsub" => Tuple {
            fixed: vec![K::STR, K::NUM],
            tail: None,
        },
        "tonumber" => Tuple::one(K::UNKNOWN),
        "next" => Tuple {
            fixed: vec![K::UNKNOWN, K::UNKNOWN],
            tail: None,
        },
        "error" => Tuple::default(),
        "assert" => args.clone(),
        _ => Tuple::unknown(),
    })
}

/// Whether block `l` always returns: it ends with `return`, with an `if`
/// that has an `else` and whose arms all always return, or with a `do`
/// block that always returns. Each statement it looks at is one unit of
/// `an`'s work; the walk bounded the nesting already.
fn returns(an: &Analysis, ast: &Ast, l: List) -> Result<bool, Unsupported> {
    let mut l = l;
    loop {
        an.charge(1)?;
        let Some(&last) = ast.list(l).last() else {
            return Ok(false);
        };
        match ast.stmt(last) {
            Stmt::Return(_) => return Ok(true),
            Stmt::Do(b) => l = b,
            Stmt::If(arms) => {
                let arms = ast.list(arms);
                if !arms.chunks(3).any(|a| a.first() == Some(&NONE)) {
                    return Ok(false);
                }
                for a in arms.chunks(3) {
                    let &[_, s, n] = a else {
                        return Ok(false);
                    };
                    if !returns(an, ast, (s, n))? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            _ => return Ok(false),
        }
    }
}

/// Walks the roots of `arena`'s analysis as [`Phase::Walk`]: `Err` with
/// the node it was at and the stop for a cap, cancellation or the time
/// budget.
pub(crate) fn walk(an: &Analysis, arena: &Arena) -> Result<(), (Option<u32>, Found)> {
    let unit = |why| Found {
        why,
        file: 0,
        line: 0,
    };
    an.clock
        .enter(Phase::Walk, None)
        .map_err(|w| (None, unit(w)))?;
    let r = Walk::new(an).roots(arena);
    let at = an.clock.retained();
    let left = an.clock.leave();
    r?;
    left.map_err(|w| (at, unit(w)))
}

#[cfg(test)]
mod tests {
    use super::{K, Tuple, format_kinds};

    #[test]
    fn formats_take_strings_and_numbers_only() {
        assert_eq!(format_kinds(b"%s"), Some(vec![super::STR | super::NUM]));
        assert_eq!(
            format_kinds(b"%05.2f %d%%"),
            Some(vec![super::NUM, super::NUM])
        );
        assert_eq!(format_kinds(b"%q"), None);
        assert_eq!(format_kinds(b"%x"), None);
    }

    #[test]
    fn an_empty_tail_pads_with_nil() {
        let t = Tuple {
            fixed: Vec::new(),
            tail: Some(K::NUM),
        };
        assert_eq!(t.first(), K::NUM.join(K::NIL));
        assert!(!t.first().only(super::NUM));
    }
}
