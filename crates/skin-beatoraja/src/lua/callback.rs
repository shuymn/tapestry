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
//! ([`super::instrument`]), their final bindings: nil, booleans, Lua
//! integers and floats, strings (interned), host functions, host modules
//! and the sandbox's library functions by identity, other closure
//! instances, and tables by identity with whether each has a metatable.
//! Of a table, only the entries on the function's static access paths
//! ([`super::syntax::Path`]: constant string keys from an upvalue or a
//! global) are copied, each read raw; the `_ENV` its globals are read
//! through is such a table. Nothing else is kept, and nothing is run.
//!
//! A consumed callback is partially evaluated on its tree and these
//! captures ([`super::residual`]), never by running it; whether each read
//! it records has a mapping is decided where it is consumed
//! (`import::callback`), and one that does not lower follows its role
//! there. The provenance marker's records ([`Analysis::marks`]) are
//! copied with the captures.
//!
//! What a fold or a binding rests on is decided apart, for every root at
//! once, by [`admit`]: the effect walk ([`super::effects`]) checks every
//! root's reach against the modeled subset, which writes no table,
//! upvalue, global or module member, calls only host readers, pure
//! library functions and closures of the same subset, and reaches no
//! metamethod. With [`Analysis::open`] unset after it (no construct
//! outside the subset, and no script, unread listed entry or font
//! fallback metatable, which run code the analysis never sees), the
//! candidate is closed: no code the source runs at frame time changes the
//! `_ENV` binding a configuration read resolves through, `skin_config` or
//! its `option` table, a binding of the host module or a helper, or a
//! metatable. That is the noninterference half of the capture proof that
//! lets a configuration read fold and a host module or helper binding
//! hold in any callback, a play-critical one included; the other halves
//! (the raw path, the final binding) are checked at the read. A root
//! whose reach writes never lowers. [`admit`] records that the walk ran
//! on the [`Analysis`], and lowering takes the analysis only through the
//! [`Admitted`] witness it gives.

use std::cell::{Cell, RefCell};

use std::collections::HashMap;

use super::limits::MAX_ANALYSIS_WORK;
use super::syntax::{Ast, CHUNK_ENV, Expr, NONE, Un};
use super::{HOST_APIS, Unsupported};
use crate::error::{ImportError, Loc};
use crate::tree::{Arena, Kind, LuaScalar};

/// The host modules, in [`super::host`]'s order.
pub(crate) const MAIN_STATE: u8 = 0;

/// One file's tree.
#[derive(Debug)]
pub(crate) struct ParsedFile {
    pub(crate) name: String,
    pub(crate) ast: Ast,
}

/// A value a closure's reader returned, or a table entry on one of its
/// static access paths: the captured object graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Captured {
    Nil,
    Bool(bool),
    Int(i64),
    /// A float, by its bits.
    Float(u64),
    /// A string, by its index among [`Analysis::names`].
    Str(u32),
    /// A host API function, by its index in [`HOST_APIS`].
    Host(u16),
    /// A host module, its proxy unchanged since the pass built it.
    Module(u8),
    /// Another closure instance.
    Closure(u32),
    /// A table, by its index in [`Analysis::tables`], and whether it has
    /// a metatable.
    Table {
        id: u32,
        meta: bool,
    },
    /// A function of the sandbox's library, by its index in
    /// [`super::sandbox::LIBS`]: the sandbox's own, by identity.
    Lib(u16),
    /// Anything else: a closure without a tag, a host module's proxy the
    /// skin changed, a coroutine or userdata.
    Other,
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
    /// Not a root, but captured by one, which the source may call it
    /// through: classified with that root, never consumed in its own
    /// field.
    Captured,
    /// Classified write-free, never consumed.
    ClassifiedOnly,
    /// Converted into an expression.
    Lowered,
    /// Compiled into the presentation program.
    Compiled,
    /// A decorative callback replaced by a reported constant.
    Replaced(Unsupported),
    /// The candidate was refused at it.
    Refused,
}

/// What a walk past [`MAX_AST_DEPTH`](super::limits::MAX_AST_DEPTH) gives: the depth cap, fatal
/// wherever it is met ([`super::LuaLimit::AstDepth`]).
pub(crate) const TOO_DEEP: Unsupported = Unsupported::Construct("nesting past the depth cap");

/// What a walk past [`MAX_ANALYSIS_WORK`] gives: fatal wherever it is
/// met ([`super::LuaLimit::AnalysisWork`]).
pub(crate) const TOO_MUCH: Unsupported = Unsupported::Construct("analysis work past its cap");

/// What a walk that saw the cancellation request gives: fatal.
pub(crate) const CANCELLED: Unsupported = Unsupported::Construct("cancelled");

pub(crate) use super::clock::{Phase, TOO_LONG};

/// How many units of analysis work run between two looks at the
/// cancellation request.
const CHECK_EVERY: u64 = 4_096;

/// The fatal refusal a walk's sentinel stands for, if it is one.
pub(crate) fn fatal(why: Unsupported) -> Option<super::LuaWhy> {
    match why {
        TOO_DEEP => Some(super::LuaWhy::Limit(super::LuaLimit::AstDepth)),
        TOO_MUCH => Some(super::LuaWhy::Limit(super::LuaLimit::AnalysisWork)),
        TOO_LONG => Some(super::LuaWhy::Limit(super::LuaLimit::AnalysisTime)),
        CANCELLED => Some(super::LuaWhy::Cancelled),
        _ => None,
    }
}

/// A located unsupported construct: the file and line of its tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) why: Unsupported,
    pub(crate) file: u16,
    pub(crate) line: u32,
}

/// Where the analysis was opened: a converted node (a script root, an
/// unread entry, a font fallback with a metatable, a root that is no
/// skin closure), or a line of a file's tree the effect walk met outside
/// the subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    Node(u32),
    Code { file: u16, line: u32 },
}

/// The first place the source may run code the analysis does not model,
/// and why ([`Analysis::open`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Open {
    pub(crate) site: Site,
    pub(crate) why: Unsupported,
}

/// A function prototype with its captured values: its file, its function
/// and the values, in its upvalues' order.
pub(crate) type ProtoKey = (u16, u32, Vec<Captured>);

/// The owned analysis data of a configured pass.
#[derive(Debug, Default)]
pub(crate) struct Analysis {
    pub(crate) files: Vec<ParsedFile>,
    pub(crate) instances: Vec<Instance>,
    pub(crate) captured: Vec<Captured>,
    /// Each captured function's upvalue positions, by its file, function
    /// and binding: one hash a read.
    pub(crate) slots: HashMap<(u16, u32, u32), u32>,
    /// Strings interned once a pass, by their bytes: the keys of copied
    /// entries and the captured strings refer to them by id.
    pub(crate) names: HashMap<Box<[u8]>, u32>,
    /// Each distinct captured table's copied entries, by key id: only
    /// those on a captured function's static access paths, each read raw.
    pub(crate) tables: Vec<HashMap<u32, Captured>>,
    /// The captured table that is the pass's own `skin_config`, if one is.
    pub(crate) config: Option<u32>,
    /// The provenance marker's records ([`super::instrument`]): each
    /// marked declaration, by its file and declaration (`file << 32 |
    /// decl`), to whether every value it read `timer_off_value` from was
    /// the host module's proxy.
    pub(crate) marks: HashMap<u64, bool>,
    /// Whether the host module's proxy is as the pass built it: its
    /// `timer_off_value` read is the host's off value.
    pub(crate) module_intact: bool,
    /// The first place where the source may run code the analysis does
    /// not model: in conversion order, a script root, an unread entry of
    /// a table it lists, or a font fallback with a metatable; then, in the
    /// effect walk's order ([`super::effects`]), a root that is no skin
    /// closure or a construct outside the subset. While it is set, the
    /// candidate is open, and no proof that rests on captured state holds
    /// ([`Unsupported::Interference`]); unset after the walk, it is
    /// closed.
    pub(crate) open: Cell<Option<Open>>,
    /// The root instances whose reach writes ([`super::effects`]): they
    /// never lower ([`Unsupported::Writes`]).
    pub(crate) writers: RefCell<std::collections::HashSet<u32>>,
    /// The time the analysis spent in its phases ([`super::clock`]).
    pub(crate) clock: super::clock::AnalysisClock,
    /// How many script roots and unread-entry roots the conversion met,
    /// for the report.
    #[cfg(any(test, feature = "dev-tools"))]
    pub(crate) scripts: u32,
    #[cfg(any(test, feature = "dev-tools"))]
    pub(crate) unread: u32,
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
    /// Whether [`admit`] ran the effect walk over every root of this
    /// analysis.
    admitted: Cell<bool>,
    /// The callbacks consumed, in order, for the report.
    #[cfg(feature = "dev-tools")]
    pub(crate) consumed: RefCell<Vec<Consumed>>,
    /// Why each callable the import read but did not consume was left,
    /// by callable, for the report.
    #[cfg(feature = "dev-tools")]
    pub(crate) unused: RefCell<HashMap<u32, Unused>>,
}

/// The witness that [`admit`] walked every root of an [`Analysis`]:
/// lowering, configuration folds included, takes its analysis only
/// through it. It borrows that analysis, so a fold is tied to the
/// analysis whose closedness and writers admission decided.
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
#[derive(Clone, Debug)]
pub(crate) struct Consumed {
    /// The node holding it.
    pub(crate) node: u32,
    pub(crate) field: &'static str,
    /// Its role: the critical part, `None` when decorative.
    pub(crate) part: Option<crate::import::CriticalPart>,
    pub(crate) disposition: Disposition,
    /// Why it did not lower, when it did not.
    pub(crate) why: Option<Unsupported>,
    /// The destination it was consumed for.
    pub(crate) element: Option<super::survey::Element>,
    /// What it lowered to, when it lowered.
    pub(crate) lowered: Option<super::survey::Lowered>,
}

impl Analysis {
    /// The witness that [`admit`] walked this analysis, if it did.
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
    /// request and the time budget seen every [`CHECK_EVERY`]:
    /// [`TOO_MUCH`] past the cap, [`CANCELLED`] once it is requested,
    /// [`TOO_LONG`] past the time budget.
    pub(crate) fn charge(&self, n: u64) -> Result<(), Unsupported> {
        let before = self.work.get();
        let after = before.saturating_add(n);
        if after > MAX_ANALYSIS_WORK {
            return Err(TOO_MUCH);
        }
        self.work.set(after);
        if before / CHECK_EVERY != after / CHECK_EVERY {
            if self
                .cancel
                .as_ref()
                .is_some_and(super::Cancel::is_cancelled)
            {
                return Err(CANCELLED);
            }
            self.clock.check()?;
        }
        Ok(())
    }

    /// Whether root instance `i` reaches a write.
    pub(crate) fn writes(&self, i: u32) -> bool {
        self.writers.borrow().contains(&i)
    }

    /// The copied entry `k` of captured table `table`: one hash of `k`,
    /// its bytes charged first; `None` when no static access path copied
    /// it (an entry the source holds as nil is copied as [`Captured::Nil`]).
    pub(crate) fn entry(&self, table: u32, k: &[u8]) -> Result<Option<Captured>, Unsupported> {
        self.charge(u64::try_from(k.len()).unwrap_or(u64::MAX))?;
        Ok(self
            .names
            .get(k)
            .and_then(|id| self.tables.get(table as usize)?.get(id))
            .copied())
    }

    /// Global `name` read through binding `env` in instance `i`: its
    /// `_ENV` captured as a table without a metatable, the entry read raw.
    pub(crate) fn global(
        &self,
        i: u32,
        env: u32,
        name: &[u8],
    ) -> Result<Option<Captured>, Unsupported> {
        match self.upvalue(i, env) {
            Some(Captured::Table { meta: true, .. }) => {
                Err(Unsupported::Capture("_ENV's metatable"))
            }
            Some(Captured::Table { id, meta: false }) => self.entry(id, name),
            _ => Err(Unsupported::Capture("_ENV")),
        }
    }

    /// The final binding `h` names in instance `i`'s body, when `h` is one
    /// the captures hold: an upvalue, a global, or `_ENV.name` of the
    /// `_ENV` binding (the same global); `None` for any other expression.
    pub(crate) fn binding(
        &self,
        ast: &Ast,
        i: u32,
        h: u32,
    ) -> Result<Option<Captured>, Unsupported> {
        match ast.expr(h) {
            Expr::Local(d) => Ok(self.upvalue(i, d)),
            Expr::Global { name, env } => self.global(i, env, ast.str(name)),
            Expr::Index { obj, key } => match (ast.expr(obj), ast.expr(key)) {
                (Expr::Local(env), Expr::Str(s)) if is_env(ast, env) => {
                    self.global(i, env, ast.str(s))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
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

    /// The prototype key of instance `i`, its values charged before they
    /// are copied; `Ok(None)` without one.
    pub(super) fn proto_key(&self, i: u32) -> Result<Option<ProtoKey>, Unsupported> {
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

/// What a call's callee is in instance `inst`'s body, proven by the
/// captured graph: `H.f` with `H`'s final binding ([`Analysis::binding`])
/// the host module, its proxy unchanged, `f` one of its APIs; or an
/// upvalue whose final binding is a closure instance, a helper. That
/// holds at frame time only while the candidate is closed: the effect
/// walk found every root inside the modeled subset, which writes nothing,
/// and no code the analysis does not see runs ([`Analysis::open`] unset),
/// which lowering checks before it resolves a call.
/// `H.f` with `f` a `main_state` API and `H` bound to anything else is
/// [`Unsupported::Binding`].
pub(crate) fn resolve_callee(
    an: &Analysis,
    ast: &Ast,
    inst: u32,
    callee: u32,
) -> Result<Callee, Unsupported> {
    match ast.expr(callee) {
        Expr::Index { obj, key } => {
            let Expr::Str(s) = ast.expr(key) else {
                return Err(Unsupported::Callee);
            };
            let name = ast.str(s);
            let bound = an.binding(ast, inst, obj)?;
            let Some(Captured::Module(module)) = bound else {
                let main = super::host::MODULES
                    .get(usize::from(MAIN_STATE))
                    .copied()
                    .unwrap_or("");
                let api = HOST_APIS
                    .iter()
                    .any(|&(m, n)| m == main && n.as_bytes() == name);
                if bound.is_some() && api {
                    return Err(Unsupported::Binding("a host module"));
                }
                return Err(Unsupported::Callee);
            };
            let module_name = super::host::MODULES
                .get(usize::from(module))
                .copied()
                .unwrap_or("");
            let api = HOST_APIS
                .iter()
                .position(|&(m, n)| m == module_name && n.as_bytes() == name)
                .and_then(|i| u16::try_from(i).ok())
                .ok_or(Unsupported::Callee)?;
            if module != MAIN_STATE {
                return Err(Unsupported::HostApi(api));
            }
            Ok(Callee::Host(api))
        }
        Expr::Local(d) => match an.upvalue(inst, d) {
            Some(Captured::Closure(j)) => Ok(Callee::Helper(j)),
            _ => Err(Unsupported::Callee),
        },
        _ => Err(Unsupported::Callee),
    }
}

/// Admits the callables a converted table holds (design.md "beatoraja
/// Lua Skins", effects by role): the effect walk over every root decides
/// whether the candidate is closed and which roots write
/// ([`super::effects`]), then each root is recorded as classified and
/// every other callable as inert or captured. Only a cap, cancellation
/// or the time budget refuses the candidate here, fatal; what a root does
/// is decided by role where it is consumed.
pub(crate) fn admit(arena: &Arena) -> Result<(), ImportError> {
    let Some(lua) = arena.lua.as_deref() else {
        return Ok(());
    };
    let Some(an) = lua.analysis.as_deref() else {
        return Ok(());
    };
    // A fatal stop located at the construct it met, or, for the time
    // budget, at the callable its phase retained.
    let stop = |node: Option<u32>, f: Found| {
        let at = if f.why == TOO_LONG {
            node.map(|n| arena.loc(n))
        } else if f.line > 0 {
            Some(an.loc(&f))
        } else {
            node.map(|n| arena.loc(n))
        };
        ImportError::Lua {
            at,
            why: fatal(f.why).unwrap_or(super::LuaWhy::Limit(super::LuaLimit::AnalysisWork)),
        }
    };
    super::effects::walk(an, arena).map_err(|(node, f)| stop(node, f))?;
    let unit = |why| Found {
        why,
        file: 0,
        line: 0,
    };
    an.clock
        .phase(super::clock::Phase::Admit, None, || {
            an.outcomes
                .borrow_mut()
                .resize(lua.callables.len(), Disposition::NeverAnalysed);
            for (_, c) in callable_nodes(arena) {
                // Each callable visited is a unit of work, cancellation
                // seen.
                an.charge(1)?;
                let Some(callable) = lua.callables.get(c as usize) else {
                    continue;
                };
                if an.outcomes.borrow().get(c as usize) != Some(&Disposition::NeverAnalysed) {
                    continue;
                }
                let d = if callable.root {
                    Disposition::ClassifiedOnly
                } else if callable.instance == NONE {
                    Disposition::Inert
                } else {
                    Disposition::Captured
                };
                an.set_outcome(c, d);
            }
            Ok(())
        })
        .map_err(|w| stop(None, unit(w)))?;
    an.admitted.set(true);
    Ok(())
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
