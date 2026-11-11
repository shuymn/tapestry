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
//!   in magnitude, floats (by their bits), strings (a captured one by its interned id, a literal
//!   by its bytes, compared by bytes), tables without a metatable,
//!   closures, host modules, host functions and library functions; and
//!   the host's off value ([`Val::OffValue`]).
//! - **What folds**: captures (upvalues, and globals through an `_ENV`
//!   without a metatable), table entries along the static paths
//!   ([`Analysis::entry`]), `skin_config` included, so the configuration
//!   folds whatever its kind; module members; locals; helper
//!   calls, inlined with their arguments; operators on known operands;
//!   `if`, `elseif` and `else` with a known condition. Each capture read
//!   holds only while the candidate is closed and its binding cell is
//!   never written at frame time ([`Unsupported::Interference`]
//!   otherwise).
//! - **Integers follow the source's doubles**: a numeral the parser
//!   records as wide ([`super::syntax::Ast::wide_numerals`]) and a
//!   captured integer past 2^53 in magnitude do not fold
//!   ([`Unsupported::Range`]), but a captured `i64::MIN` read from a
//!   never-assigned local whose every initialization the provenance marker
//!   recorded as the host module's `timer_off_value`
//!   ([`super::instrument`]), with the module's proxy as the pass built it,
//!   which is the off value. Every folded integer stays within 2^53: an
//!   integer result past it folds to the source's double, as the program
//!   computes it ([`skin_model::present::num`]). Values construction
//!   computed are taken as the source's, under the construction premise,
//!   whatever arithmetic gave them.
//! - **What stays residual**: host reads with a known integer argument
//!   ([`Node::Read`], each recorded in [`Residual::reads`] as it is
//!   created on a reached path), `main_state.time()` ([`Node::StateUs`],
//!   the program's own read, no mapping's), a timer root's slots
//!   ([`Node::Slot`], below), `-`, `not`, `+ - * / // %` of numbers,
//!   `math.floor`, `ceil`, `abs`, `max` and `min` of numbers
//!   ([`Node::Math`]), comparisons, and [`Node::Select`]; known operands
//!   fold through the program's numerics (the source's, LuaJ's). `A and B` is `Select(A, B, A)`
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
//!   loops, `goto`, function constructors, `^`, bitwise operators, `..`,
//!   `#`, library calls but the five `math` functions, method calls, table
//!   constructors, varargs, `return` lists, recursion, a host argument that
//!   is not a known integer, a definite Lua error on a reached path, and
//!   string arithmetic or ordering.
//!
//! - **Slots**: a timer root evaluated with its slot cells
//!   ([`evaluate`]'s `cells`, decided by the caller) reads each as
//!   [`Node::Slot`] and assigns it as a local of its own body: its final
//!   values are [`Residual::stores`], every return path's cell values
//!   carried with the return as a local's are, so a store survives an early
//!   return. A slot compares with `==` and `~=` only; arithmetic and order
//!   on one are refused as the off value's are. A helper reaches no slot:
//!   one that reads the cell meets a written cell ([`Unsupported::
//!   Interference`]), one that assigns it an assignment outside its locals.
//!
//! The callback's result is then converted as its consumer converts it
//! ([`Conv`]): a `draw` or `op` by truthiness, a number's `value` or an
//! image set's as an integer (nil and booleans 0), distributed into a
//! [`Node::Select`]'s arms; a text's `value` as it is, for the lowering to
//! read its shape. For a text alone, the scene's constants fold (texts 1001
//! to 1003, options 290 and 1008, recording no read) and a string read
//! compares with `""`.
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
use skin_model::present::{self, num};

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
pub(crate) const FLT: u8 = 128;
/// The number kinds.
pub(crate) const NUM: u8 = INT | FLT;
/// The kinds whose every value is true.
const TRUTHY: u8 = INT | FLT | OFF | STR | TABLE | FUNC;

/// A string known at import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum S {
    /// A captured one, by its index among [`Analysis::names`].
    Name(u32),
    /// A literal: its file and its index in that file's strings.
    Lit { file: u16, id: u32 },
    /// The empty string of a scene constant.
    Empty,
}

/// A value known at import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Val {
    Nil,
    Bool(bool),
    /// At most [`EXACT`] in magnitude.
    Int(i64),
    /// A float, by its bits.
    Float(u64),
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
            Self::Float(_) => FLT,
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
    /// `-` of a number.
    Neg(u32),
    /// `+ - * / // %` of numbers.
    Arith(Bin, u32, u32),
    /// A `math` function of one number (both operands the same), or `max`
    /// or `min` of two.
    Math(MathOp, u32, u32),
    /// A comparison: integers, booleans (`==` and `~=`), a timer read
    /// against the off value (`==` and `~=`), or, for a text's value, a
    /// string read against `""` (`==` and `~=`).
    Cmp(Bin, u32, u32),
    /// `then` where the boolean `cond` holds, else `other`.
    Select {
        cond: u32,
        then: u32,
        other: u32,
    },
    /// A value no path gives: a return that did not happen.
    Unreachable,
    /// The frame's state clock, `main_state.time()`: µs, an integer within
    /// ±2^53 and past it the double the source holds.
    StateUs,
    /// A slot's value as the invocation begins: the off value or an
    /// integer.
    Slot(u16),
}

/// A `math` function the residual keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MathOp {
    Floor,
    Ceil,
    Abs,
    Max,
    Min,
}

/// A known value as the program's numerics take it.
pub(crate) fn num_val(v: Val) -> Option<present::Val> {
    Some(match v {
        Val::Nil => present::Val::Nil,
        Val::Bool(b) => present::Val::Bool(b),
        Val::Int(n) => present::Val::Int(n),
        Val::Float(f) => present::Val::Float(f64::from_bits(f)),
        _ => return None,
    })
}

/// A value the program's numerics gave, as a known value: an integer is
/// within [`EXACT`], a result past it the source's double.
fn known_of(v: present::Val) -> Val {
    match v {
        present::Val::Nil => Val::Nil,
        present::Val::Bool(b) => Val::Bool(b),
        present::Val::Int(n) => Val::Int(n),
        present::Val::Float(f) => Val::Float(f.to_bits()),
    }
}

/// A numeric operation's failure: a definite Lua error (an unavailable
/// read is never folded).
fn failed<T>(f: present::Fail) -> R<T> {
    match f {
        present::Fail::Error => construct("a Lua error"),
        present::Fail::Unavailable => refuse(Unsupported::Range),
    }
}

/// How the consumer converts the callback's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Conv {
    /// A `draw` or an `op` item: Lua's truthiness.
    Truthy,
    /// A number's `value`: an integer, nil and booleans 0.
    Int,
    /// A text's `value`: left as it is, its shape the lowering's to read.
    Text,
    /// An image set's `value`: as [`Self::Int`], and a root that is exactly
    /// a `gauge_type()` read selects by the gauge's type.
    Index,
    /// A destination's `timer`: left as it is, a number, nil, a boolean or
    /// the off value, for the program's `tolong()`.
    Timer,
    /// A slider's `value`: numbers kept, nil and booleans 0, a string known
    /// here folded to the double the source's `tofloat()` rounds
    /// ([`super::num_scan::todouble`]), for the program's `tofloat()`.
    Float,
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
/// work. For a test, the site records what the real charge added to the
/// analysis's work, and can be made to meet an exhausted allowance at
/// its first nonempty charge, so the real check refuses it.
pub(crate) fn charge(an: &Analysis, site: &'static str, n: u64) -> R<()> {
    #[cfg(test)]
    let before = {
        if n > 0 && super::tests::fail_at(site) {
            an.work.set(super::limits::MAX_ANALYSIS_WORK);
        }
        an.work.get()
    };
    #[cfg(not(test))]
    let _ = site;
    let r = an.charge(n).map_err(stop);
    #[cfg(test)]
    super::tests::site_charged(site, an.work.get().saturating_sub(before));
    r
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
    /// Each slot's value as the invocation ends, by slot: what it stores.
    pub(crate) stores: Vec<(u16, u32)>,
}

impl Residual {
    pub(crate) fn node(&self, r: u32) -> Node {
        self.nodes
            .get(r as usize)
            .copied()
            .unwrap_or(Node::Unreachable)
    }

    /// The number of nodes.
    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.nodes.len()).unwrap_or(u32::MAX)
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
        ("main_state", "number" | "timer" | "event_index" | "judge") => Some((1, INT)),
        ("main_state", "option") => Some((1, BOOL)),
        ("main_state", "text") => Some((1, STR)),
        ("main_state", "gauge_type") => Some((0, INT)),
        _ => None,
    }
}

/// What host read `api` of `arg` answers when the scene holds it constant:
/// the table texts and the course and table options.
fn scene_constant(api: u16, arg: Option<i64>) -> Option<Val> {
    let id = i32::try_from(arg?).ok()?;
    match HOST_APIS.get(usize::from(api)).copied()? {
        ("main_state", "text") => super::facts::scene_text(id).then_some(Val::Str(S::Empty)),
        ("main_state", "option") => super::facts::scene_option(id).map(Val::Bool),
        _ => None,
    }
}

/// Whether host API `api` is `main_state.text`.
pub(crate) fn is_text(api: u16) -> bool {
    HOST_APIS.get(usize::from(api)) == Some(&("main_state", "text"))
}

/// Whether host API `api` is `main_state.timer`.
pub(crate) fn is_timer(api: u16) -> bool {
    HOST_APIS.get(usize::from(api)) == Some(&("main_state", "timer"))
}

/// Whether host API `api` is `main_state.time`.
fn is_time(api: u16) -> bool {
    HOST_APIS.get(usize::from(api)) == Some(&("main_state", "time"))
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
/// locals in scope, the root's slot cells first.
struct Frame<'a> {
    ast: &'a Ast,
    inst: u32,
    file: u16,
    env: Env,
    /// How many of `env`'s first entries are slot cells: the root's, else
    /// none.
    cells: usize,
}

struct Eval<'a> {
    /// Where the first proof this evaluation needed was voided by a
    /// frame-time write, carried with its refusal.
    interfered: &'a std::cell::Cell<Option<super::callback::Site>>,
    an: &'a Analysis,
    cancelled: &'a dyn Fn() -> bool,
    res: Residual,
    depth: u32,
    /// The instances being inlined, outermost first.
    stack: Vec<u32>,
    /// The consumer is a text's value ([`Conv::Text`]).
    text: bool,
    /// The root's slot cells, by declaration of its file, each to its slot.
    cells: &'a [(u32, u16)],
    /// The slot cells' values on the paths that returned, while one may
    /// have.
    ret: Option<Vec<u32>>,
}

/// Instance `inst` of admitted analysis `adm` evaluated into its
/// residual, converted by `conv`; `cancelled` seen at each inlined
/// function. `cells` are its slot cells, declarations of its file it
/// captures, each with its slot; only a timer root has any.
pub(crate) fn evaluate(
    adm: Admitted<'_>,
    inst: u32,
    conv: Conv,
    cells: &[(u32, u16)],
    cancelled: &dyn Fn() -> bool,
    interfered: &std::cell::Cell<Option<super::callback::Site>>,
) -> R<Residual> {
    let mut ev = Eval {
        interfered,
        an: adm.analysis(),
        cancelled,
        res: Residual::default(),
        depth: 0,
        stack: Vec::new(),
        text: conv == Conv::Text,
        cells,
        ret: None,
    };
    let v = ev.function(inst, &[])?;
    let root = match conv {
        Conv::Truthy => ev.truthy(v)?,
        Conv::Int | Conv::Index => ev.integer(v)?,
        Conv::Float => ev.float(v)?,
        Conv::Text => v,
        Conv::Timer => {
            if ev.kind(v) & !(NIL | BOOL | NUM | OFF) != 0 {
                return refuse(Unsupported::Type);
            }
            v
        }
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
            // An integer within ±2^53, past it a float (the source's double).
            Node::StateUs => (INT | FLT, 1),
            Node::Slot(_) => (INT | OFF, 1),
            Node::Not(x) => (BOOL, h(x)),
            Node::Neg(x) => (k(x) & NUM, h(x)),
            Node::Arith(op, a, b) => {
                let (ka, kb) = (k(a), k(b));
                let kind = if op == Bin::Div {
                    FLT
                } else if op == Bin::Mod {
                    // The source's `%` by a zero integer is NaN.
                    (if ka & kb & INT != 0 { INT | FLT } else { 0 }) | ((ka | kb) & FLT)
                } else {
                    (if ka & kb & INT != 0 { INT } else { 0 }) | ((ka | kb) & FLT)
                };
                (kind, h(a).max(h(b)))
            }
            Node::Math(f, a, b) => {
                let ka = k(a);
                let kind = match f {
                    MathOp::Floor | MathOp::Ceil => {
                        (if ka & NUM != 0 { INT } else { 0 }) | (ka & FLT)
                    }
                    MathOp::Abs => ka & NUM,
                    MathOp::Max | MathOp::Min => (ka | k(b)) & NUM,
                };
                (kind, h(a).max(h(b)))
            }
            Node::Cmp(_, a, b) => (BOOL, h(a).max(h(b))),
            Node::Select { cond, then, other } => {
                (k(then) | k(other), h(cond).max(h(then)).max(h(other)))
            }
        };
        let height = match node {
            Node::Known(_)
            | Node::Read { .. }
            | Node::Unreachable
            | Node::StateUs
            | Node::Slot(_) => height,
            _ => height.saturating_add(1),
        };
        // Past R4's nesting, refused as it is made. A test can lower the
        // nesting to 2, so the real check refuses the first node above a
        // leaf's parent.
        #[cfg(test)]
        let nesting = if super::tests::fail_at("height") {
            2
        } else {
            skin_model::MAX_NESTING
        };
        #[cfg(not(test))]
        let nesting = skin_model::MAX_NESTING;
        if usize::from(height) > nesting {
            return refuse(LIMITS);
        }
        // For a test, the expanded count can be exhausted, so the real
        // check refuses this node, and records what the node added.
        #[cfg(test)]
        let before = {
            if super::tests::fail_at("node") {
                self.an.expanded.set(MAX_EXPANDED_NODES);
            }
            self.an.expanded.get()
        };
        let total = self.an.expanded.get().saturating_add(1);
        if total > MAX_EXPANDED_NODES {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::Expansion)));
        }
        self.an.expanded.set(total);
        #[cfg(test)]
        super::tests::site_charged("node", self.an.expanded.get().saturating_sub(before));
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

    /// What a proof resting on the captured binding cell `d` of `file`
    /// needs: the candidate closed and the cell never written at frame
    /// time.
    fn bound(&self, file: u16, d: u32) -> R<()> {
        if !self.an.cell_holds(file, d) {
            // The first failure ends the evaluation: its place is the one.
            self.interfered.set(self.an.written_at(file, d));
            return refuse(Unsupported::Interference);
        }
        Ok(())
    }

    /// Runs `f` one level deeper: past [`MAX_AST_DEPTH`] fatal.
    fn deeper<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        // A test can put the walk at the cap, so the real check refuses.
        #[cfg(test)]
        if super::tests::fail_at("depth") {
            self.depth = MAX_AST_DEPTH;
        }
        if self.depth >= MAX_AST_DEPTH {
            return Err(Stop::Fatal(LuaWhy::Limit(LuaLimit::AstDepth)));
        }
        charge(self.an, "depth", 1)?;
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
        if !self.stack.is_empty() {
            did("scan");
        }
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
        // The root's slot cells, each read as its slot.
        let cells = if self.stack.is_empty() {
            self.cells
        } else {
            &[]
        };
        charge(
            self.an,
            "bind",
            units(params.len().max(args.len()) + cells.len()),
        )?;
        if !params.is_empty() || !args.is_empty() || !cells.is_empty() {
            did("bind");
        }
        let mut env = Vec::with_capacity(params.len() + cells.len());
        for &(d, k) in cells {
            let v = self.push(Node::Slot(k))?;
            env.push((d, v));
        }
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
            cells: cells.len(),
        };
        self.stack.push(j);
        let mut done = Done::No;
        let r = self.block(&mut frame, f.body, &mut done);
        self.stack.pop();
        r?;
        if frame.cells > 0 {
            // Each slot's final value: the returned paths' where one
            // returned, the end's elsewhere.
            let ret = self.ret.take().unwrap_or_default();
            for (i, &(_, k)) in cells.iter().enumerate() {
                let end = frame.env.get(i).map_or(0, |&(_, v)| v);
                let returned = ret.get(i).copied();
                let v = match (done, returned) {
                    (Done::No, _) | (_, None) => end,
                    (Done::Yes(_), Some(r)) => r,
                    (Done::Maybe(c, _), Some(r)) => self.select(c, r, end)?,
                };
                self.res.stores.push((k, v));
            }
        }
        match done {
            Done::No => self.known(Val::Nil),
            Done::Yes(v) => Ok(v),
            Done::Maybe(c, v) => {
                let nil = self.known(Val::Nil)?;
                self.select(c, v, nil)
            }
        }
    }

    /// A block, one level deeper: statements nest as expressions do.
    fn block(&mut self, f: &mut Frame<'a>, body: List, done: &mut Done) -> R<()> {
        self.deeper(|ev| ev.block_in(f, body, done))
    }

    fn block_in(&mut self, f: &mut Frame<'a>, body: List, done: &mut Done) -> R<()> {
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
                // Each name and value, charged before the attributes are
                // scanned and the values buffered.
                charge(self.an, "list:local", units(ns.len() + es.len()))?;
                did("list:local");
                for &d in ns {
                    if ast.decl(d).is_some_and(|x| x.attrib == Attrib::Close) {
                        return construct("<close>");
                    }
                }
                let vals = self.values(f, es, ns.len())?;
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
                charge(
                    self.an,
                    "list:assign",
                    units(ts.len() + ast.list(exprs).len()),
                )?;
                did("list:assign");
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
                let vals = self.values(f, ast.list(exprs), slots.len())?;
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
                if f.cells > 0 {
                    // The slot cells as this path returns: joined with the
                    // paths that returned before it.
                    let now: Vec<u32> = f.env.iter().take(f.cells).map(|&(_, v)| v).collect();
                    self.ret = Some(match (*done, self.ret.take()) {
                        (Done::Maybe(c, _), Some(before)) => {
                            let mut out = Vec::with_capacity(now.len());
                            for (&b, &n) in before.iter().zip(&now) {
                                out.push(self.select(c, b, n)?);
                            }
                            out
                        }
                        _ => now,
                    });
                }
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
        // The residual arms: condition, locals, return state and returned
        // slot cells after.
        let mut taken: Vec<(u32, Env, Done, Option<Vec<u32>>)> = Vec::new();
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
                    let before = (f.env.clone(), *done, self.ret.clone());
                    self.block(f, (s, n), done)?;
                    let env = std::mem::replace(&mut f.env, before.0);
                    let after = std::mem::replace(done, before.1);
                    let ret = std::mem::replace(&mut self.ret, before.2);
                    taken.push((cond, env, after, ret));
                }
            }
        }
        while let Some((cond, env, after, ret)) = taken.pop() {
            let rest = self.ret.take();
            self.ret = self.merge_cells(cond, ret, rest)?;
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

    /// The returned slot cells `a` where `cond` holds and `b` elsewhere,
    /// joined; a side where no path returned has none.
    fn merge_cells(
        &mut self,
        cond: u32,
        a: Option<Vec<u32>>,
        b: Option<Vec<u32>>,
    ) -> R<Option<Vec<u32>>> {
        let (a, b) = match (a, b) {
            (None, None) => return Ok(None),
            (a, b) => (a, b),
        };
        let n = self.cells.len();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let side = |ev: &mut Self, s: &Option<Vec<u32>>| -> R<u32> {
                match s.as_ref().and_then(|v| v.get(i)) {
                    Some(&v) => Ok(v),
                    None => ev.push(Node::Unreachable),
                }
            };
            let x = side(self, &a)?;
            let y = side(self, &b)?;
            out.push(self.select(cond, x, y)?);
        }
        Ok(Some(out))
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
            Expr::Float(bits) => self.known(Val::Float(bits)),
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
        self.bound(f.file, d)?;
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
        self.bound(f.file, env)?;
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
            // The source builds a numeric -0.0 as integer zero
            // (`LuaValue.valueOf(double)`): its sign is not kept.
            Captured::Float(bits) if f64::from_bits(bits) == 0.0 => Val::Float(0.0_f64.to_bits()),
            Captured::Float(bits) => Val::Float(bits),
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
                    S::Empty => self.an.entry(t, b"").map_err(stop)?,
                };
                match c {
                    Some(c) => self.captured(c, None),
                    None => refuse(Unsupported::Capture("an entry no path copied")),
                }
            }
            (Some(Val::Nil | Val::Bool(_) | Val::Int(_) | Val::Float(_) | Val::OffValue), _) => {
                construct("a Lua error")
            }
            _ => construct("index"),
        }
    }

    /// Expressions `es` evaluated left to right, every one of them, the
    /// first `keep` values kept; the list's length already charged.
    fn values(&mut self, f: &mut Frame<'a>, es: &[u32], keep: usize) -> R<Vec<u32>> {
        let mut vals = Vec::with_capacity(keep.min(es.len()));
        for (k, &e) in es.iter().enumerate() {
            let v = self.expr(f, e)?;
            if k < keep {
                vals.push(v);
            }
        }
        Ok(vals)
    }

    fn call(&mut self, f: &mut Frame<'a>, callee: u32, args: List) -> R<u32> {
        let c = self.expr(f, callee)?;
        let args = f.ast.list(args);
        match self.val(c) {
            Some(Val::Closure(j)) => {
                self.an.charge(1).map_err(stop)?;
                charge(self.an, "list:call", units(args.len()))?;
                did("list:call");
                let xs = self.values(f, args, args.len())?;
                self.deeper(|ev| ev.function(j, &xs))
            }
            Some(Val::Host(api)) if is_time(api) => {
                if !args.is_empty() {
                    return construct("host arguments");
                }
                self.push(Node::StateUs)
            }
            Some(Val::Host(api)) => {
                let Some((arity, _)) = reader(api) else {
                    return refuse(Unsupported::HostApi(api));
                };
                charge(self.an, "list:host", units(args.len()))?;
                did("list:host");
                let xs = self.values(f, args, args.len())?;
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
                if self.text
                    && let Some(known) = scene_constant(api, arg)
                {
                    return self.known(known);
                }
                let r = self.push(Node::Read { api, arg })?;
                charge(self.an, "read", 1)?;
                did("read");
                self.res.reads.push(r);
                Ok(r)
            }
            Some(Val::Lib(l)) => self.library(f, l, args),
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
        // A number stays: a float is the program's to convert.
        if k & !NUM == 0 {
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

    /// Node `v` as a slider's value: numbers kept, nil and booleans 0, a
    /// known string as the double `tofloat()` rounds (a −0 kept, as the
    /// source's string gives one); the off value out of range, anything
    /// else (a table, a function) refused, as for a number's value.
    fn float(&mut self, v: u32) -> R<u32> {
        self.visit()?;
        let k = self.kind(v);
        if k & !NUM == 0 {
            return Ok(v);
        }
        if k & !(NIL | BOOL) == 0 {
            return self.known(Val::Int(0));
        }
        if let Some(Val::Str(s)) = self.val(v) {
            let d = match s {
                S::Empty => super::num_scan::todouble(b""),
                S::Lit { file, id } => {
                    let b = self.bytes(file, id);
                    charge(self.an, "str", units(b.len()))?;
                    did("str");
                    super::num_scan::todouble(b)
                }
                S::Name(n) => {
                    charge(self.an, "str", units(self.an.names.len()))?;
                    did("str");
                    let b = self
                        .an
                        .names
                        .iter()
                        .find(|&(_, &m)| m == n)
                        .map(|(b, _)| &**b);
                    let Some(b) = b else {
                        return refuse(Unsupported::Type);
                    };
                    charge(self.an, "str", units(b.len()))?;
                    super::num_scan::todouble(b)
                }
            };
            return self.known(Val::Float(d.to_bits()));
        }
        if let Some((c, a, b)) = self.mixed(v) {
            let a = self.float(a)?;
            let b = self.float(b)?;
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
            k if k & !NUM == 0 => match self.val(v).and_then(num_val) {
                Some(x) => {
                    let r = num::neg(x).or_else(failed)?;
                    self.known(known_of(r))
                }
                None => self.push(Node::Neg(v)),
            },
            OFF => refuse(Unsupported::Range),
            STR => refuse(Unsupported::Type),
            _ => construct("a Lua error"),
        }
    }

    fn binary(&mut self, f: &mut Frame<'a>, op: Bin, a: u32, b: u32) -> R<u32> {
        match op {
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
        // Both operands known numbers: what the program's numerics give.
        let nums = match (self.val(x).and_then(num_val), self.val(y).and_then(num_val)) {
            (Some(p), Some(q)) if kx & !NUM == 0 && ky & !NUM == 0 => Some((p, q)),
            _ => None,
        };
        match op {
            Bin::Add | Bin::Sub | Bin::Mul | Bin::Div | Bin::IDiv | Bin::Mod => {
                if (kx | ky) & (NIL | BOOL | TABLE | FUNC) != 0 {
                    return construct("a Lua error");
                }
                if (kx | ky) & OFF != 0 {
                    return refuse(Unsupported::Range);
                }
                if (kx | ky) & STR != 0 {
                    return refuse(Unsupported::Type);
                }
                if let Some((p, q)) = nums {
                    let r = num::arith(arith(op), p, q).or_else(failed)?;
                    return self.known(known_of(r));
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
                if kx & !NUM != 0 || ky & !NUM != 0 {
                    return construct("a Lua error");
                }
                if let Some((p, q)) = nums {
                    let r = match op {
                        Bin::Lt => num::compare(present::Cmp::Lt, p, q),
                        Bin::Le => num::compare(present::Cmp::Le, p, q),
                        Bin::Gt => num::compare(present::Cmp::Lt, q, p),
                        _ => num::compare(present::Cmp::Le, q, p),
                    }
                    .or_else(failed)?;
                    return self.known(Val::Bool(r));
                }
                self.push(Node::Cmp(op, x, y))
            }
            Bin::Eq | Bin::Ne => {
                let eq = op == Bin::Eq;
                // A slot against the off value, an integer or a slot: the
                // program compares them as the source's doubles.
                let slot = |r: u32| matches!(self.res.node(r), Node::Slot(_));
                let comparable = |ev: &Self, r: u32| {
                    slot(r) || matches!(ev.val(r), Some(Val::OffValue | Val::Int(_)))
                };
                if (slot(x) || slot(y)) && comparable(self, x) && comparable(self, y) {
                    return self.push(Node::Cmp(op, x, y));
                }
                if kx == OFF || ky == OFF {
                    let (r, off) = if kx == OFF { (y, x) } else { (x, y) };
                    let timer = matches!(self.res.node(r), Node::Read { api, .. } if is_timer(api));
                    if timer && self.val(off) == Some(Val::OffValue) {
                        return self.push(Node::Cmp(op, r, off));
                    }
                    return refuse(Unsupported::Range);
                }
                let numbers = kx & !NUM == 0 && ky & !NUM == 0;
                if kx != ky && !numbers {
                    return self.known(Val::Bool(!eq));
                }
                if let (Some(a), Some(b)) = (self.val(x), self.val(y)) {
                    let same = self.equal(a, b)?;
                    return self.known(Val::Bool(same == eq));
                }
                if numbers || kx == BOOL {
                    return self.push(Node::Cmp(op, x, y));
                }
                if self.text && kx == STR {
                    let (read, other) = if self.val(y).is_some() {
                        (x, y)
                    } else {
                        (y, x)
                    };
                    if matches!(self.res.node(read), Node::Read { .. })
                        && let Some(Val::Str(s)) = self.val(other)
                        && self.is_empty(s)
                    {
                        return self.push(Node::Cmp(op, read, other));
                    }
                }
                refuse(Unsupported::Type)
            }
            _ => refuse(Unsupported::Type),
        }
    }

    /// A call of library function `l` with `args`: `math.floor`, `ceil`
    /// and `abs` of one number, `max` and `min` of one or more, folded
    /// when every argument is known; any other a refusal.
    fn library(&mut self, f: &mut Frame<'a>, l: u16, args: &[u32]) -> R<u32> {
        let name = super::sandbox::LIBS
            .get(usize::from(l))
            .copied()
            .unwrap_or("");
        let op = match name {
            "math.floor" => MathOp::Floor,
            "math.ceil" => MathOp::Ceil,
            "math.abs" => MathOp::Abs,
            "math.max" => MathOp::Max,
            "math.min" => MathOp::Min,
            _ => return construct("library call"),
        };
        charge(self.an, "list:lib", units(args.len()))?;
        did("list:lib");
        let xs = self.values(f, args, args.len())?;
        let one = matches!(op, MathOp::Floor | MathOp::Ceil | MathOp::Abs);
        if xs.is_empty() || (one && xs.len() != 1) {
            return construct("library call");
        }
        for &x in &xs {
            self.visit()?;
            if self.kind(x) & !NUM != 0 {
                return if self.mixed(x).is_some() {
                    refuse(Unsupported::Type)
                } else {
                    construct("a Lua error")
                };
            }
        }
        let mut acc = *xs.first().ok_or(Stop::Unsupported(Unsupported::Type))?;
        if one {
            if let Some(v) = self.val(acc).and_then(num_val) {
                let fun = match op {
                    MathOp::Floor => present::MathFn::Floor,
                    MathOp::Ceil => present::MathFn::Ceil,
                    _ => present::MathFn::Abs,
                };
                let r = known_of(num::math(fun, v).or_else(failed)?);
                return self.known(r);
            }
            return self.push(Node::Math(op, acc, acc));
        }
        for &x in xs.get(1..).unwrap_or(&[]) {
            acc = match (
                self.val(acc).and_then(num_val),
                self.val(x).and_then(num_val),
            ) {
                (Some(a), Some(b)) => {
                    let r = num::extreme(op == MathOp::Max, a, b).or_else(failed)?;
                    self.known(known_of(r))?
                }
                _ => self.push(Node::Math(op, acc, x))?,
            };
        }
        Ok(acc)
    }

    /// Whether known string `s` is empty.
    fn is_empty(&self, s: S) -> bool {
        match s {
            S::Empty => true,
            S::Lit { file, id } => self.bytes(file, id).is_empty(),
            S::Name(n) => self.an.names.get(&b""[..]) == Some(&n),
        }
    }

    /// Whether known values `a` and `b` are equal in Lua; strings by their
    /// bytes, charged.
    fn equal(&self, a: Val, b: Val) -> R<bool> {
        let (Val::Str(p), Val::Str(q)) = (a, b) else {
            if let (Some(x), Some(y)) = (num_val(a), num_val(b)) {
                return Ok(num::equal(x, y));
            }
            return Ok(a == b);
        };
        match (p, q) {
            (S::Empty, s) | (s, S::Empty) => Ok(self.is_empty(s)),
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

/// The program's arithmetic operator for Lua operator `op`.
pub(crate) fn arith(op: Bin) -> present::Arith {
    match op {
        Bin::Add => present::Arith::Add,
        Bin::Sub => present::Arith::Sub,
        Bin::Mul => present::Arith::Mul,
        Bin::Div => present::Arith::Div,
        Bin::IDiv => present::Arith::IDiv,
        _ => present::Arith::Mod,
    }
}
