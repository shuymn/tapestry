//! The walk's records (docs/stage3.md §3, the conversion report): one
//! TOML file per walk, one record per skin and configuration holding the
//! default report's allowlisted values, without the values that differ
//! between two runs of one build (the import's instruction count and peak
//! memory, the runtime frame's wall time).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use skin_beatoraja::lua::survey::{Survey, in_vocabulary};

/// The records' format version; another is refused, never converted.
pub const VERSION: u32 = 1;

/// One walk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub version: u32,
    /// The import facts, as the flags that give them.
    pub facts: String,
    /// What a result skin adds to them.
    pub result_facts: String,
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
    pub kind: String,
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
    pub toggle: String,
    /// The option and item, or file choice and candidate, by their
    /// indices in the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[usize; 2]>,
    pub kind: String,
    pub outcome: String,
    /// The attempts' outcomes of an `inconclusive` or `unstable` record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<At>,
    pub modules: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub callbacks: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub callables: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    pub root_script: u32,
    pub root_unread: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effects: String,
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
    pub play_timers: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gauges: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub atlas: Vec<String>,
    /// Widget charge, capacity, most quads, dropped, chips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<[u64; 5]>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub usage: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
}

/// The survey's runtime frame, without its wall time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub outputs: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status: BTreeMap<String, u32>,
    /// Its notes, by what they are.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<String, u32>,
    pub dropped: u64,
    pub instructions: u64,
    pub host_calls: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stops: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

fn at(a: Option<(usize, u32, u32)>) -> Option<At> {
    a.map(|(m, l, c)| [m as u64, u64::from(l), u64::from(c)])
}

impl Record {
    /// The record of `s`, configuration `configuration` of skin `skin`.
    pub fn of(
        skin: usize,
        configuration: usize,
        (toggle, position): (&str, Option<[usize; 2]>),
        s: &Survey,
        attempts: Vec<String>,
    ) -> Self {
        let u = s.usage;
        // Left out: the import's instruction count and peak memory, which
        // differ between two runs of one build.
        let usage: BTreeMap<String, u64> = BTreeMap::from([
            ("host_calls".to_owned(), u64::from(u.host_calls)),
            ("native_work".to_owned(), u.native_work),
            ("loads".to_owned(), u64::from(u.loads)),
            ("requires".to_owned(), u64::from(u.requires)),
            ("dofiles".to_owned(), u64::from(u.dofiles)),
            ("missing_loads".to_owned(), u64::from(u.missing_loads)),
            ("input_bytes".to_owned(), u.input_bytes),
            ("protected_calls".to_owned(), u64::from(u.protected_calls)),
            ("metatables".to_owned(), u64::from(u.metatables)),
            ("ast_nodes".to_owned(), u.ast_nodes),
            ("captures".to_owned(), u.captures),
            ("expanded".to_owned(), u.expanded),
        ]);
        Self {
            skin,
            configuration,
            toggle: toggle.to_owned(),
            position,
            kind: s
                .kind
                .map_or_else(|| "none".to_owned(), |k| format!("{k:?}")),
            outcome: s.outcome.clone(),
            attempts,
            at: at(s.at),
            modules: s.modules,
            callbacks: s.outcomes.clone(),
            callables: s
                .callables
                .iter()
                .map(|((f, k), n)| (format!("{f} {k}"), *n))
                .collect(),
            notes: s.notes.clone(),
            facts: s.facts.clone(),
            reads: s
                .reads
                .iter()
                .map(|r| format!("{:?} = {:?}", r.fact, r.value))
                .collect(),
            root_script: s.root_script,
            root_unread: s.root_unread,
            effects: s.effects.clone(),
            effects_at: at(s.effects_at),
            frame_ops: s.frame_ops.map(|(a, b)| [a, b]),
            play_ending: s
                .play_ending
                .map(|e| [e.finish_margin.0 / 1_000, e.fadeout.0 / 1_000]),
            play_elements: s.play_elements.as_ref().map(|(n, _)| *n),
            play_timers: s
                .play_elements
                .as_ref()
                .map(|(_, by)| by.clone())
                .unwrap_or_default(),
            gauges: s
                .gauges
                .iter()
                .map(|(segments, motion)| format!("segments {segments} motion {motion}"))
                .collect(),
            atlas: s.atlas.clone(),
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
                    *notes.entry(n.what.to_owned()).or_default() += 1;
                }
                Runtime {
                    outputs: rt.outputs,
                    status: rt.status.clone(),
                    notes,
                    dropped: rt.dropped,
                    instructions: rt.instructions,
                    host_calls: rt.host_calls,
                    stops: rt.stops.iter().map(|s| (*s).to_owned()).collect(),
                    stopped: rt.stopped.clone(),
                }
            }),
        }
    }

    /// Its toggle as the summary and the comparison print it.
    pub fn toggle_label(&self) -> String {
        match self.position {
            Some([a, b]) => format!("{} {a} {b}", self.toggle),
            None => self.toggle.clone(),
        }
    }

    /// Every string the record holds besides its field names, by the
    /// field that holds it.
    fn strings(&self) -> Vec<(&'static str, &str)> {
        let mut v = vec![
            ("toggle", self.toggle.as_str()),
            ("kind", self.kind.as_str()),
            ("outcome", self.outcome.as_str()),
            ("effects", self.effects.as_str()),
        ];
        v.extend(self.attempts.iter().map(|s| ("attempts", s.as_str())));
        for (field, m) in [
            ("callbacks", &self.callbacks),
            ("callables", &self.callables),
            ("notes", &self.notes),
            ("facts", &self.facts),
        ] {
            v.extend(m.keys().map(|k| (field, k.as_str())));
        }
        v.extend(self.reads.iter().map(|s| ("reads", s.as_str())));
        v.extend(self.play_timers.keys().map(|s| ("play_timers", s.as_str())));
        v.extend(self.gauges.iter().map(|s| ("gauges", s.as_str())));
        v.extend(self.atlas.iter().map(|s| ("atlas", s.as_str())));
        if let Some(rt) = &self.runtime {
            v.extend(rt.status.keys().map(|s| ("runtime status", s.as_str())));
            v.extend(rt.notes.keys().map(|s| ("runtime notes", s.as_str())));
            v.extend(rt.stops.iter().map(|s| ("runtime stops", s.as_str())));
            v.extend(rt.stopped.iter().map(|s| ("runtime stopped", s.as_str())));
        }
        v
    }

    /// The first string outside the report's vocabulary, by its field.
    pub fn outside_vocabulary(&self) -> Option<&'static str> {
        self.strings()
            .into_iter()
            .find(|(_, s)| !in_vocabulary(s))
            .map(|(field, _)| field)
    }
}

impl Records {
    pub fn new(facts: &str, result_facts: &str) -> Self {
        Self {
            version: VERSION,
            facts: facts.to_owned(),
            result_facts: result_facts.to_owned(),
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
    /// A record holding a string outside the report's vocabulary, named
    /// by its skin, configuration and field (the string is not given).
    pub fn to_text(&self) -> Result<String, String> {
        if let Some((r, field)) = self
            .record
            .iter()
            .find_map(|r| r.outside_vocabulary().map(|f| (r, f)))
        {
            return Err(format!(
                "skin {} configuration {}: its {field} holds a string outside the report's \
                 vocabulary; no records written",
                r.skin, r.configuration
            ));
        }
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
