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
//!   `tonumber`, `tostring`, `type` and `_VERSION`, and `_G`;
//! - `string` without `dump`, `pack`, `packsize` and `unpack`, its
//!   pattern functions charged their worst case first
//!   ([`MAX_NATIVE_WORK`](super::limits::MAX_NATIVE_WORK)); `table`, its
//!   `move` and `sort` charged likewise; `math` without `random` and
//!   `randomseed`;
//! - `setmetatable`, which attaches a snapshot of the metatable the
//!   skin passes, refusing one with `__gc` (a finalizer) or `__mode`
//!   (weak references), either of which lets the collector change what
//!   the skin computes: a later change to the skin's table, by
//!   assignment or `rawset`, never reaches the snapshot, and
//!   `getmetatable` gives the skin its own table back;
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
const BASE: [&str; 15] = [
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
            let mut args = args;
            // A snapshot of ours guards the table's metatable only against
            // the skin: lifted for the skin's own replacement.
            if let Some(Value::Table(t)) = args.front()
                && let Some(current) = t.metatable()
                && pass.snapshot_of(current.to_pointer().addr())
            {
                current.raw_set("__metatable", Value::Nil)?;
            }
            if let Some(Value::Table(mt)) = args.get(1) {
                let snapshot = snapshot(lua, pass, mt)?;
                if let Some(slot) = args.get_mut(1) {
                    *slot = Value::Table(snapshot);
                }
            }
            native_set.call::<MultiValue>(args)
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

/// A copy of the skin's metatable `mt`, its entries charged as native
/// work before they are copied, refused with `__gc` or `__mode`. Unless
/// the skin protected it with `__metatable`, the copy names `mt` there, so
/// `getmetatable` gives the skin its own table, which it may change
/// without reaching the copy.
fn snapshot(lua: &Lua, pass: &Pass, mt: &Table) -> mlua::Result<Table> {
    let mut entries = 0_u64;
    for pair in mt.pairs::<Value, Value>() {
        pair?;
        entries += 1;
    }
    pass.ledger.native(lua, entries)?;
    let copy = lua.create_table()?;
    for pair in mt.pairs::<Value, Value>() {
        let (k, v) = pair?;
        copy.raw_set(k, v)?;
    }
    if !copy.raw_get::<Value>("__gc")?.is_nil() || !copy.raw_get::<Value>("__mode")?.is_nil() {
        return Err(pass.ledger.fail_lua(lua, LuaWhy::Finalizer));
    }
    if copy.raw_get::<Value>("__metatable")?.is_nil() {
        copy.raw_set("__metatable", mt.clone())?;
        pass.snapshot(copy.to_pointer().addr());
    }
    Ok(copy)
}

/// How a library function's worst case is charged before it runs, from
/// its arguments as Lua coerces them.
#[derive(Clone, Copy)]
enum Cost {
    /// A pattern match over argument 0 with pattern argument 1; `find`
    /// may be plain (argument 3).
    Pattern { plain: bool },
    /// Linear in argument 0's length.
    Length,
    /// `string.rep(s, n, sep)`: the result's length.
    Rep,
    /// `string.format(fmt, ...)`: its arguments' lengths.
    Format,
    /// Linear in the number of arguments.
    Arguments,
    /// `table.move(a1, f, e, t)`: the elements copied.
    Move,
    /// `table.concat(t, sep, i, j)` and `table.unpack(t, i, j)`: the
    /// elements read, each with a separator; `range` is where `i` is.
    Range { range: usize },
    /// `table.insert` and `table.remove`: the elements shifted.
    Shift,
    /// `table.sort`: `n log n` comparisons.
    Sort,
}

/// The string and table functions wrapped, with their cost. Every other
/// one (`len`, `sub`, `math`) costs a constant or what its result's
/// charge counts.
const NATIVES: [(&str, &str, Cost); 18] = [
    ("string", "find", Cost::Pattern { plain: true }),
    ("string", "match", Cost::Pattern { plain: false }),
    ("string", "gmatch", Cost::Pattern { plain: false }),
    ("string", "gsub", Cost::Pattern { plain: false }),
    ("string", "byte", Cost::Length),
    ("string", "char", Cost::Arguments),
    ("string", "format", Cost::Format),
    ("string", "lower", Cost::Length),
    ("string", "upper", Cost::Length),
    ("string", "reverse", Cost::Length),
    ("string", "rep", Cost::Rep),
    ("table", "concat", Cost::Range { range: 2 }),
    ("table", "unpack", Cost::Range { range: 1 }),
    ("table", "pack", Cost::Arguments),
    ("table", "move", Cost::Move),
    ("table", "insert", Cost::Shift),
    ("table", "remove", Cost::Shift),
    ("table", "sort", Cost::Sort),
];

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The elements from `f` to `e`, none when `e` is below `f`.
fn span(f: i64, e: i64) -> u64 {
    u64::try_from(i128::from(e) - i128::from(f) + 1).unwrap_or(0)
}

/// Argument `i` as Lua coerces it to an integer.
fn integer(lua: &Lua, args: &MultiValue, i: usize) -> mlua::Result<Option<i64>> {
    match args.get(i) {
        Some(v) => lua.coerce_integer(v.clone()),
        None => Ok(None),
    }
}

/// The length of argument `i` as Lua coerces it to a string.
fn length(lua: &Lua, args: &MultiValue, i: usize) -> mlua::Result<u64> {
    Ok(match args.get(i) {
        Some(v) => lua
            .coerce_string(v.clone())?
            .map_or(0, |s| to_u64(s.as_bytes().len())),
        None => 0,
    })
}

/// The length of table argument 0, as the library reads it (`__len`
/// honoured).
fn table_len(args: &MultiValue) -> mlua::Result<i64> {
    match args.front() {
        Some(Value::Table(t)) => t.len(),
        _ => Ok(0),
    }
}

impl Cost {
    /// The work the call may do, before it runs.
    fn before(self, lua: &Lua, args: &MultiValue) -> mlua::Result<u64> {
        Ok(match self {
            Self::Pattern { plain } => {
                let subject = length(lua, args, 0)?;
                let plain = plain
                    && args
                        .get(3)
                        .is_some_and(|v| !matches!(v, Value::Nil | Value::Boolean(false)));
                let pattern = match args.get(1) {
                    Some(v) => lua.coerce_string(v.clone())?,
                    None => None,
                };
                let pattern = pattern
                    .as_ref()
                    .map(|p| p.as_bytes().to_vec())
                    .unwrap_or_default();
                if plain {
                    subject.saturating_mul(to_u64(pattern.len()).saturating_add(1))
                } else {
                    pattern_work(usize::try_from(subject).unwrap_or(usize::MAX), &pattern)
                }
            }
            Self::Length => length(lua, args, 0)?,
            Self::Rep => {
                let n = u64::try_from(integer(lua, args, 1)?.unwrap_or(0)).unwrap_or(0);
                let each = length(lua, args, 0)?.saturating_add(length(lua, args, 2)?);
                n.saturating_mul(each)
            }
            Self::Format => {
                let mut sum = 0_u64;
                for i in 0..args.len() {
                    sum = sum.saturating_add(length(lua, args, i)?).saturating_add(1);
                }
                sum
            }
            Self::Arguments => to_u64(args.len()),
            Self::Move => match (integer(lua, args, 1)?, integer(lua, args, 2)?) {
                (Some(f), Some(e)) => span(f, e),
                _ => 0,
            },
            Self::Range { range } => {
                let f = integer(lua, args, range)?.unwrap_or(1);
                let e = match integer(lua, args, range + 1)? {
                    Some(e) => e,
                    None => table_len(args)?,
                };
                let sep = if range == 2 { length(lua, args, 1)? } else { 0 };
                span(f, e).saturating_mul(sep.saturating_add(1))
            }
            Self::Shift => u64::try_from(table_len(args)?)
                .unwrap_or(0)
                .saturating_add(1),
            Self::Sort => {
                let n = u64::try_from(table_len(args)?).unwrap_or(0);
                let log = u64::from(u64::BITS - n.leading_zeros());
                n.saturating_mul(log.max(1))
            }
        })
    }
}

/// What a library function produced: one unit a value and a byte a
/// string's byte, charged after it returns, so repeated small results add
/// up.
fn produced(values: &MultiValue) -> u64 {
    values.iter().fold(0_u64, |sum, v| {
        let bytes = match v {
            Value::String(s) => to_u64(s.as_bytes().len()),
            _ => 0,
        };
        sum.saturating_add(bytes).saturating_add(1)
    })
}

/// `native` wrapped: the fault and cancellation checked before and after,
/// its worst case charged before it runs and its result after. Not a host
/// call: the library is Lua's own.
fn native(
    lua: &Lua,
    pass: &Rc<Pass>,
    f: Function,
    cost: Cost,
    iterator: bool,
) -> mlua::Result<Function> {
    let pass = Rc::clone(pass);
    lua.create_function(move |lua, args: MultiValue| {
        pass.ledger.check(lua)?;
        let work = cost.before(lua, &args)?;
        pass.ledger.native(lua, work)?;
        let values = f.call::<MultiValue>(args);
        pass.ledger.check(lua)?;
        let mut values = values.map_err(|e| match e {
            mlua::Error::MemoryError(_) => {
                pass.ledger.fail_lua(lua, LuaWhy::Limit(LuaLimit::Memory))
            }
            e => e,
        })?;
        pass.ledger.native(lua, produced(&values))?;
        // `gmatch`'s iterator matches on each call: each is charged as
        // a call that produces its captures.
        if iterator && let Some(Value::Function(next)) = values.pop_front() {
            values.push_front(Value::Function(native(
                lua,
                &pass,
                next,
                Cost::Arguments,
                false,
            )?));
        }
        Ok(values)
    })
}

/// The library functions whose native loops the instruction hook does not
/// see, wrapped to charge their worst case first and their result after.
fn natives(lua: &Lua, pass: &Rc<Pass>, env: &Table) -> mlua::Result<()> {
    for (lib, name, cost) in NATIVES {
        let table: Table = env.raw_get(lib)?;
        let f: Function = table.raw_get(name)?;
        table.raw_set(name, native(lua, pass, f, cost, name == "gmatch")?)?;
    }
    Ok(())
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
