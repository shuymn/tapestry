//! The developer's conversion report (docs/stage3.md §3, the conversion
//! report): what a Lua skin's import met, as counts and outcomes under
//! allowlisted labels. Nothing the skin wrote reaches it: no source,
//! name, path, string, generated definition or Lua error, only the
//! importer's own labels, the host API names of [`HOST_APIS`], numbers,
//! and module indices in place of file names.

use std::collections::BTreeMap;

use std::path::Path;

use super::convert::callables;
use super::{Cancel, HOST_APIS, LuaUsage, LuaWhy, header_pass};
use crate::error::{ImportError, Loc};
use crate::import::{Config, NoteKind};
use crate::tree::CallableKind;

/// One import's outcome under allowlisted labels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Survey {
    /// `imported`, or the refusal's label.
    pub outcome: String,
    /// The refusal's location: a module index (the order the passes
    /// consulted the files, the entry 0), a line and a column. A converted
    /// value has line 0 and its ordinal in the returned table as the
    /// column.
    pub at: Option<(usize, u32, u32)>,
    /// The files the passes consulted.
    pub modules: usize,
    /// The functions the configured definition holds, by the field that
    /// holds them and what they are.
    pub callables: BTreeMap<(&'static str, String), u32>,
    /// The import's notes, by kind.
    pub notes: BTreeMap<String, u32>,
    pub usage: LuaUsage,
}

/// The name of `v`'s variant alone, without the values it holds.
fn variant(v: &impl std::fmt::Debug) -> String {
    let s = format!("{v:?}");
    s.split(['(', ' ', '{']).next().unwrap_or("").to_owned()
}

/// The label of `e`: its kind and the importer's own constants.
fn label(e: &ImportError) -> String {
    match e {
        ImportError::Read { source, .. } => format!("read {}", variant(source)),
        ImportError::Syntax { what, .. } => format!("json syntax {what:?}"),
        ImportError::Input { limit, .. } => format!("input {limit:?}"),
        ImportError::IncludeCycle { .. } => "include cycle".into(),
        ImportError::IncludeWildcard { .. } => "include wildcard".into(),
        ImportError::Pattern { .. } => "file choice pattern".into(),
        ImportError::Type {
            field, expected, ..
        } => format!("type `{field}` {expected}"),
        ImportError::Range { field, .. } => format!("range `{field}`"),
        ImportError::NotPlay7 { .. } => "not 7-key play".into(),
        ImportError::Resolution { w, h, .. } => format!("size {w}x{h}"),
        ImportError::Limit { limit, .. } => format!("limit {limit:?}"),
        ImportError::Critical { part, why, .. } => format!("critical {part:?}: {}", note(why)),
        ImportError::Expr { .. } => "expression".into(),
        ImportError::Asset { .. } => "critical asset".into(),
        ImportError::Model { .. } => "model".into(),
        ImportError::Lua { why, .. } => match why {
            LuaWhy::LiveState { api } => {
                let (m, n) = HOST_APIS
                    .get(usize::from(*api))
                    .copied()
                    .unwrap_or(("?", "?"));
                format!("lua live state {m}.{n}")
            }
            why => format!("lua {why:?}"),
        },
    }
}

/// The label of a note's kind: its variant, and a callable's field.
fn note(kind: &NoteKind) -> String {
    match kind {
        NoteKind::UnsupportedField { field } => format!("UnsupportedField {field}"),
        NoteKind::Ignored { field } => format!("Ignored {field}"),
        other => variant(other),
    }
}

/// Where `at` is: its file's index among `modules`.
fn locate(at: Option<&Loc>, modules: &[String]) -> Option<(usize, u32, u32)> {
    let at = at?;
    let i = modules.iter().position(|m| *m == at.file)?;
    Some((i, at.line, at.col))
}

fn at_of(e: &ImportError) -> Option<&Loc> {
    match e {
        ImportError::Read { at, .. }
        | ImportError::Input { at, .. }
        | ImportError::NotPlay7 { at }
        | ImportError::Resolution { at, .. }
        | ImportError::Limit { at, .. }
        | ImportError::Model { at, .. }
        | ImportError::Lua { at, .. } => at.as_ref(),
        ImportError::Syntax { at, .. }
        | ImportError::IncludeCycle { at, .. }
        | ImportError::IncludeWildcard { at }
        | ImportError::Pattern { at }
        | ImportError::Type { at, .. }
        | ImportError::Range { at, .. }
        | ImportError::Critical { at, .. }
        | ImportError::Expr { at, .. }
        | ImportError::Asset { at, .. } => Some(at),
    }
}

/// Runs the Lua skin `entry` of skin folder `root` with its default
/// configuration, counts the functions its configured definition holds
/// and imports it, keeping what the candidate used and consulted however
/// far it got, and reports only allowlisted labels and numbers.
#[must_use]
pub fn survey(root: &Path, entry: &str, cancel: &Cancel) -> Survey {
    let header = header_pass(root, entry, cancel);
    let h = match header.result {
        Ok(h) => h,
        Err(e) => {
            return Survey {
                outcome: format!("header {}", label(&e)),
                at: locate(at_of(&e), &header.dependencies),
                modules: header.dependencies.len(),
                usage: header.counts,
                ..Survey::default()
            };
        }
    };
    let config = Config::defaults(h.header());
    let mut s = Survey::default();
    let (def, counts, store) = h.configured(&config, cancel);
    s.usage = counts;
    s.modules = store.dependencies.len();
    if let Ok(def) = def {
        {
            for (_, field, kind) in callables(&def.arena) {
                let kind = match kind {
                    CallableKind::Lua => "lua".to_owned(),
                    CallableKind::Host(api) => {
                        let (m, n) = HOST_APIS
                            .get(usize::from(api))
                            .copied()
                            .unwrap_or(("?", "?"));
                        format!("{m}.{n}")
                    }
                    CallableKind::Library => "library".to_owned(),
                };
                *s.callables.entry((field, kind)).or_default() += 1;
            }
        }
    }
    let o = h.complete(&config, cancel);
    s.usage = o.counts;
    s.modules = o.dependencies.len();
    match o.result {
        Ok(imported) => {
            s.outcome = "imported".into();
            for n in &imported.report.notes {
                *s.notes.entry(note(&n.kind)).or_default() += 1;
            }
        }
        Err(e) => {
            s.outcome = label(&e);
            s.at = locate(at_of(&e), &o.dependencies);
        }
    }
    s
}
