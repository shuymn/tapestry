//! A fresh VM and its environment, built from an allowlist.
//!
//! The VM opens only the `string`, `table` and `math` libraries besides
//! the base functions, under a memory limit set before any skin code
//! runs and an instruction hook installed as the global hook, which every
//! thread the host creates inherits. The globals are a new table holding
//! only:
//!
//! - the base functions `assert`, `getmetatable`, `next`, `pairs`,
//!   `tostring`, `type` and `_VERSION`, and `_G`;
//! - the sandbox's own library ([`super::library`]), an allowlist of what
//!   the owner's skins use: `select`, `error`, `ipairs`, `tonumber`,
//!   `setmetatable` and parts of `string`, `table` and `math`, whose long
//!   loops are metered Lua and never coerce a string to a number or back
//!   (the VM's own `string`, `table` and `math` tables stay out of reach,
//!   so `string.dump`, binary packing and random sources are absent);
//!   `setmetatable` attaches a snapshot of the metatable the skin passes,
//!   refusing one with `__gc` (a finalizer), `__mode` (weak references),
//!   either of which lets the collector change what the skin computes,
//!   or `__close` (code run while an error unwinds): a later change to
//!   the skin's table never reaches the snapshot, and `getmetatable`
//!   gives the skin its own table back;
//! - `pcall` and `xpcall`, which return an ordinary error as Lua does but
//!   raise a fault of the ledger again, never running the skin's handler
//!   once one is recorded, and refuse an error no handler saw (an
//!   allocation failure);
//! - the confined loaders `require` and `dofile` ([`super::loader`]).
//!
//! There is no `io`, `os`, `debug`, `package`, `load`, `loadfile`,
//! `collectgarbage`, `print`, `warn`, `coroutine` or `utf8`. Every
//! function the host adds to the environment, and every one the
//! protected calls use, charges one host call before its work; the
//! library's private `charge`, `refuse` and `metatable` callbacks, O(1),
//! do not.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Value, VmState};

use super::limits::{INSTRUCTION_STEP, MAX_LUA_MEMORY};
use super::{LuaLimit, LuaWhy, Pass, loader};

/// The registry name of the base library's `xpcall`.
pub(super) const XPCALL: &str = "tapestry.xpcall";
/// The registry name of the function that calls its first argument with
/// the rest and returns the results packed in one table (`n` their
/// count), for the host's protected runs.
pub(super) const PACKED: &str = "tapestry.packed";
/// The registry name of the handler the host's protected runs use.
pub(super) const HANDLER: &str = "tapestry.handler";
/// The registry name of the function that makes a host function's Lua
/// face: it raises the fault in Lua when the host reports one.
const SHIM: &str = "tapestry.shim";

/// The registry names of a configured pass's tags: closures to their
/// constructors' identities, and to their capture readers, both weak in
/// their keys; and the tagger the instrumented chunks call
/// ([`super::instrument`]).
pub(super) const TAG_IDS: &str = "tapestry.tag_ids";
pub(super) const TAG_READERS: &str = "tapestry.tag_readers";
pub(super) const TAGGER: &str = "tapestry.tagger";

/// The tagger: it records a closure's identity and reader and returns the
/// closure. The sandbox's own Lua: no skin can name it.
const TAG: &str = "local ids, readers = ...\n\
return function(id, f, r) ids[f] = id readers[f] = r return f end";

/// The error value Lua carries once the ledger holds a fault: a fixed,
/// small string, whatever the fault.
pub(super) const FAULT: &str = "the import was refused";

/// The host functions' Lua faces, compiled before any skin code. A host
/// function returns its status first and never a Rust error, which mlua
/// would wrap with a traceback outside the VM's memory limit: a fault
/// is raised here, in Lua, as [`FAULT`].
const SHIMS: &str = r##"
local error, fault = ...
local function check(ok, ...)
  if not ok then
    error(fault, 0)
  end
  return ...
end
-- A face passes its host function a fixed number of arguments, extras
-- dropped here: mlua converts every argument before the host function
-- runs, and a failure there would be an error the host never sees.
return function(raw, arity)
  if arity == 0 then
    return function()
      return check(raw())
    end
  end
  return function(a)
    return check(raw(a))
  end
end
"##;

/// The most instruction steps the hook lets run after a fault before it
/// raises the fault again: the protected calls raise it in Lua as they
/// unwind, and the hook's own error (mlua raises it unwrapped, with no
/// traceback) is raised again only past this grace, every step.
const STEPS_AFTER_FAULT: u32 = 1_000;

/// The base functions the environment keeps as they are.
const BASE: [&str; 7] = [
    "assert",
    "getmetatable",
    "next",
    "pairs",
    "tostring",
    "type",
    "_VERSION",
];

/// The protected-call wrappers, compiled in the sandbox before any skin
/// code: each call's handler marks the error as handled once it has
/// completed, so an error no handler completed (Lua runs none for an
/// allocation failure; a skin's handler can fail, recurse or exhaust its
/// stack) is told apart and stays fatal.
const PROTECTED: &str = r##"
local xpcall_native, settle, select, unpack, note, error, fault = ...
-- The protected function's results come back as one table: the native
-- xpcall pushes its status after the results without making room, so it
-- is given one result at a time only.
local function pack(...)
  return { n = select("#", ...), ... }
end
local function packed(f, ...)
  return pack(f(...))
end
local function finish(seen, ok, r)
  if ok then
    if not settle(seen[1], true, nil, r.n) then
      error(fault, 0)
    end
    return true, unpack(r, 1, r.n)
  end
  if not settle(seen[1], false, r) then
    error(fault, 0)
  end
  return false, r
end
-- Each handler charges the error it is given (`note`), and completes
-- with the fault's marker once a fault is recorded: after a fault Lua
-- runs the handler inside the instruction hook, with hooks off, so the
-- skin's handler never runs then.
local function pcall(f, ...)
  local seen = {false}
  return finish(seen, xpcall_native(packed, function(e)
    if not note(e) then
      return fault
    end
    seen[1] = true
    return e
  end, f, ...))
end
local function xpcall(f, msgh, ...)
  local seen = {false}
  return finish(seen, xpcall_native(packed, function(e)
    if not note(e) then
      return fault
    end
    local r = msgh(e)
    seen[1] = true
    return r
  end, f, ...))
end
return pcall, xpcall, packed
"##;

/// The most results a protected call gives back: what fits in the 20
/// free stack slots Lua gives a native call (`LUA_MINSTACK`), less
/// `unpack`'s own.
pub(super) const MAX_RESULTS: i64 = 16;

/// The refusal of more than [`MAX_RESULTS`] results.
pub(super) const TOO_MANY: &str = "protected call results";

/// Lua's message for an allocation failure (`lstate.c`, `MEMERRMSG`).
const MEMORY_ERROR: &str = "not enough memory";

/// A host function as the sandbox calls it: `f` runs after one host call
/// is charged, and the function returns `true` and `f`'s results, or
/// `false` once a fault is recorded (any error of `f` becomes one: a
/// failed allocation the memory fault, another a Lua error). It never
/// returns a Rust error to Lua.
pub(super) fn host_raw<F, R>(lua: &Lua, pass: &Rc<Pass>, f: F) -> mlua::Result<Function>
where
    F: Fn(&Lua, &Pass, MultiValue) -> mlua::Result<R> + 'static,
    R: mlua::IntoLuaMulti,
{
    let pass = Rc::clone(pass);
    lua.create_function(move |lua, args: MultiValue| -> mlua::Result<MultiValue> {
        let r = pass
            .ledger
            .host_call(lua)
            .and_then(|()| f(lua, &pass, args))
            .and_then(|v| v.into_lua_multi(lua));
        match r {
            Ok(mut v) => {
                v.push_front(Value::Boolean(true));
                Ok(v)
            }
            Err(e) => {
                if !pass.ledger.faulted() {
                    let why = match e {
                        mlua::Error::MemoryError(_) => LuaWhy::Limit(LuaLimit::Memory),
                        _ => LuaWhy::Error,
                    };
                    let _ = pass.ledger.fail_lua(lua, why);
                }
                Ok(MultiValue::from_iter([Value::Boolean(false)]))
            }
        }
    })
}

/// A host function as a skin calls it: [`host_raw`]'s, given exactly
/// `arity` (0 or 1) of the skin's arguments, its results returned and a
/// fault raised in Lua.
pub(super) fn host<F, R>(lua: &Lua, pass: &Rc<Pass>, arity: u8, f: F) -> mlua::Result<Function>
where
    F: Fn(&Lua, &Pass, MultiValue) -> mlua::Result<R> + 'static,
    R: mlua::IntoLuaMulti,
{
    debug_assert!(arity <= 1, "a face passes at most one argument");
    let shim: Function = lua.named_registry_value(SHIM)?;
    shim.call((host_raw(lua, pass, f)?, arity))
}

/// A fresh VM for one pass, its environment built and its caps set.
pub(super) fn vm(pass: &Rc<Pass>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        // A panic in a host function is never a Lua error.
        LuaOptions::new().catch_rust_panics(false),
    )?;
    lua.set_memory_limit(MAX_LUA_MEMORY)?;
    #[cfg(test)]
    lua.set_app_data(super::tests::Live::new());
    let hook = Rc::clone(pass);
    let after_fault = std::cell::Cell::new(0_u32);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(INSTRUCTION_STEP),
        move |lua, _| {
            #[cfg(feature = "dev-tools")]
            super::vm_hooks::step();
            if hook.ledger.faulted() {
                let n = after_fault.get().saturating_add(1);
                after_fault.set(n);
                if n <= STEPS_AFTER_FAULT {
                    return Ok(VmState::Continue);
                }
            }
            hook.ledger.step(lua)?;
            Ok(VmState::Continue)
        },
    )?;
    let base = lua.globals();
    let error: Function = base.raw_get("error")?;
    let shim: Function = lua
        .load(SHIMS)
        .set_name("=sandbox")
        .set_mode(ChunkMode::Text)
        .call((error.clone(), FAULT))?;
    lua.set_named_registry_value(SHIM, shim)?;
    let env = lua.create_table()?;
    for name in BASE {
        env.raw_set(name, base.raw_get::<Value>(name)?)?;
    }
    super::library::install(&lua, pass, &env, &base)?;

    let xpcall: Function = base.raw_get("xpcall")?;
    lua.set_named_registry_value(XPCALL, xpcall.clone())?;
    let settle = host_raw(&lua, pass, |lua, pass, args| {
        pass.ledger.update(|c| c.protected_calls += 1);
        pass.ledger.check(lua)?;
        let seen = args.front().is_some_and(|v| v.as_boolean() == Some(true));
        let ok = args.get(1).is_some_and(|v| v.as_boolean() == Some(true));
        // `unpack` gives them back within the free stack slots of a
        // native call, so it never grows the stack.
        if ok
            && args
                .get(3)
                .and_then(Value::as_integer)
                .is_none_or(|n| n > MAX_RESULTS)
        {
            return Err(pass.ledger.fail_lua(lua, LuaWhy::Library { op: TOO_MANY }));
        }
        if !ok && !seen {
            let memory = match args.get(2) {
                Some(Value::String(s)) => s.as_bytes().as_ref() == MEMORY_ERROR.as_bytes(),
                _ => false,
            };
            let limit = if memory {
                LuaLimit::Memory
            } else {
                LuaLimit::Handler
            };
            return Err(pass.ledger.fail_lua(lua, LuaWhy::Limit(limit)));
        }
        Ok(())
    })?;
    let (pcall, xpcall_wrapped, packed): (Function, Function, Function) = lua
        .load(PROTECTED)
        .set_name("=sandbox")
        .set_mode(ChunkMode::Text)
        .call((
            xpcall,
            settle,
            base.raw_get::<Function>("select")?,
            base.raw_get::<Table>("table")?
                .raw_get::<Function>("unpack")?,
            // The error's message was formatted natively: its length is
            // charged, whatever the handler then makes of it.
            host_raw(&lua, pass, |lua, pass, args| {
                pass.ledger.check(lua)?;
                if let Some(Value::String(s)) = args.front() {
                    let n = u64::try_from(s.as_bytes().len()).unwrap_or(u64::MAX);
                    pass.ledger.native(lua, n)?;
                }
                Ok(())
            })?,
            error,
            FAULT,
        ))?;
    lua.set_named_registry_value(PACKED, packed)?;
    env.raw_set("pcall", pcall)?;
    env.raw_set("xpcall", xpcall_wrapped)?;

    // The host's own runs' handler: it completes with the fault's marker
    // once one is recorded, as the protected calls' do.
    let mark = host_raw(&lua, pass, |lua, pass, _| {
        pass.handled(super::ledger::running(lua));
        Ok(())
    })?;
    let handler: Function = lua
        .load("local mark, fault = ... return function(e) if not mark() then return fault end return e end")
        .set_name("=sandbox")
        .set_mode(ChunkMode::Text)
        .call((mark, FAULT))?;
    lua.set_named_registry_value(HANDLER, handler)?;

    super::host::install(&lua, pass)?;
    if let Some(bridge) = &pass.bridge {
        super::config::install(&lua, pass, &env, bridge)?;
        // The tags, weak in their keys so that a closure's tag ends with
        // it: an address is never read as another closure's.
        let weak = lua.create_table()?;
        weak.raw_set("__mode", "k")?;
        let ids = lua.create_table()?;
        ids.set_metatable(Some(weak.clone()))?;
        let readers = lua.create_table()?;
        readers.set_metatable(Some(weak))?;
        let tagger: Function = lua
            .load(TAG)
            .set_name("=sandbox")
            .set_mode(ChunkMode::Text)
            .call((ids.clone(), readers.clone()))?;
        lua.set_named_registry_value(TAG_IDS, ids)?;
        lua.set_named_registry_value(TAG_READERS, readers)?;
        lua.set_named_registry_value(TAGGER, tagger)?;
    }
    #[cfg(test)]
    super::tests::install_continued(&lua, &env)?;
    env.raw_set("require", loader::require(&lua, pass)?)?;
    env.raw_set("dofile", loader::dofile(&lua, pass)?)?;
    env.raw_set("_G", env.clone())?;
    lua.set_globals(env)?;
    Ok(lua)
}
