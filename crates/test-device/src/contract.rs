//! The verdict contract (docs/test-device.md "Verdicts"): a row's
//! expectation, compiled into the repository, and the one checker that
//! compares a run's kept files with it, so a run passes only on an exact
//! match and every difference is named.
//!
//! The checker binds each role to its run only from the reservation the
//! runner wrote (a set's `reservation.jsonl`, an outer run's `start`, or a
//! single run's own `start`), reads every stream and verdict file with the
//! strict reader ([`crate::evidence`]), and compares: the role's runs, the
//! streams' completeness, each process's exit, the records, the runners'
//! errors, and every check's producer, outcome and inputs, each input
//! against its seal.
//!
//! Evidence counts only through one gate, [`Usable`]: a stream other than a
//! role's `run.jsonl` is read once, and its records count (for a record
//! rule, an event a check reads, a check's input) only when the digest of
//! the bytes that were parsed equals a seal bound to the role's run (its
//! own runner's, or a survivor's after it observed the writer end). A
//! `run.jsonl` holds the seals, so its digest is that of the bytes it was
//! parsed from, and its `start` binds it to the reservation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::evidence::{self, Kinds, Outcome, Process, Run, Start, Stream};

/// The runner's stream in a run's folder.
pub const RUN: &str = "run.jsonl";
/// The link's stream.
pub const LINK: &str = "link.jsonl";
/// The recorder's stream.
pub const RECORDER: &str = "recorder.jsonl";
/// A set's reservation.
pub const RESERVATION: &str = "reservation.jsonl";
/// The role of a single run, bound by its own `start`.
pub const SINGLE: &str = "run";
/// The role of the inner run an outer run reserves in its `start`.
pub const INNER: &str = "inner";
/// The name of a role's runner in an [`ExitRule`].
pub const RUNNER: &str = "runner";

/// A row's expectation: everything a run of it must show, declared before
/// it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expectation {
    pub row: &'static str,
    /// Where its roles' runs are reserved.
    pub binding: Binding,
    /// Its roles and the row each runs: `run` (and `inner` for an outer
    /// run) for [`Binding::Run`], the reserved roles for [`Binding::Set`].
    pub roles: Vec<Role>,
    /// The exit of each process it starts, every role's runner included.
    pub exits: Vec<ExitRule>,
    /// The records its streams must hold.
    pub records: Vec<RecordRule>,
    /// Its checks, exactly.
    pub checks: Vec<CheckRule>,
    /// What makes it a negative row.
    pub negative: Option<Negative>,
}

/// Where a row's roles are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A single run, bound by its own `start`, and the inner run that
    /// `start` reserves, if the row has one.
    Run,
    /// A set, bound by its `reservation.jsonl`.
    Set,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Role {
    pub name: &'static str,
    pub row: &'static str,
}

/// A process's exit: the role whose runner observes it, its name
/// ([`RUNNER`] for the role's runner, else the name the observer's `exit`
/// record gives), the process it must be, and how it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitRule {
    pub role: &'static str,
    pub process: &'static str,
    /// The stream whose writer the process is: its first record names it
    /// (a recorder's `ready`, a runner's `start`). Required for every
    /// process but a role's runner, which is its own `run.jsonl`'s writer.
    pub writes: Option<File>,
    pub exit: Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It ends by itself with this code (a runner: its `end` record's).
    Code(i32),
    /// It is killed, which the process that killed it observed.
    Killed,
}

/// A record kind a role's stream holds `count` times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRule {
    pub role: &'static str,
    pub stream: &'static str,
    pub kind: &'static str,
    pub count: Count,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Exactly(usize),
    AtLeast(usize),
}

impl Count {
    const fn admits(self, n: usize) -> bool {
        match self {
            Self::Exactly(c) => n == c,
            Self::AtLeast(c) => n >= c,
        }
    }
}

/// One check: its id, the program that writes its verdict, the outcome it
/// must have, the files it reads and the observed events it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRule {
    pub id: &'static str,
    pub producer: &'static str,
    pub outcome: Outcome,
    pub inputs: Vec<File>,
    pub events: Vec<Event>,
}

/// A file of a role's run, by its path in the run's folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct File {
    pub role: &'static str,
    pub path: &'static str,
}

/// An observed event a check reads: a record kind in a role's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub role: &'static str,
    pub stream: &'static str,
    pub kind: &'static str,
}

/// A negative row's mutation, and its trigger: a check of its failing set
/// whose failure shows the mutation took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negative {
    pub mutation: &'static str,
    pub trigger: &'static str,
}

impl Expectation {
    /// What makes the expectation itself unusable: a repeated role or check,
    /// a rule naming a role the row does not have, a role whose runner's
    /// exit is not declared, a process other than a runner without the
    /// stream it writes, a first check other than `provenance`, or a
    /// negative row without a failing trigger.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let roles: BTreeSet<&str> = self.roles.iter().map(|r| r.name).collect();
        if roles.len() != self.roles.len() {
            out.push("a role is declared twice".to_owned());
        }
        if self.binding == Binding::Run
            && (!roles.contains(SINGLE) || roles.iter().any(|r| ![SINGLE, INNER].contains(r)))
        {
            out.push(format!("a single run's roles are `{SINGLE}` and `{INNER}`"));
        }
        let mut named: Vec<&str> = self.exits.iter().map(|e| e.role).collect();
        named.extend(self.exits.iter().filter_map(|e| e.writes.map(|f| f.role)));
        named.extend(self.records.iter().map(|r| r.role));
        for c in &self.checks {
            named.extend(c.inputs.iter().map(|f| f.role));
            named.extend(c.events.iter().map(|e| e.role));
        }
        for r in named {
            if !roles.contains(r) {
                out.push(format!(
                    "a rule names role `{r}`, which the row does not have"
                ));
            }
        }
        for r in &roles {
            if !self
                .exits
                .iter()
                .any(|e| e.role == *r && e.process == RUNNER)
            {
                out.push(format!("role `{r}`'s runner has no declared exit"));
            }
        }
        for e in &self.exits {
            if (e.process == RUNNER) == e.writes.is_some() {
                out.push(format!(
                    "role `{}`'s `{}` must name the stream it writes, and only a process other than the role's runner does",
                    e.role, e.process
                ));
            }
        }
        if self
            .checks
            .first()
            .is_none_or(|c| c.id != crate::provenance::CHECK)
        {
            out.push(format!(
                "the first check is not `{}`",
                crate::provenance::CHECK
            ));
        }
        let ids: BTreeSet<&str> = self.checks.iter().map(|c| c.id).collect();
        if ids.len() != self.checks.len() {
            out.push("a check is declared twice".to_owned());
        }
        if let Some(n) = self.negative {
            let trigger = self.checks.iter().find(|c| c.id == n.trigger);
            if trigger.is_none_or(|c| c.outcome != Outcome::Fail) {
                out.push(format!(
                    "the trigger `{}` of mutation `{}` is not a check that must fail",
                    n.trigger, n.mutation
                ));
            }
        }
        out
    }
}

/// The checker's result: every difference from the expectation, named,
/// and what it dropped from a killed writer's stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub mismatches: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// `check`'s last line: `CONTRACT <row> ok` or `CONTRACT <row>
    /// mismatch <n>`.
    #[must_use]
    pub fn last_line(&self, row: &str) -> String {
        if self.ok() {
            format!("CONTRACT {row} ok")
        } else {
            format!("CONTRACT {row} mismatch {}", self.mismatches.len())
        }
    }

    fn add(&mut self, m: impl Into<String>) {
        self.mismatches.push(m.into());
    }
}

/// A role bound to its reserved run.
struct Bound {
    name: &'static str,
    /// The run's folder, relative to the folder `check` was given (empty:
    /// that folder).
    dir: String,
    /// What the reservation says the run's `start` must hold.
    run: String,
    row: &'static str,
    build: String,
    set: Option<String>,
}

/// A stream's standing as evidence: its records count only when
/// [`Usable::Sealed`].
#[derive(Debug)]
enum Usable<R> {
    /// Read once, and the digest of the parsed bytes equals the seal bound
    /// to the role's run.
    Sealed(Stream<R>),
    /// Not there.
    Missing,
    /// There, but not evidence: why.
    Not(String),
}

impl<R> Usable<R> {
    const fn stream(&self) -> Option<&Stream<R>> {
        match self {
            Self::Sealed(s) => Some(s),
            _ => None,
        }
    }

    fn why(&self) -> String {
        match self {
            Self::Sealed(_) => "sealed".to_owned(),
            Self::Missing => "missing".to_owned(),
            Self::Not(why) => why.clone(),
        }
    }
}

/// A stream file as read once: its bytes' digest and its parse.
struct Raw<R> {
    sha: String,
    parsed: Result<Stream<R>, evidence::Error>,
}

/// A bound role's run as its files show it.
struct Seen {
    /// The role's folder, relative to the folder `check` was given.
    dir: String,
    start: Start,
    run: Stream<Run>,
    /// The digest of the bytes `run` was parsed from.
    run_sha: String,
    link: Option<Raw<evidence::Link>>,
    recorder: Option<Raw<evidence::Recorder>>,
    /// The seals bound to this run, by path: its runner's own, and a
    /// survivor's (see [`seals`]).
    seals: BTreeMap<String, String>,
}

impl Seen {
    /// The process that wrote `path`: a recorder's `ready` names it; the
    /// role's runner writes everything else.
    fn writer(&self, path: &str) -> Option<Process> {
        if path == RECORDER {
            let rec = self.recorder.as_ref()?.parsed.as_ref().ok()?;
            match rec.records.first()? {
                evidence::Recorder::Ready(r) => Some(r.process),
                _ => None,
            }
        } else {
            Some(self.start.process)
        }
    }

    /// The host counter of the last record of the stream at `path`.
    fn last_q(&self, path: &str) -> Option<i64> {
        fn last<R: Kinds>(s: &Stream<R>) -> Option<i64> {
            s.records.iter().rev().find_map(Kinds::q)
        }
        match path {
            RUN => last(&self.run),
            LINK => last(self.link.as_ref()?.parsed.as_ref().ok()?),
            RECORDER => last(self.recorder.as_ref()?.parsed.as_ref().ok()?),
            _ => None,
        }
    }
}

/// An `exit` record: the role whose runner observed it, that runner, the
/// record's place in that runner's stream, and the exit.
struct Observed<'a> {
    role: &'a str,
    by: Process,
    at: usize,
    exit: &'a evidence::Exit,
}

/// Every role's `exit` records.
fn observed<'a>(seen: &'a BTreeMap<&str, Seen>) -> Vec<Observed<'a>> {
    seen.iter()
        .flat_map(|(role, s)| {
            s.run
                .records
                .iter()
                .enumerate()
                .filter_map(move |(at, r)| match r {
                    Run::Exit(exit) => Some(Observed {
                        role,
                        by: s.start.process,
                        at,
                        exit,
                    }),
                    _ => None,
                })
        })
        .collect()
}

/// How a writer's stream may end, from the exits observed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ending {
    /// Nothing excuses a stream cut short.
    Whole,
    /// It was cut short: killed by another process, or ended with its
    /// role's killed runner and observed by that runner's killer.
    Cut,
    /// The records name the writer's end in ways that disagree, or an exit
    /// that precedes the writer's last record: why.
    Ambiguous(String),
}

/// Whether process `w`, the writer of a stream of the role whose runner is
/// `runner`, ended so that its stream may stop short. An exit counts only
/// for the process its record names by id and creation time, and only when
/// it is not before the writer's last record (`last_q`).
fn ending(exits: &[Observed], w: Process, runner: Process, last_q: Option<i64>) -> Ending {
    let of: Vec<&Observed> = exits.iter().filter(|o| o.exit.process == w).collect();
    if let Some(first) = of.first()
        && of
            .iter()
            .any(|o| (o.exit.code, o.exit.killed) != (first.exit.code, first.exit.killed))
    {
        return Ending::Ambiguous(format!("the exits recorded for process {} disagree", w.pid));
    }
    if let Some(last) = last_q
        && let Some(o) = of.iter().find(|o| o.exit.q < last)
    {
        return Ending::Ambiguous(format!(
            "process {}'s exit is recorded at {}, before its stream's last record at {last}",
            w.pid, o.exit.q
        ));
    }
    if of.iter().any(|o| o.exit.killed && o.by != w) {
        return Ending::Cut;
    }
    if w == runner {
        return Ending::Whole;
    }
    // Its role's runner saw it end by itself: nothing cut it.
    if of.iter().any(|o| o.by == runner) {
        return Ending::Whole;
    }
    // Ended with its runner's tree: observed, after the kill, by the
    // process that killed the runner.
    let tree = exits.iter().any(|kill| {
        kill.exit.process == runner
            && kill.exit.killed
            && kill.by != runner
            && of.iter().any(|o| o.by == kill.by && o.at > kill.at)
    });
    if tree { Ending::Cut } else { Ending::Whole }
}

/// Compares the run, set or outer run in `dir` with `e`.
#[must_use]
pub fn check(e: &Expectation, dir: &Path) -> Report {
    let mut report = Report::default();
    for p in e.problems() {
        report.add(format!("the expectation: {p}"));
    }
    if !report.ok() {
        return report;
    }
    let Some((bound, id)) = bind(e, dir, &mut report) else {
        return report;
    };
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    for b in &bound {
        if let Some(s) = read_role(b, dir, &mut report) {
            seen.insert(b.name, s);
        }
    }
    seals(&mut seen, &mut report);
    let exits = observed(&seen);
    let mut usable: BTreeMap<&str, Streams> = BTreeMap::new();
    for (role, s) in &seen {
        let runner = s.start.process;
        let end_of = |path: &str| {
            s.writer(path).map_or_else(
                || Ending::Ambiguous(format!("{path} names no writer")),
                |w| ending(&exits, w, runner, s.last_q(path)),
            )
        };
        let run_end = end_of(RUN);
        finished(role, RUN, &s.run, &run_end, &mut report);
        let link = s.link.as_ref().map_or(Usable::Missing, |raw| {
            usable_of(role, LINK, raw, s, &end_of(LINK), &mut report)
        });
        if let Some(l) = link.stream() {
            link_errors(role, l, &mut report);
        }
        let recorder = s.recorder.as_ref().map_or(Usable::Missing, |raw| {
            usable_of(role, RECORDER, raw, s, &end_of(RECORDER), &mut report)
        });
        let dropped = [
            (RUN, s.run.dropped.as_deref()),
            (LINK, link.stream().and_then(|l| l.dropped.as_deref())),
            (
                RECORDER,
                recorder.stream().and_then(|r| r.dropped.as_deref()),
            ),
        ];
        for (name, line) in dropped {
            if let Some(line) = line {
                report.notes.push(format!(
                    "role `{role}`: {name}: dropped the unterminated last line {line:?}"
                ));
            }
        }
        for r in &s.run.records {
            if let Run::End(end) = r {
                for error in &end.errors {
                    report.add(format!("role `{role}`: the runner's error: {error}"));
                }
            }
        }
        usable.insert(role, (link, recorder));
    }
    let view = View {
        seen: &seen,
        usable: &usable,
    };
    check_exits(e, &seen, &exits, &mut report);
    check_records(e, &view, &mut report);
    check_verdicts(e, dir, &id, &view, &mut report);
    report
}

/// The checker's view of the evidence.
struct View<'a> {
    seen: &'a BTreeMap<&'a str, Seen>,
    usable: &'a BTreeMap<&'a str, Streams>,
}

/// A role's link and recorder streams as evidence.
type Streams = (Usable<evidence::Link>, Usable<evidence::Recorder>);

impl View<'_> {
    /// How many records of `kind` a role's stream holds, or why it cannot
    /// say: the stream is missing or not evidence.
    fn count(&self, role: &str, stream: &str, kind: &str) -> Result<usize, String> {
        let s = self.seen.get(role).ok_or("its run is not read")?;
        let (link, recorder) = self.usable.get(role).ok_or("its run is not read")?;
        match stream {
            RUN => Ok(s.run.of(kind).count()),
            LINK => link
                .stream()
                .map(|l| l.of(kind).count())
                .ok_or_else(|| link.why()),
            RECORDER => recorder
                .stream()
                .map(|r| r.of(kind).count())
                .ok_or_else(|| recorder.why()),
            _ => Err("no such stream".to_owned()),
        }
    }
}

/// The roles' runs from the reservation and the id the verdicts must name
/// (a single run's, or the set's), or `None` (with the mismatch) when it
/// cannot be read or does not reserve the row's roles.
fn bind(e: &Expectation, dir: &Path, report: &mut Report) -> Option<(Vec<Bound>, String)> {
    let role_row = |name: &str| e.roles.iter().find(|r| r.name == name).map(|r| r.row);
    match e.binding {
        Binding::Set => {
            let set = read_file::<evidence::Reservation>(dir, RESERVATION, report)?;
            set.finished(false)
                .map_err(|err| report.add(format!("{RESERVATION}: {err}")))
                .ok()?;
            let Some(evidence::Reservation::Reserved(set)) = set.records.into_iter().next() else {
                return None;
            };
            if set.row != e.row {
                report.add(format!(
                    "{RESERVATION} reserves row `{}`, not `{}`",
                    set.row, e.row
                ));
                return None;
            }
            let mut out = Vec::new();
            for r in &set.roles {
                let Some(role) = e.roles.iter().find(|x| x.name == r.role) else {
                    report.add(format!(
                        "{RESERVATION} reserves role `{}`, which the row does not have",
                        r.role
                    ));
                    continue;
                };
                if out.iter().any(|b: &Bound| b.name == role.name) {
                    report.add(format!("{RESERVATION} reserves role `{}` twice", r.role));
                    continue;
                }
                if r.row != role.row {
                    report.add(format!(
                        "{RESERVATION} reserves role `{}` for row `{}`, not `{}`",
                        r.role, r.row, role.row
                    ));
                }
                if !one_segment(&r.dir) {
                    report.add(format!(
                        "{RESERVATION}: role `{}`'s folder `{}` is not a folder name",
                        r.role, r.dir
                    ));
                    continue;
                }
                out.push(Bound {
                    name: role.name,
                    dir: r.dir.clone(),
                    run: r.run.clone(),
                    row: role.row,
                    build: set.build.clone(),
                    set: Some(set.set.clone()),
                });
            }
            for role in &e.roles {
                if !set.roles.iter().any(|r| r.role == role.name) {
                    report.add(format!(
                        "{RESERVATION} does not reserve role `{}`",
                        role.name
                    ));
                }
            }
            Some((out, set.set))
        }
        Binding::Run => {
            let run = read_file::<Run>(dir, RUN, report)?;
            let Some(Run::Start(start)) = run.records.first() else {
                return None;
            };
            let mut out = vec![Bound {
                name: SINGLE,
                dir: String::new(),
                run: start.run.clone(),
                row: role_row(SINGLE)?,
                build: start.build.clone(),
                set: None,
            }];
            match (role_row(INNER), &start.inner) {
                (Some(row), Some(inner)) => {
                    if inner.role != INNER || inner.row != row || !one_segment(&inner.dir) {
                        report.add(format!(
                            "the run's `start` reserves {inner:?}, not role `{INNER}` of row `{row}` in a folder of its own"
                        ));
                        return Some((out, start.run.clone()));
                    }
                    out.push(Bound {
                        name: INNER,
                        dir: inner.dir.clone(),
                        run: inner.run.clone(),
                        row,
                        build: start.build.clone(),
                        set: Some(start.run.clone()),
                    });
                }
                (Some(_), None) => {
                    report.add(format!("the run's `start` reserves no `{INNER}` run"))
                }
                (None, Some(inner)) => report.add(format!(
                    "the run's `start` reserves {inner:?}, which the row does not have"
                )),
                (None, None) => {}
            }
            Some((out, start.run.clone()))
        }
    }
}

/// A role's `run.jsonl`, read strictly once, its `start` against its
/// reservation, and its other streams read once (not yet judged).
fn read_role(b: &Bound, dir: &Path, report: &mut Report) -> Option<Seen> {
    let folder = dir.join(&b.dir);
    let at = |name: &str| {
        if b.dir.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", b.dir)
        }
    };
    let Some(raw) = read_raw::<Run>(&folder, RUN, report) else {
        report.add(format!("role `{}` has no readable {}", b.name, at(RUN)));
        return None;
    };
    let run = match raw.parsed {
        Ok(run) => run,
        Err(err) => {
            report.add(format!("{}: {err}", folder.join(RUN).display()));
            report.add(format!("role `{}` has no readable {}", b.name, at(RUN)));
            return None;
        }
    };
    let Some(Run::Start(start)) = run.records.first().cloned() else {
        return None;
    };
    let want = [
        ("run id", start.run.as_str(), b.run.as_str()),
        ("role", start.role.as_str(), b.name),
        ("row", start.row.as_str(), b.row),
        ("build", start.build.as_str(), b.build.as_str()),
        (
            "set",
            start.set.as_deref().unwrap_or("none"),
            b.set.as_deref().unwrap_or("none"),
        ),
    ];
    for (what, got, reserved) in want {
        if got != reserved {
            report.add(format!(
                "role `{}`: {} names {what} `{got}`, the reservation `{reserved}`",
                b.name,
                at(RUN)
            ));
        }
    }
    if !is_hex(&start.run, 32) {
        report.add(format!(
            "role `{}`: the run id `{}` is not 32 lowercase hex digits",
            b.name, start.run
        ));
    }
    let link = folder
        .join(LINK)
        .exists()
        .then(|| read_raw::<evidence::Link>(&folder, LINK, report))
        .flatten();
    let recorder = folder
        .join(RECORDER)
        .exists()
        .then(|| read_raw::<evidence::Recorder>(&folder, RECORDER, report))
        .flatten();
    Some(Seen {
        dir: b.dir.clone(),
        start,
        run,
        run_sha: raw.sha,
        link,
        recorder,
        seals: BTreeMap::new(),
    })
}

/// Each role's seals: those its own runner recorded for its own run, and
/// those a survivor recorded for it. A survivor's seal is the role's when
/// the role's runner was killed by another process, and the survivor
/// recorded the exit of the sealed file's writer before the seal, so the
/// file it sealed could no longer grow. Any other seal is named.
fn seals(seen: &mut BTreeMap<&str, Seen>, report: &mut Report) {
    let mut found: Vec<(&str, String, String)> = Vec::new();
    {
        let exits = observed(seen);
        for (role, s) in seen.iter() {
            for (at, r) in s.run.records.iter().enumerate() {
                let Run::Seal(seal) = r else { continue };
                let well_formed = is_path(&seal.path) && is_hex(&seal.sha256, 64);
                if seal.role == *role && seal.run == s.start.run && well_formed {
                    found.push((role, seal.path.clone(), seal.sha256.clone()));
                    continue;
                }
                let survivor = seen.get_key_value(seal.role.as_str()).filter(|(_, t)| {
                    let killed = exits.iter().any(|o| {
                        o.exit.process == t.start.process
                            && o.exit.killed
                            && o.by != t.start.process
                    });
                    let writer_ended_before = t.writer(&seal.path).is_some_and(|w| {
                        exits
                            .iter()
                            .any(|o| o.exit.process == w && o.by == s.start.process && o.at < at)
                    });
                    seal.role != *role
                        && seal.run == t.start.run
                        && well_formed
                        && killed
                        && writer_ended_before
                });
                match survivor {
                    Some((target, _)) => {
                        found.push((target, seal.path.clone(), seal.sha256.clone()));
                    }
                    None => report.add(format!(
                        "role `{role}`: a seal not of this run's files, nor of a killed run's file whose writer this runner saw end before it: {seal:?}"
                    )),
                }
            }
        }
    }
    for (role, path, sha) in found {
        let Some(s) = seen.get_mut(role) else {
            continue;
        };
        if s.seals.insert(path.clone(), sha).is_some() {
            report.add(format!("role `{role}`: `{path}` is sealed twice"));
        }
    }
}

/// Whether a role's stream other than `run.jsonl` is evidence: it parsed,
/// it ended or its writer was cut short, and the digest of the bytes it
/// was parsed from equals its seal.
fn usable_of<R: Kinds + Clone>(
    role: &str,
    name: &str,
    raw: &Raw<R>,
    s: &Seen,
    ending: &Ending,
    report: &mut Report,
) -> Usable<R> {
    let stream = match &raw.parsed {
        Ok(stream) => stream,
        Err(err) => {
            report.add(format!("role `{role}`: {name}: {err}"));
            return Usable::Not(format!("unreadable: {err}"));
        }
    };
    if !finished(role, name, stream, ending, report) {
        return Usable::Not("unfinished".to_owned());
    }
    match s.seals.get(name) {
        Some(seal) if *seal == raw.sha => Usable::Sealed(stream.clone()),
        Some(seal) => Usable::Not(format!(
            "its bytes have sha256 {}, unlike its seal {seal}",
            raw.sha
        )),
        None => Usable::Not("unsealed".to_owned()),
    }
}

/// A sealed link that recorded an error (a lost link, a refusal, or
/// problems at its end) fails the run whatever the runner's `end` says.
fn link_errors(role: &str, link: &Stream<evidence::Link>, report: &mut Report) {
    for r in &link.records {
        let what = match r {
            evidence::Link::Lost(e) => format!("lost: {}", e.error),
            evidence::Link::Refused(e) => format!("refused: {}", e.error),
            evidence::Link::End(end) if !end.problems.is_empty() => {
                format!("problems at its end: {}", end.problems.join("; "))
            }
            _ => continue,
        };
        report.add(format!("role `{role}`: {LINK} records an error: {what}"));
    }
}

fn read_raw<R: Kinds>(dir: &Path, name: &str, report: &mut Report) -> Option<Raw<R>> {
    let bytes = fs::read(dir.join(name))
        .map_err(|e| report.add(format!("{}: {e}", dir.join(name).display())))
        .ok()?;
    Some(Raw {
        sha: hex(&Sha256::digest(&bytes)),
        parsed: evidence::read(&bytes),
    })
}

fn read_file<R: Kinds>(dir: &Path, name: &str, report: &mut Report) -> Option<Stream<R>> {
    let raw = read_raw(dir, name, report)?;
    raw.parsed
        .map_err(|e| report.add(format!("{}: {e}", dir.join(name).display())))
        .ok()
}

/// Whether the stream is complete for its writer's [`Ending`]; a mismatch
/// names why not.
fn finished<R: Kinds>(
    role: &str,
    name: &str,
    s: &Stream<R>,
    ending: &Ending,
    report: &mut Report,
) -> bool {
    if let Ending::Ambiguous(why) = ending
        && (!s.ended || s.dropped.is_some())
    {
        report.add(format!(
            "role `{role}`: {name}: the stream stops short and its writer's end is ambiguous: {why}"
        ));
        return false;
    }
    match s.finished(*ending == Ending::Cut) {
        Ok(()) => true,
        Err(e) => {
            report.add(format!("role `{role}`: {name}: {e}"));
            false
        }
    }
}

fn check_exits(
    e: &Expectation,
    seen: &BTreeMap<&str, Seen>,
    exits: &[Observed],
    report: &mut Report,
) {
    for rule in &e.exits {
        let Some(s) = seen.get(rule.role) else {
            continue;
        };
        let name = format!("role `{}`'s {}", rule.role, rule.process);
        if rule.process == RUNNER {
            let end = s.run.records.iter().find_map(|r| match r {
                Run::End(end) => Some(end.exit),
                _ => None,
            });
            // A kill that came after the runner wrote its end is a kill
            // all the same.
            let killed = exits.iter().any(|o| {
                o.exit.process == s.start.process && o.exit.killed && o.by != s.start.process
            });
            match (rule.exit, end, killed) {
                (Exit::Code(c), Some(got), false) if got == c => {}
                (Exit::Killed, _, true) => {}
                (Exit::Killed, _, false) => report.add(format!(
                    "{name} must be killed, and no other process observed its exit as a kill"
                )),
                (want, got, killed) => report.add(format!(
                    "{name} exits {want:?}, the run shows end {got:?} and killed {killed}"
                )),
            }
            continue;
        }
        let records: Vec<&evidence::Exit> = exits
            .iter()
            .filter(|o| o.role == rule.role && o.exit.name == rule.process)
            .map(|o| o.exit)
            .collect();
        // The process must be the writer of the stream the rule names.
        let writer = rule
            .writes
            .and_then(|f| Some((f, seen.get(f.role)?.writer(f.path))));
        for x in &records {
            match writer {
                Some((_, Some(w))) if w == x.process => {}
                Some((f, w)) => report.add(format!(
                    "{name}'s exit names process {:?}, not {}/{}'s writer {w:?}",
                    x.process, f.role, f.path
                )),
                None => {}
            }
        }
        match (rule.exit, records.as_slice()) {
            (Exit::Code(c), [x]) if !x.killed && x.code == c => {}
            (Exit::Killed, [x]) if x.killed => {}
            (Exit::Killed, []) => report.add(format!(
                "{name} must be killed, and no runner observed its exit"
            )),
            (want, got) => report.add(format!("{name} exits {want:?}, the run observed {got:?}")),
        }
    }
    for o in exits {
        if !e
            .exits
            .iter()
            .any(|r| r.role == o.role && r.process == o.exit.name)
        {
            report.add(format!(
                "role `{}` observed the exit of `{}`, which the row does not declare",
                o.role, o.exit.name
            ));
        }
        if o.exit.process == o.by {
            report.add(format!(
                "role `{}`'s runner records its own exit as `{}`",
                o.role, o.exit.name
            ));
        }
    }
}

fn check_records(e: &Expectation, view: &View, report: &mut Report) {
    for rule in &e.records {
        if !view.seen.contains_key(rule.role) {
            continue;
        }
        match view.count(rule.role, rule.stream, rule.kind) {
            Ok(n) if rule.count.admits(n) => {}
            Ok(n) => report.add(format!(
                "role `{}`'s {} holds {n} `{}` records, the row expects {:?}",
                rule.role, rule.stream, rule.kind, rule.count
            )),
            Err(why) => report.add(format!(
                "role `{}`'s {} is not evidence ({why}), the row expects {:?} `{}` records",
                rule.role, rule.stream, rule.count, rule.kind
            )),
        }
    }
}

/// Why a check is held incomplete whatever its producer says: a stream it
/// reads has a recorder error, or an event it reads is missing or comes
/// from a stream that is not evidence.
fn held_incomplete(rule: &CheckRule, view: &View) -> Option<String> {
    let recorders = rule
        .inputs
        .iter()
        .filter(|f| f.path == RECORDER)
        .map(|f| f.role)
        .chain(
            rule.events
                .iter()
                .filter(|e| e.stream == RECORDER)
                .map(|e| e.role),
        );
    for role in recorders {
        if let Ok(n @ 1..) = view.count(role, RECORDER, "error") {
            return Some(format!(
                "role `{role}`'s {RECORDER} has {n} recorder errors"
            ));
        }
    }
    for ev in &rule.events {
        match view.count(ev.role, ev.stream, ev.kind) {
            Ok(0) => {
                return Some(format!(
                    "role `{}`'s {} holds no `{}` record",
                    ev.role, ev.stream, ev.kind
                ));
            }
            Ok(_) => {}
            Err(why) => {
                return Some(format!(
                    "role `{}`'s {} is not evidence: {why}",
                    ev.role, ev.stream
                ));
            }
        }
    }
    None
}

fn check_verdicts(e: &Expectation, dir: &Path, id: &str, view: &View, report: &mut Report) {
    let mut verdicts: BTreeMap<String, Vec<evidence::Verdict>> = BTreeMap::new();
    let names = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|d| d.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("verdicts-") && n.ends_with(".jsonl"))
            .collect::<BTreeSet<_>>(),
        Err(err) => {
            report.add(format!("{}: {err}", dir.display()));
            return;
        }
    };
    for name in names {
        let producer = &name["verdicts-".len()..name.len() - ".jsonl".len()];
        let Some(file) = read_file::<evidence::Verdict>(dir, &name, report) else {
            continue;
        };
        if let Err(err) = file.finished(false) {
            report.add(format!("{name}: {err}"));
        }
        for v in file.records {
            if v.producer.name != producer {
                report.add(format!(
                    "{name}: check `{}` is produced by `{}`",
                    v.check, v.producer.name
                ));
            }
            if v.row != e.row {
                report.add(format!("{name}: check `{}` is of row `{}`", v.check, v.row));
            }
            if v.run != id {
                report.add(format!(
                    "{name}: check `{}` judges run `{}`, not this run's `{id}`",
                    v.check, v.run
                ));
            }
            verdicts.entry(v.check.clone()).or_default().push(v);
        }
    }
    for rule in &e.checks {
        match verdicts.remove(rule.id).as_deref() {
            None | Some([]) => report.add(format!("check `{}` is missing", rule.id)),
            Some([v]) => check_one(rule, v, dir, view, report),
            Some(more) => report.add(format!("check `{}` has {} verdicts", rule.id, more.len())),
        }
    }
    for id in verdicts.keys() {
        report.add(format!("check `{id}` is not the row's"));
    }
}

fn check_one(
    rule: &CheckRule,
    v: &evidence::Verdict,
    dir: &Path,
    view: &View,
    report: &mut Report,
) {
    let id = rule.id;
    if v.producer.name != rule.producer {
        report.add(format!(
            "check `{id}` is produced by `{}`, the row declares `{}`",
            v.producer.name, rule.producer
        ));
    }
    if v.outcome != rule.outcome {
        report.add(format!(
            "check `{id}` is {:?}, the row expects {:?}: {}",
            v.outcome, rule.outcome, v.detail
        ));
    }
    if let Some(why) = held_incomplete(rule, view)
        && v.outcome != Outcome::Incomplete
    {
        report.add(format!(
            "check `{id}` is {:?}, but its evidence is incomplete: {why}",
            v.outcome
        ));
    }
    let declared: BTreeSet<(&str, &str)> = rule.inputs.iter().map(|f| (f.role, f.path)).collect();
    let read: Vec<(&str, &str)> = v
        .inputs
        .iter()
        .map(|i| (i.role.as_str(), i.path.as_str()))
        .collect();
    let read_set: BTreeSet<(&str, &str)> = read.iter().copied().collect();
    if read_set != declared || read.len() != read_set.len() {
        let mut m = format!("check `{id}` read");
        for (role, path) in &read {
            let _ = write!(m, " {role}/{path}");
        }
        m.push_str(", not exactly its declared files");
        for f in &rule.inputs {
            let _ = write!(m, " {}/{}", f.role, f.path);
        }
        report.add(m);
    }
    for f in &rule.inputs {
        let Some(sealed) = seal_of(f, view, dir, report) else {
            continue;
        };
        if let Some(i) = v
            .inputs
            .iter()
            .find(|i| i.role == f.role && i.path == f.path)
            && i.sha256 != sealed
        {
            report.add(format!(
                "check `{id}` read {}/{} with sha256 {}, its seal is {sealed}",
                f.role, f.path, i.sha256
            ));
        }
    }
}

/// The sha256 a check's input must have. A role's [`RUN`] stream holds the
/// seals: its digest is that of the bytes it was parsed from. A record
/// stream must be [`Usable::Sealed`]; any other file must match its seal.
fn seal_of(f: &File, view: &View, dir: &Path, report: &mut Report) -> Option<String> {
    let s = view.seen.get(f.role)?;
    if f.path == RUN {
        return Some(s.run_sha.clone());
    }
    let (link, recorder) = view.usable.get(f.role)?;
    let why = match f.path {
        LINK => link.stream().is_none().then(|| link.why()),
        RECORDER => recorder.stream().is_none().then(|| recorder.why()),
        _ => None,
    };
    if let Some(why) = why {
        report.add(format!("{}/{} is not evidence: {why}", f.role, f.path));
        return None;
    }
    let Some(sealed) = s.seals.get(f.path) else {
        report.add(format!("{}/{} has no seal", f.role, f.path));
        return None;
    };
    let actual = fs::read(dir.join(&s.dir).join(f.path)).map(|b| hex(&Sha256::digest(b)));
    match actual {
        Ok(a) if a == *sealed => {}
        Ok(a) => report.add(format!(
            "{}/{} has sha256 {a}, unlike its seal {sealed}",
            f.role, f.path
        )),
        Err(e) => report.add(format!("{}/{}: {e}", f.role, f.path)),
    }
    Some(sealed.clone())
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One folder name: no separator, not `.` or `..`.
fn one_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':'])
}

/// A relative path inside a run's folder: `/`-separated folder names.
fn is_path(s: &str) -> bool {
    s.split('/').all(one_segment)
}

#[cfg(test)]
mod tests;
