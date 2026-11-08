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
        let offsets = header
            .offsets
            .iter()
            .filter_map(|o| {
                let v = config.offsets.get(&o.id).copied().unwrap_or_default();
                Some((o.name.clone()?, v))
            })
            .collect();
        Self {
            options,
            enabled,
            files,
            offsets,
            choices,
        }
    }

    /// `path` with its file choice substituted.
    fn path(&self, path: &str) -> Option<String> {
        match self.choices.substitute(path) {
            Path::File(p) => Some(p),
            Path::Missing | Path::Glob => None,
        }
    }
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
        let t = lua.create_table()?;
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
        offset.raw_set(name.as_str(), t)?;
    }
    // The library checks the argument; the host returns a path or nil and
    // never raises an error.
    let native = host(lua, pass, 1, |lua, pass, args| {
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
