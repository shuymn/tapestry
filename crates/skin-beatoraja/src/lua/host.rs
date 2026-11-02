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
//! rather than answered with an invented value. A name the source does
//! not export is `nil`. `main_state.timer_off_value` is the source's off
//! sentinel, the smallest integer, kept so that a skin can test a timer
//! for off.

use mlua::{Lua, Table, Value};

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
        // The header pass's modules are empty.
        let configured = pass.bridge.is_some();
        for (index, &(m, api)) in (0_u16..).zip(&HOST_APIS) {
            if m != module || !configured {
                continue;
            }
            let f = host(lua, pass, move |lua, pass, _| -> mlua::Result<()> {
                Err(pass.ledger.fail_lua(lua, LuaWhy::LiveState { api: index }))
            })?;
            pass.host_api(f.to_pointer().addr(), index);
            t.raw_set(api, f)?;
        }
        if module == "main_state" && configured {
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
