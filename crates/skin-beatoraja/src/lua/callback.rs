//! The per-frame callbacks of a configured pass's definition: the owned
//! capture data copied before the VM is dropped, and their classes
//! (design.md "beatoraja Lua Skins", per-frame closures; docs/stage3.md
//! §3, per-frame functions).
//!
//! Each root the converted table holds (a function at a property field
//! or an `op` entry, [`super::schema`]; any other is inert) is a closure
//! instance, as is each closure a root captures: its
//! function's tree in the file that defined it ([`super::syntax`]) and
//! the values its capture reader returned after the pass
//! ([`super::instrument`]): Lua integers (the integer subtype only),
//! booleans, host functions and modules by identity, other closure
//! instances, and the `_ENV` it reads, inspected raw. Nothing else is
//! kept, and nothing is run.
//!
//! [`classify`] decides a callback's class on its tree and its captures
//! by a positive allowlist, never by running it:
//!
//! - its body is `local name = E` lines and then `return E`, without
//!   parameters;
//! - `E` is built from integer numerals, `true` and `false`; captured
//!   integers and booleans; `H.f(n)` with `H` a chunk local bound once to
//!   `require("main_state")` (or to another such local) and captured as
//!   the host module, `f` one of `number`, `option`, `timer`, `gauge_type`,
//!   and `n` integer numerals; `H.timer_off_value`; calls of chunk-local
//!   helpers that are themselves of this form; `+ - *` and comparisons of
//!   integers; `==` and `~=` of `gauge_type()` with a numeral, of a timer
//!   with the off value, and of `skin_config.option["name"]` with a
//!   numeral, `skin_config` proven to be the pass's own table with no
//!   metatable on the way and its entry a Lua integer; `and`, `or` and
//!   `not` of booleans.
//!
//! A callback of this form is write-free: it reads, and whether each read
//! has a mapping is decided where it is consumed (`import::callback`).
//! Anything else has an effect the analysis does not rule out, and
//! refuses the candidate ([`admit`]).
//!
//! So once [`admit`] accepts every root, no callback the import can
//! reach writes anything at frame time, through any helper: not the
//! `_ENV` binding a configuration read resolves through, not
//! `skin_config` or its `option` table, and no metatable. With
//! [`Analysis::open`] unset (no script, unread listed entry or font
//! fallback metatable, which run code the analysis never sees), that is
//! the noninterference half of the capture proof that lets a
//! configuration read fold in any callback, a play-critical one included;
//! the other halves (the raw path, the final integer entry) are checked at
//! the read. [`admit`] records its acceptance on the [`Analysis`], and
//! lowering takes the analysis only through the [`Admitted`] witness it
//! gives.

use std::cell::{Cell, RefCell};

use std::collections::HashMap;

use super::limits::{MAX_ANALYSIS_WORK, MAX_AST_DEPTH};
use super::syntax::{Ast, Bin, CHUNK_ENV, DeclKind, Expr, NONE, Stmt, Un};
use super::{HOST_APIS, Unsupported};
use crate::error::{ImportError, Loc};
use crate::tree::{Arena, CallableKind, Kind, LuaScalar};

/// The host modules, in [`super::host`]'s order.
pub(crate) const MAIN_STATE: u8 = 0;

/// One file's tree.
#[derive(Debug)]
pub(crate) struct ParsedFile {
    pub(crate) name: String,
    pub(crate) ast: Ast,
}

/// A value a closure's reader returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Captured {
    Nil,
    Bool(bool),
    Int(i64),
    /// A host API function, by its index in [`HOST_APIS`].
    Host(u16),
    /// A host module, unchanged since the pass built it.
    Module(u8),
    /// Another closure instance.
    Closure(u32),
    /// The `_ENV` table, by its index in [`Analysis::envs`].
    Env(u32),
    /// Anything else: a float, a string, another table, a library
    /// function, a closure without a tag.
    Other,
}

/// An `_ENV` table, inspected raw.
#[derive(Debug, Default)]
pub(crate) struct EnvCapture {
    /// Why `skin_config.option` is not proven: `None` when it is.
    pub(crate) unproven: Option<&'static str>,
    /// Its `skin_config.option` table's integer entries, by its index in
    /// [`Analysis::option_sets`].
    pub(crate) options: u32,
}

/// A closure instance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Instance {
    /// Its file, an index of [`Analysis::files`], and function.
    pub(crate) file: u16,
    pub(crate) func: u32,
    /// Its captured values, a span of [`Analysis::captured`] in the order
    /// of its function's upvalues; `None` when it had no reader to call.
    pub(crate) values: Option<(u32, u32)>,
}

/// What became of a callable, for the developer's report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Outside the traversal: the import stopped first.
    NeverAnalysed,
    /// Never read by the source: not at a property field or an `op`
    /// entry ([`super::schema`]). Converted and charged, never classified.
    Inert,
    /// Classified write-free, never consumed.
    ClassifiedOnly,
    /// Converted into an expression.
    Lowered,
    /// A decorative callback replaced by a reported constant.
    Replaced(Unsupported),
    /// The candidate was refused at it.
    Refused,
}

/// A callback's type as classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ty {
    Int,
    Bool,
    /// `gauge_type()`.
    Gauge,
    /// `timer(n)`.
    Timer,
    /// `timer_off_value`.
    TimerOff,
    /// `skin_config.option["name"]`.
    Config,
}

/// What a walk past [`MAX_AST_DEPTH`] gives: the depth cap, fatal
/// wherever it is met ([`super::LuaLimit::AstDepth`]).
pub(crate) const TOO_DEEP: Unsupported = Unsupported::Construct("nesting past the depth cap");

/// What a walk past [`MAX_ANALYSIS_WORK`] gives: fatal wherever it is
/// met ([`super::LuaLimit::AnalysisWork`]).
pub(crate) const TOO_MUCH: Unsupported = Unsupported::Construct("analysis work past its cap");

/// What a walk that saw the cancellation request gives: fatal.
pub(crate) const CANCELLED: Unsupported = Unsupported::Construct("cancelled");

/// How many units of analysis work run between two looks at the
/// cancellation request.
const CHECK_EVERY: u64 = 4_096;

/// The fatal refusal a walk's sentinel stands for, if it is one.
pub(crate) fn fatal(why: Unsupported) -> Option<super::LuaWhy> {
    match why {
        TOO_DEEP => Some(super::LuaWhy::Limit(super::LuaLimit::AstDepth)),
        TOO_MUCH => Some(super::LuaWhy::Limit(super::LuaLimit::AnalysisWork)),
        CANCELLED => Some(super::LuaWhy::Cancelled),
        _ => None,
    }
}

/// The refusal of `why`: a cap or the cancellation request is fatal, any
/// other reason an unknown effect.
pub(crate) fn effect(why: Unsupported) -> super::LuaWhy {
    fatal(why).unwrap_or(super::LuaWhy::Effect(why))
}

/// A located unsupported construct: the file and line of its tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) why: Unsupported,
    pub(crate) file: u16,
    pub(crate) line: u32,
}

/// A classification in progress or done, by instance.
#[derive(Clone, Copy, Debug)]
enum State {
    Unvisited,
    InProgress,
    Done(Result<Ty, Found>),
}

/// A function prototype with its captured values: its file, its function
/// and the values, in its upvalues' order.
type ProtoKey = (u16, u32, Vec<Captured>);

/// The owned analysis data of a configured pass.
#[derive(Debug, Default)]
pub(crate) struct Analysis {
    pub(crate) files: Vec<ParsedFile>,
    pub(crate) instances: Vec<Instance>,
    pub(crate) captured: Vec<Captured>,
    pub(crate) envs: Vec<EnvCapture>,
    /// Each captured function's upvalue positions, by its file, function
    /// and binding: one hash a read.
    pub(crate) slots: HashMap<(u16, u32, u32), u32>,
    /// Option names interned once a pass, by their bytes; every option
    /// table refers to them by id.
    pub(crate) option_names: HashMap<Box<[u8]>, u32>,
    /// Each distinct option table's integer entries, by name id.
    pub(crate) option_sets: Vec<HashMap<u32, i64>>,
    /// The first place, in conversion order, where the source runs code
    /// the analysis does not see: a script root, an unread entry of a
    /// table it lists, or a font fallback with a metatable; by the
    /// converted node. While it is set, no proof that rests on captured
    /// state holds ([`Unsupported::Interference`]).
    pub(crate) open: Option<u32>,
    /// How many script roots and unread-entry roots the conversion met.
    pub(crate) scripts: u32,
    pub(crate) unread: u32,
    classes: RefCell<Vec<State>>,
    /// The classes of function prototypes by their file, function and
    /// captured values: instances that share all three share a class,
    /// which is walked once.
    protos: RefCell<HashMap<ProtoKey, Result<Ty, Found>>>,
    /// By callable.
    pub(crate) outcomes: RefCell<Vec<Disposition>>,
    /// Expression nodes expanded so far by lowering.
    pub(crate) expanded: Cell<u64>,
    /// Units of work the walks over the trees have done
    /// ([`MAX_ANALYSIS_WORK`]): an expression or statement visited, a
    /// captured value compared.
    pub(crate) work: Cell<u64>,
    /// The candidate's cancellation request.
    pub(crate) cancel: Option<super::Cancel>,
    /// Whether [`admit`] accepted every callable of this analysis.
    admitted: Cell<bool>,
    /// The callbacks consumed, in order, for the report.
    #[cfg(feature = "dev-tools")]
    pub(crate) consumed: RefCell<Vec<Consumed>>,
    /// Why each callable the import read but did not consume was left,
    /// by callable, for the report.
    #[cfg(feature = "dev-tools")]
    pub(crate) unused: RefCell<HashMap<u32, Unused>>,
}

/// The witness that [`admit`] accepted every callable of an [`Analysis`]:
/// lowering, configuration folds included, takes its analysis only
/// through it. It borrows that analysis, so a fold is tied to the
/// analysis admission proved write-free.
#[derive(Clone, Copy)]
pub(crate) struct Admitted<'a>(&'a Analysis);

impl<'a> Admitted<'a> {
    /// The admitted analysis.
    pub(crate) fn analysis(self) -> &'a Analysis {
        self.0
    }
}

/// Why a classified callback the import read was never consumed, for the
/// report; one it never read is in a field no consumer reads.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unused {
    /// Its element was dropped before the callback was consumed.
    Dropped,
    /// A condition before it folded false.
    FoldedFalse,
}

/// A callback where the import consumed it, for the report.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Consumed {
    /// The node holding it.
    pub(crate) node: u32,
    pub(crate) field: &'static str,
    /// Its role: the critical part, `None` when decorative.
    pub(crate) part: Option<crate::import::CriticalPart>,
    pub(crate) disposition: Disposition,
    /// Why it did not lower, when it did not.
    pub(crate) why: Option<Unsupported>,
}

impl Analysis {
    /// The witness that [`admit`] accepted this analysis, if it did.
    pub(crate) fn admitted(&self) -> Option<Admitted<'_>> {
        self.admitted.get().then_some(Admitted(self))
    }

    /// Records that the import left callable `c` unconsumed for `why`,
    /// replacing an earlier reason.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn unused(&self, c: u32, why: Unused) {
        self.unused.borrow_mut().insert(c, why);
    }

    /// An empty analysis that sees `cancel`, set before any capture so
    /// that every charge polls it.
    pub(crate) fn with_cancel(cancel: super::Cancel) -> Self {
        Self {
            cancel: Some(cancel),
            ..Self::default()
        }
    }

    /// `n` units of work charged before they are done, the cancellation
    /// request seen every [`CHECK_EVERY`]: [`TOO_MUCH`] past the cap,
    /// [`CANCELLED`] once it is requested.
    pub(crate) fn charge(&self, n: u64) -> Result<(), Unsupported> {
        let before = self.work.get();
        let after = before.saturating_add(n);
        if after > MAX_ANALYSIS_WORK {
            return Err(TOO_MUCH);
        }
        self.work.set(after);
        if before / CHECK_EVERY != after / CHECK_EVERY
            && self
                .cancel
                .as_ref()
                .is_some_and(super::Cancel::is_cancelled)
        {
            return Err(CANCELLED);
        }
        Ok(())
    }

    /// Entry `k` of option table `set`: one hash of `k`, its bytes
    /// charged first.
    pub(crate) fn option(&self, set: u32, k: &[u8]) -> Result<Option<i64>, Unsupported> {
        self.charge(u64::try_from(k.len()).unwrap_or(u64::MAX))?;
        Ok(self
            .option_names
            .get(k)
            .and_then(|id| self.option_sets.get(set as usize)?.get(id))
            .copied())
    }

    pub(crate) fn instance(&self, i: u32) -> Option<&Instance> {
        self.instances.get(i as usize)
    }

    pub(crate) fn ast(&self, file: u16) -> Option<&Ast> {
        self.files.get(usize::from(file)).map(|f| &f.ast)
    }

    /// Instance `i`'s value for its function's upvalue `up`.
    pub(crate) fn upvalue(&self, i: u32, up: u32) -> Option<Captured> {
        let inst = self.instance(i)?;
        let k = *self.slots.get(&(inst.file, inst.func, up))? as usize;
        let (start, len) = inst.values?;
        if k >= len as usize {
            return None;
        }
        self.captured.get(start as usize + k).copied()
    }

    /// The location of `found`.
    pub(crate) fn loc(&self, found: &Found) -> Loc {
        Loc {
            file: self
                .files
                .get(usize::from(found.file))
                .map(|f| f.name.clone())
                .unwrap_or_default(),
            line: found.line,
            col: 0,
        }
    }

    /// Records what became of callable `c`.
    pub(crate) fn set_outcome(&self, c: u32, d: Disposition) {
        if let Some(slot) = self.outcomes.borrow_mut().get_mut(c as usize) {
            *slot = d;
        }
    }

    /// The class of instance `i`: its result type when it is write-free,
    /// else the construct that has an unknown effect.
    pub(crate) fn classify(&self, i: u32) -> Result<Ty, Found> {
        self.classify_at(i, 0)
    }

    /// [`Self::classify`] `depth` levels into a walk: every level of an
    /// expression and every helper call counts one, and a walk past
    /// [`MAX_AST_DEPTH`] stops at [`TOO_DEEP`].
    fn classify_at(&self, i: u32, depth: u32) -> Result<Ty, Found> {
        {
            let mut classes = self.classes.borrow_mut();
            if classes.len() < self.instances.len() {
                classes.resize(self.instances.len(), State::Unvisited);
            }
            match classes.get(i as usize).copied() {
                Some(State::Done(r)) => return r,
                Some(State::InProgress) => {
                    let inst = self.instance(i);
                    return Err(Found {
                        why: Unsupported::Recursion,
                        file: inst.map_or(0, |x| x.file),
                        line: inst
                            .and_then(|x| self.ast(x.file)?.func(x.func).map(|f| f.line))
                            .unwrap_or(0),
                    });
                }
                _ => {}
            }
            if let Some(s) = classes.get_mut(i as usize) {
                *s = State::InProgress;
            }
        }
        let key = match self.proto_key(i) {
            Ok(key) => key,
            Err(why) => {
                let r = Err(Found {
                    why,
                    file: self.instance(i).map_or(0, |x| x.file),
                    line: 0,
                });
                if let Some(s) = self.classes.borrow_mut().get_mut(i as usize) {
                    *s = State::Done(r);
                }
                return r;
            }
        };
        let cached = key
            .as_ref()
            .and_then(|k| self.protos.borrow().get(k).copied());
        let r = match cached {
            Some(r) => r,
            None => {
                let r = self.classify_body(i, depth);
                // A cap or cancellation is not a class.
                if let Some(k) = key
                    && !r.is_err_and(|f| fatal(f.why).is_some())
                {
                    self.protos.borrow_mut().insert(k, r);
                }
                r
            }
        };
        if let Some(s) = self.classes.borrow_mut().get_mut(i as usize) {
            *s = State::Done(r);
        }
        r
    }

    /// The prototype key of instance `i`, its captured values charged as
    /// they are copied.
    /// [`Self::classify_at`]'s key for instance `i`, its values charged
    /// before they are copied; `Ok(None)` without one.
    fn proto_key(&self, i: u32) -> Result<Option<ProtoKey>, Unsupported> {
        let Some(inst) = self.instance(i) else {
            return Ok(None);
        };
        let (start, len) = inst.values.unwrap_or((0, 0));
        self.charge(u64::from(len))?;
        let Some(values) = self
            .captured
            .get(start as usize..start as usize + len as usize)
        else {
            return Ok(None);
        };
        Ok(Some((inst.file, inst.func, values.to_vec())))
    }

    fn classify_body(&self, i: u32, depth: u32) -> Result<Ty, Found> {
        let untagged = |file| Found {
            why: Unsupported::Capture("an untagged closure"),
            file,
            line: 0,
        };
        let inst = *self.instance(i).ok_or(untagged(0))?;
        let ast = self.ast(inst.file).ok_or(untagged(inst.file))?;
        let f = ast.func(inst.func).ok_or(untagged(inst.file))?;
        let at = |why, line| Found {
            why,
            file: inst.file,
            line,
        };
        if f.params.1 > 0 || f.vararg {
            return Err(at(Unsupported::Construct("parameters"), f.line));
        }
        if inst.values.is_none() && f.upvalues.1 > 0 {
            return Err(at(Unsupported::Capture("no reader"), f.line));
        }
        let mut cx = Classify {
            an: self,
            ast,
            inst: i,
            file: inst.file,
            locals: Vec::new(),
            depth: std::cell::Cell::new(depth),
        };
        for &s in ast.list(f.body) {
            let line = ast.stmt_line(s);
            self.charge(1).map_err(|why| at(why, line))?;
            match ast.stmt(s) {
                Stmt::Empty => {}
                Stmt::Local { names, exprs } => {
                    let (&[d], &[e]) = (ast.list(names), ast.list(exprs)) else {
                        return Err(at(Unsupported::Construct("local list"), line));
                    };
                    match ast.decl(d).map(|x| x.attrib) {
                        Some(super::syntax::Attrib::Close) => {
                            return Err(at(Unsupported::Construct("<close>"), line));
                        }
                        Some(_) => {}
                        None => return Err(at(Unsupported::Construct("local"), line)),
                    }
                    let ty = cx.expr(e)?;
                    cx.locals.push((d, ty));
                }
                Stmt::Return(list) => {
                    let &[e] = ast.list(list) else {
                        return Err(at(Unsupported::Construct("return list"), line));
                    };
                    return cx.expr(e);
                }
                Stmt::Other(what) => return Err(at(Unsupported::Construct(what), line)),
            }
        }
        Err(at(Unsupported::Construct("no return"), f.line))
    }
}

/// Whether `e` is an integer numeral, negated or not, and its value.
pub(crate) fn numeral(ast: &Ast, e: u32) -> Option<i64> {
    match ast.expr(e) {
        Expr::Int(n) => Some(n),
        Expr::Paren(x) => numeral(ast, x),
        Expr::Unary(Un::Neg, x) => match ast.expr(x) {
            Expr::Int(n) => n.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

/// The host API a call's callee names when it is `H.f` with `H` a proven
/// module local: the module and the function's name.
pub(crate) enum Callee {
    /// `H.f`, by [`HOST_APIS`] index.
    Host(u16),
    /// A helper instance.
    Helper(u32),
}

/// The walk of one instance's body.
struct Classify<'a> {
    an: &'a Analysis,
    ast: &'a Ast,
    inst: u32,
    file: u16,
    /// The body's locals so far and their types.
    locals: Vec<(u32, Ty)>,
    /// How deep the walk is.
    depth: std::cell::Cell<u32>,
}

impl Classify<'_> {
    fn at(&self, why: Unsupported, e: u32) -> Found {
        Found {
            why,
            file: self.file,
            line: self.ast.expr_line(e),
        }
    }

    fn expr(&self, e: u32) -> Result<Ty, Found> {
        self.an.charge(1).map_err(|why| self.at(why, e))?;
        let d = self.depth.get();
        if d >= MAX_AST_DEPTH {
            return Err(self.at(TOO_DEEP, e));
        }
        self.depth.set(d + 1);
        let r = self.expr_inner(e);
        self.depth.set(d);
        r
    }

    fn expr_inner(&self, e: u32) -> Result<Ty, Found> {
        let construct = |w: &'static str| Err(self.at(Unsupported::Construct(w), e));
        match self.ast.expr(e) {
            Expr::Int(_) => Ok(Ty::Int),
            Expr::True | Expr::False => Ok(Ty::Bool),
            Expr::Float => construct("float"),
            Expr::Str(_) => construct("string"),
            Expr::Nil => construct("nil"),
            Expr::Vararg => construct("vararg"),
            Expr::Func(_) => construct("function"),
            Expr::Table(_) => construct("table"),
            Expr::Method { .. } => construct("method call"),
            Expr::Paren(x) => self.expr(x),
            Expr::Local(d) => {
                // The body's locals, at most Lua's 200, scanned and charged.
                let n = u64::try_from(self.locals.len()).unwrap_or(u64::MAX);
                self.an.charge(n).map_err(|why| self.at(why, e))?;
                if let Some(&(_, ty)) = self.locals.iter().rev().find(|(l, _)| *l == d) {
                    return Ok(ty);
                }
                match self.an.upvalue(self.inst, d) {
                    Some(Captured::Int(_)) => Ok(Ty::Int),
                    Some(Captured::Bool(_)) => Ok(Ty::Bool),
                    _ => Err(self.at(Unsupported::Capture("an upvalue"), e)),
                }
            }
            Expr::Global { .. } => Err(self.at(Unsupported::Global, e)),
            Expr::Index { obj, key } => self.index(e, obj, key),
            Expr::Call { callee, args } => self.call(e, callee, self.ast.list(args)),
            Expr::Unary(op, x) => {
                let t = self.expr(x)?;
                match (op, t) {
                    (Un::Neg, Ty::Int) => Ok(Ty::Int),
                    (Un::Not, Ty::Bool) => Ok(Ty::Bool),
                    (Un::Not, Ty::Int) => Err(self.at(Unsupported::Truthiness, e)),
                    (Un::Len, _) => construct("#"),
                    (Un::BNot, _) => construct("bitwise"),
                    _ => Err(self.at(Unsupported::Type, e)),
                }
            }
            Expr::Binary(op, a, b) => self.binary(e, op, a, b),
        }
    }

    fn binary(&self, e: u32, op: Bin, a: u32, b: u32) -> Result<Ty, Found> {
        let construct = |w: &'static str| Err(self.at(Unsupported::Construct(w), e));
        match op {
            Bin::Div => return construct("/"),
            Bin::IDiv => return construct("//"),
            Bin::Mod => return construct("%"),
            Bin::Pow => return construct("^"),
            Bin::Concat => return construct(".."),
            Bin::BAnd | Bin::BOr | Bin::BXor | Bin::Shl | Bin::Shr => return construct("bitwise"),
            _ => {}
        }
        let (ta, tb) = (self.expr(a)?, self.expr(b)?);
        let lit = |x: u32| numeral(self.ast, x).is_some();
        match op {
            Bin::Add | Bin::Sub | Bin::Mul if ta == Ty::Int && tb == Ty::Int => Ok(Ty::Int),
            Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge if ta == Ty::Int && tb == Ty::Int => Ok(Ty::Bool),
            Bin::Eq | Bin::Ne => match (ta, tb) {
                (Ty::Int, Ty::Int) => Ok(Ty::Bool),
                (Ty::Gauge | Ty::Config, Ty::Int) if lit(b) => Ok(Ty::Bool),
                (Ty::Int, Ty::Gauge | Ty::Config) if lit(a) => Ok(Ty::Bool),
                (Ty::Timer, Ty::TimerOff) | (Ty::TimerOff, Ty::Timer) => Ok(Ty::Bool),
                _ => Err(self.at(Unsupported::Type, e)),
            },
            Bin::And | Bin::Or => match (ta, tb) {
                (Ty::Bool, Ty::Bool) => Ok(Ty::Bool),
                (Ty::Int, _) | (_, Ty::Int) => Err(self.at(Unsupported::Truthiness, e)),
                _ => Err(self.at(Unsupported::Type, e)),
            },
            _ => Err(self.at(Unsupported::Type, e)),
        }
    }

    fn index(&self, e: u32, obj: u32, key: u32) -> Result<Ty, Found> {
        let key_str = match self.ast.expr(key) {
            Expr::Str(s) => Some(self.ast.str(s)),
            _ => None,
        };
        // `H.timer_off_value`.
        if let Expr::Local(d) = self.ast.expr(obj)
            && key_str == Some(b"timer_off_value")
        {
            self.module(obj, d)?;
            return Ok(Ty::TimerOff);
        }
        // `skin_config.option["name"]`.
        if let Expr::Index { obj: cfg, key: opt } = self.ast.expr(obj)
            && let Expr::Global { name, env } = self.ast.expr(cfg)
            && self.ast.str(name) == b"skin_config"
            && matches!(self.ast.expr(opt), Expr::Str(s) if self.ast.str(s) == b"option")
            && let Some(k) = key_str
        {
            self.config(e, env, k)?;
            return Ok(Ty::Config);
        }
        Err(self.at(Unsupported::Construct("index"), e))
    }

    /// The proven integer entry `k` of `skin_config.option`, read through
    /// `env`.
    fn config(&self, e: u32, env: u32, k: &[u8]) -> Result<i64, Found> {
        let Some(Captured::Env(i)) = self.an.upvalue(self.inst, env) else {
            return Err(self.at(Unsupported::Capture("_ENV"), e));
        };
        let capture = self
            .an
            .envs
            .get(i as usize)
            .ok_or(self.at(Unsupported::Capture("_ENV"), e))?;
        if let Some(why) = capture.unproven {
            return Err(self.at(Unsupported::Capture(why), e));
        }
        self.an
            .option(capture.options, k)
            .map_err(|why| self.at(why, e))?
            .ok_or(self.at(Unsupported::Capture("an option entry"), e))
    }

    /// Checks that local `d` read at `e` is the host module `main_state`,
    /// bound once to `require("main_state")` or another such local.
    fn module(&self, e: u32, d: u32) -> Result<(), Found> {
        match self.an.upvalue(self.inst, d) {
            Some(Captured::Module(MAIN_STATE)) => {}
            Some(Captured::Module(_)) => {
                return Err(self.at(Unsupported::Binding("another host module"), e));
            }
            _ => return Err(self.at(Unsupported::Binding("a host module"), e)),
        }
        if module_binding(self.ast, d, 0, true) {
            Ok(())
        } else {
            Err(self.at(Unsupported::Binding("a host module"), e))
        }
    }

    fn call(&self, e: u32, callee: u32, args: &[u32]) -> Result<Ty, Found> {
        match resolve_callee(self.an, self.ast, self.inst, callee) {
            Ok(Callee::Host(api)) => {
                let (_, name) = HOST_APIS.get(usize::from(api)).copied().unwrap_or(("", ""));
                let (arity, ty) = match name {
                    "number" => (1, Ty::Int),
                    "option" => (1, Ty::Bool),
                    "timer" => (1, Ty::Timer),
                    "gauge_type" => (0, Ty::Gauge),
                    _ => return Err(self.at(Unsupported::HostApi(api), e)),
                };
                if args.len() != arity || args.iter().any(|&a| numeral(self.ast, a).is_none()) {
                    return Err(self.at(Unsupported::Construct("host arguments"), e));
                }
                Ok(ty)
            }
            Ok(Callee::Helper(j)) => {
                if !args.is_empty() {
                    return Err(self.at(Unsupported::Construct("helper arguments"), e));
                }
                match self.an.classify_at(j, self.depth.get() + 1) {
                    Ok(t @ (Ty::Int | Ty::Bool)) => Ok(t),
                    Ok(_) => Err(self.at(Unsupported::Type, e)),
                    Err(f) if f.why == Unsupported::Recursion => {
                        Err(self.at(Unsupported::Recursion, e))
                    }
                    Err(f) => Err(f),
                }
            }
            Err(why) => Err(self.at(why, e)),
        }
    }
}

/// Whether declaration `d` is initialized by `require("main_state")`,
/// directly or through locals, `depth` aliases deep; when `once`, each of
/// them a chunk local never assigned after its declaration.
fn module_binding(ast: &Ast, d: u32, depth: u32, once: bool) -> bool {
    let Some(decl) = ast.decl(d) else {
        return false;
    };
    if depth > 16 || (once && (decl.func != 0 || decl.writes > 0 || decl.kind != DeclKind::Local)) {
        return false;
    }
    match ast.expr(decl.init) {
        Expr::Call { callee, args } => {
            let req = matches!(ast.expr(callee), Expr::Global { name, .. } if ast.str(name) == b"require");
            let arg = matches!(ast.list(args), &[a] if matches!(ast.expr(a), Expr::Str(s) if ast.str(s) == b"main_state"));
            req && arg
        }
        Expr::Local(other) => module_binding(ast, other, depth + 1, once),
        _ => false,
    }
}

/// What a call's callee is in instance `inst`'s body: a mapped host API
/// of the proven module, or a chunk-local helper instance.
pub(crate) fn resolve_callee(
    an: &Analysis,
    ast: &Ast,
    inst: u32,
    callee: u32,
) -> Result<Callee, Unsupported> {
    match ast.expr(callee) {
        Expr::Index { obj, key } => {
            let Expr::Local(d) = ast.expr(obj) else {
                return Err(Unsupported::Callee);
            };
            let Expr::Str(s) = ast.expr(key) else {
                return Err(Unsupported::Callee);
            };
            // A local bound to the module and written again, or bound to
            // it through something else, is not proven.
            if module_binding(ast, d, 0, false) && !module_binding(ast, d, 0, true) {
                return Err(Unsupported::Binding("a host module"));
            }
            let module = match an.upvalue(inst, d) {
                Some(Captured::Module(m)) => m,
                _ => return Err(Unsupported::Callee),
            };
            let module_name = super::host::MODULES
                .get(usize::from(module))
                .copied()
                .unwrap_or("");
            let name = ast.str(s);
            let api = HOST_APIS
                .iter()
                .position(|&(m, n)| m == module_name && n.as_bytes() == name)
                .and_then(|i| u16::try_from(i).ok())
                .ok_or(Unsupported::Callee)?;
            if module != MAIN_STATE {
                return Err(Unsupported::HostApi(api));
            }
            if !module_binding(ast, d, 0, true) {
                return Err(Unsupported::Binding("a host module"));
            }
            Ok(Callee::Host(api))
        }
        Expr::Local(d) => {
            let Some(Captured::Closure(j)) = an.upvalue(inst, d) else {
                return Err(Unsupported::Callee);
            };
            let ok = ast.decl(d).is_some_and(|x| {
                x.func == 0
                    && x.writes == 0
                    && matches!(x.kind, DeclKind::Local | DeclKind::LocalFunction)
            });
            if !ok {
                return Err(Unsupported::Binding("a helper"));
            }
            Ok(Callee::Helper(j))
        }
        _ => Err(Unsupported::Callee),
    }
}

/// Classifies every root a converted table holds, recording each as
/// classified or refused, and every other callable as inert: the first
/// root with an unknown effect refuses the candidate at its construct
/// (design.md "beatoraja Lua Skins", what cannot be converted). A host or
/// library function at a root, which has no tree, is refused at its
/// value.
pub(crate) fn admit(arena: &Arena) -> Result<(), ImportError> {
    let Some(lua) = arena.lua.as_deref() else {
        return Ok(());
    };
    let Some(an) = lua.analysis.as_deref() else {
        return Ok(());
    };
    an.outcomes
        .borrow_mut()
        .resize(lua.callables.len(), Disposition::NeverAnalysed);
    let mut first: Option<ImportError> = None;
    for (id, c) in callable_nodes(arena) {
        // Each callable visited is a unit of work, cancellation seen.
        if let Err(why) = an.charge(1) {
            return Err(ImportError::Lua {
                at: Some(arena.loc(id)),
                why: effect(why),
            });
        }
        let Some(callable) = lua.callables.get(c as usize) else {
            continue;
        };
        if an.outcomes.borrow().get(c as usize) != Some(&Disposition::NeverAnalysed) {
            continue;
        }
        if !callable.root {
            an.set_outcome(c, Disposition::Inert);
            continue;
        }
        let r = if callable.kind == CallableKind::Lua && callable.instance != NONE {
            an.classify(callable.instance)
                .map(|_| ())
                .map_err(|f| (Some(an.loc(&f)), f.why))
        } else {
            Err((None, Unsupported::Capture("not a skin closure")))
        };
        match r {
            Ok(()) => an.set_outcome(c, Disposition::ClassifiedOnly),
            Err((at, why)) => {
                an.set_outcome(c, Disposition::Refused);
                if first.is_none() {
                    first = Some(ImportError::Lua {
                        at: at.or_else(|| Some(arena.loc(id))),
                        why: effect(why),
                    });
                }
            }
        }
    }
    match first {
        Some(e) => Err(e),
        None => {
            an.admitted.set(true);
            Ok(())
        }
    }
}

/// The nodes holding a callable, in conversion order, with the
/// callable's index.
pub(crate) fn callable_nodes(arena: &Arena) -> impl Iterator<Item = (u32, u32)> + '_ {
    let lua = arena.lua.as_deref();
    (0_u32..).zip(&arena.nodes).filter_map(move |(id, n)| {
        let lua = lua?;
        if n.kind != Kind::Lua {
            return None;
        }
        match lua.values.get(n.end as usize)? {
            LuaScalar::Callable(i) => Some((id, *i)),
            _ => None,
        }
    })
}

/// The tree of instance `i`: its file's tree and its function's index.
pub(crate) fn tree(an: &Analysis, i: u32) -> Option<(&Ast, u32)> {
    let inst = an.instance(i)?;
    Some((an.ast(inst.file)?, inst.func))
}

/// Whether upvalue `up` of a function in `ast` is the binding its globals
/// are read through.
pub(crate) fn is_env(ast: &Ast, up: u32) -> bool {
    up == CHUNK_ENV || ast.decl(up).is_some_and(|d| d.env)
}
