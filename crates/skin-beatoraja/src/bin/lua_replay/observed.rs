//! What the reference observed, as plain data the comparator reads.

/// A consumer occurrence: its destination, its consumer (`value`, `op`,
/// `draw`, `timer`, or the key a listed object holds it under), its index
/// (an `op` entry's position in the list, 0 otherwise), its callable, and
/// its object path (empty for a top-level object; a listed object's
/// nested part's table keys). Production's records carry no path, so the
/// comparator joins on the other four (s7-06e-1-r1-tables.md, A4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub dest: usize,
    pub field: &'static str,
    pub index: u32,
    pub callable: u32,
    pub path: &'static str,
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
    /// Not invoked for `why`.
    NotReached { key: Key, why: Cause },
}

/// A timer callback's invocations, as the source uses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerOut {
    pub key: Key,
    /// The first invocation's `tolong()` was not off.
    pub on: bool,
    /// The second invocation's `tolong()`.
    pub start: Option<i64>,
    /// Whether the object's selected source is available, so it draws
    /// once its region does (`SkinImage.java:133-139`); `None` when the
    /// facts leave that entry unknown.
    pub shown: Option<bool>,
}

/// One frame.
#[derive(Clone, Debug, Default)]
pub struct RefFrame {
    pub points: Vec<Point>,
    pub timers: Vec<TimerOut>,
    /// Each installed object's points, in schedule order: the range of
    /// `points` its prepare path gave.
    pub objects: Vec<std::ops::Range<usize>>,
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
    /// Objects of kinds the reference does not model, their nested parts'
    /// callables included.
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
    /// Each installed object's consumers, in schedule order: the
    /// occurrences each frame must give exactly one first point each.
    pub installed: Vec<Vec<Key>>,
}

/// `s` with a static lifetime, each distinct one leaked once (an object
/// path, for a key).
pub fn intern(s: &str) -> &'static str {
    use std::collections::BTreeSet;
    thread_local! {
        static SEEN: std::cell::RefCell<BTreeSet<&'static str>> =
            const { std::cell::RefCell::new(BTreeSet::new()) };
    }
    SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        if let Some(&p) = seen.get(s) {
            return p;
        }
        let p: &'static str = Box::leak(s.to_owned().into_boxed_str());
        seen.insert(p);
        p
    })
}
