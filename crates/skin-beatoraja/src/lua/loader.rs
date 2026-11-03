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

use std::rc::Rc;
use std::sync::Arc;

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, MultiValue, Value};
use skin_model::{FileId, MAX_PATH_BYTES, Root, RootError};

use super::{LuaLimit, LuaWhy, Module, Pass, host, sandbox};
use crate::error::{ImportError, InputLimit, Loc};
use crate::limits::{MAX_FILE_BYTES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES, MAX_INPUT_BYTES};
use crate::lua::ledger::running;

/// One source read: the name it was asked by, its identity and its bytes.
#[derive(Clone, Debug)]
struct Snapshot {
    key: String,
    id: FileId,
    bytes: Arc<[u8]>,
}

/// The sources one candidate read, shared by its passes as they were
/// first read, and the files consulted, in order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Store {
    snapshots: Vec<Snapshot>,
    pub(crate) dependencies: Vec<String>,
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
        let found = self.snapshots.iter().find(|s| s.key == key).cloned();
        let (id, bytes) = match found {
            Some(s) => (s.id, s.bytes),
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
                let bytes = match self.snapshots.iter().find(|s| s.id == id) {
                    Some(s) => Arc::clone(&s.bytes),
                    None => Arc::from(bytes),
                };
                self.snapshots.push(Snapshot {
                    key: key.to_owned(),
                    id: id.clone(),
                    bytes: Arc::clone(&bytes),
                });
                (id, bytes)
            }
        };
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > MAX_FILE_BYTES {
            return Err(ImportError::Input {
                at: at(),
                limit: InputLimit::FileBytes,
            });
        }
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
        if !self.dependencies.iter().any(|d| d == name) {
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
    let f = lua
        .load(&*bytes)
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
    Ok((f, id))
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
pub(super) fn require(lua: &Lua, pass: &Rc<Pass>) -> mlua::Result<Function> {
    host(lua, pass, |lua, pass, args| {
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
pub(super) fn dofile(lua: &Lua, pass: &Rc<Pass>) -> mlua::Result<Function> {
    host(lua, pass, |lua, pass, args| {
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
