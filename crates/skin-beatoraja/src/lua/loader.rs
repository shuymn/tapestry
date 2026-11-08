//! The confined loaders and the source snapshots.
//!
//! Lua comes only as text through the skin folder's [`Root`], named as
//! includes are ([`Root::check_name`]: no rooted, drive, UNC or `:`
//! names, `..` never above the folder, the canonical path inside it, a
//! regular file). Neither loader searches the working directory,
//! environment paths or native modules.
//!
//! - `require(name)` loads the module `name`, whose dot-separated parts
//!   (letters, digits, `_` and `-`) become folders of the entry's folder
//!   and the last a `.lua` file, as the source's search path of the skin
//!   folder does; it runs it once a pass, caching its value by the file's
//!   canonical identity, and refuses a module that requires itself again
//!   while it loads. `main_state`, `timer_util` and `event_util` are the
//!   host's modules ([`super::host`]).
//! - `dofile(name)` runs the file `name`, relative to the entry's folder,
//!   on every call and returns its chunk's results: how skins compose
//!   themselves from the selected local parts.
//!
//! Each invocation is charged before it does anything (one of
//! [`MAX_INCLUDE_PARSES`], at most [`MAX_INCLUDE_DEPTH`] active), and
//! each read of a source, a snapshot reused included, is charged its
//! bytes. A loader that fails, its source or the chunk it runs, is a
//! fault: a required part that does not load never leaves a partial
//! definition behind a protected call.
//!
//! In the configured pass each chunk is parsed by the analysis's parser
//! first ([`super::syntax`], once a pass per file) and compiled
//! instrumented ([`super::instrument`]); a chunk Lua compiles that the
//! parser refuses is a fault ([`LuaWhy::Unbound`]), and one neither
//! accepts is Lua's syntax error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, MultiValue, Value};
use skin_model::{FileId, MAX_PATH_BYTES, Root, RootError};

use super::callback::ParsedFile;
use super::limits::MAX_REWRITTEN_BYTES;
use super::syntax::{self, Fail};
use super::{LuaLimit, LuaWhy, Module, Pass, host, instrument, sandbox};
use crate::error::{ImportError, InputLimit, Loc};
use crate::limits::{MAX_FILE_BYTES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES, MAX_INPUT_BYTES};
use crate::lua::ledger::running;

/// One source read: its identity and its bytes, found by the name it was
/// asked by ([`Store::source`]).
#[derive(Clone, Debug)]
struct Snapshot {
    id: FileId,
    bytes: Arc<[u8]>,
}

/// The sources one candidate read, shared by its passes as they were
/// first read, and the files consulted, in order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Store {
    snapshots: Vec<Snapshot>,
    /// [`Self::snapshots`] by the name each was asked by, and by identity:
    /// one hash a load.
    by_key: HashMap<String, usize>,
    by_id: HashMap<FileId, usize>,
    pub(crate) dependencies: Vec<String>,
    /// [`Self::dependencies`] as a set.
    depended: HashSet<String>,
}

impl Store {
    /// The source `key` names: its snapshot, or the file read through
    /// `root` with what remains of the input allowance. Its bytes are
    /// charged to `used` either way.
    pub(crate) fn source(
        &mut self,
        root: &Root,
        key: &str,
        used: &mut u64,
        at: impl Fn() -> Option<Loc>,
    ) -> Result<(FileId, Arc<[u8]>), ImportError> {
        let found = self
            .by_key
            .get(key)
            .and_then(|&i| self.snapshots.get(i))
            .map(|s| (s.id.clone(), Arc::clone(&s.bytes)));
        let (id, bytes) = match found {
            Some(found) => found,
            None => {
                let max = MAX_FILE_BYTES.min(MAX_INPUT_BYTES.saturating_sub(*used));
                let (bytes, id) = root.read(key, max).map_err(|source| match source {
                    RootError::TooLarge { len, .. } => ImportError::Input {
                        at: at(),
                        limit: if len > MAX_FILE_BYTES {
                            InputLimit::FileBytes
                        } else {
                            InputLimit::InputBytes
                        },
                    },
                    source => ImportError::Read {
                        at: at(),
                        name: key.to_owned(),
                        source,
                    },
                })?;
                // Another spelling of a file already read shares its
                // snapshot.
                let bytes = match self.by_id.get(&id).and_then(|&i| self.snapshots.get(i)) {
                    Some(s) => Arc::clone(&s.bytes),
                    None => Arc::from(bytes),
                };
                let i = self.snapshots.len();
                self.by_key.insert(key.to_owned(), i);
                self.by_id.entry(id.clone()).or_insert(i);
                self.snapshots.push(Snapshot {
                    id: id.clone(),
                    bytes: Arc::clone(&bytes),
                });
                (id, bytes)
            }
        };
        // A snapshot was read within MAX_FILE_BYTES; only the cumulative
        // allowance is checked again.
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if used.saturating_add(len) > MAX_INPUT_BYTES {
            return Err(ImportError::Input {
                at: at(),
                limit: InputLimit::InputBytes,
            });
        }
        *used += len;
        self.depend(id.name());
        Ok((id, bytes))
    }

    /// Records `name` among the files consulted, once.
    pub(crate) fn depend(&mut self, name: &str) {
        if self.depended.insert(name.to_owned()) {
            self.dependencies.push(name.to_owned());
        }
    }
}

/// `name` joined to the entry's folder `dir`.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Whether `name` is a module name: dot-separated parts of ASCII letters,
/// digits, `_` and `-`.
fn module_name(name: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(name).ok()?;
    let ok = s.split('.').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    ok.then(|| format!("{}.lua", s.replace('.', "/")))
}

/// Reads and compiles the source `key` names, charging it: its chunk and
/// identity, or a fault when it cannot be read or does not compile.
pub(super) fn compile(lua: &Lua, pass: &Pass, key: &str) -> mlua::Result<(Function, FileId)> {
    pass.ledger.check(lua)?;
    let (id, bytes) = pass
        .source(key, || running(lua))
        .map_err(|e| pass.ledger.fail(e))?;
    pass.ledger.check(lua)?;
    let name = id.name().to_owned();
    pass.module(&name);
    let code = if pass.bridge.is_some() {
        instrumented(lua, pass, &id, &bytes)?
    } else {
        None
    };
    let f = lua
        .load(code.as_deref().unwrap_or(&bytes))
        .set_name(format!("@{name}"))
        .set_mode(ChunkMode::Text)
        .into_function();
    pass.ledger.check(lua)?;
    let f = f.map_err(|e| {
        let why = match e {
            mlua::Error::MemoryError(_) => LuaWhy::Limit(LuaLimit::Memory),
            _ => LuaWhy::Syntax,
        };
        let at = match &e {
            mlua::Error::SyntaxError { message, .. } => syntax_loc(&name, message),
            _ => None,
        };
        pass.ledger.fail(ImportError::Lua {
            at: at.or_else(|| {
                Some(Loc {
                    file: name.clone(),
                    line: 0,
                    col: 0,
                })
            }),
            why,
        })
    })?;
    if code.is_none() {
        return Ok((f, id));
    }
    // The instrumented chunk returns the skin's chunk, given the tagger.
    let tagger: Function = lua.named_registry_value(sandbox::TAGGER)?;
    let inner = f.call::<Function>(tagger).map_err(|e| {
        let why = match e {
            mlua::Error::MemoryError(_) => LuaWhy::Limit(LuaLimit::Memory),
            _ => LuaWhy::Error,
        };
        pass.ledger.fail_lua(lua, why)
    })?;
    pass.ledger.check(lua)?;
    Ok((inner, id))
}

/// The instrumented chunk of source `id`, parsed and charged on its first
/// load in the pass; `None` when it has no constructor, or when neither
/// the parser nor Lua reads it (Lua's own compilation then reports its
/// syntax error): the source is compiled as it is.
fn instrumented(
    lua: &Lua,
    pass: &Pass,
    id: &FileId,
    bytes: &[u8],
) -> mlua::Result<Option<Arc<[u8]>>> {
    if let Some(code) = pass.parsed.borrow().get(id).cloned() {
        return Ok(code);
    }
    let name = id.name().to_owned();
    let at = |line: u32| {
        Some(Loc {
            file: name.clone(),
            line,
            col: 0,
        })
    };
    let fail = |line: u32, why: LuaWhy| pass.ledger.fail(ImportError::Lua { at: at(line), why });
    let mut nodes = pass.ledger.counts().ast_nodes;
    let r = syntax::parse(bytes, &mut nodes, &|| pass.ledger.cancelled());
    pass.ledger.update(|c| c.ast_nodes = nodes);
    let ast = match r {
        Ok(ast) => ast,
        Err(Fail::Nodes) => return Err(fail(0, LuaWhy::Limit(LuaLimit::AstNodes))),
        Err(Fail::Depth(line)) => return Err(fail(line, LuaWhy::Limit(LuaLimit::AstDepth))),
        Err(Fail::Cancelled) => return Err(fail(0, LuaWhy::Cancelled)),
        Err(Fail::Syntax(line)) => {
            let compiles = lua
                .load(bytes)
                .set_name(format!("@{name}"))
                .set_mode(ChunkMode::Text)
                .into_function()
                .is_ok();
            pass.ledger.check(lua)?;
            return if compiles {
                Err(fail(line, LuaWhy::Unbound))
            } else {
                Ok(None)
            };
        }
    };
    // A chunk with no constructor makes no closure: it runs as written.
    if ast.funcs.len() <= 1 {
        pass.parsed.borrow_mut().insert(id.clone(), None);
        return Ok(None);
    }
    let Ok(file) = u16::try_from(pass.files.borrow().len()) else {
        return Err(fail(0, LuaWhy::Limit(LuaLimit::AstNodes)));
    };
    // The rewritten chunks the pass keeps share one allowance, checked
    // before each is built.
    let used = pass.rewritten.get();
    let budget = usize::try_from(MAX_REWRITTEN_BYTES)
        .unwrap_or(usize::MAX)
        .saturating_sub(used);
    let code = match instrument::instrument(bytes, &ast, file, budget, &|| pass.ledger.cancelled())
    {
        Ok(code) => code,
        Err(instrument::Stop::Generated) => {
            return Err(fail(0, LuaWhy::Limit(LuaLimit::Generated)));
        }
        Err(instrument::Stop::Cancelled) => return Err(fail(0, LuaWhy::Cancelled)),
    };
    pass.rewritten.set(used.saturating_add(code.len()));
    let code: Arc<[u8]> = Arc::from(code);
    pass.files.borrow_mut().push(ParsedFile { name, ast });
    pass.parsed
        .borrow_mut()
        .insert(id.clone(), Some(Arc::clone(&code)));
    Ok(Some(code))
}

/// The line of a syntax error in chunk `name`: Lua prefixes its message
/// with the chunk's name and the line (`ldebug.c`, `luaG_addinfo`).
fn syntax_loc(name: &str, message: &str) -> Option<Loc> {
    let rest = message.strip_prefix(name)?.strip_prefix(':')?;
    let digits = rest.split(':').next()?;
    Some(Loc {
        file: name.to_owned(),
        line: digits.parse().ok()?,
        col: 0,
    })
}

/// Runs `f` with `args` through the base library's `xpcall` and the
/// host's handler: its results, or a fault for any error, located where
/// it was raised.
pub(super) fn run(
    lua: &Lua,
    pass: &Pass,
    f: Function,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let xpcall: Function = lua.named_registry_value(sandbox::XPCALL)?;
    let handler: Function = lua.named_registry_value(sandbox::HANDLER)?;
    // The results come back packed: the native xpcall pushes its status
    // after them without making room.
    let packed: Function = lua.named_registry_value(sandbox::PACKED)?;
    let seen = pass.handled_count();
    let mut call = MultiValue::new();
    call.push_back(Value::Function(packed));
    call.push_back(Value::Function(handler));
    call.push_back(Value::Function(f));
    call.extend(args);
    let mut r = match xpcall.call::<MultiValue>(call) {
        Ok(r) => r,
        Err(e) => {
            if pass.ledger.faulted() {
                return Err(e);
            }
            let why = match e {
                mlua::Error::MemoryError(_) => LuaWhy::Limit(LuaLimit::Memory),
                _ => LuaWhy::Error,
            };
            return Err(pass.ledger.fail_lua(lua, why));
        }
    };
    pass.ledger.check(lua)?;
    let ok = r.pop_front().is_some_and(|v| v.as_boolean() == Some(true));
    if ok {
        let Some(Value::Table(t)) = r.pop_front() else {
            return Err(pass.ledger.fail_lua(lua, LuaWhy::Error));
        };
        let n: i64 = t.raw_get("n")?;
        if n > sandbox::MAX_RESULTS {
            return Err(pass.ledger.fail_lua(
                lua,
                LuaWhy::Library {
                    op: sandbox::TOO_MANY,
                },
            ));
        }
        return (1..=n).map(|i| t.raw_get::<Value>(i)).collect();
    }
    if pass.handled_count() == seen {
        // No handler ran: Lua runs none for an allocation failure.
        return Err(pass.ledger.fail_lua(lua, LuaWhy::Limit(LuaLimit::Memory)));
    }
    let at = pass.last_error();
    Err(pass.ledger.fail(ImportError::Lua {
        at,
        why: LuaWhy::Error,
    }))
}

/// Charges one load invocation and checks the depth, before anything is
/// read.
fn invocation(lua: &Lua, pass: &Pass) -> mlua::Result<()> {
    let loads = pass.ledger.counts().loads;
    if loads >= MAX_INCLUDE_PARSES {
        return Err(pass.ledger.fail(ImportError::Input {
            at: running(lua),
            limit: InputLimit::IncludeParses,
        }));
    }
    pass.ledger.update(|c| c.loads = loads + 1);
    if pass.depth.get() >= MAX_INCLUDE_DEPTH {
        return Err(pass.ledger.fail(ImportError::Input {
            at: running(lua),
            limit: InputLimit::IncludeDepth,
        }));
    }
    Ok(())
}

/// Runs `f` one loader deeper.
fn deeper(lua: &Lua, pass: &Pass, f: Function, args: MultiValue) -> mlua::Result<MultiValue> {
    pass.depth.set(pass.depth.get() + 1);
    let r = run(lua, pass, f, args);
    pass.depth.set(pass.depth.get().saturating_sub(1));
    r
}

/// A name a loader refuses before reading.
fn refused(lua: &Lua, pass: &Pass, name: String) -> mlua::Error {
    pass.ledger.fail(ImportError::Read {
        at: running(lua),
        name,
        source: RootError::Rejected,
    })
}

/// The string argument of a loader.
fn name_arg(lua: &Lua, pass: &Pass, args: &MultiValue) -> mlua::Result<Vec<u8>> {
    // The length is checked on the borrowed bytes, before any copy.
    match args.front() {
        Some(Value::String(s)) if s.as_bytes().len() <= MAX_PATH_BYTES => Ok(s.as_bytes().to_vec()),
        _ => Err(pass.ledger.fail_lua(lua, LuaWhy::LoaderName)),
    }
}

/// The confined `require`.
pub(super) fn require(lua: &Lua, pass: &Arc<Pass>) -> mlua::Result<Function> {
    host(lua, pass, 1, |lua, pass, args| {
        invocation(lua, pass)?;
        pass.ledger.update(|c| c.requires += 1);
        let name = name_arg(lua, pass, &args)?;
        if let Some(t) = super::host::module(lua, &name)? {
            return Ok(Value::Table(t));
        }
        let Some(file) = module_name(&name) else {
            return Err(refused(
                lua,
                pass,
                String::from_utf8_lossy(&name).into_owned(),
            ));
        };
        let key = join(&pass.entry_dir, &file);
        if let Some(v) = pass.cached(lua, &key)? {
            return Ok(v);
        }
        let (f, id) = compile(lua, pass, &key)?;
        match pass.cached_id(lua, &id)? {
            Module::Loading => {
                return Err(pass.ledger.fail_lua(lua, LuaWhy::RequireCycle));
            }
            Module::Done(v) => return Ok(v),
            Module::Absent => {}
        }
        pass.start_module(id.clone(), &key);
        let mut args = MultiValue::new();
        args.push_back(Value::String(lua.create_string(&name)?));
        args.push_back(Value::String(lua.create_string(&key)?));
        let r = deeper(lua, pass, f, args)?;
        let v = match r.into_iter().next() {
            None | Some(Value::Nil) => Value::Boolean(true),
            Some(v) => v,
        };
        pass.finish_module(lua, &id, &v)?;
        Ok(v)
    })
}

/// The confined `dofile`.
pub(super) fn dofile(lua: &Lua, pass: &Arc<Pass>) -> mlua::Result<Function> {
    host(lua, pass, 1, |lua, pass, args| {
        invocation(lua, pass)?;
        pass.ledger.update(|c| c.dofiles += 1);
        let name = name_arg(lua, pass, &args)?;
        let Ok(name) = String::from_utf8(name) else {
            return Err(pass.ledger.fail_lua(lua, LuaWhy::LoaderName));
        };
        // A wildcard is a file choice left unresolved.
        if name.contains('*') {
            return Err(refused(lua, pass, name));
        }
        if let Err(source) = Root::check_name(&name) {
            return Err(pass.ledger.fail(ImportError::Read {
                at: running(lua),
                name,
                source,
            }));
        }
        let key = join(&pass.entry_dir, &name);
        let (f, _) = compile(lua, pass, &key)?;
        deeper(lua, pass, f, MultiValue::new())
    })
}

#[cfg(test)]
mod tests {
    use super::{module_name, syntax_loc};

    #[test]
    fn module_names_are_dotted_identifiers() {
        assert_eq!(module_name(b"a.b-c.d_e").as_deref(), Some("a/b-c/d_e.lua"));
        assert_eq!(module_name(b"x").as_deref(), Some("x.lua"));
        for bad in [
            &b""[..],
            b"a..b",
            b".a",
            b"a/b",
            b"a\\b",
            b"c:x",
            b"a b",
            b"\xff",
        ] {
            assert_eq!(module_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn syntax_errors_are_located_by_their_line() {
        let at = syntax_loc("a/b.lua", "a/b.lua:12: unexpected symbol near '}'").unwrap();
        assert_eq!((at.file.as_str(), at.line), ("a/b.lua", 12));
        assert!(syntax_loc("a/b.lua", "other:1: x").is_none());
    }
}
