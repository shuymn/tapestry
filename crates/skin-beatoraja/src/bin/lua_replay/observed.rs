//! What the reference observed, as plain data the comparator reads.

/// A consumer occurrence: its destination, its field and its callable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub dest: usize,
    pub field: &'static str,
    pub callable: u32,
}

/// A returned value as the comparator reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Raw {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A string, table, function or other value no program holds.
    Other(&'static str),
}

/// What one invocation came to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    Value(Raw),
    Error,
    Unavailable,
    /// A host read the reference cannot answer, by API.
    Unanswered(&'static str),
    Budget,
}

/// Why an occurrence of an installed object was not invoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// An image's selector was negative.
    Selector,
    /// A number's value was a sentinel.
    Sentinel,
    /// An earlier condition was false.
    Condition,
    /// An earlier read was unavailable.
    Unavailable,
    /// An earlier invocation could not complete.
    Inconclusive,
}

/// One point of a frame's schedule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Point {
    Ran {
        key: Key,
        call: u8,
        outcome: Outcome,
    },
    NotReached {
        key: Key,
        why: Cause,
    },
}

/// A timer callback's invocations, as the source uses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerOut {
    pub key: Key,
    /// The first invocation's `tolong()` was not off.
    pub on: bool,
    /// The second invocation's `tolong()`.
    pub start: Option<i64>,
}

/// One frame.
#[derive(Clone, Debug, Default)]
pub struct RefFrame {
    pub points: Vec<Point>,
    pub timers: Vec<TimerOut>,
}

/// An object the reference did not run, with the callables it holds.
#[derive(Clone, Debug)]
pub struct Listed {
    pub dest: usize,
    /// The kind, or why it was not installed.
    pub what: String,
    pub callables: Vec<Key>,
}

/// Everything the reference observed.
#[derive(Clone, Debug, Default)]
pub struct Observed {
    pub frames: Vec<RefFrame>,
    /// Objects of kinds the reference does not model.
    pub unmodelled: Vec<Listed>,
    /// Objects the source removes at installation.
    pub removed: Vec<Listed>,
    /// Destinations whose id resolves to nothing, or whose installation the
    /// reference cannot decide.
    pub unresolved: Vec<Listed>,
    /// Each callable's definition (file, line), where the VM knows it.
    pub sites: std::collections::BTreeMap<u32, (String, u32)>,
    /// Each callable's last line, where the VM knows it.
    pub ends: std::collections::BTreeMap<u32, u32>,
    /// Custom timers and events holding a function.
    pub custom: usize,
    /// The skin folder, which chunk names are relative to.
    pub root: std::path::PathBuf,
}
