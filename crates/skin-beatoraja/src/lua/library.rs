//! The sandbox's library (`library.lua`): the base, `string`, `table` and
//! `math` functions a skin may call, an allowlist of what the owner's
//! skins use, every one implemented or wrapped in Lua.
//!
//! - base: `select`, `error`, `tonumber` (a numeral of at most 200
//!   bytes, or base 10) and `setmetatable` from the library; `assert`,
//!   `getmetatable`, `ipairs`, `next`, `pairs`, `tostring` and `type` as
//!   Lua has them (none coerces or loops natively over skin data);
//! - `string`: `len`, `sub`, `byte` (16 results at most), `char`,
//!   `upper`, `lower`, `find`, `match`, `gsub` and `format` (`%d`, `%f`,
//!   `%s` and `%%`, with `-`, `0`, a width and a precision of two
//!   digits); strings' methods are these, and strings have no arithmetic
//!   metamethods;
//! - `table`: `insert`, `remove`, `concat` and `sort`;
//! - `math`: `floor`, `ceil`, `abs`, `max` and `min`.
//!
//! Every loop whose length a skin chooses (a pattern match, a table
//! shift, a sort, a format's scan, a metatable's copy) runs as metered
//! Lua and sees cancellation at each instruction step. A native function
//! the library keeps is its private upvalue, called only with arguments
//! of the exact Lua type it takes, checked first and never coerced, and
//! only for a result whose size is known and charged in bytes to
//! [`MAX_NATIVE_WORK`](super::limits::MAX_NATIVE_WORK) first, a result
//! over [`MAX_PRIMITIVE_BYTES`](super::limits::MAX_PRIMITIVE_BYTES)
//! refused. A table with a metatable, which Lua's own table functions
//! would consult, is refused, as are `%b`, `%f`, `%z` and back
//! references, an unsized format conversion and any other `tonumber`
//! base. An ordinary error stays a Lua value raised in Lua; the host's
//! primitives fail only with the ledger's fault, and its bridge functions
//! take their arguments checked here and never raise one.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, Table, Value};

use super::limits::MAX_PRIMITIVE_BYTES;
use super::{LuaWhy, Pass};

/// The library, compiled in the sandbox before any skin code.
const LIBRARY: &str = include_str!("library.lua");

/// The registry name of the library's wrapper for a host function taking
/// one string.
pub(super) const STRING_BRIDGE: &str = "tapestry.string_bridge";

/// What `library.lua` refuses, by its refusal code (1 first); code 8 is a
/// finalizer, [`LuaWhy::Finalizer`].
pub(crate) const REFUSED: [&str; 9] = [
    "pattern %b",
    "pattern %f",
    "pattern back reference",
    "table with a metatable",
    "result over the primitive cap",
    "format conversion",
    "pattern %z",
    "metatable with __gc or __mode",
    "tonumber of a long numeral or another base",
];

/// The code of a finalizer refusal.
const FINALIZER: i64 = 8;

/// Installs the library in `env`, from the natives of `base`, the VM's
/// own globals, which no skin reaches.
pub(super) fn install(lua: &Lua, pass: &Rc<Pass>, env: &Table, base: &Table) -> mlua::Result<()> {
    let string: Table = base.raw_get("string")?;
    let table: Table = base.raw_get("table")?;
    let math: Table = base.raw_get("math")?;
    let native = lua.create_table()?;
    for name in ["byte", "sub", "char", "upper", "lower", "format"] {
        native.raw_set(name, string.raw_get::<Function>(name)?)?;
    }
    for name in ["concat", "unpack"] {
        native.raw_set(name, table.raw_get::<Function>(name)?)?;
    }
    for name in [
        "tonumber",
        "tostring",
        "setmetatable",
        "getmetatable",
        "select",
        "error",
        "next",
        "rawget",
        "rawset",
        "rawlen",
        "type",
    ] {
        native.raw_set(name, base.raw_get::<Function>(name)?)?;
    }
    for name in ["floor", "ceil", "abs", "tointeger"] {
        native.raw_set(name, math.raw_get::<Function>(name)?)?;
    }
    native.raw_set("mathtype", math.raw_get::<Function>("type")?)?;

    let p = Rc::clone(pass);
    let charge = lua.create_function(move |lua, n: i64| {
        p.ledger.check(lua)?;
        let n = u64::try_from(n).unwrap_or(0);
        p.ledger.native(lua, n)
    })?;
    let p = Rc::clone(pass);
    let refuse = lua.create_function(move |lua, code: i64| -> mlua::Result<()> {
        if code == FINALIZER {
            return Err(p.ledger.fail_lua(lua, LuaWhy::Finalizer));
        }
        let op = usize::try_from(code - 1)
            .ok()
            .and_then(|i| REFUSED.get(i))
            .copied()
            .unwrap_or("library");
        Err(p.ledger.fail_lua(lua, LuaWhy::Library { op }))
    })?;
    let bridge = lua.create_table()?;
    let p = Rc::clone(pass);
    bridge.raw_set(
        "metatable",
        lua.create_function(move |lua, ()| {
            p.ledger.check(lua)?;
            p.ledger.update(|c| c.metatables += 1);
            Ok(())
        })?,
    )?;
    let (string_lib, table_lib, math_lib, lib): (Table, Table, Table, Table) = lua
        .load(LIBRARY)
        .set_name("=library")
        .set_mode(ChunkMode::Text)
        // No globals: every name the library uses is its own.
        .set_environment(lua.create_table()?)
        .call((native, charge, refuse, bridge, MAX_PRIMITIVE_BYTES))?;
    // Strings' methods are the library's, and strings have no other
    // metamethod: none of their native arithmetic coercions is reachable.
    if let Some(mt) = lua
        .load("return getmetatable('')")
        .set_name("=library")
        .eval::<Option<Table>>()?
    {
        let keys = mt
            .pairs::<Value, Value>()
            .map(|p| p.map(|(k, _)| k))
            .collect::<mlua::Result<Vec<Value>>>()?;
        for k in keys {
            mt.raw_set(k, Value::Nil)?;
        }
        mt.raw_set("__index", string_lib.clone())?;
    }
    env.raw_set("string", string_lib)?;
    env.raw_set("table", table_lib)?;
    env.raw_set("math", math_lib)?;
    for name in ["tonumber", "select", "error", "setmetatable"] {
        env.raw_set(name, lib.raw_get::<Function>(name)?)?;
    }
    lua.set_named_registry_value(STRING_BRIDGE, lib.raw_get::<Function>("string_bridge")?)?;
    Ok(())
}
