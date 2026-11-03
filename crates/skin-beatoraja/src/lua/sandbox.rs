//! A fresh VM and its environment, built from an allowlist.
//!
//! The VM opens only the `string`, `table` and `math` libraries besides
//! the base functions, under a memory limit set before any skin code
//! runs and an instruction hook installed as the global hook, which every
//! thread the host creates inherits. The globals are a new table holding
//! only:
//!
//! - the base functions `assert`, `error`, `getmetatable`, `ipairs`,
//!   `next`, `pairs`, `rawequal`, `rawget`, `rawlen`, `rawset`, `select`,
//!   `tostring`, `type` and `_VERSION`, and `_G`;
//! - `string`, `table`, `tonumber` and `setmetatable` from the sandbox's
//!   own library ([`super::library`]), whose long loops are metered Lua;
//!   `string` without `dump`, `pack`, `packsize` and `unpack`, and the
//!   strings' methods the same; `setmetatable` attaches a snapshot of the
//!   metatable the skin passes, refusing one with `__gc` (a finalizer) or
//!   `__mode` (weak references), either of which lets the collector change
//!   what the skin computes: a later change to the skin's table never
//!   reaches the snapshot, and `getmetatable` gives the skin its own table
//!   back;
//! - `math` without `random` and `randomseed`;
//! - `pcall` and `xpcall`, which return an ordinary error as Lua does but
//!   raise a fault of the ledger again, and refuse an error no handler
//!   saw (an allocation failure);
//! - the confined loaders `require` and `dofile` ([`super::loader`]).
//!
//! There is no `io`, `os`, `debug`, `package`, `load`, `loadfile`,
//! `collectgarbage`, `print`, `warn`, `coroutine` or `utf8`. Every
//! function the host adds charges one host call before its work.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Table, Value, VmState};

use super::limits::{INSTRUCTION_STEP, MAX_LUA_MEMORY};
use super::{LuaLimit, LuaWhy, Pass, loader};

/// The registry name of the base library's `xpcall`.
pub(super) const XPCALL: &str = "tapestry.xpcall";
/// The registry name of the handler the host's protected runs use.
pub(super) const HANDLER: &str = "tapestry.handler";

/// The base functions the environment keeps as they are.
const BASE: [&str; 14] = [
    "assert",
    "error",
    "getmetatable",
    "ipairs",
    "next",
    "pairs",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "select",
    "tostring",
    "type",
    "_VERSION",
];

/// The library functions removed: bytecode, binary packing and random
/// sources.
const REMOVED: [(&str, &str); 6] = [
    ("string", "dump"),
    ("string", "pack"),
    ("string", "packsize"),
    ("string", "unpack"),
    ("math", "random"),
    ("math", "randomseed"),
];

/// The protected-call wrappers, compiled in the sandbox before any skin
/// code: each call's handler marks the error as handled once it has
/// completed, so an error no handler completed (Lua runs none for an
/// allocation failure; a skin's handler can fail, recurse or exhaust its
/// stack) is told apart and stays fatal.
const PROTECTED: &str = r"
local xpcall_native, settle = ...
local function finish(seen, ok, ...)
  settle(seen[1], ok, (...))
  return ok, ...
end
local function pcall(f, ...)
  local seen = {false}
  return finish(seen, xpcall_native(f, function(e) seen[1] = true return e end, ...))
end
local function xpcall(f, msgh, ...)
  local seen = {false}
  return finish(seen, xpcall_native(f, function(e)
    local r = msgh(e)
    seen[1] = true
    return r
  end, ...))
end
return pcall, xpcall
";

/// Lua's message for an allocation failure (`lstate.c`, `MEMERRMSG`).
const MEMORY_ERROR: &str = "not enough memory";

/// Creates a host function: `f` runs after one host call is charged, and
/// a failed allocation in it is the memory fault.
pub(super) fn host<F, R>(lua: &Lua, pass: &Rc<Pass>, f: F) -> mlua::Result<Function>
where
    F: Fn(&Lua, &Pass, MultiValue) -> mlua::Result<R> + 'static,
    R: mlua::IntoLuaMulti,
{
    let pass = Rc::clone(pass);
    lua.create_function(move |lua, args: MultiValue| {
        pass.ledger.host_call(lua)?;
        match f(lua, &pass, args) {
            Err(mlua::Error::MemoryError(_)) => {
                Err(pass.ledger.fail_lua(lua, LuaWhy::Limit(LuaLimit::Memory)))
            }
            r => r,
        }
    })
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
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(INSTRUCTION_STEP),
        move |lua, _| {
            hook.ledger.step(lua)?;
            Ok(VmState::Continue)
        },
    )?;
    let base = lua.globals();
    let env = lua.create_table()?;
    for name in BASE {
        env.raw_set(name, base.raw_get::<Value>(name)?)?;
    }
    for (lib, name) in REMOVED {
        base.raw_get::<Table>(lib)?.raw_set(name, Value::Nil)?;
    }
    env.raw_set("math", base.raw_get::<Table>("math")?)?;
    super::library::install(&lua, pass, &env, &base)?;

    let xpcall: Function = base.raw_get("xpcall")?;
    lua.set_named_registry_value(XPCALL, xpcall.clone())?;
    let settle = host(&lua, pass, |lua, pass, args| {
        pass.ledger.update(|c| c.protected_calls += 1);
        pass.ledger.check(lua)?;
        let seen = args.front().is_some_and(|v| v.as_boolean() == Some(true));
        let ok = args.get(1).is_some_and(|v| v.as_boolean() == Some(true));
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
    let (pcall, xpcall_wrapped): (Function, Function) = lua
        .load(PROTECTED)
        .set_name("=sandbox")
        .set_mode(ChunkMode::Text)
        .call((xpcall, settle))?;
    env.raw_set("pcall", pcall)?;
    env.raw_set("xpcall", xpcall_wrapped)?;

    let handler = host(&lua, pass, |lua, pass, args| {
        pass.handled(super::ledger::running(lua));
        Ok(args)
    })?;
    lua.set_named_registry_value(HANDLER, handler)?;

    super::host::install(&lua, pass)?;
    if let Some(bridge) = &pass.bridge {
        super::config::install(&lua, pass, &env, bridge)?;
    }
    env.raw_set("require", loader::require(&lua, pass)?)?;
    env.raw_set("dofile", loader::dofile(&lua, pass)?)?;
    env.raw_set("_G", env.clone())?;
    lua.set_globals(env)?;
    Ok(lua)
}
