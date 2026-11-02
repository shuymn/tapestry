//! The configured pass's `skin_config`: the user's settings as the
//! header declares them, read-only.
//!
//! - `option[name]`: the number of the item selected in the option
//!   `name`, as [`Config::options`] selects it; absent when none is.
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
//! Each table lists its entries through `pairs`; a write to one, by
//! assignment or `rawset`, is a fault ([`LuaWhy::ConfigWrite`]), which a
//! protected call cannot hide, and its metatable is hidden and fixed. Of
//! entries sharing a name, the last declared wins. An entry without a
//! name is absent.

use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Lua, Table, Value};

use super::{LuaWhy, Pass, host};
use crate::header::Header;
use crate::import::asset::{Choices, Path};
use crate::import::{Config, OffsetValue};

/// The read-only views, built in the sandbox.
const VIEWS: &str = r#"
local option, file_path, offset, get_path, setmetatable, next, refuse = ...
local views = {}
local function view(t)
  local v = {}
  views[#views + 1] = v
  return setmetatable(v, {
    __index = t,
    __newindex = refuse,
    __pairs = function() return next, t, nil end,
    __len = function() return #t end,
    __metatable = false,
  })
end
local names = {}
for k in next, offset do names[#names + 1] = k end
for _, k in next, names do offset[k] = view(offset[k]) end
return view({
  option = view(option),
  file_path = view(file_path),
  offset = view(offset),
  get_path = get_path,
}), views
"#;

/// The settings a configured pass exposes.
pub(crate) struct Bridge {
    options: Vec<(String, i32)>,
    files: Vec<(String, String)>,
    offsets: Vec<(String, OffsetValue)>,
    choices: Choices,
}

impl Bridge {
    pub(crate) fn new(header: &Header, config: &Config) -> Self {
        let options = header
            .options
            .iter()
            .zip(config.options.iter().chain(std::iter::repeat(&None)))
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

/// Sets the global `skin_config` of a configured pass's VM.
pub(super) fn install(
    lua: &Lua,
    pass: &Rc<Pass>,
    env: &Table,
    bridge: &Bridge,
) -> mlua::Result<()> {
    let option = lua.create_table()?;
    for (name, op) in &bridge.options {
        option.raw_set(name.as_str(), *op)?;
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
    let refuse = host(lua, pass, |lua, pass, _| -> mlua::Result<()> {
        Err(pass.ledger.fail_lua(lua, LuaWhy::ConfigWrite))
    })?;
    let base = lua.globals();
    let (config, views): (Table, Table) = lua
        .load(VIEWS)
        .set_name("=skin_config")
        .set_mode(ChunkMode::Text)
        .call((
            option,
            file_path,
            offset,
            get_path,
            base.raw_get::<Value>("setmetatable")?,
            base.raw_get::<Value>("next")?,
            refuse,
        ))?;
    for v in views.sequence_values::<Table>() {
        pass.views.borrow_mut().push(v?.to_pointer().addr());
    }
    env.raw_set("skin_config", config)
}
