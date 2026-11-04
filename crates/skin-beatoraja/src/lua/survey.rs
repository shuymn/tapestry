//! The developer's conversion report (docs/stage3.md §3, the conversion
//! report): what a Lua skin's import met, as counts and outcomes under
//! allowlisted labels. Nothing the skin wrote reaches it: no source,
//! name, path, string, generated definition or Lua error, only the
//! importer's own labels, the host API names of [`HOST_APIS`], numbers,
//! and module indices in place of file names.

use std::collections::BTreeMap;

use std::path::Path;

use skin_model::ScreenKind;

use super::convert::callables;
use super::facts::{FactRead, ImportFacts};
use super::ledger::Stop;
use super::{
    Cancel, Disposition, HOST_APIS, LAST_FACTS, LuaUsage, LuaWhy, Unsupported, header_pass,
};
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
    /// What became of them: `lowered`, `replaced` with its reason,
    /// `refused`, `classified only` (returned but never consumed) and
    /// `never analysed` (the import stopped before them).
    pub outcomes: BTreeMap<String, u32>,
    /// The import's notes, by kind.
    pub notes: BTreeMap<String, u32>,
    /// The configured pass's reads of `main_state.option`, by what they
    /// met: `option <id> read`, `pending`, `unavailable`, `constant` or
    /// `live state`, `option unknown` and `option argument`.
    pub facts: BTreeMap<String, u32>,
    /// The facts the configured pass recorded, in first-read order.
    pub reads: Vec<FactRead>,
    pub usage: LuaUsage,
    /// The screen kind the header pass gave, when it gave a header.
    pub kind: Option<ScreenKind>,
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
        ImportError::UnsupportedKind { .. } => "unsupported screen kind".into(),
        ImportError::Slot { .. } => "skin in another slot".into(),
        ImportError::NoImporter { kind } => format!("no importer for {kind:?}"),
        ImportError::Resolution { w, h, .. } => format!("size {w}x{h}"),
        ImportError::Limit { limit, .. } => format!("limit {limit:?}"),
        ImportError::Critical { part, why, .. } => format!("critical {part:?}: {}", note(why)),
        ImportError::Expr { .. } => "expression".into(),
        ImportError::Asset { .. } => "critical asset".into(),
        ImportError::Model { .. } => "model".into(),
        ImportError::Lua { why, .. } => match why {
            LuaWhy::LiveState { api: a } => format!("lua live state {}", api(*a)),
            LuaWhy::Unavailable { api: a, id } => format!("lua unavailable {} {id}", api(*a)),
            LuaWhy::Effect(why) => format!("lua effect {}", unsupported(why)),
            LuaWhy::Critical { field, why } => {
                format!("lua critical {field} {}", unsupported(why))
            }
            why => format!("lua {why:?}"),
        },
    }
}

/// The label of a host API.
fn api(a: u16) -> String {
    let (m, n) = HOST_APIS.get(usize::from(a)).copied().unwrap_or(("?", "?"));
    format!("{m}.{n}")
}

/// The label of why a callback does not convert: the importer's own
/// constructs, host API names and numbers.
fn unsupported(why: &Unsupported) -> String {
    match why {
        Unsupported::Construct(c) => format!("Construct {c}"),
        Unsupported::Binding(b) => format!("Binding {b}"),
        Unsupported::Capture(c) => format!("Capture {c}"),
        Unsupported::HostApi(a) => format!("HostApi {}", api(*a)),
        Unsupported::Unmapped { api: a, id } => format!("Unmapped {} {id}", api(*a)),
        other => variant(other),
    }
}

/// The label of what became of a callable.
fn disposition(d: &Disposition) -> String {
    match d {
        Disposition::NeverAnalysed => "never analysed".into(),
        Disposition::ClassifiedOnly => "classified only".into(),
        Disposition::Lowered => "lowered".into(),
        Disposition::Replaced(why) => format!("replaced {}", unsupported(why)),
        Disposition::Refused => "refused".into(),
    }
}

/// The label of a note's kind.
fn note(kind: &NoteKind) -> String {
    match kind {
        NoteKind::UnsupportedField { field } => format!("UnsupportedField {field}"),
        NoteKind::Ignored { field } => format!("Ignored {field}"),
        NoteKind::Callback { field, why } => format!("Callback {field} {}", unsupported(why)),
        NoteKind::OverCap { cap } => format!("OverCap {cap}"),
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
        | ImportError::UnsupportedKind { at, .. }
        | ImportError::Slot { at, .. }
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
        ImportError::NoImporter { .. } => None,
    }
}

/// Runs the Lua skin `entry` of skin folder `root` with its default
/// configuration against the import facts `facts` on the scene of the
/// kind its header pass gives (`facts.scene` is not read), counts the
/// functions its configured definition holds and imports it, keeping what
/// the candidate used and consulted however far it got, and reports only
/// allowlisted labels and numbers.
#[must_use]
pub fn survey(root: &Path, entry: &str, facts: &ImportFacts, cancel: &Cancel) -> Survey {
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
    let kind = h.header().kind;
    let facts = &ImportFacts {
        scene: kind,
        ..*facts
    };
    let mut s = Survey {
        kind: Some(kind),
        ..Survey::default()
    };
    // One configured pass: its functions counted, then its definition
    // imported.
    let (def, counts, store, reads) = h.configured(&config, facts, cancel);
    s.facts = LAST_FACTS.with(|f| f.take());
    if let Ok(def) = &def {
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
    let o = h.finish(def, counts, store, reads, &config);
    s.reads.clone_from(&o.reads);
    for d in &o.callbacks {
        *s.outcomes.entry(disposition(d)).or_default() += 1;
    }
    s.usage = o.counts;
    s.modules = o.dependencies.len();
    match o.result {
        Ok(imported) => {
            s.outcome = "imported".into();
            for n in &imported.report.notes {
                *s.notes.entry(note(&n.kind)).or_default() += 1;
            }
        }
        Err(Stop::Pending(fact)) => s.outcome = format!("pending {fact:?}"),
        Err(Stop::Fault(e)) => {
            s.outcome = label(&e);
            s.at = locate(at_of(&e), &o.dependencies);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{Cancel, ImportFacts, survey};
    use skin_model::ScreenKind;

    fn folder(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(d.path().join(name), text).unwrap();
        }
        d
    }

    #[test]
    fn a_failed_configured_pass_keeps_its_modules_and_counters() {
        // The module only the configured pass loads fails at its line 2.
        let d = folder(&[
            (
                "s.luaskin",
                "if skin_config then require(\"late\") end\nreturn { type = 0 }",
            ),
            ("late.lua", "local x = 1\nerror(\"late\")"),
        ]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
        );
        assert_eq!(s.outcome, "lua Error");
        assert_eq!(s.modules, 2);
        assert_eq!(s.at, Some((1, 2, 0)));
        assert_eq!(s.usage.requires, 1);
    }

    #[test]
    fn a_converted_value_is_located_by_its_ordinal() {
        let d = folder(&[("s.luaskin", "return { type = 0, extra = { 1, x = 2 } }")]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
        );
        assert_eq!(s.outcome, "header lua Mixed");
        let (module, line, col) = s.at.unwrap();
        assert_eq!((module, line), (0, 0));
        assert!(col > 1, "{col}");
    }

    #[test]
    fn the_survey_runs_each_pass_once() {
        let d = folder(&[(
            "s.luaskin",
            "if skin_config then for i = 1, 4000000 do end end
return { type = 0 }",
        )]);
        crate::lua::tests::PASSES.with(|n| n.set(0));
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
        );
        assert_eq!(crate::lua::tests::PASSES.with(std::cell::Cell::get), 2);
        assert!(s.usage.instructions >= 4_000_000 && s.usage.instructions < 5_000_000);
    }

    #[test]
    fn the_library_s_functions_are_counted_as_library() {
        let d = folder(&[(
            "s.luaskin",
            "if skin_config then\n\
               return { type = 0, extra = { string.find, string.len, type, function() end } }\n\
             end\nreturn { type = 0 }",
        )]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
        );
        let kinds: Vec<(&str, u32)> = s
            .callables
            .iter()
            .map(|((_, kind), n)| (kind.as_str(), *n))
            .collect();
        assert_eq!(kinds, [("library", 3), ("lua", 1)]);
        // A library function has no tree, and the empty function returns
        // nothing: each refuses the candidate, the first at its value.
        assert_eq!(s.outcome, "lua effect Capture not a skin closure");
        assert_eq!(s.outcomes.get("refused"), Some(&4));
    }

    #[test]
    fn an_include_cycle_keeps_the_charge_of_its_last_read() {
        let entry = "return { type = 0, property = { include = \"p.json\" } }";
        let json = "[{\"include\": \"p.json\"}]";
        let d = folder(&[("s.luaskin", entry), ("p.json", json)]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
        );
        assert_eq!(s.outcome, "header include cycle");
        assert_eq!(
            s.usage.input_bytes,
            u64::try_from(entry.len() + 2 * json.len()).unwrap()
        );
    }

    #[test]
    fn screen_kind_refusals_have_their_labels_and_locations() {
        let run = |entry: &str| {
            let d = folder(&[("s.luaskin", entry)]);
            // The snapshot's scene is the header's kind, whatever is given.
            survey(
                d.path(),
                "s.luaskin",
                &ImportFacts::unavailable(ScreenKind::Result),
                &Cancel::new(),
            )
        };
        let s = run("return { type = 1 }");
        assert_eq!(s.outcome, "header unsupported screen kind");
        assert_eq!(s.kind, None);
        let (module, line, _) = s.at.unwrap();
        assert_eq!((module, line), (0, 0));
        for (ty, kind, label) in [
            (5, ScreenKind::Selection, "no importer for Selection"),
            (6, ScreenKind::Decision, "no importer for Decision"),
            (7, ScreenKind::Result, "no importer for Result"),
        ] {
            let s = run(&format!("return {{ type = {ty} }}"));
            assert_eq!(
                (s.outcome.as_str(), s.at, s.kind),
                (label, None, Some(kind))
            );
        }
        let s = run("if skin_config then return { type = 7 } end\nreturn { type = 0 }");
        assert_eq!(s.outcome, "skin in another slot");
        assert_eq!(s.kind, Some(ScreenKind::Play));
        let (module, line, _) = s.at.unwrap();
        assert_eq!((module, line), (0, 0));
    }
}
