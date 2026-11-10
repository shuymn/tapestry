//! The configured pass's `skin_config`: the user's settings as the
//! header declares them, the names the pinned source exports
//! (`SkinLuaAccessor.java:926-987` at `0.8.9`).
//!
//! - `option[name]`: the number of the item selected in the option
//!   `name`, as [`Config::options`] selects it; absent when none is.
//! - `enabled_options`: the selected items' numbers, in the header's
//!   order of options.
//! - `file_path[name]`: the name of the candidate the file choice `name`
//!   uses, the file's or folder's name within the choice's folder, as the
//!   source's configuration holds it (`SkinConfigurationView.java:392`);
//!   absent with no candidate.
//! - `offset[name]`: the offset `name`'s `x`, `y`, `w`, `h`, `r` and `a`,
//!   the configured value of an offset the header declares, else zeros.
//!
//! The same values answer `main_state.offset(id)` by id
//! ([`Bridge::offset`]), a nameless declared offset's included.
//! - `get_path(path)`: `path`, relative to the entry's folder, with the
//!   file choice that prefixes it substituted by the same candidate, as
//!   the importer resolves a source path ([`Choices`]); `nil` when the
//!   choice has no candidate or a wildcard is left. A skin may also join
//!   a choice's folder and its `file_path` and pass that.
//!
//! The tables are plain and the skin may change them, as the source
//! lets it: each pass builds them afresh from the validated header and
//! settings and discards them with its VM, so a write reaches nothing
//! else, not the host's settings, not the offsets the host applies and
//! not the other pass. It grants nothing the skin lacks: its Lua already
//! decides the definition it returns. Of entries sharing a name, the
//! last declared wins. An entry without a name is absent.

use std::collections::BTreeMap;
use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};
use skin_model::MAX_PATH_BYTES;

use super::{Pass, host};
use crate::header::Header;
use crate::import::asset::{Choices, Path};
use crate::import::{Config, OffsetValue};

/// The settings a configured pass exposes.
pub(crate) struct Bridge {
    options: Vec<(String, i32)>,
    enabled: Vec<i32>,
    files: Vec<(String, String)>,
    offsets: Vec<(String, OffsetValue)>,
    /// Every offset the header declares, by id, named or not: its
    /// configured value, else zeros.
    offset_ids: BTreeMap<i32, OffsetValue>,
    choices: Choices,
}

impl Bridge {
    pub(crate) fn new(header: &Header, config: &Config) -> Self {
        let selected = || {
            header
                .options
                .iter()
                .zip(config.options.iter().chain(std::iter::repeat(&None)))
        };
        let enabled = selected().filter_map(|(_, &sel)| sel).collect();
        let options = selected()
            .filter_map(|(o, &sel)| Some((o.name.clone()?, sel?)))
            .collect();
        let mut picks = Vec::with_capacity(header.files.len());
        let choices = Choices::new(header, config, &mut picks);
        // Each choice's own pick, the one `get_path` substitutes: an entry
        // name in the choice's folder, never a path.
        let files = header
            .files
            .iter()
            .zip(&picks)
            .filter_map(|(f, pick)| {
                f.path.as_ref()?;
                let chosen = f.candidates.get(usize::from(pick.candidate?))?;
                Some((f.name.clone()?, chosen.clone()))
            })
            .collect();
        let configured = |id: i32| config.offsets.get(&id).copied().unwrap_or_default();
        let offsets = header
            .offsets
            .iter()
            .filter_map(|o| Some((o.name.clone()?, configured(o.id))))
            .collect();
        let offset_ids = header
            .offsets
            .iter()
            .map(|o| (o.id, configured(o.id)))
            .collect();
        Self {
            options,
            enabled,
            files,
            offsets,
            offset_ids,
            choices,
        }
    }

    /// Offset `id` as the import applies it: its configured value when
    /// the header declares it (`import/mod.rs`, a destination's offsets),
    /// else zeros.
    pub(crate) fn offset(&self, id: i32) -> OffsetValue {
        self.offset_ids.get(&id).copied().unwrap_or_default()
    }

    /// `path` with its file choice substituted.
    fn path(&self, path: &str) -> Option<String> {
        match self.choices.substitute(path) {
            Path::File(p) => Some(p),
            Path::Missing | Path::Glob => None,
        }
    }
}

/// A fresh table of `v`'s `x`, `y`, `w`, `h`, `r` and `a`, Lua integers:
/// an entry of `skin_config.offset`, and `main_state.offset`'s answer.
pub(super) fn offset_table(lua: &Lua, v: OffsetValue) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(0, 6)?;
    for (k, n) in [
        ("x", v.x),
        ("y", v.y),
        ("w", v.w),
        ("h", v.h),
        ("r", v.r),
        ("a", v.a),
    ] {
        t.raw_set(k, n)?;
    }
    Ok(t)
}

/// Sets the global `skin_config` of a configured pass's VM: plain tables
/// the pass may change, built afresh for it.
pub(super) fn install(
    lua: &Lua,
    pass: &Arc<Pass>,
    env: &Table,
    bridge: &Bridge,
) -> mlua::Result<()> {
    let option = lua.create_table()?;
    let enabled = lua.create_table()?;
    for (name, op) in &bridge.options {
        option.raw_set(name.as_str(), *op)?;
    }
    for (i, op) in (1_i64..).zip(&bridge.enabled) {
        enabled.raw_set(i, *op)?;
    }
    let file_path = lua.create_table()?;
    for (name, path) in &bridge.files {
        file_path.raw_set(name.as_str(), path.as_str())?;
    }
    let offset = lua.create_table()?;
    for (name, v) in &bridge.offsets {
        offset.raw_set(name.as_str(), offset_table(lua, *v)?)?;
    }
    // The library checks the argument; the host returns a path or nil and
    // never raises an error.
    let native = host(lua, pass, 1, |lua, pass, args| {
        pass.import_only(lua)?;
        // Its length is checked before any copy, as the library's is.
        let path = match args.front() {
            Some(Value::String(p)) if p.as_bytes().len() <= MAX_PATH_BYTES => p
                .to_str()
                .ok()
                .and_then(|p| pass.bridge.as_ref().and_then(|b| b.path(&p))),
            _ => None,
        };
        match path {
            Some(s) if s.len() <= MAX_PATH_BYTES => Ok(Value::String(lua.create_string(s)?)),
            _ => Ok(Value::Nil),
        }
    })?;
    let wrap: Function = lua.named_registry_value(super::library::STRING_BRIDGE)?;
    let get_path: Function = wrap.call((native, "get_path"))?;
    let config = lua.create_table()?;
    config.raw_set("option", option)?;
    config.raw_set("enabled_options", enabled)?;
    config.raw_set("file_path", file_path)?;
    config.raw_set("offset", offset)?;
    config.raw_set("get_path", get_path)?;
    lua.set_named_registry_value(CONFIG, config.clone())?;
    env.raw_set("skin_config", config)
}

/// The registry name of the pass's own `skin_config` table, which a
/// callback's capture proves its reads against.
pub(super) const CONFIG: &str = "tapestry.skin_config";
