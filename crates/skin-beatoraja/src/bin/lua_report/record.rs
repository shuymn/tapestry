//! The walk's records (docs/stage3.md §3, the conversion report): one
//! TOML file per walk, one record per skin and configuration holding the
//! default report's allowlisted values, without the values that differ
//! between two runs of one build (the import's instruction count and peak
//! memory, the runtime frame's wall time). Every string a record holds is
//! a [`Label`]: one the default report prints, taken from the same
//! function, or one of the walk's own words.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skin_beatoraja::lua::survey::Survey;

use super::label::{self, Label};

/// The records' format version; another is refused, never converted.
pub const VERSION: u32 = 2;

/// One walk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub version: u32,
    /// The import facts, as the flags that give them.
    pub facts: String,
    /// What a result skin adds to them.
    pub result_facts: String,
    /// The walking build's identity: the SHA-256 of its executable (never
    /// printed). Two runs of one build that disagree on an outcome are
    /// not judged.
    pub build: String,
    pub skins: usize,
    pub records: usize,
    #[serde(default)]
    pub skin: Vec<SkinEntry>,
    #[serde(default)]
    pub record: Vec<Record>,
}

/// A skin walked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinEntry {
    pub index: usize,
    /// Its catalogue digest (never printed).
    pub digest: String,
    pub kind: Label,
    pub configurations: usize,
}

/// A located value: a module index, a line and a column.
pub type At = [u64; 3];

/// One configuration of one skin.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub skin: usize,
    pub configuration: usize,
    /// `default`, `option` or `file`.
    pub toggle: Label,
    /// The option and item, or file choice and candidate, by their
    /// indices in the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[usize; 2]>,
    pub kind: Label,
    pub outcome: Label,
    /// The attempts' outcomes of an `inconclusive` or `unstable` record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<At>,
    pub modules: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub callbacks: BTreeMap<Label, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub callables: BTreeMap<Label, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<Label, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<Label, u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<Label>,
    pub root_script: u32,
    pub root_unread: u32,
    #[serde(default, skip_serializing_if = "Label::is_empty")]
    pub effects: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effects_at: Option<At>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_ops: Option<[usize; 2]>,
    /// The play ending's finish margin and fade-out, in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_ending: Option<[i64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_elements: Option<usize>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub play_timers: BTreeMap<Label, usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gauges: Vec<Label>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub atlas: Vec<Label>,
    /// Widget charge, capacity, most quads, dropped, chips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<[u64; 5]>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub usage: BTreeMap<Label, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
}

/// The survey's runtime frame, without its wall time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub outputs: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status: BTreeMap<Label, u32>,
    /// Its notes, by what they are.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<Label, u32>,
    pub dropped: u64,
    pub instructions: u64,
    pub host_calls: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stops: Vec<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<Label>,
}

fn at(a: Option<(usize, u32, u32)>) -> Option<At> {
    a.map(|(m, l, c)| [m as u64, u64::from(l), u64::from(c)])
}

impl Record {
    /// The record of `s`, configuration `configuration` of skin `skin`.
    pub fn of(
        skin: usize,
        configuration: usize,
        (toggle, position): (&'static str, Option<[usize; 2]>),
        s: &Survey,
        attempts: Vec<Label>,
    ) -> Self {
        let u = s.usage;
        // Left out: the import's instruction count and peak memory, which
        // differ between two runs of one build.
        let usage: BTreeMap<Label, u64> = [
            ("host_calls", u64::from(u.host_calls)),
            ("native_work", u.native_work),
            ("loads", u64::from(u.loads)),
            ("requires", u64::from(u.requires)),
            ("dofiles", u64::from(u.dofiles)),
            ("missing_loads", u64::from(u.missing_loads)),
            ("input_bytes", u.input_bytes),
            ("protected_calls", u64::from(u.protected_calls)),
            ("metatables", u64::from(u.metatables)),
            ("ast_nodes", u.ast_nodes),
            ("captures", u.captures),
            ("expanded", u.expanded),
        ]
        .into_iter()
        .map(|(k, v)| (Label::fixed(k), v))
        .collect();
        Self {
            skin,
            configuration,
            toggle: Label::fixed(toggle),
            position,
            kind: label::kind(s.kind),
            outcome: label::outcome(s),
            attempts,
            at: at(s.at),
            modules: s.modules,
            callbacks: label::callbacks(s),
            callables: label::callables(s),
            notes: label::notes(s),
            facts: label::facts(s),
            reads: s.reads.iter().map(label::read).collect(),
            root_script: s.root_script,
            root_unread: s.root_unread,
            effects: label::effects(s),
            effects_at: at(s.effects_at),
            frame_ops: s.frame_ops.map(|(a, b)| [a, b]),
            play_ending: s
                .play_ending
                .map(|e| [e.finish_margin.0 / 1_000, e.fadeout.0 / 1_000]),
            play_elements: s.play_elements.as_ref().map(|(n, _)| *n),
            play_timers: s
                .play_elements
                .as_ref()
                .map(|(_, by)| label::timers(by))
                .unwrap_or_default(),
            gauges: s
                .gauges
                .iter()
                .map(|(segments, motion)| label::gauge(*segments, motion))
                .collect(),
            atlas: s.atlas.iter().map(|a| label::atlas(a)).collect(),
            frames: s.frames.map(|f| {
                [
                    u64::from(f.widget_quads),
                    u64::from(f.capacity),
                    f.most as u64,
                    u64::from(f.dropped),
                    f.chips as u64,
                ]
            }),
            usage,
            runtime: s.runtime.as_ref().map(|rt| {
                let mut notes = BTreeMap::new();
                for n in &rt.notes {
                    *notes.entry(label::runtime_what(n)).or_default() += 1;
                }
                Runtime {
                    outputs: rt.outputs,
                    status: label::status(rt),
                    notes,
                    dropped: rt.dropped,
                    instructions: rt.instructions,
                    host_calls: rt.host_calls,
                    stops: label::stops(rt),
                    stopped: label::stopped(rt),
                }
            }),
        }
    }

    /// Its toggle as the summary and the comparison print it.
    pub fn toggle_label(&self) -> String {
        match self.position {
            Some([a, b]) => format!("{} {a} {b}", self.toggle),
            None => self.toggle.to_string(),
        }
    }

    /// The labels it took from its survey (not its toggle, its usage keys
    /// or its attempts), each one the default report prints.
    #[cfg(test)]
    pub fn survey_labels(&self) -> Vec<&Label> {
        let mut v = vec![&self.kind, &self.outcome];
        if !self.effects.is_empty() {
            v.push(&self.effects);
        }
        for m in [&self.callbacks, &self.callables, &self.notes, &self.facts] {
            v.extend(m.keys());
        }
        v.extend(&self.reads);
        v.extend(self.play_timers.keys());
        v.extend(&self.gauges);
        v.extend(&self.atlas);
        if let Some(rt) = &self.runtime {
            v.extend(rt.status.keys());
            v.extend(rt.notes.keys());
            v.extend(&rt.stops);
            v.extend(&rt.stopped);
        }
        v
    }
}

impl Records {
    pub fn new(facts: &str, result_facts: &str, build: &str) -> Self {
        Self {
            version: VERSION,
            facts: facts.to_owned(),
            result_facts: result_facts.to_owned(),
            build: build.to_owned(),
            skins: 0,
            records: 0,
            skin: Vec::new(),
            record: Vec::new(),
        }
    }

    /// Adds a skin and its records.
    pub fn push_skin(&mut self, skin: SkinEntry, records: Vec<Record>) {
        self.skins += 1;
        self.records += records.len();
        self.skin.push(skin);
        self.record.extend(records);
    }

    /// The text written: skins first, then records.
    ///
    /// # Errors
    ///
    /// The serializer's failure.
    pub fn to_text(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| format!("the records could not be written: {e}"))
    }

    /// Reads the records `name` (`kept` or `new`) holds in `text`,
    /// checked for structure and counts.
    ///
    /// # Errors
    ///
    /// What is wrong, naming `name`, a line or the skin and configuration,
    /// and never quoting the file.
    pub fn read(name: &str, text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Version {
            version: Option<i64>,
        }
        let version: Version = toml::from_str(text).map_err(|e| malformed(name, text, e.span()))?;
        match version.version {
            Some(v) if v == i64::from(VERSION) => {}
            Some(_) => {
                return Err(format!(
                    "{name}: records of another version (this build reads version {VERSION}); \
                     walk again"
                ));
            }
            None => return Err(format!("{name}: not a records file (no version)")),
        }
        let r: Self = toml::from_str(text).map_err(|e| malformed(name, text, e.span()))?;
        r.check(name)?;
        Ok(r)
    }

    /// The structure and counts a walk writes.
    fn check(&self, name: &str) -> Result<(), String> {
        if self.skins != self.skin.len() {
            return Err(format!(
                "{name}: declares {} skins and holds {}",
                self.skins,
                self.skin.len()
            ));
        }
        if self.records != self.record.len() {
            return Err(format!(
                "{name}: declares {} records and holds {}",
                self.records,
                self.record.len()
            ));
        }
        for (i, s) in self.skin.iter().enumerate() {
            if s.index != i {
                return Err(format!("{name}: skin {i} is numbered {}", s.index));
            }
        }
        let mut by_skin: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for r in &self.record {
            by_skin.entry(r.skin).or_default().push(r.configuration);
        }
        if let Some(extra) = by_skin.keys().find(|k| **k >= self.skin.len()) {
            return Err(format!(
                "{name}: records of skin {extra}, which it does not hold"
            ));
        }
        for s in &self.skin {
            let mut configs = by_skin.remove(&s.index).unwrap_or_default();
            if configs.len() != s.configurations {
                return Err(format!(
                    "{name}: skin {} declares {} configurations and holds {}",
                    s.index,
                    s.configurations,
                    configs.len()
                ));
            }
            configs.sort_unstable();
            if configs.iter().enumerate().any(|(i, c)| *c != i) {
                return Err(format!(
                    "{name}: skin {}'s configurations are not numbered 0 to {} once each",
                    s.index,
                    s.configurations.saturating_sub(1)
                ));
            }
        }
        Ok(())
    }
}

/// A parse failure of `name`, located by line and by the table it is in
/// when that is one of the records' own; the TOML library's message is
/// not given (it quotes the input).
fn malformed(name: &str, text: &str, span: Option<std::ops::Range<usize>>) -> String {
    let Some(span) = span else {
        return format!("{name}: not a records file of this version");
    };
    let before = text.get(..span.start).unwrap_or("");
    let line = before.matches('\n').count() + 1;
    let table = before
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('['))
        .and_then(|l| {
            [
                "[[skin]]",
                "[[record]]",
                "[record.callbacks]",
                "[record.callables]",
                "[record.notes]",
                "[record.facts]",
                "[record.play_timers]",
                "[record.usage]",
                "[record.runtime]",
                "[record.runtime.status]",
                "[record.runtime.notes]",
            ]
            .into_iter()
            .find(|t| *t == l)
        })
        .unwrap_or("the top level");
    format!(
        "{name}: line {line}: not a records file of this version (a malformed value, or a \
         missing, unknown or mistyped field in {table})"
    )
}
