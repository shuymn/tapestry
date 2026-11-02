//! The host's modules: `main_state`, `timer_util` and `event_util`.
//!
//! A skin may require them during import and keep their functions, which
//! its per-frame functions call; a call during import reads live play
//! state, which does not exist then, and is refused rather than answered
//! with an invented value. `main_state.timer_off_value` is the source's
//! off sentinel, the smallest integer, kept so that a skin can test a
//! timer for off.

use mlua::{Lua, Table, Value};

use super::{LuaWhy, Pass, host};

/// The host API functions, by module: what a report names, and what a
/// [`CallableKind::Host`](crate::CallableKind::Host) indexes.
pub const HOST_APIS: [(&str, &str); 38] = [
    ("main_state", "number"),
    ("main_state", "float_number"),
    ("main_state", "text"),
    ("main_state", "offset"),
    ("main_state", "rate"),
    ("main_state", "exscore"),
    ("main_state", "rate_best"),
    ("main_state", "exscore_best"),
    ("main_state", "rate_rival"),
    ("main_state", "exscore_rival"),
    ("main_state", "volume_sys"),
    ("main_state", "set_volume_sys"),
    ("main_state", "volume_key"),
    ("main_state", "set_volume_key"),
    ("main_state", "volume_bg"),
    ("main_state", "set_volume_bg"),
    ("main_state", "judge"),
    ("main_state", "gauge"),
    ("main_state", "gauge_type"),
    ("main_state", "event_exec"),
    ("main_state", "event_index"),
    ("main_state", "option"),
    ("main_state", "timer"),
    ("main_state", "set_timer"),
    ("main_state", "time"),
    ("main_state", "audio_play"),
    ("main_state", "audio_loop"),
    ("main_state", "audio_stop"),
    ("timer_util", "now_timer"),
    ("timer_util", "is_timer_on"),
    ("timer_util", "is_timer_off"),
    ("timer_util", "timer_function"),
    ("timer_util", "new_passive_timer"),
    ("timer_util", "timer_observe_boolean"),
    ("event_util", "event_min_interval"),
    ("event_util", "event_observe_turn_true"),
    ("event_util", "event_observe_boolean"),
    ("event_util", "event_observe_timer"),
];

/// The host modules' names.
const MODULES: [&str; 3] = ["main_state", "timer_util", "event_util"];

/// The registry name of host module `name`'s table in this VM.
fn registry_name(name: &str) -> String {
    format!("tapestry.host.{name}")
}

/// Builds the host modules of a fresh VM, kept in its registry for
/// `require`.
pub(super) fn install(lua: &Lua, pass: &std::rc::Rc<Pass>) -> mlua::Result<()> {
    for module in MODULES {
        let t = lua.create_table()?;
        for (index, &(m, api)) in (0_u16..).zip(&HOST_APIS) {
            if m != module {
                continue;
            }
            let f = host(lua, pass, move |lua, pass, _| -> mlua::Result<()> {
                Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: index }))
            })?;
            pass.host_api(f.to_pointer().addr(), index);
            t.raw_set(api, f)?;
        }
        if module == "main_state" {
            t.raw_set("timer_off_value", i64::MIN)?;
        }
        lua.set_named_registry_value(&registry_name(module), t)?;
    }
    Ok(())
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
