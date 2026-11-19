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

/// A process's exit: its role, its name ([`RUNNER`] for the role's runner,
/// else the name its runner's `exit` record gives), and how it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitRule {
    pub role: &'static str,
    pub process: &'static str,
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
    /// exit is not declared, a first check other than `provenance`, or a
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

/// A bound role's run as its files show it.
struct Seen {
    /// The role's folder, relative to the folder `check` was given.
    dir: String,
    start: Start,
    run: Stream<Run>,
    link: Option<Stream<evidence::Link>>,
    recorder: Option<Stream<evidence::Recorder>>,
    /// The run's seals by path.
    seals: BTreeMap<String, String>,
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
    let Some(bound) = bind(e, dir, &mut report) else {
        return report;
    };
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    for b in &bound {
        if let Some(s) = read_role(b, dir, &mut report) {
            seen.insert(b.name, s);
        }
    }
    let exits: Vec<Observed> = seen
        .iter()
        .flat_map(|(role, s)| {
            s.run.records.iter().filter_map(move |r| match r {
                Run::Exit(exit) => Some(Observed {
                    role,
                    by: s.start.process,
                    exit,
                }),
                _ => None,
            })
        })
        .collect();
    for (role, s) in &seen {
        // A stream may stop short only when its writer was killed by
        // another process, or ended with its killed role's tree and was
        // seen to end by another process.
        let runner_killed = killed(&exits, s.start.process);
        let cut = |writer: Process| {
            killed(&exits, writer) || (runner_killed && seen_ending(&exits, writer))
        };
        finished(role, RUN, &s.run, cut(s.start.process), &mut report);
        if let Some(link) = &s.link {
            finished(role, LINK, link, cut(s.start.process), &mut report);
        }
        if let Some(rec) = &s.recorder {
            let writer = rec.records.first().and_then(|r| match r {
                evidence::Recorder::Ready(r) => Some(r.process),
                _ => None,
            });
            finished(role, RECORDER, rec, writer.is_some_and(cut), &mut report);
        }
        let dropped = [
            (RUN, s.run.dropped.as_deref()),
            (LINK, s.link.as_ref().and_then(|l| l.dropped.as_deref())),
            (
                RECORDER,
                s.recorder.as_ref().and_then(|r| r.dropped.as_deref()),
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
    }
    check_exits(e, &seen, &exits, &mut report);
    check_records(e, &seen, &mut report);
    check_verdicts(e, dir, &seen, &mut report);
    report
}

/// The roles' runs from the reservation, or `None` (with the mismatch) when
/// it cannot be read or does not reserve the row's roles.
fn bind(e: &Expectation, dir: &Path, report: &mut Report) -> Option<Vec<Bound>> {
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
            Some(out)
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
                        return Some(out);
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
            Some(out)
        }
    }
}

/// A role's streams, read strictly, and its `start` against its
/// reservation.
fn read_role(b: &Bound, dir: &Path, report: &mut Report) -> Option<Seen> {
    let folder = dir.join(&b.dir);
    let at = |name: &str| {
        if b.dir.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", b.dir)
        }
    };
    let run = read_file::<Run>(&folder, RUN, report).or_else(|| {
        report.add(format!("role `{}` has no readable {}", b.name, at(RUN)));
        None
    })?;
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
    let mut seals = BTreeMap::new();
    for r in &run.records {
        if let Run::Seal(s) = r {
            if s.run != start.run || s.role != b.name || !is_path(&s.path) || !is_hex(&s.sha256, 64)
            {
                report.add(format!(
                    "role `{}`: a seal not of this run's files: {s:?}",
                    b.name
                ));
            } else if seals.insert(s.path.clone(), s.sha256.clone()).is_some() {
                report.add(format!("role `{}`: `{}` is sealed twice", b.name, s.path));
            }
        }
    }
    let link = folder
        .join(LINK)
        .exists()
        .then(|| read_file::<evidence::Link>(&folder, LINK, report))
        .flatten();
    let recorder = folder
        .join(RECORDER)
        .exists()
        .then(|| read_file::<evidence::Recorder>(&folder, RECORDER, report))
        .flatten();
    Some(Seen {
        dir: b.dir.clone(),
        start,
        run,
        link,
        recorder,
        seals,
    })
}

fn read_file<R: Kinds>(dir: &Path, name: &str, report: &mut Report) -> Option<Stream<R>> {
    let bytes = fs::read(dir.join(name))
        .map_err(|e| report.add(format!("{}: {e}", dir.join(name).display())))
        .ok()?;
    evidence::read(&bytes)
        .map_err(|e| report.add(format!("{}: {e}", dir.join(name).display())))
        .ok()
}

fn finished<R: Kinds>(role: &str, name: &str, s: &Stream<R>, observed: bool, report: &mut Report) {
    if let Err(e) = s.finished(observed) {
        report.add(format!("role `{role}`: {name}: {e}"));
    }
}

/// An `exit` record: the role whose runner observed it, that runner, and
/// the exit.
struct Observed<'a> {
    role: &'a str,
    by: Process,
    exit: &'a evidence::Exit,
}

/// Another process killed `p` and observed its exit.
fn killed(exits: &[Observed], p: Process) -> bool {
    exits
        .iter()
        .any(|o| o.exit.process == p && o.exit.killed && o.by != p)
}

/// Another process observed `p`'s exit.
fn seen_ending(exits: &[Observed], p: Process) -> bool {
    exits.iter().any(|o| o.exit.process == p && o.by != p)
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
            let killed = killed(exits, s.start.process);
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

/// How many records of `kind` a role's stream holds, or `None` when the
/// stream is missing.
fn count(s: &Seen, stream: &str, kind: &str) -> Option<usize> {
    match stream {
        RUN => Some(s.run.of(kind).count()),
        LINK => s.link.as_ref().map(|l| l.of(kind).count()),
        RECORDER => s.recorder.as_ref().map(|r| r.of(kind).count()),
        _ => None,
    }
}

fn check_records(e: &Expectation, seen: &BTreeMap<&str, Seen>, report: &mut Report) {
    for rule in &e.records {
        let Some(s) = seen.get(rule.role) else {
            continue;
        };
        match count(s, rule.stream, rule.kind) {
            Some(n) if rule.count.admits(n) => {}
            got => report.add(format!(
                "role `{}`'s {} holds {got:?} `{}` records, the row expects {:?}",
                rule.role, rule.stream, rule.kind, rule.count
            )),
        }
    }
}

/// Why a check is held incomplete whatever its producer says: a stream it
/// reads has a recorder error, or an event it reads is missing.
fn held_incomplete(rule: &CheckRule, seen: &BTreeMap<&str, Seen>) -> Option<String> {
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
        let errors = seen.get(role).and_then(|s| count(s, RECORDER, "error"));
        if let Some(n @ 1..) = errors {
            return Some(format!(
                "role `{role}`'s {RECORDER} has {n} recorder errors"
            ));
        }
    }
    for ev in &rule.events {
        let n = seen.get(ev.role).and_then(|s| count(s, ev.stream, ev.kind));
        if n.unwrap_or(0) == 0 {
            return Some(format!(
                "role `{}`'s {} holds no `{}` record",
                ev.role, ev.stream, ev.kind
            ));
        }
    }
    None
}

fn check_verdicts(e: &Expectation, dir: &Path, seen: &BTreeMap<&str, Seen>, report: &mut Report) {
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
            verdicts.entry(v.check.clone()).or_default().push(v);
        }
    }
    for rule in &e.checks {
        match verdicts.remove(rule.id).as_deref() {
            None | Some([]) => report.add(format!("check `{}` is missing", rule.id)),
            Some([v]) => check_one(rule, v, dir, seen, report),
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
    seen: &BTreeMap<&str, Seen>,
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
    if let Some(why) = held_incomplete(rule, seen)
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
        let Some(s) = seen.get(f.role) else {
            continue;
        };
        let Some(sealed) = seal_of(f, s, dir, report) else {
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

/// The sha256 a file must have: its seal, the file's own bytes checked
/// against it. A role's [`RUN`] stream, which holds the seals, has none:
/// its digest is its bytes' as read.
fn seal_of(f: &File, s: &Seen, dir: &Path, report: &mut Report) -> Option<String> {
    let folder = dir.join(&s.dir);
    let actual = fs::read(folder.join(f.path)).map(|b| hex(&Sha256::digest(b)));
    if f.path == RUN {
        return actual
            .map_err(|e| report.add(format!("{}/{}: {e}", f.role, f.path)))
            .ok();
    }
    let Some(sealed) = s.seals.get(f.path) else {
        report.add(format!("{}/{} has no seal", f.role, f.path));
        return None;
    };
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
