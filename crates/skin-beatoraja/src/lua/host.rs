//! The host's modules: `main_state`, `timer_util` and `event_util`, with
//! exactly the names the pinned source exports, at beatoraja's `0.8.9`
//! tag under `src/bms/player/beatoraja/skin/lua/`:
//!
//! - `main_state`: `MainStatePropertyLuaApiExporter.java:33-87` (its
//!   `timer_off_value` a value, the rest functions),
//!   `SkinFileLuaApiExporter.java:29-86`,
//!   `SkinRuntimeLuaApiExporter.java:42-77` (the four functions it sets on
//!   the module), `SkinHttpLuaApiExporter.java:30-31` and
//!   `SkinAudioLuaApiExporter.java:20-48`, in the order
//!   `MainStateAccessor.java:29-35` exports them;
//! - `timer_util`: `TimerUtility.java:19-24`;
//! - `event_util`: `EventUtility.java:18-22`.
//!
//! In the header pass they are empty tables, as the source loads them
//! there (`SkinLuaAccessor.java:97-101`). In the configured pass a skin may
//! require them and keep their functions, which its per-frame functions
//! call; a call during import reads live play state, or acts on files,
//! the network or audio, none of which an import may, and is refused
//! rather than answered with an invented value; only `main_state.option`,
//! `main_state.number` and `main_state.text` answer, from the import facts
//! of the pass's snapshot and the scene's constants ([`super::facts`]).
//! These three return a status into Lua, never a Rust error, and are
//! counted as host calls like every other.
//! There each module is read-only: the skin gets an empty proxy whose `__index` is the
//! module's table, out of the skin's reach, and whose `__newindex`
//! refuses the write as a fault ([`LuaWhy::HostWrite`]), whatever local
//! it is written through; its metatable is protected. So `main_state.f`
//! is the host function by identity when a callback reads it. A name the source does
//! not export is `nil`. `main_state.timer_off_value` is the source's off
//! sentinel, the smallest integer, kept so that a skin can test a timer
//! for off.

use std::sync::Arc;

use mlua::{Lua, MultiValue, Table, Value};

use super::facts::{self, NumberRead, OptionRead, TextRead};
use super::{LuaWhy, Pass, host};

/// The host API functions, by module: what a report names, and what a
/// [`CallableKind::Host`](crate::CallableKind::Host) indexes.
pub const HOST_APIS: [(&str, &str); 65] = [
    ("main_state", "option"),
    ("main_state", "number"),
    ("main_state", "float_number"),
    ("main_state", "text"),
    ("main_state", "offset"),
    ("main_state", "timer"),
    ("main_state", "timer_is_on"),
    ("main_state", "timer_is_off"),
    ("main_state", "timer_elapsed"),
    ("main_state", "timer_elapsed_ms"),
    ("main_state", "timer_elapsed_seconds"),
    ("main_state", "time"),
    ("main_state", "set_timer"),
    ("main_state", "event_exec"),
    ("main_state", "event_index"),
    ("main_state", "key_pressed"),
    ("main_state", "numbers"),
    ("main_state", "screen_width"),
    ("main_state", "screen_height"),
    ("main_state", "rate"),
    ("main_state", "exscore"),
    ("main_state", "rate_best"),
    ("main_state", "exscore_best"),
    ("main_state", "rate_rival"),
    ("main_state", "exscore_rival"),
    ("main_state", "volume_sys"),
    ("main_state", "volume_key"),
    ("main_state", "volume_bg"),
    ("main_state", "gauge"),
    ("main_state", "gauge_type"),
    ("main_state", "set_volume_sys"),
    ("main_state", "set_volume_key"),
    ("main_state", "set_volume_bg"),
    ("main_state", "judge"),
    ("main_state", "file_delete"),
    ("main_state", "file_exists"),
    ("main_state", "file_mkdir"),
    ("main_state", "file_list"),
    ("main_state", "file_read_lines"),
    ("main_state", "file_write"),
    ("main_state", "file_append"),
    ("main_state", "file_clear"),
    ("main_state", "file_count_lines"),
    ("main_state", "open_url"),
    ("main_state", "http_get_async"),
    ("main_state", "http_poll"),
    ("main_state", "http_cancel"),
    ("main_state", "http_get_lines"),
    ("main_state", "http_get"),
    ("main_state", "audio_play"),
    ("main_state", "audio_loop"),
    ("main_state", "audio_preload"),
    ("main_state", "audio_stop"),
    ("main_state", "audio_dispose"),
    ("timer_util", "now_timer"),
    ("timer_util", "is_timer_on"),
    ("timer_util", "is_timer_off"),
    ("timer_util", "timer_function"),
    ("timer_util", "timer_observe_boolean"),
    ("timer_util", "new_passive_timer"),
    ("event_util", "event_observe_turn_true"),
    ("event_util", "event_observe_timer"),
    ("event_util", "event_observe_timer_on"),
    ("event_util", "event_observe_timer_off"),
    ("event_util", "event_min_interval"),
];

/// `main_state.option`'s index in [`HOST_APIS`].
const OPTION: u16 = 0;
/// `main_state.number`'s index in [`HOST_APIS`].
const NUMBER: u16 = 1;
/// `main_state.text`'s index in [`HOST_APIS`].
const TEXT: u16 = 3;

/// A host API's argument as an import fact's id: a Lua integer within
/// `i32` (the integer subtype: `33.0` and `"33"` are not), or a refusal,
/// fatal and located, labelled `<api> argument`.
fn fact_id(lua: &Lua, pass: &Pass, args: &MultiValue, api: &str) -> mlua::Result<i32> {
    let n = match args.front() {
        Some(Value::Integer(n)) => i32::try_from(*n).ok(),
        _ => None,
    };
    n.ok_or_else(|| {
        pass.count_fact(|| format!("{api} argument"));
        pass.ledger.fail_lua(lua, LuaWhy::FactArgument)
    })
}

/// `main_state.option(n)` during the configured pass: an import fact from
/// the pass's snapshot ([`facts::option`]), recorded on its first read;
/// an id the source does not know or a scene's constant, unrecorded; and
/// a refusal, fatal and located, for an argument that is not an integer
/// within `i32` (the integer subtype: `33.0` and `"33"` refuse), a fact
/// the snapshot has unavailable and a built-in that is not a fact. A fact
/// the snapshot has pending stops the pass by the same fatal path.
fn option(lua: &Lua, pass: &Pass, args: MultiValue) -> mlua::Result<bool> {
    #[cfg(feature = "dev-tools")]
    if pass.in_runtime()
        && let Some(r) = super::replay::answer(lua, pass, OPTION, &args)
    {
        return r.map(|v| matches!(v, Value::Boolean(true)));
    }
    let n = fact_id(lua, pass, &args, "option")?;
    if pass.in_runtime() {
        return Ok(runtime_option(pass, n));
    }
    let Some(snapshot) = &pass.facts else {
        return Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: OPTION }));
    };
    let id = n.unsigned_abs();
    match facts::option(snapshot, n) {
        OptionRead::Constant { value, known } => {
            pass.count_fact(|| {
                if known {
                    format!("option {id} constant")
                } else {
                    "option unknown".to_owned()
                }
            });
            Ok(value)
        }
        OptionRead::Fact { value, read } => {
            pass.count_fact(|| format!("option {id} read"));
            pass.read_fact(read);
            Ok(value)
        }
        OptionRead::Pending(fact) => {
            pass.count_fact(|| format!("option {id} pending"));
            Err(pass.ledger.pending(fact))
        }
        OptionRead::Unavailable => {
            pass.count_fact(|| format!("option {id} unavailable"));
            Err(pass
                .ledger
                .fail_lua(lua, LuaWhy::Unavailable { api: OPTION, id: n }))
        }
        OptionRead::LiveState => {
            pass.count_fact(|| format!("option {id} live state"));
            Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: OPTION }))
        }
    }
}

/// `main_state.number(n)` during the configured pass: the play level, an
/// import fact ([`facts::number`]), answered and recorded on its first
/// read, or ending the pass pending, or refused when unavailable; every
/// other id reads play state, refused. An argument that is not an
/// integer within `i32` refuses ([`LuaWhy::FactArgument`]). Every refusal
/// is fatal and located, as `option`'s.
fn number(lua: &Lua, pass: &Pass, args: MultiValue) -> mlua::Result<Value> {
    #[cfg(feature = "dev-tools")]
    if pass.in_runtime()
        && let Some(r) = super::replay::answer(lua, pass, NUMBER, &args)
    {
        return r;
    }
    let n = fact_id(lua, pass, &args, "number")?;
    if pass.in_runtime() {
        return Ok(Value::Number(f64::from(runtime_number(pass, n))));
    }
    let Some(snapshot) = &pass.facts else {
        return Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: NUMBER }));
    };
    match facts::number(snapshot, n) {
        NumberRead::Fact { value, read } => {
            pass.count_fact(|| format!("number {n} read"));
            pass.read_fact(read);
            Ok(Value::Integer(i64::from(value)))
        }
        NumberRead::Pending(fact) => {
            pass.count_fact(|| format!("number {n} pending"));
            Err(pass.ledger.pending(fact))
        }
        NumberRead::Unavailable => {
            pass.count_fact(|| format!("number {n} unavailable"));
            Err(pass
                .ledger
                .fail_lua(lua, LuaWhy::Unavailable { api: NUMBER, id: n }))
        }
        NumberRead::LiveState => {
            pass.count_fact(|| format!("number {n} live state"));
            Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: NUMBER }))
        }
    }
}

/// `main_state.text(n)` during the configured pass: a scene's constant
/// ([`facts::text`]), a string made in the VM under its memory cap and
/// recorded nowhere; every other id reads play state, refused. Its
/// argument is checked as `number`'s.
fn text(lua: &Lua, pass: &Pass, args: MultiValue) -> mlua::Result<mlua::LuaString> {
    let n = fact_id(lua, pass, &args, "text")?;
    if pass.in_runtime() {
        // Texts are not bridged at runtime yet: the empty text, noted.
        pass.runtime_notes
            .borrow_mut()
            .add(super::runtime::RuntimeNote::Absent {
                api: "text",
                id: n,
                value: 0,
            });
        return lua.create_string("");
    }
    let Some(snapshot) = &pass.facts else {
        return Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: TEXT }));
    };
    match facts::text(snapshot, n) {
        TextRead::Constant(s) => {
            pass.count_fact(|| format!("text {n} constant"));
            lua.create_string(s)
        }
        TextRead::LiveState => {
            pass.count_fact(|| format!("text {n} live state"));
            Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: TEXT }))
        }
    }
}

/// `main_state.number(n)` on a screen's runtime: the profile's bridge
/// over the frame's fields; for an id it omits, the source's value in the
/// feature's absence ([`super::runtime::absent`]), else 0, noted once.
fn runtime_number(pass: &Pass, n: i32) -> i32 {
    let mut screen = pass.screen.borrow_mut();
    #[cfg(feature = "dev-tools")]
    super::vm_hooks::holding_context();
    let Some(s) = screen.as_mut() else {
        return 0;
    };
    if let Some(v) = s.number(n) {
        return v;
    }
    let value = super::runtime::absent(s.kind(), n).unwrap_or(0);
    drop(screen);
    pass.runtime_notes
        .borrow_mut()
        .add(super::runtime::RuntimeNote::Absent {
            api: "number",
            id: n,
            value,
        });
    value
}

/// `main_state.option(n)` on a screen's runtime: the profile's bridge, a
/// negative id negated; false for an id it omits, noted once.
fn runtime_option(pass: &Pass, n: i32) -> bool {
    let mut screen = pass.screen.borrow_mut();
    let Some(s) = screen.as_mut() else {
        return false;
    };
    if let Some(v) = s.option(n.unsigned_abs().try_into().unwrap_or(i32::MAX)) {
        return v != (n < 0);
    }
    drop(screen);
    pass.runtime_notes
        .borrow_mut()
        .add(super::runtime::RuntimeNote::Absent {
            api: "option",
            id: n,
            value: 0,
        });
    false
}

/// The host modules' names.
pub(super) const MODULES: [&str; 3] = ["main_state", "timer_util", "event_util"];

/// The registry name of host module `name`'s table in this VM.
fn registry_name(name: &str) -> String {
    format!("tapestry.host.{name}")
}

/// The registry name of host module `name`'s proxy metatable.
fn metatable_name(name: &str) -> String {
    format!("tapestry.host_mt.{name}")
}

/// Builds the host modules of a fresh VM, kept in its registry for
/// `require`.
pub(super) fn install(lua: &Lua, pass: &Arc<Pass>) -> mlua::Result<()> {
    let configured = pass.bridge.is_some();
    let write = if configured {
        Some(host(lua, pass, 0, |lua, pass, _| -> mlua::Result<()> {
            Err(pass.ledger.fail_lua(lua, LuaWhy::HostWrite))
        })?)
    } else {
        None
    };
    for module in MODULES {
        let t = lua.create_table()?;
        // The header pass's modules are empty.
        for (index, &(m, api)) in (0_u16..).zip(&HOST_APIS) {
            if m != module || !configured {
                continue;
            }
            let f = if index == OPTION {
                host(lua, pass, 1, option)?
            } else if index == NUMBER {
                host(lua, pass, 1, number)?
            } else if index == TEXT {
                host(lua, pass, 1, text)?
            } else {
                // A reference's play host answers what it can (dev-tools),
                // its one argument passed.
                #[cfg(feature = "dev-tools")]
                let arity = 1;
                #[cfg(not(feature = "dev-tools"))]
                let arity = 0;
                host(
                    lua,
                    pass,
                    arity,
                    move |lua, pass, _args| -> mlua::Result<Value> {
                        #[cfg(feature = "dev-tools")]
                        if pass.in_runtime()
                            && let Some(r) = super::replay::answer(lua, pass, index, &_args)
                        {
                            return r;
                        }
                        Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: index }))
                    },
                )?
            };
            pass.host_api(f.to_pointer().addr(), index);
            t.raw_set(api, f)?;
        }
        if module == "main_state" && configured {
            t.raw_set("timer_off_value", i64::MIN)?;
        }
        let face = match &write {
            Some(write) => {
                let mt = lua.create_table()?;
                mt.raw_set("__index", t)?;
                mt.raw_set("__newindex", write.clone())?;
                mt.raw_set("__metatable", false)?;
                let proxy = lua.create_table()?;
                proxy.set_metatable(Some(mt.clone()))?;
                lua.set_named_registry_value(&metatable_name(module), mt)?;
                proxy
            }
            None => t,
        };
        lua.set_named_registry_value(&registry_name(module), face)?;
    }
    Ok(())
}

/// The configured pass's host module proxies: each one's address, its
/// [`MODULES`] index, and whether it is as the pass built it (no entry of
/// its own, and the metatable it was given).
pub(super) fn proxies(lua: &Lua) -> mlua::Result<Vec<(usize, u8, bool)>> {
    let mut out = Vec::with_capacity(MODULES.len());
    for (index, module) in (0_u8..).zip(MODULES) {
        let Value::Table(proxy) = lua.named_registry_value::<Value>(&registry_name(module))? else {
            continue;
        };
        let Value::Table(mt) = lua.named_registry_value::<Value>(&metatable_name(module))? else {
            continue;
        };
        let empty = proxy.clone().pairs::<Value, Value>().next().is_none();
        let same = proxy
            .metatable()
            .is_some_and(|m| m.to_pointer() == mt.to_pointer());
        out.push((proxy.to_pointer().addr(), index, empty && same));
    }
    Ok(out)
}

/// Host module `name`; `None` when `name` names none.
pub(super) fn module(lua: &Lua, name: &[u8]) -> mlua::Result<Option<Table>> {
    let Some(&module) = MODULES.iter().find(|m| m.as_bytes() == name) else {
        return Ok(None);
    };
    match lua.named_registry_value::<Value>(&registry_name(module))? {
        Value::Table(t) => Ok(Some(t)),
        _ => Ok(None),
    }
}
