//! The sandbox's `string` and `table` libraries, `tonumber` and
//! `setmetatable` (`library.lua`): Lua implementations, so that every loop
//! whose length a skin chooses (a pattern match, a table copy, a sort, a
//! format's scan) runs as metered Lua instructions and sees cancellation
//! at each instruction step.
//!
//! The native functions they keep are called only by `library.lua`, never
//! by a skin, and only with validated arguments whose result's size is
//! known first: that size is charged in bytes to
//! [`MAX_NATIVE_WORK`](super::limits::MAX_NATIVE_WORK), and a result over
//! [`MAX_PRIMITIVE_BYTES`](super::limits::MAX_PRIMITIVE_BYTES) is refused
//! before the call. Tables are read raw, and one with a metatable, which
//! Lua's own table functions would consult, is refused. The pattern forms
//! the library does not implement, `%b`, `%f` and back references, are
//! refused, as is a format conversion it does not size. An ordinary error
//! stays a Lua value raised in Lua; the host's three primitives (`charge`,
//! `refuse`, `snapshot`) fail only with the ledger's fault.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, Table, Value};

use super::limits::MAX_PRIMITIVE_BYTES;
use super::{LuaLimit, LuaWhy, Pass};

/// The library, compiled in the sandbox before any skin code.
const LIBRARY: &str = include_str!("library.lua");

/// What `library.lua` refuses, by its refusal code (1 first).
pub(crate) const REFUSED: [&str; 6] = [
    "pattern %b",
    "pattern %f",
    "pattern back reference",
    "table with a metatable",
    "result over the primitive cap",
    "format conversion",
];

/// A failed allocation is the memory fault; any other failure of a
/// primitive is already the ledger's fault.
fn fault(pass: &Pass, lua: &Lua, e: mlua::Error) -> mlua::Error {
    match e {
        mlua::Error::MemoryError(_) => pass.ledger.fail_lua(lua, LuaWhy::Limit(LuaLimit::Memory)),
        e => e,
    }
}

/// Installs the library in `env`, from the natives of `base`, the VM's
/// own globals, which no skin reaches.
pub(super) fn install(lua: &Lua, pass: &Rc<Pass>, env: &Table, base: &Table) -> mlua::Result<()> {
    let string: Table = base.raw_get("string")?;
    let table: Table = base.raw_get("table")?;
    let math: Table = base.raw_get("math")?;
    let native = lua.create_table()?;
    for name in [
        "byte", "sub", "len", "char", "rep", "upper", "lower", "reverse", "format",
    ] {
        native.raw_set(name, string.raw_get::<Function>(name)?)?;
    }
    for name in ["concat", "unpack"] {
        native.raw_set(name, table.raw_get::<Function>(name)?)?;
    }
    for name in ["tonumber", "tostring", "setmetatable", "getmetatable"] {
        native.raw_set(name, base.raw_get::<Function>(name)?)?;
    }
    native.raw_set("tointeger", math.raw_get::<Function>("tointeger")?)?;
    native.raw_set("mathtype", math.raw_get::<Function>("type")?)?;
    native.raw_set("maxinteger", i64::MAX)?;

    let p = Rc::clone(pass);
    let charge = lua.create_function(move |lua, n: i64| {
        p.ledger.check(lua)?;
        let n = u64::try_from(n).unwrap_or(0);
        p.ledger.native(lua, n)
    })?;
    let p = Rc::clone(pass);
    let refuse = lua.create_function(move |lua, code: i64| -> mlua::Result<()> {
        let op = usize::try_from(code - 1)
            .ok()
            .and_then(|i| REFUSED.get(i))
            .copied()
            .unwrap_or("library");
        Err(p.ledger.fail_lua(lua, LuaWhy::Library { op }))
    })?;
    let p = Rc::clone(pass);
    let snapshot = lua.create_function(move |lua, mt: Table| {
        p.ledger.check(lua)?;
        p.ledger.update(|c| c.metatables += 1);
        snapshot(lua, &p, &mt).map_err(|e| fault(&p, lua, e))
    })?;
    let (string_lib, table_lib, tonumber, setmetatable): (Table, Table, Function, Function) = lua
        .load(LIBRARY)
        .set_name("=library")
        .set_mode(ChunkMode::Text)
        .call((native, charge, refuse, snapshot, MAX_PRIMITIVE_BYTES))?;
    // Strings' methods are the library's too.
    if let Some(mt) = lua
        .load("return getmetatable('')")
        .set_name("=library")
        .eval::<Option<Table>>()?
    {
        mt.raw_set("__index", string_lib.clone())?;
    }
    env.raw_set("string", string_lib)?;
    env.raw_set("table", table_lib)?;
    env.raw_set("tonumber", tonumber)?;
    env.raw_set("setmetatable", setmetatable)?;
    Ok(())
}

/// A copy of the skin's metatable `mt`, its entries charged before they
/// are copied, refused with `__gc` (a finalizer) or `__mode` (weak
/// references). Unless the skin protected it with `__metatable`, the copy
/// names `mt` there, so that `getmetatable` gives the skin its own table,
/// which it may change without reaching the copy.
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
    }
    Ok(copy)
}
