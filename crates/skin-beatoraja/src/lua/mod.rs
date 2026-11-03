//! Lua skins (design.md "beatoraja Lua Skins" and "Threat Model", Lua
//! skins; docs/stage3.md §3, Lua): a `.luaskin` entry that returns its
//! definition as a table.
//!
//! [`read_lua_header`] runs the header pass: a fresh VM ([`sandbox`])
//! runs the entry with no `skin_config`, and the table it returns is
//! converted ([`convert`]) and read through the definition's header path.
//! [`LuaHeader::import`] runs the configured pass for a [`Config`]: another
//! fresh VM runs the entry with a pass-local `skin_config`
//! ([`config`]), and the table it returns is converted into the resolved
//! representation and imported through [`crate::import`], the one import
//! path. The passes share only the immutable snapshots of the sources
//! they read ([`loader`]) and the candidate's accounting ([`ledger`]);
//! each VM is dropped when its pass ends, before the common import.
//! Changing the settings runs the configured pass again from the header
//! pass's state.
//!
//! A function the returned table holds is a typed callable, never run:
//! until per-frame functions are analysed (S7-03), any function anywhere
//! in the configured pass's table refuses the candidate at its definition,
//! after the developer report has counted them ([`convert::callables`]);
//! the header pass refuses one only where its reading of the header
//! meets it, as a value, a record, a list or a condition, so that the
//! configured pass runs and the report sees what else it meets.
//!
//! What the common reader reads of the JSON files a table includes is
//! charged to the candidate as the passes are: its bytes count toward the
//! same allowance in both passes, its files join the dependencies, and it
//! sees the cancellation request.
//!
//! Every failure is owned and crosses threads: the results and errors
//! hold no Lua handle.

mod config;
mod convert;
mod host;
mod ledger;
mod library;
pub mod limits;
mod loader;
mod sandbox;
#[cfg(feature = "dev-tools")]
pub mod survey;
#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use mlua::{Lua, MultiValue, RegistryKey, Value};
use skin_model::{FileId, Root};

use self::config::Bridge;
pub use self::convert::MAX_CONVERTED_BYTES;
pub use self::host::HOST_APIS;
pub use self::ledger::{Cancel, LuaUsage};
use self::ledger::{Counts, Ledger};
use self::loader::Store;
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
}

impl fmt::Display for LuaLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Instructions => "the instruction limit",
            Self::Memory => "the memory limit",
            Self::Handler => "an error handler's failure",
            Self::HostCalls => "the host call limit",
            Self::NativeWork => "the library work limit",
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
    /// A metatable with `__gc` or `__mode`.
    Finalizer,
    /// A library operation the sandbox's library does not implement, or a
    /// primitive's result over its cap ([`limits::MAX_PRIMITIVE_BYTES`]).
    Library { op: &'static str },
    /// A host API function called during import, by its index in
    /// [`HOST_APIS`]: there is no play state to read.
    LiveState { api: u16 },
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
    /// A function where field `field` takes none, or where its element
    /// could show a play-critical value.
    Callable { field: &'static str },
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
            Self::Finalizer => f.write_str("a metatable with __gc or __mode"),
            Self::Library { op } => write!(f, "an unsupported library operation: {op}"),
            Self::LiveState { api } => {
                let (m, n) = HOST_APIS
                    .get(usize::from(*api))
                    .copied()
                    .unwrap_or(("", ""));
                write!(f, "{m}.{n} reads play state during import")
            }
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
        }
    }
}

/// A module's state in a pass's `require` cache.
pub(crate) enum Module {
    Absent,
    Loading,
    Done(Value),
}

/// One pass's state, shared by the VM's host functions.
pub(crate) struct Pass {
    ledger: Ledger,
    root: Root,
    entry_dir: String,
    store: RefCell<Store>,
    /// The files this pass loaded, the entry first.
    modules: RefCell<Vec<String>>,
    /// `require`'s cache: names to identities, identities to values.
    names: RefCell<Vec<(String, FileId)>>,
    cache: RefCell<Vec<(FileId, Option<RegistryKey>)>>,
    depth: Cell<u32>,
    /// The host API functions, by address.
    hosts: RefCell<HashMap<usize, u16>>,
    handled: Cell<u64>,
    last_error: RefCell<Option<Loc>>,
    bridge: Option<Bridge>,
    #[cfg(feature = "dev-tools")]
    timings: Cell<Timings>,
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
        cancel: &Cancel,
        bridge: Option<Bridge>,
    ) -> Self {
        let entry_dir = entry
            .rsplit_once(['/', '\\'])
            .map_or(String::new(), |(d, _)| d.to_owned());
        Self {
            ledger: Ledger::new(counts, cancel.clone()),
            root: root.clone(),
            entry_dir,
            store: RefCell::new(store),
            modules: RefCell::new(Vec::new()),
            names: RefCell::new(Vec::new()),
            cache: RefCell::new(Vec::new()),
            depth: Cell::new(0),
            hosts: RefCell::new(HashMap::new()),
            handled: Cell::new(0),
            last_error: RefCell::new(None),
            bridge,
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
        let mut m = self.modules.borrow_mut();
        if !m.iter().any(|n| n == name) {
            m.push(name.to_owned());
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

    fn host_api(&self, addr: usize, api: u16) {
        self.hosts.borrow_mut().insert(addr, api);
    }

    fn host_api_of(&self, addr: usize) -> Option<u16> {
        self.hosts.borrow().get(&addr).copied()
    }

    /// The cached value of the module `key` named before, or a fault for
    /// one still loading.
    fn cached(&self, lua: &Lua, key: &str) -> mlua::Result<Option<Value>> {
        let id = self
            .names
            .borrow()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, id)| id.clone());
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
        Ok(match cache.iter().find(|(i, _)| i == id) {
            None => Module::Absent,
            Some((_, None)) => Module::Loading,
            Some((_, Some(k))) => Module::Done(lua.registry_value(k)?),
        })
    }

    fn start_module(&self, id: FileId, key: &str) {
        self.names.borrow_mut().push((key.to_owned(), id.clone()));
        self.cache.borrow_mut().push((id, None));
    }

    fn finish_module(&self, lua: &Lua, id: &FileId, v: &Value) -> mlua::Result<()> {
        let k = lua.create_registry_value(v.clone())?;
        if let Some(slot) = self.cache.borrow_mut().iter_mut().find(|(i, _)| i == id) {
            slot.1 = Some(k);
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
}

/// A Lua skin imported.
#[derive(Debug)]
pub struct LuaImported {
    pub imported: Imported,
    /// Every source file the passes consulted, the entry first.
    pub dependencies: Vec<String>,
    /// What the candidate used of the sandbox's caps.
    pub usage: LuaUsage,
}

/// One pass's outcome: the converted table, and the files it loaded.
struct Converted {
    arena: Arena,
    top: u32,
    entry: FileId,
}

/// Runs one pass over `entry` in a fresh VM, starting from `counts`, with
/// `bridge` as `skin_config` when configured; the VM is dropped before it
/// returns.
fn pass(
    root: &Root,
    entry: &str,
    store: Store,
    counts: Counts,
    cancel: &Cancel,
    bridge: Option<Bridge>,
) -> (Result<Converted, ImportError>, Store, Counts) {
    #[cfg(test)]
    tests::pass_witness();
    let pass = Rc::new(Pass::new(root, entry, store, counts, cancel, bridge));
    let r = run_pass(&pass, entry);
    let counts = pass.ledger.counts();
    let store = pass.store.replace(Store::default());
    #[cfg(feature = "dev-tools")]
    LAST_TIMINGS.with(|t| t.set(pass.timings.get()));
    (r, store, counts)
}

#[cfg(feature = "dev-tools")]
thread_local! {
    /// The stages of this thread's last pass.
    static LAST_TIMINGS: Cell<Timings> = Cell::new(Timings::default());
}

/// [`pass`]'s work: the VM lives only inside.
fn run_pass(pass: &Rc<Pass>, entry: &str) -> Result<Converted, ImportError> {
    if pass.ledger.cancelled() {
        return Err(ImportError::Lua {
            at: None,
            why: LuaWhy::Cancelled,
        });
    }
    #[cfg(feature = "dev-tools")]
    let start = std::time::Instant::now();
    let r = sandbox::vm(pass).map(|lua| {
        let run = loader::compile(&lua, pass, entry).and_then(|(f, id)| {
            let values = loader::run(&lua, pass, f, MultiValue::new())?;
            pass.ledger.memory(lua.used_memory());
            Ok((values, id))
        });
        #[cfg(feature = "dev-tools")]
        let executed = std::time::Instant::now();
        let converted = run.map(|(values, id)| {
            let value = values.into_iter().next().unwrap_or(Value::Nil);
            let modules = pass.modules.borrow().clone();
            convert::convert(pass, value, &id, modules).map(|(arena, top)| Converted {
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
    if let Some(fault) = pass.ledger.take_fault() {
        return Err(fault);
    }
    match r.and_then(|r| r) {
        Ok(converted) => converted,
        Err(mlua::Error::MemoryError(_)) => Err(ImportError::Lua {
            at: None,
            why: LuaWhy::Limit(LuaLimit::Memory),
        }),
        Err(_) => Err(ImportError::Lua {
            at: None,
            why: LuaWhy::Error,
        }),
    }
}

/// A stage's result at the candidate boundary with what the candidate
/// used and consulted so far, kept whether or not it succeeded.
pub(crate) struct Outcome<T> {
    pub(crate) result: Result<T, ImportError>,
    pub(crate) counts: Counts,
    pub(crate) dependencies: Vec<String>,
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
            };
        }
    };
    let (r, mut store, mut counts) = pass(
        &root,
        entry,
        Store::default(),
        Counts::default(),
        cancel,
        None,
    );
    let result = r.and_then(|converted| {
        let mut def = definition(&root, converted, counts, cancel);
        let header =
            resolve(&mut def, &std::collections::BTreeSet::new()).and_then(|mut r| r.header());
        common_reads(&def, &mut counts, &mut store);
        header
    });
    let dependencies = store.dependencies.clone();
    Outcome {
        result: result.map(|header| LuaHeader {
            root,
            entry: entry.to_owned(),
            header,
            store,
            counts,
        }),
        counts,
        dependencies,
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

fn definition(root: &Root, c: Converted, counts: Counts, cancel: &Cancel) -> Definition {
    Definition::from_lua(
        root.clone(),
        c.entry,
        c.arena,
        c.top,
        counts.input_bytes,
        cancel.clone(),
    )
}

/// Carries what the common reader read of `def`'s includes into the
/// candidate: their bytes, which the next pass starts from, and their
/// names among the dependencies.
fn common_reads(def: &Definition, counts: &mut Counts, store: &mut Store) {
    counts.input_bytes = counts.input_bytes.max(def.input_bytes());
    for name in def.included() {
        store.depend(name);
    }
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

    /// Runs the configured pass with `config` and imports the table it
    /// returns. It starts from the header pass's state each time.
    ///
    /// # Errors
    ///
    /// What [`read_lua_header`] refuses, and what [`crate::import`]
    /// refuses of the converted definition.
    pub fn import(&self, config: &Config, cancel: &Cancel) -> Result<LuaImported, ImportError> {
        let o = self.complete(config, cancel);
        o.result.map(|imported| LuaImported {
            imported,
            dependencies: o.dependencies,
            usage: o.counts,
        })
    }

    /// Runs the configured pass with `config` and imports its table, as
    /// [`Self::import`] does, and returns how long each stage took.
    ///
    /// # Errors
    ///
    /// What [`Self::import`] refuses.
    #[cfg(feature = "dev-tools")]
    pub fn timed(&self, config: &Config, cancel: &Cancel) -> Result<Timings, ImportError> {
        let mut def = self.configured(config, cancel).0?;
        convert::refuse_callables(&def.arena)?;
        let mut t = LAST_TIMINGS.with(Cell::get);
        let start = std::time::Instant::now();
        std::hint::black_box(import(&mut def, &self.header, config)?);
        t.import = start.elapsed();
        Ok(t)
    }

    /// The configured pass and the common import, with the candidate's
    /// metadata kept on every outcome.
    pub(crate) fn complete(&self, config: &Config, cancel: &Cancel) -> Outcome<Imported> {
        let (def, counts, store) = self.configured(config, cancel);
        self.finish(def, counts, store, config)
    }

    /// The common import of a configured pass's outcome, with the
    /// candidate's metadata kept on every outcome.
    pub(crate) fn finish(
        &self,
        def: Result<Definition, ImportError>,
        mut counts: Counts,
        mut store: Store,
        config: &Config,
    ) -> Outcome<Imported> {
        let result = def.and_then(|mut def| {
            let r = convert::refuse_callables(&def.arena)
                .and_then(|()| import(&mut def, &self.header, config));
            common_reads(&def, &mut counts, &mut store);
            r
        });
        Outcome {
            result,
            counts,
            dependencies: store.dependencies,
        }
    }

    /// The configured pass: the converted definition, the candidate's
    /// usage and its sources; on failure, the error with the usage and
    /// sources so far.
    pub(crate) fn configured(
        &self,
        config: &Config,
        cancel: &Cancel,
    ) -> (Result<Definition, ImportError>, Counts, Store) {
        let bridge = Bridge::new(&self.header, config);
        let (r, store, counts) = pass(
            &self.root,
            &self.entry,
            self.store.clone(),
            self.counts,
            cancel,
            Some(bridge),
        );
        let def = r.map(|converted| definition(&self.root, converted, counts, cancel));
        (def, counts, store)
    }
}
