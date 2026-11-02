//! The configured pass's `skin_config`: the user's settings as the
//! header declares them, the names the pinned source exports
//! (`SkinLuaAccessor.java:926-987` at `0.8.9`).
//!
//! - `option[name]`: the number of the item selected in the option
//!   `name`, as [`Config::options`] selects it; absent when none is.
//! - `enabled_options`: the selected items' numbers, in the header's
//!   order of options.
//! - `file_path[name]`: the file choice `name`'s path with its chosen
//!   candidate substituted, relative to the entry's folder, as the
//!   importer resolves a source path ([`Choices`]); absent with no
//!   candidate.
//! - `offset[name]`: the offset `name`'s `x`, `y`, `w`, `h`, `r` and `a`,
//!   the configured value of an offset the header declares, else zeros.
//! - `get_path(path)`: `path` with the file choice that prefixes it
//!   substituted, as `file_path`; `nil` when the choice has no candidate
//!   or a wildcard is left.
//!
//! The tables are plain and the skin may change them, as the source
//! lets it: each pass builds them afresh from the validated header and
//! settings and discards them with its VM, so a write reaches nothing
//! else, not the host's settings, not the offsets the host applies and
//! not the other pass. It grants nothing the skin lacks: its Lua already
//! decides the definition it returns. Of entries sharing a name, the
//! last declared wins. An entry without a name is absent.

use std::rc::Rc;

use mlua::{Lua, Table, Value};

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
        let choices = Choices::new(header, config, &mut Vec::new());
        let files = header
            .files
            .iter()
            .filter_map(|f| {
                let name = f.name.clone()?;
                match choices.substitute(f.path.as_deref()?) {
                    Path::File(p) => Some((name, p)),
                    _ => None,
                }
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
    pass: &Rc<Pass>,
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
    let get_path = host(lua, pass, |lua, pass, args| {
        let Some(Value::String(p)) = args.front() else {
            return Err(mlua::Error::runtime("get_path takes a path"));
        };
        let p = p.to_str()?.to_owned();
        match pass.bridge.as_ref().and_then(|b| b.path(&p)) {
            Some(s) => Ok(Value::String(lua.create_string(s)?)),
            None => Ok(Value::Nil),
        }
    })?;
    let config = lua.create_table()?;
    config.raw_set("option", option)?;
    config.raw_set("enabled_options", enabled)?;
    config.raw_set("file_path", file_path)?;
    config.raw_set("offset", offset)?;
    config.raw_set("get_path", get_path)?;
    env.raw_set("skin_config", config)
}
