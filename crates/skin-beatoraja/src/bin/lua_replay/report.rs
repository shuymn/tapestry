//! What a replay found, and how it is printed: without `--local`, no
//! name, string or path the skin wrote, only labels, indices and lines.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::observed::{Key, Listed};

/// One finding, located.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub key: Key,
    /// The callable's definition (file, line).
    pub site: (String, u32),
    pub frame: usize,
    /// The frame's round and stage, or its outside kind.
    pub place: String,
    pub what: String,
}

/// A root whose reference invocation could not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stopped {
    /// The kind of frame it stopped on: `play` or `outside`.
    pub kind: &'static str,
    pub site: (String, u32),
    pub api: &'static str,
    pub count: usize,
    /// Its production record.
    pub disposition: String,
    /// The isolation entry covering it, if one does, or why it does not.
    pub covered: Result<(), String>,
}

/// A replay's result.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub frames: usize,
    pub callbacks: usize,
    /// The program's callbacks by consumer field and object kind.
    pub compiled: Vec<(u16, Key, (String, u32))>,
    pub points: usize,
    pub production_runs: usize,
    pub matched: usize,
    pub agreed: usize,
    /// Source invocations attributed to a production record, by bucket.
    pub buckets: BTreeMap<String, usize>,
    pub unmodelled: Vec<Listed>,
    pub removed: Vec<Listed>,
    pub unresolved: Vec<Listed>,
    pub custom: usize,
    pub stopped: Vec<Stopped>,
    pub findings: Vec<Finding>,
    /// Per frame kind (loading, page, ready, …): frames and agreed
    /// invocations.
    pub kinds: BTreeMap<&'static str, (usize, usize)>,
    /// Each round's line.
    pub rounds: Vec<String>,
    /// The agreed invocations of each round's first frame.
    pub first_agreed: Vec<usize>,
    /// Lowered invocations compared and agreed, by frame kind and consumer.
    pub lowered: BTreeMap<(&'static str, &'static str), usize>,
    /// Compared invocations where tapestry's rule differs from the
    /// source's on purpose, by difference: counted, never a finding.
    pub intended: BTreeMap<&'static str, usize>,
}

impl Report {
    /// Whether the run passes: no finding, every stopped root covered.
    pub fn passes(&self) -> bool {
        self.findings.is_empty() && self.stopped.iter().all(|s| s.covered.is_ok())
    }

    /// The report as text; file names only under `local`.
    /// Every finding under `all`, else the first [`SHOWN`].
    pub fn text(&self, index: usize, local: bool, all: bool) -> String {
        let site = |s: &(String, u32)| {
            if local {
                format!("{}:{}", short(&s.0), s.1)
            } else {
                format!("line {}", s.1)
            }
        };
        let mut o = String::new();
        let _ = writeln!(o, "skin {index}");
        let _ = writeln!(o, "frames {}", self.frames);
        let _ = writeln!(o, "program callbacks {}", self.callbacks);
        for (k, key, s) in &self.compiled {
            let _ = writeln!(
                o,
                "compiled {k}: destination {} {} {}",
                key.dest,
                consumer(key, local),
                site(s)
            );
        }
        let _ = writeln!(o, "source points {}", self.points);
        let _ = writeln!(o, "production invocations {}", self.production_runs);
        let _ = writeln!(o, "matched {}", self.matched);
        let _ = writeln!(o, "agreed {}", self.agreed);
        for (b, n) in &self.buckets {
            let _ = writeln!(o, "attributed {b}: {n}");
        }
        for (k, (n, a)) in &self.kinds {
            let _ = writeln!(o, "frames {k}: {n}, agreed {a}");
        }
        for (r, a) in self.rounds.iter().zip(
            self.first_agreed
                .iter()
                .map(Some)
                .chain(std::iter::repeat(None)),
        ) {
            let _ = writeln!(o, "{r}; first frame agreed {}", a.map_or(0, |a| *a));
        }
        for ((kind, field), n) in &self.lowered {
            let _ = writeln!(o, "lowered {field} agreed on {kind} frames: {n}");
        }
        for (what, n) in &self.intended {
            let _ = writeln!(o, "intended difference {what}: {n}");
        }
        let listed = |o: &mut String, label: &str, l: &[Listed]| {
            let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
            for e in l {
                let x = by.entry(e.what.as_str()).or_default();
                x.0 += 1;
                x.1 += e.callables.len();
            }
            for (what, (n, c)) in by {
                let what = if local { what } else { "-" };
                let _ = writeln!(o, "{label} {what}: {n} objects, {c} callables");
            }
        };
        listed(&mut o, "unmodelled", &self.unmodelled);
        listed(&mut o, "removed", &self.removed);
        listed(&mut o, "unresolved", &self.unresolved);
        let _ = writeln!(
            o,
            "custom timers and events with a function {}",
            self.custom
        );
        for s in &self.stopped {
            let _ = writeln!(
                o,
                "stopped {} on {} frames at {} x{} ({}): {}",
                site(&s.site),
                s.kind,
                s.api,
                s.count,
                s.disposition,
                match &s.covered {
                    Ok(()) => "covered".to_owned(),
                    Err(why) => format!("NOT COVERED ({why})"),
                }
            );
        }
        let _ = writeln!(o, "findings {}", self.findings.len());
        let shown = if all { usize::MAX } else { SHOWN };
        for f in self.findings.iter().take(shown) {
            let _ = writeln!(
                o,
                "finding destination {} {} {} frame {} ({}): {}",
                f.key.dest,
                consumer(&f.key, local),
                site(&f.site),
                f.frame,
                f.place,
                f.what
            );
        }
        if self.findings.len() > shown {
            let _ = writeln!(
                o,
                "... {} more findings not printed (--all prints every one)",
                self.findings.len() - shown
            );
        }
        let _ = writeln!(
            o,
            "result {}",
            if self.passes() { "agree" } else { "DISAGREE" }
        );
        o
    }
}

/// The findings a report prints without `--all`.
pub const SHOWN: usize = 200;

/// An occurrence's consumer as a report prints it: the field, an `op`
/// entry's index, and a listed part's object path (the skin's own keys,
/// so only under `local`).
fn consumer(k: &Key, local: bool) -> String {
    let mut s = k.field.to_owned();
    if k.field == "op" {
        s.push_str(&format!("[{}]", k.index));
    }
    if !k.path.is_empty() {
        s.push_str(if local { " at " } else { " at a nested part" });
        if local {
            s.push_str(k.path);
        }
    }
    s
}

/// A chunk name without its directories.
fn short(s: &str) -> &str {
    s.rsplit(['/', '\\']).next().unwrap_or(s)
}
