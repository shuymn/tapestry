//! The developer's conversion report (docs/stage3.md §3, the conversion
//! report): what a Lua skin's import met, as counts and outcomes under
//! allowlisted labels. Nothing the skin wrote reaches it: no source,
//! name, path, string, generated definition or Lua error, only the
//! importer's own labels, the host API names of [`HOST_APIS`], numbers,
//! and module indices in place of file names. The one exception is a
//! survey asked for developer-local detail ([`Local`]), whose output
//! stays on the developer's PC.

use std::collections::BTreeMap;

use std::path::Path;

use skin_model::ScreenKind;

use super::callback::{Analysis, Found, Site, Unused, callable_nodes};
use super::convert::{callables, field};
use super::facts::{FactRead, ImportFacts};
use super::ledger::Stop;
use super::{
    Cancel, Disposition, HOST_APIS, LAST_FACTS, LuaUsage, LuaWhy, Unsupported, header_pass,
};
use crate::error::{ImportError, Loc};
use crate::import::ledger;
pub use crate::import::ledger::{Became, DestinationEntry, DestinationNote};
use crate::import::{Config, DeviationId, NoteKind};
use crate::tree::{Arena, CallableKind};

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
    /// `refused`, `classified only` (returned but never consumed),
    /// `inert` (never read by the source), `captured` (no root, but reached
    /// through one) and `never analysed` (the import stopped before them).
    pub outcomes: BTreeMap<String, u32>,
    /// One record per consumer the import reached, in the order it
    /// reached them, then one per callable classified but never
    /// consumed, with why, or inert.
    pub consumers: Vec<Consumer>,
    /// The configured pass's script roots: strings at property fields
    /// that their field's name loader does not resolve.
    pub root_script: u32,
    /// The configured pass's unread-entry roots: places where the source
    /// lists every entry of a table whose entries the conversion left
    /// unread hold a function, a string or a table.
    pub root_unread: u32,
    /// Whether the configured definition's roots stay inside the modeled
    /// subset once the effect walk ran: `closed`, or `open` with why;
    /// empty when the import stopped before the walk.
    pub effects: String,
    /// Where the analysis was opened, located as [`Self::at`].
    pub effects_at: Option<(usize, u32, u32)>,
    /// The import's notes, by kind.
    pub notes: BTreeMap<String, u32>,
    /// The configured pass's reads of `main_state.option`, `number` and
    /// `text`, by what they met: `<api> <id> read`, `pending`,
    /// `unavailable`, `constant` or `live state`, `option unknown` and
    /// `<api> argument`.
    pub facts: BTreeMap<String, u32>,
    /// The facts the configured pass recorded, in first-read order.
    pub reads: Vec<FactRead>,
    pub usage: LuaUsage,
    /// The screen kind the header pass gave, when it gave a header.
    pub kind: Option<ScreenKind>,
    /// The configuration as resolved, only when [`survey_with`] is asked
    /// for developer-local detail.
    pub resolved: Option<Resolved>,
    /// What the import made of each destination ([`ledger`]), only when
    /// asked for developer-local detail.
    pub destinations: Vec<DestinationEntry>,
    /// A result import drawn over [`Choice::result`], when one was given
    /// and the import completed.
    /// An imported skin's verified per-frame cost: the operations its
    /// expressions and presentation program run, of
    /// [`skin_model::MAX_SKIN_OPS`], and its program's callbacks.
    pub frame_ops: Option<(usize, usize)>,
    pub frames: Option<Frames>,
}

/// A configuration by the names the skin gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Each option's name and its selected item's name and number.
    pub options: Vec<(Option<String>, Option<String>, Option<i32>)>,
    /// Each file choice of the header, its name and the candidate the
    /// import takes, `None` for none.
    pub files: Vec<(Option<String>, Option<String>)>,
}

/// The options and file choices a survey names, by the skin's names; the
/// rest take the header's defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub options: Vec<(String, String)>,
    pub files: Vec<(String, String)>,
    /// A result view a result import is drawn over once imported
    /// ([`Survey::frames`]).
    pub result: Option<skin_model::ResultView>,
}

/// A result import drawn over [`Choice::result`]: its widget charge, its
/// quad capacity, and over frames at 0, 0.25, 1 and 5 s of the screen
/// timer the most quads one drew, the quads every one dropped, and the
/// last one's chip quads (drawn on a chip grid).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frames {
    pub widget_quads: u32,
    pub capacity: u32,
    pub most: usize,
    pub dropped: u32,
    pub chips: usize,
}

/// Generous atlas limits for drawing a surveyed skin.
const SURVEY_LIMITS: skin_model::AtlasLimits = skin_model::AtlasLimits {
    max_side: 8_192,
    max_layers: 64,
    reserved_layers: 1,
};

/// `skin` drawn over `view` ([`Frames`]); `None` when its atlas cannot
/// be built or a build is refused.
fn frames(
    skin: &skin_model::Skin,
    widget_quads: u32,
    view: &skin_model::ResultView,
) -> Option<Frames> {
    use skin_model::{Filter, Layout, QuadBatch, ResultFrame, ResultLabels, ResultTimers, Variant};
    let atlas = skin.atlas(Variant::Lo, SURVEY_LIMITS, &|| false).ok()?;
    let mut res = skin.prepare_result();
    skin.update_result(&mut res, view, &ResultLabels::default())
        .ok()?;
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut f = Frames {
        widget_quads,
        capacity: skin.quad_capacity(),
        ..Frames::default()
    };
    let layout = Layout::AUTHOR;
    for ms in [0, 250, 1_000, 5_000] {
        batch.clear();
        let frame = ResultFrame {
            now: bms_model::TimeUs(ms * 1_000),
            target: [1_920, 1_080],
            layout: &layout,
            timers: ResultTimers {
                screen: Some(bms_model::TimeUs(0)),
                fadeout: None,
            },
        };
        skin.build_result(&mut res, &frame, atlas.uvs(), &mut batch)
            .ok()?;
        f.most = f.most.max(batch.quads().len());
        f.dropped = f.dropped.saturating_add(batch.dropped());
    }
    f.chips = batch
        .quads()
        .iter()
        .filter(|q| q.filter == Filter::NearestBase)
        .count();
    Some(f)
}

/// A callback where the import consumed it, or a callable it classified
/// and never consumed, under allowlisted labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consumer {
    /// Its definition: a module index, a line and a column.
    pub at: Option<(usize, u32, u32)>,
    /// The field that holds it ([`super::convert::FIELDS`], or `other`).
    pub field: &'static str,
    /// `decorative`, `critical <part>`, or `-` when never consumed.
    pub role: String,
    /// What became of it, labelled as [`Survey::outcomes`].
    pub disposition: String,
    /// Why it did not lower, or why it was never consumed: `element
    /// dropped earlier`, `condition folded false`, `field not consumed`,
    /// `import stopped earlier` when the import ended before reading it,
    /// `never read by the source` when it is inert, or `reached through a
    /// root` when a root captures it.
    pub reason: Option<String>,
    /// What the skin wrote about it, only when [`survey`] is asked for
    /// developer-local detail.
    pub local: Option<Local>,
}

/// A record's developer-local detail (docs/stage3.md §3, the conversion
/// report): names and source the skin wrote, which never leave the
/// developer's PC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Local {
    /// Its file, relative to the skin folder.
    pub file: String,
    /// The destination it was consumed for: its index in the destination
    /// list, its `id` and what that names; `None` when never consumed.
    pub element: Option<Element>,
    /// What it lowered to, when it lowered.
    pub lowered: Option<Lowered>,
}

/// The destination a callback was consumed for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// Its index in the destination list.
    pub index: usize,
    /// Its `id`, when it has one.
    pub id: Option<String>,
    /// The kind of object its `id` names, when it was looked up.
    pub kind: Option<String>,
}

/// A lowered callback's generated expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lowered {
    pub src: String,
    /// The availability guards of its reads, in order.
    pub guards: Vec<&'static str>,
    /// Whether it folded to a known value, which `src` writes.
    pub constant: bool,
}

/// What an import's analysis recorded for the report.
#[derive(Debug, Default)]
pub(crate) struct Records {
    /// The callbacks ([`Consumer`]), each located by file name.
    pub(crate) consumers: Vec<(Loc, Consumer)>,
    /// `closed`, or `open` with why and where, once the effect walk ran.
    pub(crate) effects: Option<(String, Option<Loc>)>,
}

/// The effects label of `an` once its walk ran: `closed`, or `open` with
/// why, located at the open site.
fn effects(arena: &Arena, an: &Analysis) -> Option<(String, Option<Loc>)> {
    an.admitted()?;
    Some(match an.open.get() {
        None => ("closed".to_owned(), None),
        Some(o) => {
            let at = match o.site {
                Site::Node(n) => arena.loc(n),
                Site::Code { file, line } => an.loc(&Found {
                    why: o.why,
                    file,
                    line,
                }),
            };
            (format!("open {}", unsupported(&o.why)), Some(at))
        }
    })
}

/// The records of `arena`'s callbacks after an import ([`Records`]);
/// `completed` when the import finished, so that a callback it never read
/// was in a field no consumer reads. The consumed records are taken out
/// of `an`.
pub(crate) fn records(arena: &Arena, an: &Analysis, completed: bool) -> Records {
    Records {
        consumers: consumers(arena, an, completed),
        effects: effects(arena, an),
    }
}

fn consumers(arena: &Arena, an: &Analysis, completed: bool) -> Vec<(Loc, Consumer)> {
    let mut out = Vec::new();
    for c in an.consumed.take() {
        out.push((
            arena.loc(c.node),
            Consumer {
                at: None,
                field: c.field,
                role: c
                    .part
                    .map_or_else(|| "decorative".to_owned(), |p| format!("critical {p:?}")),
                disposition: disposition(&c.disposition),
                reason: c.why.as_ref().map(unsupported),
                local: Some(Local {
                    file: String::new(),
                    element: c.element,
                    lowered: c.lowered,
                }),
            },
        ));
    }
    let outcomes = an.outcomes.borrow();
    let unused = an.unused.borrow();
    let mut seen = std::collections::HashSet::new();
    for (id, c) in callable_nodes(arena) {
        let Some(d) = outcomes.get(c as usize).copied() else {
            continue;
        };
        let reason = match d {
            Disposition::Inert => "never read by the source",
            Disposition::Captured => "reached through a root",
            Disposition::ClassifiedOnly => match unused.get(&c) {
                Some(Unused::Dropped) => "element dropped earlier",
                Some(Unused::FoldedFalse) => "condition folded false",
                None if completed => "field not consumed",
                None => "import stopped earlier",
            },
            _ => continue,
        };
        if !seen.insert(c) {
            continue;
        }
        out.push((
            arena.loc(id),
            Consumer {
                at: None,
                field: field(arena, id),
                role: "-".to_owned(),
                disposition: disposition(&d),
                reason: Some(reason.to_owned()),
                local: Some(Local::default()),
            },
        ));
    }
    out
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
        ImportError::Essential {
            part,
            why: Some(why),
            ..
        } => format!("essential {part:?}: {}", note(why)),
        ImportError::Essential {
            part, why: None, ..
        } => format!("essential {part:?} missing"),
        ImportError::Expr { .. } => "expression".into(),
        ImportError::Asset { .. } => "critical asset".into(),
        ImportError::Model { .. } => "model".into(),
        ImportError::Lua { why, .. } => match why {
            LuaWhy::LiveState { api: a } => format!("lua live state {}", api(*a)),
            LuaWhy::Unavailable { api: a, id } => format!("lua unavailable {} {id}", api(*a)),
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
        Unsupported::Consumer(c) => format!("Consumer {c}"),
        Unsupported::Unresolved(w) => format!("Unresolved {w}"),
        other => variant(other),
    }
}

/// The label of what became of a callable.
fn disposition(d: &Disposition) -> String {
    match d {
        Disposition::NeverAnalysed => "never analysed".into(),
        Disposition::Inert => "inert".into(),
        Disposition::Captured => "captured".into(),
        Disposition::ClassifiedOnly => "classified only".into(),
        Disposition::Lowered => "lowered".into(),
        Disposition::Compiled => "compiled".into(),
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
        NoteKind::Deviation(DeviationId::D20) => "Deviation D20 entrance lost".to_owned(),
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
        | ImportError::Essential { at, .. }
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

/// `config` by the names `header` gives it.
fn resolved(header: &crate::Header, config: &Config) -> Resolved {
    let options = header
        .options
        .iter()
        .zip(&config.options)
        .map(|(o, op)| {
            let item = o.items.iter().find(|i| Some(i.op) == *op);
            (o.name.clone(), item.and_then(|i| i.name.clone()), *op)
        })
        .collect();
    let files = header
        .files
        .iter()
        .map(|f| {
            let (pick, ..) = crate::import::asset::choose(
                &f.candidates,
                f.name.as_deref(),
                f.def.as_deref(),
                config,
            );
            (
                f.name.clone(),
                pick.and_then(|i| f.candidates.get(i)).cloned(),
            )
        })
        .collect();
    Resolved { options, files }
}

/// Runs the Lua skin `entry` of skin folder `root` with its default
/// configuration against the import facts `facts` on the scene of the
/// kind its header pass gives (`facts.scene` is not read), counts the
/// functions its configured definition holds and imports it, keeping what
/// the candidate used and consulted however far it got, and reports only
/// allowlisted labels and numbers, unless `local` asks for each record's
/// developer-local detail ([`Consumer::local`]).
#[must_use]
pub fn survey(
    root: &Path,
    entry: &str,
    facts: &ImportFacts,
    cancel: &Cancel,
    local: bool,
) -> Survey {
    survey_with(root, entry, facts, cancel, local, &Choice::default())
}

/// [`survey`] with the configuration `choice` names. Under `local`, also
/// the configuration as resolved and the destination ledger.
#[must_use]
pub fn survey_with(
    root: &Path,
    entry: &str,
    facts: &ImportFacts,
    cancel: &Cancel,
    local: bool,
    choice: &Choice,
) -> Survey {
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
    let config = match Config::named(
        h.header(),
        choice.options.iter().map(|(o, i)| (o.as_str(), i.as_str())),
        choice.files.iter().map(|(f, c)| (f.as_str(), c.as_str())),
    ) {
        Ok(config) => config,
        Err(e) => {
            return Survey {
                // The names are those the command line gave.
                outcome: if local {
                    format!("config: {e}")
                } else {
                    "config".to_owned()
                },
                kind: Some(h.header().kind),
                ..Survey::default()
            };
        }
    };
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
        if let Some(an) = def.arena.lua.as_deref().and_then(|l| l.analysis.as_deref()) {
            s.root_script = an.scripts;
            s.root_unread = an.unread;
        }
    }
    if local {
        s.resolved = Some(resolved(h.header(), &config));
    }
    let o = if local {
        let (o, destinations) = ledger::recording(|| h.finish(def, counts, store, reads, &config));
        s.destinations = destinations;
        o
    } else {
        h.finish(def, counts, store, reads, &config)
    };
    s.reads.clone_from(&o.reads);
    if let Some((label, at)) = &o.records.effects {
        s.effects.clone_from(label);
        s.effects_at = locate(at.as_ref(), &o.dependencies);
    }
    s.consumers = o
        .records
        .consumers
        .into_iter()
        .map(|(at, c)| Consumer {
            at: locate(Some(&at), &o.dependencies),
            local: c.local.filter(|_| local).map(|l| Local {
                file: at.file.clone(),
                ..l
            }),
            ..c
        })
        .collect();
    for d in &o.callbacks {
        *s.outcomes.entry(disposition(d)).or_default() += 1;
    }
    s.usage = o.counts;
    s.modules = o.dependencies.len();
    match o.result {
        Ok(imported) => {
            s.outcome = "imported".into();
            s.frame_ops = Some((
                imported.skin.ops().total(),
                imported.skin.program_callbacks(),
            ));
            for n in &imported.report.notes {
                *s.notes.entry(note(&n.kind)).or_default() += 1;
            }
            if kind == ScreenKind::Result
                && let Some(view) = &choice.result
            {
                let charge = imported.report.usage.widget_quads;
                s.frames = frames(&imported.skin, charge, view);
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
    use super::Became;
    use super::{
        Cancel, Choice, Element, ImportFacts, Local, Lowered, Resolved, survey, survey_with,
    };
    use crate::import::{NoteKind, PropertyKind, Severity};
    use skin_model::ScreenKind;

    fn folder(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(d.path().join(name), text).unwrap();
        }
        d
    }

    #[test]
    fn each_consumer_has_a_record_and_an_unused_callback_its_reason() {
        let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
               property = { { name = \"fc\", item = { { name = \"off\", op = 905 }, { name = \"on\", op = 906 } } } } }\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             skin.source = { { id = \"a\", path = \"a.png\" } }\n\
             skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
             skin.gauge = { id = \"g\", nodes = { \"i\", \"i\", \"i\", \"i\" }, parts = 10 }\n\
             skin.extra = { function() return true end } skin.customEvents = { { id = 1000, action = function() return true end } }\n\
             skin.destination = {\n\
               { id = \"i\", op = { 906 }, draw = function() return true end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"i\", mouseRect = { x = 0, y = 0, w = 1, h = 1 }, draw = function() return true end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"g\", draw = function() return ms.gauge_type() == 4 end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"i\", draw = function() return ms.number(426) > 0 end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"ex\", dst = { { w = 6, h = 6 } } }, { id = \"odd\", dst = { { w = 6, h = 6 } } },\n\
             }\n\
             skin.value = {\n\
               { id = \"ex\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, value = function() return ms.number(101) end },\n\
               { id = \"odd\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, value = function() return ms.number(426) end },\n\
             }\n\
             return skin\n";
        let d = crate::lua::callback_tests::folder(entry);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            false,
        );
        assert_eq!(s.outcome, "imported");
        let got: Vec<_> = s
            .consumers
            .iter()
            .map(|c| {
                (
                    c.at.map(|(_, line, _)| line),
                    c.field,
                    c.role.as_str(),
                    c.disposition.as_str(),
                    c.reason.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (Some(12), "draw", "critical Gauge", "lowered", None),
                (
                    Some(13),
                    "draw",
                    "decorative",
                    "replaced Unmapped main_state.number 426",
                    Some("Unmapped main_state.number 426")
                ),
                (Some(17), "value", "critical ExScore", "lowered", None),
                (
                    Some(18),
                    "value",
                    "decorative",
                    "replaced Unmapped main_state.number 426",
                    Some("Unmapped main_state.number 426")
                ),
                (
                    Some(8),
                    "other",
                    "-",
                    "classified only",
                    Some("field not consumed")
                ),
                (
                    Some(10),
                    "draw",
                    "-",
                    "classified only",
                    Some("condition folded false")
                ),
                (
                    Some(11),
                    "draw",
                    "-",
                    "classified only",
                    Some("element dropped earlier")
                ),
                (
                    Some(8),
                    "other",
                    "-",
                    "inert",
                    Some("never read by the source")
                ),
            ]
        );
    }

    #[test]
    fn local_detail_names_the_file_element_and_lowered_expression_only_when_asked() {
        let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             local m = require(\"parts.m\")\n\
             skin.source = { { id = \"a\", path = \"a.png\" } }\n\
             skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
             skin.gauge = { id = \"g\", nodes = { \"i\", \"i\", \"i\", \"i\" }, parts = 10 }\n\
             skin.destination = {\n\
               { id = \"i\", draw = m.draw, dst = { { w = 64, h = 64 } } },\n\
               { id = \"i\", draw = function() return 1 > 0 end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"g\", draw = function() return ms.gauge_type() == 4 end, dst = { { w = 64, h = 64 } } },\n\
               { id = \"odd\", dst = { { w = 6, h = 6 } } },\n\
             }\n\
             skin.value = {\n\
               { id = \"odd\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4, value = function() return ms.number(426) end },\n\
             }\n\
             return skin\n";
        let d = crate::lua::callback_tests::folder(entry);
        std::fs::create_dir(d.path().join("parts")).unwrap();
        std::fs::write(
            d.path().join("parts").join("m.lua"),
            "local ms = require(\"main_state\")\n\
             return { draw = function() return ms.number(426) > 0 end }\n",
        )
        .unwrap();
        let run = |local| {
            survey(
                d.path(),
                "s.luaskin",
                &ImportFacts::unavailable(ScreenKind::Play),
                &Cancel::new(),
                local,
            )
        };
        let s = run(true);
        assert_eq!(s.outcome, "imported");
        let element = |index, id: &str, kind: &str| {
            Some(Element {
                index,
                id: Some(id.to_owned()),
                kind: Some(kind.to_owned()),
            })
        };
        let lowered = |src: &str, guards: &[&'static str], constant| {
            Some(Lowered {
                src: src.to_owned(),
                guards: guards.to_vec(),
                constant,
            })
        };
        let got: Vec<_> = s
            .consumers
            .iter()
            .map(|c| (c.disposition.as_str(), c.local.clone()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "replaced Unmapped main_state.number 426",
                    Some(Local {
                        file: "parts/m.lua".to_owned(),
                        element: element(0, "i", "Image"),
                        lowered: None,
                    })
                ),
                (
                    "lowered",
                    Some(Local {
                        file: "s.luaskin".to_owned(),
                        element: element(1, "i", "Image"),
                        lowered: lowered("true", &[], true),
                    })
                ),
                (
                    "lowered",
                    Some(Local {
                        file: "s.luaskin".to_owned(),
                        element: element(2, "g", "Gauge"),
                        lowered: lowered("(gauge.kind == ex_hard)", &["gauge.valid"], false),
                    })
                ),
                (
                    "replaced Unmapped main_state.number 426",
                    Some(Local {
                        file: "s.luaskin".to_owned(),
                        element: element(3, "odd", "Value"),
                        lowered: None,
                    })
                ),
            ]
        );
        // Without the request, no record carries any of it.
        let s = run(false);
        assert_eq!(s.consumers.len(), 4);
        assert!(s.consumers.iter().all(|c| c.local.is_none()));
    }

    /// A skin whose destinations each become something else: `on` is its
    /// option's other item.
    const LEDGER: &str = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
           property = { { name = \"fc\", item = { { name = \"off\", op = 905 }, { name = \"on\", op = 906 } } } } }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"a\", path = \"a.png\" } }\n\
         skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
         skin.value = { { id = \"lv\", src = \"a\", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2, ref = 426 } }\n\
         skin.judgegraph = { { id = \"jg\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
         skin.destination = {\n\
           { id = \"i\", dst = { { w = 64, h = 64 } } },\n\
           { id = \"i\", op = { 906 }, dst = { { w = 64, h = 64 } } },\n\
           { id = \"lv\", dst = { { w = 6, h = 6 } } },\n\
           { id = \"jg\", dst = { { w = 6, h = 6 } } },\n\
           { id = \"none\", dst = { { w = 6, h = 6 } } },\n\
           { id = \"i\", dst = { { w = 64, h = 64 } } },\n\
         }\n\
         return skin\n";

    fn ledger_survey(d: &tempfile::TempDir, local: bool, choice: &Choice) -> super::Survey {
        survey_with(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            local,
            choice,
        )
    }

    #[test]
    fn the_ledger_says_what_became_of_each_destination_with_its_notes() {
        let d = crate::lua::callback_tests::folder(LEDGER);
        let s = ledger_survey(&d, true, &Choice::default());
        assert_eq!(s.outcome, "imported");
        let got: Vec<_> = s
            .destinations
            .iter()
            .map(|e| {
                (
                    e.index,
                    e.id.as_deref(),
                    e.kind.as_deref(),
                    e.became,
                    e.elements.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (0, Some("i"), Some("Image"), Became::Retained, 0..1),
                (1, Some("i"), Some("Image"), Became::Folded, 1..1),
                (2, Some("lv"), Some("Value"), Became::Dropped, 1..1),
                (3, Some("jg"), Some("JudgeGraph"), Became::Dropped, 1..1),
                (4, Some("none"), None, Became::Ignored, 1..1),
                (5, Some("i"), Some("Image"), Became::Retained, 1..2),
            ]
        );
        // The property's own drop, and one through the policy's direct path.
        let note = |i: usize| {
            s.destinations[i]
                .notes
                .first()
                .map(|n| (n.severity, n.kind))
        };
        assert_eq!(
            note(2),
            Some((
                Severity::Dropped,
                NoteKind::UnsupportedProperty {
                    kind: PropertyKind::Integer,
                    id: Some(426)
                }
            ))
        );
        assert!(matches!(
            note(3),
            Some((Severity::Dropped, NoteKind::Unsupported { .. }))
        ));
        let (file, ..) = s.destinations[2].notes[0].at.clone().unwrap();
        assert_eq!(file, "s.luaskin");
        assert_eq!(
            s.resolved,
            Some(Resolved {
                options: vec![(Some("fc".to_owned()), Some("off".to_owned()), Some(905))],
                files: Vec::new(),
            })
        );
    }

    #[test]
    fn without_local_detail_there_is_no_ledger_and_no_names() {
        let d = crate::lua::callback_tests::folder(LEDGER);
        let s = ledger_survey(&d, false, &Choice::default());
        assert_eq!(s.outcome, "imported");
        assert!(s.destinations.is_empty());
        assert_eq!(s.resolved, None);
        // A later survey of the thread, asked, is not given an old ledger.
        let again = ledger_survey(&d, true, &Choice::default());
        assert_eq!(again.destinations.len(), 6);
    }

    #[test]
    fn a_named_option_changes_what_the_ledger_finds() {
        let d = crate::lua::callback_tests::folder(LEDGER);
        let choice = Choice {
            options: vec![("fc".to_owned(), "on".to_owned())],
            files: Vec::new(),
            result: None,
        };
        let s = ledger_survey(&d, true, &choice);
        assert_eq!(s.outcome, "imported");
        assert_eq!(s.destinations[1].became, Became::Retained);
        assert_eq!(
            s.resolved.unwrap().options[0],
            (Some("fc".to_owned()), Some("on".to_owned()), Some(906))
        );
        let bad = Choice {
            options: vec![("fc".to_owned(), "never".to_owned())],
            files: Vec::new(),
            result: None,
        };
        let s = ledger_survey(&d, true, &bad);
        assert_eq!(s.outcome, "config: option \"fc\" has no item \"never\"");
        assert!(s.destinations.is_empty());
        // Without the request the outcome carries no name.
        assert_eq!(ledger_survey(&d, false, &bad).outcome, "config");
    }

    #[test]
    fn a_refusal_is_in_the_ledger_with_what_it_never_reached() {
        let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
             if not skin_config then return skin end\n\
             skin.source = { { id = \"a\", path = \"a.png\" } }\n\
             skin.image = { { id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 } }\n\
             skin.gauge = { id = \"g\", nodes = { \"i\", \"i\", \"i\", \"i\" }, parts = 10 }\n\
             skin.destination = {\n\
               { id = \"i\", dst = { { w = 64, h = 64 } } },\n\
               { id = \"g\", dst = { { time = 0, w = 64, h = 64 }, { time = 100, w = 32, h = 64 } } },\n\
               { id = \"i\", dst = { { w = 64, h = 64 } } },\n\
             }\n\
             return skin\n";
        let d = crate::lua::callback_tests::folder(entry);
        let s = ledger_survey(&d, true, &Choice::default());
        assert!(s.outcome.starts_with("critical Gauge"), "{}", s.outcome);
        let got: Vec<_> = s
            .destinations
            .iter()
            .map(|e| (e.index, e.id.as_deref(), e.became))
            .collect();
        assert_eq!(
            got,
            [
                (0, Some("i"), Became::Retained),
                (1, Some("g"), Became::Refused),
                (2, None, Became::NotReached),
            ]
        );
    }

    #[test]
    fn each_destination_has_only_its_own_notes() {
        let d = crate::lua::callback_tests::folder(LEDGER);
        let s = ledger_survey(&d, true, &Choice::default());
        let kinds: Vec<Vec<_>> = s
            .destinations
            .iter()
            .map(|e| e.notes.iter().map(|n| n.kind).collect())
            .collect();
        assert_eq!(kinds[0], []);
        assert_eq!(kinds[1], [NoteKind::Ignored { field: "op" }]);
        assert_eq!(
            kinds[2],
            [NoteKind::UnsupportedProperty {
                kind: PropertyKind::Integer,
                id: Some(426)
            }]
        );
        assert!(matches!(kinds[3][..], [NoteKind::Unsupported { .. }]));
        assert_eq!(kinds[4], [NoteKind::UndefinedObject]);
        assert_eq!(kinds[5], []);
    }

    #[test]
    fn a_note_past_the_cap_still_says_what_became_of_its_destination() {
        let entry = "local s = { type = 0, w = 1920, h = 1080 }\n\
             if not skin_config then return s end\n\
             s.judgegraph = { { id = \"jg\" } }\n\
             s.destination = {}\n\
             for i = 1, 4097 do s.destination[i] = { id = \"jg\", dst = { { w = 1, h = 1 } } } end\n\
             return s\n";
        let d = crate::lua::callback_tests::folder(entry);
        let s = ledger_survey(&d, true, &Choice::default());
        assert_eq!(s.outcome, "imported");
        let last = s.destinations.last().unwrap();
        assert_eq!((last.index, last.suppressed), (4096, 1));
        assert!(last.notes.is_empty());
        assert_eq!(last.became, Became::Dropped);
    }

    #[test]
    fn a_refusal_before_the_first_destination_leaves_them_all_not_reached() {
        let entry = "return { type = 0, w = 1920, h = 1080,\n\
               customTimers = { { id = true } },\n\
               destination = { { id = \"a\" }, { id = \"b\" } } }\n";
        let d = crate::lua::callback_tests::folder(entry);
        let s = ledger_survey(&d, true, &Choice::default());
        assert_ne!(s.outcome, "imported");
        let got: Vec<_> = s.destinations.iter().map(|e| (e.index, e.became)).collect();
        assert_eq!(got, [(0, Became::NotReached), (1, Became::NotReached)]);
    }

    #[test]
    fn every_file_choice_is_resolved_as_the_import_decides_it() {
        let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
               filepath = {\n\
                 { name = \"by default\", path = \"*.png\", def = \"b\" },\n\
                 { name = \"first\", path = \"*.png\" },\n\
                 { name = \"named\", path = \"*.png\" },\n\
                 { name = \"nothing\", path = \"none/*.png\" },\n\
                 { path = \"*.png\" },\n\
               } }\n\
             if not skin_config then return skin end\n\
             skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
             return skin\n";
        let d = crate::lua::callback_tests::folder(entry);
        std::fs::copy(d.path().join("a.png"), d.path().join("b.png")).unwrap();
        let choice = Choice {
            options: Vec::new(),
            files: vec![("named".to_owned(), "b.png".to_owned())],
            result: None,
        };
        let s = ledger_survey(&d, true, &choice);
        assert_eq!(s.outcome, "imported");
        let file = |name: Option<&str>, candidate: Option<&str>| {
            (name.map(str::to_owned), candidate.map(str::to_owned))
        };
        assert_eq!(
            s.resolved.unwrap().files,
            [
                file(Some("by default"), Some("b.png")),
                file(Some("first"), Some("a.png")),
                file(Some("named"), Some("b.png")),
                file(Some("nothing"), None),
                file(None, Some("a.png")),
            ]
        );
    }

    #[test]
    fn a_system_image_s_callback_names_its_kind() {
        let entry = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
             if not skin_config then return skin end\n\
             skin.destination = {\n\
               { id = -110, draw = function() return true end, dst = { { w = 64, h = 64 } } },\n\
             }\n\
             return skin\n";
        let d = crate::lua::callback_tests::folder(entry);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            true,
        );
        assert_eq!(s.outcome, "imported");
        let elements: Vec<_> = s
            .consumers
            .iter()
            .map(|c| c.local.as_ref().and_then(|l| l.element.clone()))
            .collect();
        assert_eq!(
            elements,
            [Some(Element {
                index: 0,
                id: Some("-110".to_owned()),
                kind: Some("SystemImage".to_owned()),
            })]
        );
    }

    #[test]
    fn a_callback_an_import_never_reached_is_not_called_unconsumed() {
        // The import refuses a mixed destination list after admission: it
        // never reads the classified action.
        let d = folder(&[(
            "s.luaskin",
            "if not skin_config then return { type = 0 } end\n\
             return { type = 0, customEvents = { { action = function() return true end } },\n\
               destination = { k = 1, {} } }",
        )]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            false,
        );
        assert_eq!(s.outcome, "lua Mixed");
        assert_eq!(s.effects, "open Construct an unread entry");
        let reasons: Vec<_> = s.consumers.iter().map(|c| c.reason.as_deref()).collect();
        assert_eq!(reasons, [Some("import stopped earlier")]);
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
            false,
        );
        assert_eq!(s.outcome, "lua Error");
        assert_eq!(s.modules, 2);
        assert_eq!(s.at, Some((1, 2, 0)));
        assert_eq!(s.usage.requires, 1);
    }

    #[test]
    fn a_converted_value_is_located_by_its_ordinal() {
        let d = folder(&[(
            "s.luaskin",
            "return { type = 0, extra = { [1] = 1, [3] = 2 } }",
        )]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            false,
        );
        assert_eq!(s.outcome, "header lua Sparse");
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
            false,
        );
        assert_eq!(crate::lua::tests::PASSES.with(std::cell::Cell::get), 2);
        assert!(s.usage.instructions >= 4_000_000 && s.usage.instructions < 5_000_000);
    }

    #[test]
    fn the_library_s_functions_are_counted_as_library() {
        let d = folder(&[(
            "s.luaskin",
            "if skin_config then\n\
               return { type = 0, customEvents = { { action = string.find }, { action = string.len },\n\
                 { action = type }, { action = function() end } } }\n\
             end\nreturn { type = 0 }",
        )]);
        let s = survey(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            false,
        );
        let kinds: Vec<(&str, u32)> = s
            .callables
            .iter()
            .map(|((_, kind), n)| (kind.as_str(), *n))
            .collect();
        assert_eq!(kinds, [("library", 3), ("lua", 1)]);
        // A library function has no tree: the first opens the analysis at
        // its value; nothing consumes any of them.
        assert_eq!(s.outcome, "imported");
        assert_eq!(s.effects, "open Capture not a skin closure");
        assert_eq!(s.effects_at.map(|(m, line, _)| (m, line)), Some((0, 0)));
        assert_eq!(s.outcomes.get("classified only"), Some(&4));
    }

    #[test]
    fn an_entrance_lost_has_its_label_and_a_looping_track_stops() {
        let entry = |gauge_loop: i32| {
            format!(
                "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
                 if not skin_config then return skin end\n\
                 skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
                 skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 1, h = 1 }} }}\n\
                 skin.gauge = {{ id = \"g\", nodes = {{ \"i\", \"i\", \"i\", \"i\" }}, parts = 10 }}\n\
                 skin.destination = {{\n\
                   {{ id = \"g\", loop = {gauge_loop}, dst = {{ {{ w = 64, h = 0 }},\n\
                     {{ time = 250, h = 24 }} }} }},\n\
                   {{ id = \"g\", loop = 1700, dst = {{ {{ w = 64, h = 64, y = 2160 }},\n\
                     {{ time = 1700, y = 1080 }} }} }},\n\
                 }}\n\
                 return skin\n"
            )
        };
        let run = |text: &str| {
            let d = folder(&[("s.luaskin", text)]);
            let mut png = Vec::new();
            image::RgbaImage::new(1, 1)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            std::fs::write(d.path().join("a.png"), png).unwrap();
            survey(
                d.path(),
                "s.luaskin",
                &ImportFacts::unavailable(ScreenKind::Play),
                &Cancel::new(),
                false,
            )
        };
        let settled = run(&entry(250));
        assert_eq!(settled.outcome, "imported");
        assert_eq!(settled.notes.get("Deviation D20 entrance lost"), Some(&2));
        let looping = run(&entry(100));
        assert_eq!(looping.outcome, "critical Gauge: Timer");
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
            false,
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
                false,
            )
        };
        let s = run("return { type = 1 }");
        assert_eq!(s.outcome, "header unsupported screen kind");
        assert_eq!(s.kind, None);
        let (module, line, _) = s.at.unwrap();
        assert_eq!((module, line), (0, 0));
        for (ty, kind, label) in [
            (5, ScreenKind::Selection, "essential SongList missing"),
            (6, ScreenKind::Decision, "no importer for Decision"),
            (7, ScreenKind::Result, "essential ResultScore missing"),
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
