//! The labels the default report prints and the walk's records hold
//! (docs/stage3.md §3, the conversion report). Each label has one
//! function here, from the survey, and the default report and the record
//! both take it from that function, so a record holds no string the
//! default report would not print. A [`Label`] is made only here: from a
//! survey through those functions, or from one of the program's own
//! literals ([`Label::fixed`]).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use skin_beatoraja::lua::facts::FactRead;
use skin_beatoraja::lua::survey::{RuntimeFrame, RuntimeNoteRecord, Survey};
use skin_model::ScreenKind;

/// A label of the default report.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Label(String);

impl Label {
    /// One of the program's own words.
    pub fn fixed(s: &'static str) -> Self {
        Self(s.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for Label {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// The survey's outcome: `imported` or its refusal's label.
pub fn outcome(s: &Survey) -> Label {
    Label(s.outcome.clone())
}

/// A screen kind, `none` without one.
pub fn kind(k: Option<ScreenKind>) -> Label {
    Label(k.map_or_else(|| "none".to_owned(), |k| format!("{k:?}")))
}

/// The callables, by field and kind.
pub fn callables(s: &Survey) -> BTreeMap<Label, u32> {
    s.callables
        .iter()
        .map(|((f, k), n)| (Label(format!("{f} {k}")), *n))
        .collect()
}

/// The callbacks' outcomes.
pub fn callbacks(s: &Survey) -> BTreeMap<Label, u32> {
    keys(&s.outcomes)
}

/// The notes, by kind.
pub fn notes(s: &Survey) -> BTreeMap<Label, u32> {
    keys(&s.notes)
}

/// The fact reads' counts, by label.
pub fn facts(s: &Survey) -> BTreeMap<Label, u32> {
    keys(&s.facts)
}

/// A recorded fact read: the fact and the value given.
pub fn read(r: &FactRead) -> Label {
    Label(format!("{:?} = {:?}", r.fact, r.value))
}

/// The effects' analysis.
pub fn effects(s: &Survey) -> Label {
    Label(s.effects.clone())
}

/// A play skin's elements by timer.
pub fn timers(by: &BTreeMap<String, usize>) -> BTreeMap<Label, usize> {
    keys(by)
}

/// A gauge's segments and motion.
pub fn gauge(segments: u16, motion: &str) -> Label {
    Label(format!("segments {segments} motion {motion}"))
}

/// An atlas finding.
pub fn atlas(a: &str) -> Label {
    Label(a.to_owned())
}

/// The runtime frame's call statuses.
pub fn status(rt: &RuntimeFrame) -> BTreeMap<Label, u32> {
    keys(&rt.status)
}

/// What a runtime note is.
pub fn runtime_what(n: &RuntimeNoteRecord) -> Label {
    Label(n.what.to_owned())
}

/// The runtime frame's stops.
pub fn stops(rt: &RuntimeFrame) -> Vec<Label> {
    rt.stops.iter().map(|s| Label((*s).to_owned())).collect()
}

/// What stopped the runtime frame, if anything did.
pub fn stopped(rt: &RuntimeFrame) -> Option<Label> {
    rt.stopped.clone().map(Label)
}

fn keys<V: Copy>(m: &BTreeMap<String, V>) -> BTreeMap<Label, V> {
    m.iter().map(|(k, v)| (Label(k.clone()), *v)).collect()
}
