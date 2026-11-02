//! Lua skins (design.md "beatoraja Lua Skins" and "Threat Model", Lua
//! skins; docs/stage3.md §3, Lua): a `.luaskin` entry that returns its
//! definition as a table.
//!
//! [`read_lua_header`] runs the header pass: a fresh VM ([`sandbox`])
//! runs the entry with no `skin_config`, and the table it returns is
//! converted ([`convert`]) and read through the definition's header path.
//! [`LuaHeader::import`] runs the configured pass for a [`Config`]: another
//! fresh VM runs the entry with the read-only `skin_config`
//! ([`config`]), and the table it returns is converted into the resolved
//! representation and imported through [`crate::import`], the one import
//! path. The passes share only the immutable snapshots of the sources
//! they read ([`loader`]) and the candidate's accounting ([`ledger`]);
//! each VM is dropped when its pass ends, before the common import.
//! Changing the settings runs the configured pass again from the header
//! pass's state.
//!
//! A function the returned table holds is a typed callable, never run:
//! until per-frame functions are converted, a callable that reaches a
//! play-critical element, a number's value (which may show the score) or
//! a field that takes no script refuses the import at the function's
//! definition; one that reaches a decorative element's condition, timer
//! or value freezes it as never drawn (a false condition, an off timer),
//! with a note.
//!
//! Every failure is owned and crosses threads: the results and errors
//! hold no Lua handle.

mod config;
mod convert;
mod host;
mod ledger;
pub mod limits;
mod loader;
mod sandbox;
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
    /// The C stack ran out while an error was handled.
    Stack,
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
            Self::Stack => "the stack while an error was handled",
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
    /// A write to `skin_config`, which is read-only.
    ConfigWrite,
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
            Self::ConfigWrite => f.write_str("a write to the read-only skin_config"),
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
    /// The tables of `skin_config`, by address.
    views: RefCell<Vec<usize>>,
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
            views: RefCell::new(Vec::new()),
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
    let pass = Rc::new(Pass::new(root, entry, store, counts, cancel, bridge));
    let r = run_pass(&pass, entry);
    let counts = pass.ledger.counts();
    let store = pass.store.replace(Store::default());
    (r, store, counts)
}

/// [`pass`]'s work: the VM lives only inside.
fn run_pass(pass: &Rc<Pass>, entry: &str) -> Result<Converted, ImportError> {
    if pass.ledger.cancelled() {
        return Err(ImportError::Lua {
            at: None,
            why: LuaWhy::Cancelled,
        });
    }
    let r = sandbox::vm(pass).map(|lua| {
        let run = loader::compile(&lua, pass, entry).and_then(|(f, id)| {
            let values = loader::run(&lua, pass, f, MultiValue::new())?;
            pass.ledger.memory(lua.used_memory());
            Ok((values, id))
        });
        let converted = run.map(|(values, id)| {
            let value = values.into_iter().next().unwrap_or(Value::Nil);
            let modules = pass.modules.borrow().clone();
            convert::convert(pass, value, &id, modules).map(|(arena, top)| Converted {
                arena,
                top,
                entry: id,
            })
        });
        drop(lua);
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

/// Runs the header pass of the Lua skin `entry`, a name relative to the
/// skin folder `root`, and reads its header as a JSON definition's.
///
/// # Errors
///
/// The folder cannot be read; the sandbox, a loader or the conversion
/// refuses ([`ImportError::Lua`], [`ImportError::Read`],
/// [`ImportError::Input`]); or the header refuses as a definition's does.
pub fn read_lua_header(
    root: &Path,
    entry: &str,
    cancel: &Cancel,
) -> Result<LuaHeader, ImportError> {
    let root = Root::new(root).map_err(|source| ImportError::Read {
        at: None,
        name: root.display().to_string(),
        source,
    })?;
    let (r, store, counts) = pass(
        &root,
        entry,
        Store::default(),
        Counts::default(),
        cancel,
        None,
    );
    let converted = r?;
    let mut def = definition(&root, converted, counts);
    let header = resolve(&mut def, &std::collections::BTreeSet::new())?.header()?;
    Ok(LuaHeader {
        root,
        entry: entry.to_owned(),
        header,
        store,
        counts,
    })
}

fn definition(root: &Root, c: Converted, counts: Counts) -> Definition {
    Definition::from_lua(root.clone(), c.entry, c.arena, c.top, counts.input_bytes)
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
        let (def, counts, dependencies) = self.configured(config, cancel)?;
        let mut def = def;
        let imported = import(&mut def, &self.header, config)?;
        Ok(LuaImported {
            imported,
            dependencies,
            usage: counts,
        })
    }

    /// The configured pass: the converted definition, the candidate's
    /// usage and its dependencies.
    fn configured(
        &self,
        config: &Config,
        cancel: &Cancel,
    ) -> Result<(Definition, Counts, Vec<String>), ImportError> {
        let bridge = Bridge::new(&self.header, config);
        let (r, store, counts) = pass(
            &self.root,
            &self.entry,
            self.store.clone(),
            self.counts,
            cancel,
            Some(bridge),
        );
        let converted = r?;
        if cancel.is_cancelled() {
            return Err(ImportError::Lua {
                at: None,
                why: LuaWhy::Cancelled,
            });
        }
        Ok((
            definition(&self.root, converted, counts),
            counts,
            store.dependencies,
        ))
    }
}
