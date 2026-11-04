//! Lua skins (design.md "beatoraja Lua Skins" and "Threat Model", Lua
//! skins; docs/stage3.md §3, Lua): a `.luaskin` entry that returns its
//! definition as a table.
//!
//! [`read_lua_header`] runs the header pass: a fresh VM ([`sandbox`])
//! runs the entry with no `skin_config`, and the table it returns is
//! converted ([`convert`]) and read through the definition's header path.
//! [`LuaHeader::import`] runs the configured pass for a [`Config`] and a
//! snapshot of import facts ([`facts`]): another fresh VM runs the entry
//! with a pass-local `skin_config` ([`config`]) and `main_state.option`
//! answering the snapshot's facts, and the table it returns is converted
//! into the resolved representation and imported through
//! [`crate::import`], the one import path. The passes share only the
//! immutable snapshots of the sources they read ([`loader`]) and the
//! candidate's accounting ([`ledger`]); each VM is dropped when its pass
//! ends, before the common import. Changing the settings runs the
//! configured pass again from the header pass's state.
//!
//! A function the returned table holds is a typed callable, never run.
//! The configured pass parses every chunk it loads with the importer's
//! own parser ([`syntax`]) and runs it instrumented ([`instrument`]), so
//! that each closure is tagged with its constructor and a capture reader;
//! the conversion copies each callable's captures before the VM is
//! dropped, and every callable is classified before the common import
//! ([`callback::admit`]): one with an effect the analysis does not rule
//! out refuses the candidate. The import converts the write-free ones
//! where it consumes them (`import::callback`). The header pass refuses
//! a function where its reading of the header meets it, as a value, a
//! record, a list or a condition, so that the configured pass runs and
//! the report sees what else it meets.
//!
//! What the common reader reads of the JSON files a table includes is
//! charged to the candidate as the passes are: its bytes count toward the
//! same allowance in both passes, its files join the dependencies, and it
//! sees the cancellation request.
//!
//! Every failure is owned and crosses threads: the results and errors
//! hold no Lua handle.

pub(crate) mod callback;
#[cfg(test)]
mod callback_tests;
mod config;
mod convert;
pub mod facts;
mod host;
mod instrument;
mod ledger;
mod library;
pub mod limits;
mod loader;
mod sandbox;
#[cfg(feature = "dev-tools")]
pub mod survey;
pub(crate) mod syntax;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(feature = "dev-tools")]
pub mod vm_hooks;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use mlua::{Lua, MultiValue, RegistryKey, Value};
use skin_model::{FileId, Root, ScreenKind};

pub use self::callback::Disposition;
use self::config::Bridge;
pub use self::convert::MAX_CONVERTED_BYTES;
pub use self::facts::{
    ChartRank, Fact, FactDeps, FactId, FactRead, FactValue, ImportFacts, JudgeClass,
};
pub use self::host::HOST_APIS;
pub use self::ledger::{Cancel, LuaUsage};
use self::ledger::{Counts, Ledger, Stop};
pub(crate) use self::loader::Store;
use self::sandbox::host;
use crate::error::{ImportError, Loc};
use crate::header::Header;
use crate::import::{Config, Imported, import};
use crate::resolve::{Definition, resolve};
use crate::tree::Arena;

/// A cap of the sandbox ([`limits`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LuaLimit {
    /// [`limits::MAX_INSTRUCTIONS`].
    Instructions,
    /// [`limits::MAX_LUA_MEMORY`]: an allocation failed.
    Memory,
    /// An error handler did not complete: it failed, recursed or ran out
    /// of stack or memory.
    Handler,
    /// [`limits::MAX_HOST_CALLS`].
    HostCalls,
    /// [`limits::MAX_NATIVE_WORK`].
    NativeWork,
    /// [`limits::MAX_LUA_TIME`].
    Time,
    /// [`limits::MAX_AST_NODES`].
    AstNodes,
    /// [`limits::MAX_AST_DEPTH`].
    AstDepth,
    /// [`limits::MAX_CAPTURES`].
    Captures,
    /// [`limits::MAX_EXPANDED_NODES`].
    Expansion,
    /// [`limits::MAX_ANALYSIS_WORK`].
    AnalysisWork,
    /// The generated source's bound: an instrumented chunk's
    /// ([`instrument::MAX_ADDED_BYTES`]), the rewritten chunks a pass
    /// keeps ([`limits::MAX_REWRITTEN_BYTES`]) or a lowered expression's
    /// ([`skin_model::MAX_SOURCE_BYTES`]).
    Generated,
}

impl fmt::Display for LuaLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Instructions => "the instruction limit",
            Self::Memory => "the memory limit",
            Self::Handler => "an error handler's failure",
            Self::HostCalls => "the host call limit",
            Self::NativeWork => "the library work limit",
            Self::Time => "the Lua time limit",
            Self::AstNodes => "the analysis node limit",
            Self::AstDepth => "the analysis depth limit",
            Self::Captures => "the capture limit",
            Self::Expansion => "the helper expansion limit",
            Self::AnalysisWork => "the analysis work limit",
            Self::Generated => "the generated source limit",
        })
    }
}

/// Why the sandbox, a loader or the conversion refused a Lua skin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LuaWhy {
    /// A cap was reached.
    Limit(LuaLimit),
    /// Cancellation was requested.
    Cancelled,
    /// A chunk does not compile, or is not text.
    Syntax,
    /// An error the skin raised and did not catch, or one raised while a
    /// loader ran a part.
    Error,
    /// A loader's argument is not a name.
    LoaderName,
    /// A module required while it loads.
    RequireCycle,
    /// A metatable with `__gc`, `__mode` or `__close`.
    Finalizer,
    /// A library operation the sandbox's library does not implement, or a
    /// primitive's result over its cap ([`limits::MAX_PRIMITIVE_BYTES`]).
    Library { op: &'static str },
    /// A host API function called during import, by its index in
    /// [`HOST_APIS`]: there is no play state to read.
    LiveState { api: u16 },
    /// A read of an import fact the snapshot has as unavailable
    /// ([`facts::Fact::Unavailable`]), by host API and id.
    Unavailable { api: u16, id: i32 },
    /// A host API's argument that is not a Lua integer within `i32`.
    FactArgument,
    /// The entry returned no table.
    Returned,
    /// A table key that is neither a string nor an integer, or a table the
    /// conversion cannot read.
    Key,
    /// A table with both string and integer keys.
    Mixed,
    /// A table of integer keys other than `1..=n`.
    Sparse,
    /// A table inside itself.
    Cycle,
    /// A value other than a table, string, number, boolean or function.
    Value,
    /// A string that is not UTF-8.
    NotUtf8,
    /// A function where field `field` takes none.
    Callable { field: &'static str },
    /// A write to a host module's table, directly or through an alias.
    HostWrite,
    /// A chunk Lua compiles whose bindings the analysis's parser cannot
    /// establish.
    Unbound,
    /// A callback with an effect the analysis does not rule out, located
    /// at its construct.
    Effect(Unsupported),
    /// A play-critical callback in field `field` that does not convert.
    Critical {
        field: &'static str,
        why: Unsupported,
    },
}

/// Why a callback does not convert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// A construct outside the converted form.
    Construct(&'static str),
    /// A global other than `skin_config`.
    Global,
    /// A host module or helper not bound once as a chunk local.
    Binding(&'static str),
    /// A host API outside the bridges, by its index in [`HOST_APIS`]: a
    /// writer, or a reader with no conversion.
    HostApi(u16),
    /// A call of something that is not a host API or a helper.
    Callee,
    /// `and`, `or` or `not` of an integer, whose truthiness is Lua's.
    Truthiness,
    /// Operands of types the operator does not take.
    Type,
    /// A helper that calls itself.
    Recursion,
    /// A captured value or binding not proven.
    Capture(&'static str),
    /// A read of host API `api` with an id that has no mapping.
    Unmapped { api: u16, id: i64 },
    /// An integer intermediate that could leave the expressions' range.
    Range,
    /// A configuration read in a play-critical callback.
    Config,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let api = |a: u16| {
            let (m, n) = HOST_APIS.get(usize::from(a)).copied().unwrap_or(("", ""));
            format!("{m}.{n}")
        };
        match self {
            Self::Construct(c) => write!(f, "an unsupported construct ({c})"),
            Self::Global => f.write_str("a global other than skin_config"),
            Self::Binding(b) => write!(f, "{b} not bound once as a chunk local"),
            Self::HostApi(a) => write!(f, "{} has no conversion", api(*a)),
            Self::Callee => f.write_str("a call of an unknown function"),
            Self::Truthiness => f.write_str("an integer used as a condition"),
            Self::Type => f.write_str("operands of an unsupported type"),
            Self::Recursion => f.write_str("a helper that calls itself"),
            Self::Capture(c) => write!(f, "{c} not proven"),
            Self::Unmapped { api: a, id } => write!(f, "{}({id}) has no mapping", api(*a)),
            Self::Range => f.write_str("an integer that could leave the expression range"),
            Self::Config => f.write_str("a configuration read in a play-critical callback"),
        }
    }
}

impl fmt::Display for LuaWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(l) => write!(f, "{l} was reached"),
            Self::Cancelled => f.write_str("the import was cancelled"),
            Self::Syntax => f.write_str("a Lua chunk does not compile"),
            Self::Error => f.write_str("a Lua error"),
            Self::LoaderName => f.write_str("a loader's argument is not a file name"),
            Self::RequireCycle => f.write_str("a module requires itself"),
            Self::Finalizer => f.write_str("a metatable with __gc, __mode or __close"),
            Self::Library { op } => write!(f, "an unsupported library operation: {op}"),
            Self::LiveState { api } => {
                let (m, n) = HOST_APIS
                    .get(usize::from(*api))
                    .copied()
                    .unwrap_or(("", ""));
                write!(f, "{m}.{n} reads play state during import")
            }
            Self::Unavailable { api, id } => {
                let (m, n) = HOST_APIS
                    .get(usize::from(*api))
                    .copied()
                    .unwrap_or(("", ""));
                write!(f, "{m}.{n}({id}) reads a fact this scene cannot give")
            }
            Self::FactArgument => f.write_str("a host API's argument is not an integer id"),
            Self::Returned => f.write_str("the skin returned no table"),
            Self::Key => f.write_str("a table key that is not a string or an integer"),
            Self::Mixed => f.write_str("a table with string and integer keys"),
            Self::Sparse => f.write_str("an array with holes"),
            Self::Cycle => f.write_str("a table inside itself"),
            Self::Value => f.write_str("an unsupported Lua value"),
            Self::NotUtf8 => f.write_str("a string that is not UTF-8"),
            Self::Callable { field } => {
                write!(f, "an unsupported function in `{field}`")
            }
            Self::HostWrite => f.write_str("a write to a host module"),
            Self::Unbound => f.write_str("a Lua chunk the analysis cannot bind"),
            Self::Effect(why) => write!(f, "a callback with an unknown effect: {why}"),
            Self::Critical { field, why } => {
                write!(f, "a play-critical callback in `{field}`: {why}")
            }
        }
    }
}

/// A module's state in a pass's `require` cache.
pub(crate) enum Module {
    Absent,
    Loading,
    Done(Value),
}

/// A parsed file's chunk as the configured pass compiles it: instrumented,
/// or `None` to run as written (it has no constructor).
type Chunk = Option<Arc<[u8]>>;

/// One pass's state, shared by the VM's host functions.
pub(crate) struct Pass {
    ledger: Ledger,
    root: Root,
    entry_dir: String,
    store: RefCell<Store>,
    /// The files this pass loaded, the entry first, and as a set.
    modules: RefCell<Vec<String>>,
    loaded: RefCell<std::collections::HashSet<String>>,
    /// `require`'s cache: names to identities, identities to values.
    names: RefCell<HashMap<String, FileId>>,
    cache: RefCell<HashMap<FileId, Option<RegistryKey>>>,
    depth: Cell<u32>,
    /// The host API functions, by address.
    hosts: RefCell<HashMap<usize, u16>>,
    handled: Cell<u64>,
    last_error: RefCell<Option<Loc>>,
    bridge: Option<Bridge>,
    /// The configured pass's import facts, and the facts it read once
    /// each, in first-read order.
    facts: Option<ImportFacts>,
    reads: RefCell<Vec<FactRead>>,
    /// The fact reads by allowlisted label, for the report.
    #[cfg(feature = "dev-tools")]
    fact_counts: RefCell<std::collections::BTreeMap<String, u32>>,
    /// The configured pass's chunks by identity, and the parsed files
    /// that have constructors, in the order they were parsed.
    parsed: RefCell<HashMap<FileId, Chunk>>,
    files: RefCell<Vec<callback::ParsedFile>>,
    /// The bytes of the rewritten chunks kept
    /// ([`limits::MAX_REWRITTEN_BYTES`]).
    rewritten: Cell<usize>,
    #[cfg(feature = "dev-tools")]
    timings: Cell<Timings>,
}

/// What [`LuaHeader::timed`] gives: the stages' times of an import, or
/// the pending result [`LuaHeader::import`] gives.
#[cfg(feature = "dev-tools")]
#[derive(Debug)]
pub enum LuaTimed {
    Imported(Timings),
    Pending(LuaPending),
}

/// How long a configured pass's stages took, for the benchmarks: the
/// VM's creation and the entry's run, the conversion, the VM's teardown,
/// and the common import.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub execution: std::time::Duration,
    pub conversion: std::time::Duration,
    pub teardown: std::time::Duration,
    pub import: std::time::Duration,
}

impl Pass {
    fn new(
        root: &Root,
        entry: &str,
        store: Store,
        counts: Counts,
        spent: Duration,
        cancel: &Cancel,
        bridge: Option<(Bridge, ImportFacts)>,
    ) -> Self {
        let (bridge, facts) = bridge.unzip();
        let entry_dir = entry
            .rsplit_once(['/', '\\'])
            .map_or(String::new(), |(d, _)| d.to_owned());
        Self {
            ledger: Ledger::new(counts, spent, cancel.clone()),
            root: root.clone(),
            entry_dir,
            store: RefCell::new(store),
            modules: RefCell::new(Vec::new()),
            loaded: RefCell::new(std::collections::HashSet::new()),
            names: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            depth: Cell::new(0),
            hosts: RefCell::new(HashMap::new()),
            handled: Cell::new(0),
            last_error: RefCell::new(None),
            bridge,
            facts,
            reads: RefCell::new(Vec::new()),
            #[cfg(feature = "dev-tools")]
            fact_counts: RefCell::new(std::collections::BTreeMap::new()),
            parsed: RefCell::new(HashMap::new()),
            files: RefCell::new(Vec::new()),
            rewritten: Cell::new(0),
            #[cfg(feature = "dev-tools")]
            timings: Cell::new(Timings::default()),
        }
    }

    fn source(
        &self,
        key: &str,
        at: impl Fn() -> Option<Loc>,
    ) -> Result<(FileId, Arc<[u8]>), ImportError> {
        let mut used = self.ledger.counts().input_bytes;
        let r = self
            .store
            .borrow_mut()
            .source(&self.root, key, &mut used, at);
        self.ledger.update(|c| c.input_bytes = used);
        r
    }

    fn module(&self, name: &str) {
        if self.loaded.borrow_mut().insert(name.to_owned()) {
            self.modules.borrow_mut().push(name.to_owned());
        }
    }

    fn handled(&self, at: Option<Loc>) {
        self.handled.set(self.handled.get() + 1);
        *self.last_error.borrow_mut() = at;
    }

    fn handled_count(&self) -> u64 {
        self.handled.get()
    }

    fn last_error(&self) -> Option<Loc> {
        self.last_error.borrow().clone()
    }

    /// Records the read `read` unless its fact was read before.
    fn read_fact(&self, read: FactRead) {
        let mut reads = self.reads.borrow_mut();
        if !reads.iter().any(|r| r.fact == read.fact) {
            reads.push(read);
        }
    }

    /// Counts one fact read under `label`, for the report.
    #[cfg(feature = "dev-tools")]
    fn count_fact(&self, label: impl FnOnce() -> String) {
        *self.fact_counts.borrow_mut().entry(label()).or_default() += 1;
    }

    #[cfg(not(feature = "dev-tools"))]
    fn count_fact(&self, _: impl FnOnce() -> String) {}

    fn host_api(&self, addr: usize, api: u16) {
        self.hosts.borrow_mut().insert(addr, api);
    }

    fn host_api_of(&self, addr: usize) -> Option<u16> {
        self.hosts.borrow().get(&addr).copied()
    }

    /// The cached value of the module `key` named before, or a fault for
    /// one still loading.
    fn cached(&self, lua: &Lua, key: &str) -> mlua::Result<Option<Value>> {
        let id = self.names.borrow().get(key).cloned();
        match id {
            Some(id) => match self.cached_id(lua, &id)? {
                Module::Loading => Err(self.ledger.fail_lua(lua, LuaWhy::RequireCycle)),
                Module::Done(v) => Ok(Some(v)),
                Module::Absent => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn cached_id(&self, lua: &Lua, id: &FileId) -> mlua::Result<Module> {
        let cache = self.cache.borrow();
        Ok(match cache.get(id) {
            None => Module::Absent,
            Some(None) => Module::Loading,
            Some(Some(k)) => Module::Done(lua.registry_value(k)?),
        })
    }

    fn start_module(&self, id: FileId, key: &str) {
        self.names.borrow_mut().insert(key.to_owned(), id.clone());
        self.cache.borrow_mut().insert(id, None);
    }

    fn finish_module(&self, lua: &Lua, id: &FileId, v: &Value) -> mlua::Result<()> {
        let k = lua.create_registry_value(v.clone())?;
        if let Some(slot) = self.cache.borrow_mut().get_mut(id) {
            *slot = Some(k);
        }
        Ok(())
    }
}

/// A header pass's state over `root`, for tests that drive a VM.
#[cfg(test)]
fn test_pass(root: Root) -> Pass {
    Pass::new(
        &root,
        "s.luaskin",
        Store::default(),
        Counts::default(),
        Duration::ZERO,
        &Cancel::new(),
        None,
    )
}

/// The header pass's result: the header, and what the configured pass
/// starts from. Owned, with no Lua state.
#[derive(Debug)]
pub struct LuaHeader {
    root: Root,
    entry: String,
    header: Header,
    store: Store,
    counts: Counts,
    /// The time the header pass ran Lua.
    pub(crate) spent: Duration,
}

/// A Lua skin imported.
#[derive(Debug)]
pub struct LuaImported {
    pub imported: Imported,
    /// Every source file the passes consulted, the entry first.
    pub dependencies: Vec<String>,
    /// What the candidate used of the sandbox's caps.
    pub usage: LuaUsage,
    /// What became of each function the configured definition holds, in
    /// conversion order.
    pub callbacks: Vec<Disposition>,
    /// The scene it was imported for and the facts it read.
    pub facts: FactDeps,
}

/// A configured pass that reached a fact its snapshot has pending: what
/// it read before, the fact, and the files and usage so far, with no
/// definition.
#[derive(Debug)]
pub struct LuaPending {
    /// The scene and the facts read before the pending one.
    pub facts: FactDeps,
    pub pending: FactId,
    /// Every source file the passes consulted, the entry first.
    pub dependencies: Vec<String>,
    pub usage: LuaUsage,
}

/// What [`LuaHeader::import`] gives for a snapshot.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one outcome an import, moved to its caller once"
)]
pub enum LuaOutcome {
    Imported(LuaImported),
    Pending(LuaPending),
}

/// One pass's outcome: the converted table, and the files it loaded.
struct Converted {
    arena: Arena,
    top: u32,
    entry: FileId,
}

/// Runs one pass over `entry` in a fresh VM, starting from `counts` and
/// the time `spent` by the candidate's earlier pass, with `bridge` as
/// `skin_config` when configured; the VM is dropped before it returns.
fn pass(
    root: &Root,
    entry: &str,
    store: Store,
    (counts, spent): (Counts, Duration),
    cancel: &Cancel,
    bridge: Option<(Bridge, ImportFacts)>,
) -> (
    Result<Converted, Stop>,
    Store,
    Counts,
    Duration,
    Vec<FactRead>,
) {
    #[cfg(test)]
    tests::pass_witness();
    let pass = Rc::new(Pass::new(root, entry, store, counts, spent, cancel, bridge));
    let r = run_pass(&pass, entry);
    let counts = pass.ledger.counts();
    let spent = pass.ledger.spent();
    let store = pass.store.replace(Store::default());
    let reads = pass.reads.take();
    #[cfg(feature = "dev-tools")]
    {
        LAST_TIMINGS.with(|t| t.set(pass.timings.get()));
        LAST_FACTS.with(|f| f.replace(pass.fact_counts.take()));
    }
    (r, store, counts, spent, reads)
}

#[cfg(feature = "dev-tools")]
thread_local! {
    /// The stages of this thread's last pass.
    static LAST_TIMINGS: Cell<Timings> = Cell::new(Timings::default());
    /// The fact reads of this thread's last pass, by label.
    pub(crate) static LAST_FACTS: RefCell<std::collections::BTreeMap<String, u32>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
}

/// [`pass`]'s work: the VM lives only inside.
fn run_pass(pass: &Rc<Pass>, entry: &str) -> Result<Converted, Stop> {
    if pass.ledger.cancelled() {
        return Err(Stop::Fault(ImportError::Lua {
            at: None,
            why: LuaWhy::Cancelled,
        }));
    }
    #[cfg(feature = "dev-tools")]
    let start = std::time::Instant::now();
    let r = sandbox::vm(pass).map(|lua| {
        let run = loader::compile(&lua, pass, entry).and_then(|(f, id)| {
            let values = loader::run(&lua, pass, f, MultiValue::new())?;
            pass.ledger.memory(lua.used_memory());
            Ok((values, id))
        });
        pass.ledger.stop_clock();
        #[cfg(feature = "dev-tools")]
        let executed = std::time::Instant::now();
        let converted = run.map(|(values, id)| {
            let value = values.into_iter().next().unwrap_or(Value::Nil);
            let modules = pass.modules.borrow().clone();
            convert::convert(&lua, pass, value, &id, modules).map(|(arena, top)| Converted {
                arena,
                top,
                entry: id,
            })
        });
        #[cfg(feature = "dev-tools")]
        let done = std::time::Instant::now();
        drop(lua);
        #[cfg(feature = "dev-tools")]
        pass.timings.set(Timings {
            execution: executed - start,
            conversion: done - executed,
            teardown: done.elapsed(),
            import: std::time::Duration::ZERO,
        });
        converted
    });
    if let Some(stop) = pass.ledger.take_stop() {
        return Err(stop);
    }
    match r.and_then(|r| r) {
        Ok(converted) => converted.map_err(Stop::Fault),
        Err(mlua::Error::MemoryError(_)) => Err(Stop::Fault(ImportError::Lua {
            at: None,
            why: LuaWhy::Limit(LuaLimit::Memory),
        })),
        Err(_) => Err(Stop::Fault(ImportError::Lua {
            at: None,
            why: LuaWhy::Error,
        })),
    }
}

/// A stage's result at the candidate boundary with what the candidate
/// used and consulted so far, kept whether or not it succeeded.
pub(crate) struct Outcome<T, E = ImportError> {
    pub(crate) result: Result<T, E>,
    pub(crate) counts: Counts,
    pub(crate) dependencies: Vec<String>,
    /// What became of each callable of the configured definition.
    pub(crate) callbacks: Vec<callback::Disposition>,
    /// The facts the configured pass read, however far it got.
    pub(crate) reads: Vec<FactRead>,
}

/// The header pass, with its metadata.
pub(crate) fn header_pass(root: &Path, entry: &str, cancel: &Cancel) -> Outcome<LuaHeader> {
    let root = match Root::new(root) {
        Ok(root) => root,
        Err(source) => {
            return Outcome {
                result: Err(ImportError::Read {
                    at: None,
                    name: root.display().to_string(),
                    source,
                }),
                counts: Counts::default(),
                dependencies: Vec::new(),
                callbacks: Vec::new(),
                reads: Vec::new(),
            };
        }
    };
    let (r, mut store, mut counts, spent, _) = pass(
        &root,
        entry,
        Store::default(),
        (Counts::default(), Duration::ZERO),
        cancel,
        None,
    );
    let result = match r {
        Ok(converted) => {
            let mut def = definition(&root, converted, counts, store, cancel);
            let header =
                resolve(&mut def, &std::collections::BTreeSet::new()).and_then(|mut r| r.header());
            store = common_reads(&mut def, &mut counts);
            header
        }
        Err(Stop::Fault(e)) => Err(e),
        // The header pass has no facts, so it never reads a pending one.
        Err(Stop::Pending(_)) => Err(ImportError::Lua {
            at: None,
            why: LuaWhy::Error,
        }),
    };
    let dependencies = store.dependencies.clone();
    Outcome {
        result: result.map(|header| LuaHeader {
            root,
            entry: entry.to_owned(),
            header,
            store,
            counts,
            spent,
        }),
        counts,
        dependencies,
        callbacks: Vec::new(),
        reads: Vec::new(),
    }
}

/// Runs the header pass of the Lua skin `entry`, a name relative to the
/// skin folder `root`, and reads its header as a JSON definition's.
///
/// # Errors
///
/// The folder cannot be read; the sandbox, a loader or the conversion
/// refuses ([`ImportError::Lua`], [`ImportError::Read`],
/// [`ImportError::Input`]), the header's reading meets a function
/// ([`LuaWhy::Callable`]); or the header refuses as a definition's does.
pub fn read_lua_header(
    root: &Path,
    entry: &str,
    cancel: &Cancel,
) -> Result<LuaHeader, ImportError> {
    header_pass(root, entry, cancel).result
}

fn definition(
    root: &Root,
    c: Converted,
    counts: Counts,
    store: Store,
    cancel: &Cancel,
) -> Definition {
    Definition::from_lua(
        root.clone(),
        c.entry,
        c.arena,
        c.top,
        counts.input_bytes,
        counts.loads,
        store,
        cancel.clone(),
    )
}

/// Refuses a configured definition whose `type` names another kind than
/// `slot` ([`ImportError::Slot`], located at it), then admits its
/// callables ([`callback::admit`]). A configured pass may return another
/// type than its header pass, and that refusal comes before any
/// callable's.
fn admit(def: &mut Definition, slot: ScreenKind) -> Result<(), ImportError> {
    let (found, node) = def.kind()?;
    if found != slot {
        return Err(ImportError::Slot {
            at: Some(def.loc(node)),
            slot,
            found,
        });
    }
    callback::admit(&def.arena)
}

/// Carries what the common reader did with `def`'s includes into the
/// candidate: the bytes and loads it charged, which the next pass starts
/// from, and the snapshots it read and added, its files among the
/// dependencies; the snapshots are taken back.
fn common_reads(def: &mut Definition, counts: &mut Counts) -> Store {
    counts.input_bytes = counts.input_bytes.max(def.input_bytes());
    counts.loads = counts.loads.max(def.parses());
    def.take_store()
}

impl LuaHeader {
    /// The header, which [`Config`]s are made against.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// What the header pass used.
    #[must_use]
    pub fn usage(&self) -> LuaUsage {
        self.counts
    }

    /// The files the header pass consulted, the entry first.
    #[must_use]
    pub fn dependencies(&self) -> &[String] {
        &self.store.dependencies
    }

    /// Runs the configured pass with `config` against the import facts
    /// `facts` and imports the table it returns. It starts from the header
    /// pass's state each time. A read of a fact `facts` has pending ends
    /// it with [`LuaOutcome::Pending`].
    ///
    /// # Errors
    ///
    /// What [`read_lua_header`] refuses; a snapshot of another scene than
    /// the skin's kind ([`ImportError::Slot`]), by its header or by the
    /// `type` the configured pass returns, or of a kind no importer maps
    /// yet ([`ImportError::NoImporter`]); a fact it has unavailable
    /// ([`LuaWhy::Unavailable`]) or a bad id ([`LuaWhy::FactArgument`]);
    /// and what [`crate::import`] refuses of the converted definition.
    pub fn import(
        &self,
        config: &Config,
        facts: &ImportFacts,
        cancel: &Cancel,
    ) -> Result<LuaOutcome, ImportError> {
        let o = self.complete(config, facts, cancel);
        let deps = FactDeps {
            scene: facts.scene,
            reads: o.reads,
        };
        match o.result {
            Ok(imported) => Ok(LuaOutcome::Imported(LuaImported {
                imported,
                dependencies: o.dependencies,
                usage: o.counts,
                callbacks: o.callbacks,
                facts: deps,
            })),
            Err(Stop::Pending(pending)) => Ok(LuaOutcome::Pending(LuaPending {
                facts: deps,
                pending,
                dependencies: o.dependencies,
                usage: o.counts,
            })),
            Err(Stop::Fault(e)) => Err(e),
        }
    }

    /// Runs the configured pass with `config` and `facts` and imports its
    /// table, as [`Self::import`] does, and returns how long each stage
    /// took, or the pending result [`Self::import`] gives.
    ///
    /// # Errors
    ///
    /// What [`Self::import`] refuses.
    #[cfg(feature = "dev-tools")]
    pub fn timed(
        &self,
        config: &Config,
        facts: &ImportFacts,
        cancel: &Cancel,
    ) -> Result<LuaTimed, ImportError> {
        let (def, counts, store, reads) = self.configured(config, facts, cancel);
        let mut def = match def {
            Ok(def) => def,
            Err(Stop::Fault(e)) => return Err(e),
            Err(Stop::Pending(pending)) => {
                return Ok(LuaTimed::Pending(LuaPending {
                    facts: FactDeps {
                        scene: facts.scene,
                        reads,
                    },
                    pending,
                    dependencies: store.dependencies,
                    usage: counts,
                }));
            }
        };
        admit(&mut def, facts.scene)?;
        let mut t = LAST_TIMINGS.with(Cell::get);
        let start = std::time::Instant::now();
        std::hint::black_box(import(&mut def, &self.header, config, facts.scene)?);
        t.import = start.elapsed();
        Ok(LuaTimed::Imported(t))
    }

    /// The configured pass and the common import, with the candidate's
    /// metadata kept on every outcome.
    pub(crate) fn complete(
        &self,
        config: &Config,
        facts: &ImportFacts,
        cancel: &Cancel,
    ) -> Outcome<Imported, Stop> {
        let (def, counts, store, reads) = self.configured(config, facts, cancel);
        self.finish(def, counts, store, reads, config)
    }

    /// The common import of a configured pass's outcome into the slot of
    /// the header's kind, which [`Self::configured`] checked the snapshot's
    /// scene against, with the candidate's metadata kept on every outcome.
    pub(crate) fn finish(
        &self,
        def: Result<Definition, Stop>,
        mut counts: Counts,
        mut store: Store,
        reads: Vec<FactRead>,
        config: &Config,
    ) -> Outcome<Imported, Stop> {
        let slot = self.header.kind;
        let mut callbacks = Vec::new();
        let result = def.and_then(|mut def| {
            let r =
                admit(&mut def, slot).and_then(|()| import(&mut def, &self.header, config, slot));
            store = common_reads(&mut def, &mut counts);
            if let Some(an) = def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref()) {
                counts.expanded = an.expanded.get();
                counts.analysis_work = an.work.get();
                callbacks = an.outcomes.borrow().clone();
            }
            r.map_err(Stop::Fault)
        });
        Outcome {
            result,
            counts,
            dependencies: store.dependencies,
            callbacks,
            reads,
        }
    }

    /// The configured pass: the converted definition, holding the
    /// candidate's snapshots, its usage and the facts it read; on failure
    /// or a pending fact, the stop with the usage, the snapshots and the
    /// facts read so far. A snapshot of another scene than the header's
    /// kind ([`ImportError::Slot`]), or of a kind no importer maps yet
    /// ([`ImportError::NoImporter`]), refuses before the configured pass
    /// runs, so no asset or configured-only include is read.
    pub(crate) fn configured(
        &self,
        config: &Config,
        facts: &ImportFacts,
        cancel: &Cancel,
    ) -> (Result<Definition, Stop>, Counts, Store, Vec<FactRead>) {
        let refused = if facts.scene != self.header.kind {
            Some(ImportError::Slot {
                at: None,
                slot: facts.scene,
                found: self.header.kind,
            })
        } else if facts.scene != ScreenKind::Play {
            Some(ImportError::NoImporter { kind: facts.scene })
        } else {
            None
        };
        if let Some(e) = refused {
            return (
                Err(Stop::Fault(e)),
                self.counts,
                self.store.clone(),
                Vec::new(),
            );
        }
        let bridge = Bridge::new(&self.header, config);
        let (r, store, counts, _, reads) = pass(
            &self.root,
            &self.entry,
            self.store.clone(),
            (self.counts, self.spent),
            cancel,
            Some((bridge, *facts)),
        );
        match r {
            Ok(converted) => (
                Ok(definition(&self.root, converted, counts, store, cancel)),
                counts,
                Store::default(),
                reads,
            ),
            Err(e) => (Err(e), counts, store, reads),
        }
    }
}
