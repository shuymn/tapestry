//! A fresh VM and its environment, built from an allowlist.
//!
//! The VM opens only the `string`, `table` and `math` libraries besides
//! the base functions, under a memory limit set before any skin code
//! runs and an instruction hook installed as the global hook, which every
//! thread the host creates inherits. The globals are a new table holding
//! only:
//!
//! - the base functions `assert`, `error`, `getmetatable`, `ipairs`,
//!   `next`, `pairs`, `rawequal`, `rawget`, `rawlen`, `select`,
//!   `tonumber`, `tostring`, `type` and `_VERSION`, and `_G`;
//! - `rawset`, refusing a write to `skin_config` ([`super::config`]);
//! - `string` without `dump`, `pack`, `packsize` and `unpack`, its
//!   pattern functions charged their worst case first
//!   ([`MAX_NATIVE_WORK`](super::limits::MAX_NATIVE_WORK)); `table`, its
//!   `move` and `sort` charged likewise; `math` without `random` and
//!   `randomseed`;
//! - `setmetatable`, refusing a metatable with `__gc` (a finalizer) or
//!   `__mode` (weak references), either of which lets the collector
//!   change what the skin computes;
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
    "select",
    "tonumber",
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
/// code: each call's handler marks the error as seen, so an error no
/// handler saw (Lua runs none for an allocation failure) is told apart.
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
  return finish(seen, xpcall_native(f, function(e) seen[1] = true return msgh(e) end, ...))
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
    for lib in ["string", "table", "math"] {
        env.raw_set(lib, base.raw_get::<Table>(lib)?)?;
    }
    for (lib, name) in REMOVED {
        env.raw_get::<Table>(lib)?.raw_set(name, Value::Nil)?;
    }
    natives(&lua, pass, &env)?;

    let native_set: Function = base.raw_get("setmetatable")?;
    env.raw_set(
        "setmetatable",
        host(&lua, pass, move |lua, pass, args| {
            pass.ledger.update(|c| c.metatables += 1);
            if let Some(Value::Table(mt)) = args.get(1)
                && (!mt.raw_get::<Value>("__gc")?.is_nil()
                    || !mt.raw_get::<Value>("__mode")?.is_nil())
            {
                return Err(pass.ledger.fail_lua(lua, LuaWhy::Finalizer));
            }
            native_set.call::<MultiValue>(args)
        })?,
    )?;

    let native_rawset: Function = base.raw_get("rawset")?;
    env.raw_set(
        "rawset",
        host(&lua, pass, move |lua, pass, args| {
            if let Some(Value::Table(t)) = args.front()
                && pass.views.borrow().contains(&t.to_pointer().addr())
            {
                return Err(pass.ledger.fail_lua(lua, LuaWhy::ConfigWrite));
            }
            native_rawset.call::<MultiValue>(args)
        })?,
    )?;

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
                LuaLimit::Stack
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

/// The library functions whose native loops the instruction hook does not
/// see, wrapped to charge their worst case first.
fn natives(lua: &Lua, pass: &Rc<Pass>, env: &Table) -> mlua::Result<()> {
    let string: Table = env.raw_get("string")?;
    for name in ["find", "match", "gmatch", "gsub"] {
        let native: Function = string.raw_get(name)?;
        let plain_find = name == "find";
        let f = host(lua, pass, move |lua, pass, args| {
            let len = |i: usize| match args.get(i) {
                Some(Value::String(s)) => s.as_bytes().len(),
                // A number is converted to at most a few dozen bytes.
                _ => 64,
            };
            let plain = plain_find
                && args
                    .get(3)
                    .is_some_and(|v| !matches!(v, Value::Nil | Value::Boolean(false)));
            let pattern = match args.get(1) {
                Some(Value::String(p)) => p.as_bytes().to_vec(),
                _ => Vec::new(),
            };
            let work = if plain {
                to_u64(len(0)).saturating_mul(to_u64(len(1)).saturating_add(1))
            } else {
                pattern_work(len(0), &pattern)
            };
            pass.ledger.native(lua, work)?;
            native.call::<MultiValue>(args)
        })?;
        string.raw_set(name, f)?;
    }
    let table: Table = env.raw_get("table")?;
    let native_move: Function = table.raw_get("move")?;
    table.raw_set(
        "move",
        host(lua, pass, move |lua, pass, args| {
            let int = |i: usize| match args.get(i) {
                Some(Value::Integer(n)) => Some(*n),
                _ => None,
            };
            if let (Some(f), Some(e)) = (int(1), int(2))
                && e >= f
            {
                let count = u64::try_from(i128::from(e) - i128::from(f) + 1).unwrap_or(u64::MAX);
                pass.ledger.native(lua, count)?;
            }
            native_move.call::<MultiValue>(args)
        })?,
    )?;
    let native_sort: Function = table.raw_get("sort")?;
    table.raw_set(
        "sort",
        host(lua, pass, move |lua, pass, args| {
            if let Some(Value::Table(t)) = args.front() {
                let n = to_u64(t.raw_len());
                let log = u64::from(u64::BITS - n.leading_zeros());
                pass.ledger.native(lua, n.saturating_mul(log.max(1)))?;
            }
            native_sort.call::<MultiValue>(args)
        })?,
    )?;
    Ok(())
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A pattern match's worst case over a subject of `len` bytes: every
/// start tried (one when anchored), and each repetition (`*`, `+`, `-`,
/// `?`, `%b`) backtracking over the whole subject, times the pattern's
/// length. Lua's matcher is a backtracking one (`lstrlib.c`, `match`).
pub(super) fn pattern_work(len: usize, pattern: &[u8]) -> u64 {
    let anchored = pattern.first() == Some(&b'^');
    let mut i = usize::from(anchored);
    let mut repeats: u32 = 0;
    while let Some(&c) = pattern.get(i) {
        let single = match c {
            b'%' => match pattern.get(i + 1) {
                Some(b'b') => {
                    repeats += 1;
                    i += 4;
                    false
                }
                Some(b'f') => {
                    i = skip_set(pattern, i + 2);
                    false
                }
                _ => {
                    i += 2;
                    true
                }
            },
            b'[' => {
                i = skip_set(pattern, i);
                true
            }
            b'(' | b')' => {
                i += 1;
                false
            }
            _ => {
                i += 1;
                true
            }
        };
        if single && matches!(pattern.get(i), Some(b'*' | b'+' | b'-' | b'?')) {
            repeats += 1;
            i += 1;
        }
    }
    let base = to_u64(len).saturating_add(1);
    let power = repeats + u32::from(!anchored);
    base.saturating_pow(power)
        .saturating_mul(to_u64(pattern.len()).saturating_add(1))
}

/// The index past the set `[...]` starting at `i` (Lua's `classEnd`).
fn skip_set(p: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    if p.get(j) == Some(&b'^') {
        j += 1;
    }
    // The first `]` is part of the set.
    if p.get(j) == Some(&b']') {
        j += 1;
    }
    while let Some(&c) = p.get(j) {
        j += 1;
        match c {
            b'%' => j += 1,
            b']' => return j,
            _ => {}
        }
    }
    j
}

#[cfg(test)]
mod tests {
    use super::pattern_work;

    #[test]
    fn pattern_work_counts_repetitions() {
        // No repetition: every start, the pattern once.
        assert_eq!(pattern_work(9, b"ab"), 10 * 3);
        // Anchored: one start.
        assert_eq!(pattern_work(9, b"^ab"), 4);
        assert_eq!(pattern_work(9, b"^a*b"), 10 * 5);
        assert_eq!(pattern_work(9, b"(.-)%.(.-)"), 1000 * 11);
        // An escaped quantifier is a literal; a set is one class.
        assert_eq!(pattern_work(9, b"%*[*+]"), 10 * 7);
        assert_eq!(pattern_work(9, b"[]x]+"), 100 * 6);
        assert_eq!(pattern_work(9, b"%b()"), 100 * 5);
        assert_eq!(pattern_work(usize::MAX, b"a*a*a*"), u64::MAX);
    }
}
