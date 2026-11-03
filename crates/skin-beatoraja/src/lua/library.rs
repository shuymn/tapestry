//! The sandbox's library (`library.lua`): the base, `string`, `table` and
//! `math` functions a skin may call, an allowlist of what the owner's
//! skins use, every one implemented or wrapped in Lua.
//!
//! - base: `select`, `error`, `ipairs`, `tonumber` (a numeral of at most
//!   200 bytes, or base 10) and `setmetatable` from the library;
//!   `assert`, `getmetatable`, `next`, `pairs`, `tostring` and `type` as
//!   Lua has them;
//! - `string`: `len`, `sub`, `byte` (16 results at most), `char`,
//!   `upper`, `lower`, `find`, `match`, `gsub` and `format` (`%d`, `%f`,
//!   `%s` and `%%`, with `-`, `0`, a width and a precision of two digits,
//!   32 conversions at most); strings' methods are these, and strings
//!   have no other metamethod;
//! - `table`: `insert`, `remove`, `concat` and `sort`;
//! - `math`: `floor`, `ceil`, `abs`, `max` and `min`.
//!
//! Every loop whose length a skin chooses (a pattern match, a table
//! shift, a sort, a format's scan, a metatable's copy, an `ipairs`
//! step) runs as metered Lua and sees cancellation at each instruction
//! step. A native function the library keeps is its private upvalue,
//! called only with arguments of the exact Lua type it takes, checked
//! first and never coerced, and only for a result whose size is known
//! and charged in bytes to
//! [`MAX_NATIVE_WORK`](super::limits::MAX_NATIVE_WORK) first, a result
//! over [`MAX_PRIMITIVE_BYTES`](super::limits::MAX_PRIMITIVE_BYTES)
//! refused. A table with a metatable, which Lua's own table functions
//! would consult, is refused, as are `%b`, `%f`, `%z` and back
//! references, an unsized format conversion and any other `tonumber`
//! base. An ordinary error stays a Lua value raised in Lua; the host's
//! primitives fail only with the ledger's fault, and its bridge functions
//! take their arguments checked here and never raise one.
//!
//! # The bound of every native
//!
//! Each native a skin reaches, directly or through the library, with the
//! arguments it is given (checked in Lua first), its native work, the
//! bound that makes that work finite and charged, and whether it runs
//! skin code (always as Lua under the instruction hook). `n` is a
//! count or length the skin chooses; "VM memory" is the 64 MiB cap
//! ([`MAX_LUA_MEMORY`](super::limits::MAX_LUA_MEMORY)).
//!
//! | Native | Arguments | Native work | Bound | Skin code |
//! |---|---|---|---|---|
//! | `assert` | any | O(1) when it holds, its arguments staying in place; when it fails, as `error` (the message and its position copied, O(#message)) | a caught message's length charged by the handler; an uncaught one ends the pass | no |
//! | `getmetatable` | any | O(1) raw `__metatable` read | none needed | no |
//! | `type` | any | O(1) | none needed | no |
//! | `next` (and `pairs`' iterator) | table, key | a key's hash (long strings: once, cached) and a scan of empty slots, O(table capacity) | the table's capacity, held in VM memory: the one native loop over skin data, bounded by memory and not charged | no |
//! | `pairs` | any | O(1) | none needed | `__pairs`, hooked |
//! | `tostring` (also `%s`, `concat`'s and `gsub`'s numbers) | any | a number's digits, or `__name` (at most 64 bytes) and an address; a string is returned uncopied | `__name` over 64 bytes refused when the metatable is attached | `__tostring`, hooked |
//! | `select` | an integer or `"#"`, values | O(1): the values stay in place | integer checked in Lua | no |
//! | `error` | a value, an integer level | with a level, the position and the message copied, O(#message) | each caught message's length charged by the handler, before the skin's runs; an uncaught one ends the pass | no |
//! | `ipairs` | any | none: the step is Lua (`t[i]`) | integer index checked in Lua | `__index`, hooked |
//! | `tonumber` | a string of at most 200 numeral bytes | O(200) | longer refused; base 10 parsed in Lua, any other base refused | no |
//! | `setmetatable`, `getmetatable` (private) | a table, the Lua-made snapshot | O(1) | the snapshot copied in Lua; `__gc`, `__mode` and `__close` refused | no |
//! | `string.sub` (private) | a string, integer bounds | O(result) | at most 1 MiB, charged | no |
//! | `string.byte` (private) | a string, integer bounds | O(results) | at most 16 results, within a native call's free stack | no |
//! | `string.char` (private) | integers in 0..255 | O(n) | at most 4 096, charged | no |
//! | `string.upper`, `string.lower` (private) | a string | O(#s) | at most 1 MiB, charged | no |
//! | `string.format` (private) | the checked format, its arguments | O(format + output + the bytes `%s` reads) | 32 conversions, two-digit widths and precisions, output at most 1 MiB, all charged | `__tostring`, hooked |
//! | `table.concat` (private) | the library's own table of strings | O(entries + output) | output at most 1 MiB, entries and output charged | no |
//! | `rawget`, `rawset`, `rawlen` (private) | a plain table | O(1) amortized; a resize O(capacity) | VM memory | no |
//! | `math.floor`, `ceil`, `abs`, `tointeger`, `math.type` (private) | a number | O(1) | number checked in Lua | no |
//! | `table.unpack` (private, the protected calls only) | the packed results | O(n) | at most 16, within a native call's free stack | no |
//! | `xpcall` (the protected calls' base) | the library's packer | O(1): one result | the skin's handler run only while no fault is recorded | the protected function and handler, hooked |
//! | host: `settle`, the handlers' `note`, `charge`, `refuse`, `bridge.metatable` | the library's own values | O(1); a refusal's location looks up at most 2 048 frames, once | fatal once | no |
//! | host: `require`, `dofile` | one argument (extras dropped in Lua), a name of at most 1 024 bytes, checked before it is copied | a confined read and a compilation, O(file) | 16 MiB a file, 32 MiB read in all, 1 024 loads, depth 16; 16 results | the chunk, hooked |
//! | host: `skin_config.get_path` | one argument, a string of at most 1 024 bytes, checked in Lua and again before it is copied | a substitution, O(path) | the result at most 1 024 bytes, else nil | no |
//! | host: the pinned module APIs | none (the skin's dropped in Lua) | O(1): the fault | fatal | no |
//! | the strings' `__index` | a key | a raw lookup in the library's `string` table | none needed | no |
//! | VM: `..` | strings, numbers | O(result) a concatenation | each instruction at most O(VM memory), at most [`INSTRUCTION_STEP`](super::limits::INSTRUCTION_STEP) (100) instructions between checks, all of them within [`MAX_LUA_TIME`](super::limits::MAX_LUA_TIME) across the candidate's passes and [`MAX_INSTRUCTIONS`](super::limits::MAX_INSTRUCTIONS) | no |
//! | VM: `<`, `<=`, `==` on long strings | strings | O(length) a comparison | each instruction at most O(VM memory), at most [`INSTRUCTION_STEP`](super::limits::INSTRUCTION_STEP) (100) instructions between checks, all of them within [`MAX_LUA_TIME`](super::limits::MAX_LUA_TIME) across the candidate's passes and [`MAX_INSTRUCTIONS`](super::limits::MAX_INSTRUCTIONS) | no |
//! | VM: a numeral string as a `for` bound | strings | O(length) a numeral read (`luaV_tonumber_`) | each instruction at most O(VM memory), at most [`INSTRUCTION_STEP`](super::limits::INSTRUCTION_STEP) (100) instructions between checks, all of them within [`MAX_LUA_TIME`](super::limits::MAX_LUA_TIME) across the candidate's passes and [`MAX_INSTRUCTIONS`](super::limits::MAX_INSTRUCTIONS) | no |
//! | VM: a table constructor or call with many values | values on the stack | O(n) copies | n at most the stack; each instruction at most O(VM memory), at most [`INSTRUCTION_STEP`](super::limits::INSTRUCTION_STEP) (100) instructions between checks, all of them within [`MAX_LUA_TIME`](super::limits::MAX_LUA_TIME) across the candidate's passes and [`MAX_INSTRUCTIONS`](super::limits::MAX_INSTRUCTIONS) | no |
//! | VM: an error message (type errors, names) | the failing operation | O(message): constant names and `__name` | caught messages charged by the handler, `__name` at most 64 bytes | no |
//! | VM: compilation, garbage collection | sources, VM memory | O(source), O(VM memory) | the read caps and VM memory | no |
//!
//! No host function returns a Rust error to Lua: each returns its status
//! and its Lua face raises a fault as a fixed marker, so no error carries
//! a traceback outside the VM's memory. Each takes a fixed number of
//! arguments, at most four (the protected calls' `settle`), extras
//! dropped in Lua before the call: mlua converts and pins every argument
//! before the host function runs, and its auxiliary stack starts with 17
//! free slots (`LUA_MINSTACK` less its reserve of 3), so a host call's
//! own arguments never make it grow; nested loads (at most 16 deep) pin a
//! few more each. The instruction hook raises its fault as mlua's own
//! error, with no traceback; after it, the protected calls raise the
//! marker as they unwind, and the hook raises the fault again only past a
//! grace of 1 000 steps.
//!
//! Skin code never runs with the hook off: Lua runs the message handler
//! where an error is raised, inside the instruction hook for a fault, so
//! the protected calls' handlers check the ledger first and complete with
//! the marker instead of running the skin's; finalizers (`__gc`) and
//! to-be-closed variables (`__close`) are refused at the metatable, and
//! the sandbox's own weak tables have neither. The strings' metatable is
//! out of reach (`getmetatable` of a string gives `false`), so a skin adds
//! no metamethod a native would call. Lua runs no other skin code
//! itself.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, Table, Value};

use skin_model::MAX_PATH_BYTES;

use super::limits::MAX_PRIMITIVE_BYTES;
use super::{LuaWhy, Pass};

/// The library, compiled in the sandbox before any skin code.
const LIBRARY: &str = include_str!("library.lua");

/// The registry name of the library's wrapper for a host function taking
/// one string.
pub(super) const STRING_BRIDGE: &str = "tapestry.string_bridge";

/// What `library.lua` refuses, by its refusal code (1 first); code 8 is a
/// finalizer, [`LuaWhy::Finalizer`].
pub(crate) const REFUSED: [&str; 11] = [
    "pattern %b",
    "pattern %f",
    "pattern back reference",
    "table with a metatable",
    "result over the primitive cap",
    "format conversion",
    "pattern %z",
    "metatable with __gc, __mode or __close",
    "tonumber of a long numeral or another base",
    "metatable __name over 64 bytes",
    "bridge argument over the path limit",
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
    native.raw_set("concat", table.raw_get::<Function>("concat")?)?;
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
    // Each returns its status, never a Rust error: `library.lua` raises
    // the fault in Lua.
    let charge = lua.create_function(move |lua, n: i64| {
        let n = u64::try_from(n).unwrap_or(0);
        Ok(p.ledger
            .check(lua)
            .and_then(|()| p.ledger.native(lua, n))
            .is_ok())
    })?;
    let p = Rc::clone(pass);
    let refuse = lua.create_function(move |lua, code: i64| {
        let why = if code == FINALIZER {
            LuaWhy::Finalizer
        } else {
            let op = usize::try_from(code - 1)
                .ok()
                .and_then(|i| REFUSED.get(i))
                .copied()
                .unwrap_or("library");
            LuaWhy::Library { op }
        };
        let _ = p.ledger.fail_lua(lua, why);
        Ok(false)
    })?;
    let bridge = lua.create_table()?;
    let p = Rc::clone(pass);
    bridge.raw_set(
        "metatable",
        lua.create_function(move |lua, ()| {
            let ok = p.ledger.check(lua).is_ok();
            if ok {
                p.ledger.update(|c| c.metatables += 1);
            }
            Ok(ok)
        })?,
    )?;
    let (string_lib, table_lib, math_lib, lib): (Table, Table, Table, Table) = lua
        .load(LIBRARY)
        .set_name("=library")
        .set_mode(ChunkMode::Text)
        // No globals: every name the library uses is its own.
        .set_environment(lua.create_table()?)
        .call((
            native,
            charge,
            refuse,
            bridge,
            MAX_PRIMITIVE_BYTES,
            MAX_PATH_BYTES,
            super::sandbox::FAULT,
        ))?;
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
        // Unreachable from a skin: `getmetatable` of a string gives this
        // marker, so no skin adds a metamethod Lua's natives would call
        // (a `__tostring` that `%s` runs, say).
        mt.raw_set("__metatable", false)?;
    }
    env.raw_set("string", string_lib)?;
    env.raw_set("table", table_lib)?;
    env.raw_set("math", math_lib)?;
    for name in ["tonumber", "select", "error", "setmetatable", "ipairs"] {
        env.raw_set(name, lib.raw_get::<Function>(name)?)?;
    }
    lua.set_named_registry_value(STRING_BRIDGE, lib.raw_get::<Function>("string_bridge")?)?;
    Ok(())
}
