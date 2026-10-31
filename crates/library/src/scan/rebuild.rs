//! The bests phase of a scan pass (design.md "Personal Bests"): after the
//! chart phase, the records under `records\` are verified, evaluated and
//! counted into `scores`, one at a time, in batches of [`BEST_BATCH`].
//!
//! - **When.** A full catch-up runs while one is pending: `requested`
//!   exceeds `completed`, or a request whose write failed is held in
//!   memory. Otherwise, when this pass's chart phase read files, only the
//!   records waiting for those charts are retried.
//! - **Each record** is skipped without reading when its path is counted,
//!   or refused for a terminal reason, with the same size and
//!   modification time; else read, decoded and hashed, and skipped when its
//!   identity is counted; else checked, its chart looked up and loaded,
//!   admitted against the memory budget, and evaluated. The first failure
//!   is its refusal, and a panic is one too.
//! - **Pausing** works as in the chart phase, with a checkpoint before each
//!   of those steps; a chart load and an admitted evaluation are the units
//!   that cannot be interrupted.
//! - **Completion.** A catch-up that listed `records\` completely and
//!   failed nowhere sets `completed` to the `requested` it read first, in
//!   its last commit; a request made meanwhile raised `requested` beyond
//!   it and stays pending.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use bms_parser::Limits;
use replay::verify::{self, VerifyError};
use replay::{DecodeLimits, PlayRecord};
use rusqlite::{Connection, OptionalExtension as _, params};

use super::{Scanner, stamp};
use crate::bests::{self, Contribution, PlayId, Verified};
use crate::db;

/// Contributions and refusals per commit.
pub const BEST_BATCH: usize = 64;
/// The memory each record's evaluation is admitted within, the budget of a
/// play's recording.
pub const REBUILD_MEMORY: usize = 512 << 20;

/// Why the bests phase did not count a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The file went before it was read.
    Missing,
    /// The file or its name cannot be read.
    Unreadable,
    /// The record does not decode.
    Decode,
    /// Another evaluator version made it.
    Evaluator,
    /// Its preset copy does not decode.
    PresetCopy,
    /// Another loader version made it.
    Loader,
    /// No indexed chart has its file's SHA-256.
    ChartNotInLibrary,
    /// Its chart's file cannot be read.
    ChartUnreadable,
    /// Its chart does not load.
    Load,
    /// Its chart resolves to another chart.
    Resolved,
    /// Its chart plays otherwise at the record's speed.
    Played,
    /// Its chart transform is unknown.
    Transform,
    /// Evaluating it would hold more than the budget.
    TooLarge,
    /// The evaluator refused it.
    Eval,
    /// Checking or evaluating it panicked.
    Panicked,
}

impl Refusal {
    /// Every refusal, by its stored number (its index).
    const ALL: [Self; 15] = [
        Self::Missing,
        Self::Unreadable,
        Self::Decode,
        Self::Evaluator,
        Self::PresetCopy,
        Self::Loader,
        Self::ChartNotInLibrary,
        Self::ChartUnreadable,
        Self::Load,
        Self::Resolved,
        Self::Played,
        Self::Transform,
        Self::TooLarge,
        Self::Eval,
        Self::Panicked,
    ];

    fn number(self) -> i64 {
        Self::ALL
            .iter()
            .position(|&r| r == self)
            .and_then(|i| i64::try_from(i).ok())
            .unwrap_or(-1)
    }

    /// Whether the refusal stands as long as the record file and the
    /// interpretation tuple do: such a record is skipped by its stamp. A
    /// file that cannot be read now may be later, and a record waiting
    /// for its chart is retried once the chart is read.
    const fn terminal(self) -> bool {
        !matches!(
            self,
            Self::Missing | Self::Unreadable | Self::ChartNotInLibrary | Self::ChartUnreadable
        )
    }

    fn of(e: &VerifyError) -> Self {
        match e {
            VerifyError::UnsupportedEvaluator(_) => Self::Evaluator,
            VerifyError::PresetCopy(_) => Self::PresetCopy,
            VerifyError::UnsupportedLoader(_) => Self::Loader,
            VerifyError::Load(_) => Self::Load,
            VerifyError::ResolvedChartMismatch => Self::Resolved,
            VerifyError::PlayedChartMismatch => Self::Played,
            VerifyError::UnknownTransform { .. } => Self::Transform,
            VerifyError::Eval(_) => Self::Eval,
            VerifyError::TooLarge { .. } => Self::TooLarge,
        }
    }
}

/// Refused records, by reason: one count per [`Refusal`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refusals {
    /// [`Refusal::Missing`].
    pub missing: u64,
    /// [`Refusal::Unreadable`].
    pub unreadable: u64,
    /// [`Refusal::Decode`].
    pub decode: u64,
    /// [`Refusal::Evaluator`].
    pub evaluator: u64,
    /// [`Refusal::PresetCopy`].
    pub preset_copy: u64,
    /// [`Refusal::Loader`].
    pub loader: u64,
    /// [`Refusal::ChartNotInLibrary`].
    pub chart_not_in_library: u64,
    /// [`Refusal::ChartUnreadable`].
    pub chart_unreadable: u64,
    /// [`Refusal::Load`].
    pub load: u64,
    /// [`Refusal::Resolved`].
    pub resolved: u64,
    /// [`Refusal::Played`].
    pub played: u64,
    /// [`Refusal::Transform`].
    pub transform: u64,
    /// [`Refusal::TooLarge`].
    pub too_large: u64,
    /// [`Refusal::Eval`].
    pub eval: u64,
    /// [`Refusal::Panicked`].
    pub panicked: u64,
}

impl Refusals {
    fn count(&mut self, r: Refusal) {
        let n = match r {
            Refusal::Missing => &mut self.missing,
            Refusal::Unreadable => &mut self.unreadable,
            Refusal::Decode => &mut self.decode,
            Refusal::Evaluator => &mut self.evaluator,
            Refusal::PresetCopy => &mut self.preset_copy,
            Refusal::Loader => &mut self.loader,
            Refusal::ChartNotInLibrary => &mut self.chart_not_in_library,
            Refusal::ChartUnreadable => &mut self.chart_unreadable,
            Refusal::Load => &mut self.load,
            Refusal::Resolved => &mut self.resolved,
            Refusal::Played => &mut self.played,
            Refusal::Transform => &mut self.transform,
            Refusal::TooLarge => &mut self.too_large,
            Refusal::Eval => &mut self.eval,
            Refusal::Panicked => &mut self.panicked,
        };
        *n += 1;
    }

    /// Every refusal counted.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.missing
            + self.unreadable
            + self.decode
            + self.evaluator
            + self.preset_copy
            + self.loader
            + self.chart_not_in_library
            + self.chart_unreadable
            + self.load
            + self.resolved
            + self.played
            + self.transform
            + self.too_large
            + self.eval
            + self.panicked
    }
}

impl fmt::Display for Refusals {
    /// The reasons counted, `reason n` each, comma separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let all = [
            ("missing", self.missing),
            ("unreadable", self.unreadable),
            ("decode", self.decode),
            ("evaluator", self.evaluator),
            ("preset copy", self.preset_copy),
            ("loader", self.loader),
            ("chart not in library", self.chart_not_in_library),
            ("chart unreadable", self.chart_unreadable),
            ("load", self.load),
            ("resolved", self.resolved),
            ("played", self.played),
            ("transform", self.transform),
            ("too large", self.too_large),
            ("eval", self.eval),
            ("panicked", self.panicked),
        ];
        let mut first = true;
        for (name, n) in all.into_iter().filter(|&(_, n)| n > 0) {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "{name} {n}")?;
            first = false;
        }
        Ok(())
    }
}

/// What the bests phase did, so far or in all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BestsReport {
    /// The phase is running.
    pub running: bool,
    /// The phase ran a full catch-up.
    pub catch_up: bool,
    /// Records retried because their chart was read this pass, outside a
    /// catch-up.
    pub retried: u64,
    /// Plays counted.
    pub counted: u64,
    /// Records read whose play was counted before.
    pub already: u64,
    /// Records skipped without reading: counted with this size and
    /// modification time.
    pub skipped_unchanged: u64,
    /// Records skipped without reading: refused for a terminal reason with
    /// this size and modification time.
    pub skipped_refused: u64,
    /// Records refused, by reason.
    pub refused: Refusals,
    /// Batches committed.
    pub commits: u64,
    /// The catch-up listed `records\` to its end: completing it, with no
    /// failure, settles the requests it started from.
    pub enumeration_complete: bool,
}

/// One entry of the phase's batch.
#[derive(Debug)]
pub(super) enum Item {
    /// A play to count.
    Counted(Box<Contribution>),
    /// A play counted before, whose file changed only its stamp.
    Restamp(PlayId),
    /// A record refused for a reason that is stored.
    Refused {
        path: String,
        size: i64,
        mtime_ns: i64,
        reason: Refusal,
        waits_for: Option<[u8; 32]>,
    },
}

/// What processing one record came to.
enum Outcome {
    Item(Item),
    Skipped,
    Refused(Refusal),
}

/// How the phase goes on after a step.
enum Flow {
    Go,
    /// The scan is being dropped.
    Stop,
}

/// Runs `f`, a panic becoming [`Refusal::Panicked`].
fn guarded<T>(f: impl FnOnce() -> Result<T, Refusal>) -> Result<T, Refusal> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(Err(Refusal::Panicked))
}

/// Reads at most `cap` bytes of `path` and one more, so a larger file is
/// told apart.
fn read_capped(path: &Path, cap: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `path` is counted with this stamp.
fn counted_unchanged(
    conn: &Connection,
    path: &str,
    size: i64,
    mtime: i64,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM counted WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3 LIMIT 1",
        params![path, size, mtime],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Whether `path` is refused for a terminal reason with this stamp.
fn refused_unchanged(
    conn: &Connection,
    path: &str,
    size: i64,
    mtime: i64,
) -> rusqlite::Result<bool> {
    let reason: Option<i64> = conn
        .query_row(
            "SELECT reason FROM refused WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3",
            params![path, size, mtime],
            |r| r.get(0),
        )
        .optional()?;
    Ok(reason
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|i| Refusal::ALL.get(i))
        .is_some_and(|r| r.terminal()))
}

/// Whether the play `(path, body)` is counted.
fn is_counted(conn: &Connection, path: &str, body: &[u8; 32]) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM counted WHERE path = ?1 AND body_sha256 = ?2",
        params![path, body],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// The indexed chart file of `sha256`, the first by path.
fn chart_path(conn: &Connection, sha256: &[u8; 32]) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT path FROM charts WHERE sha256 = ?1 ORDER BY path LIMIT 1",
        [sha256],
        |r| r.get(0),
    )
    .optional()
}

/// The paths of records waiting for one of `charts`, by path.
fn waiting(conn: &Connection, charts: &HashSet<[u8; 32]>) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT path, waits_for FROM refused WHERE waits_for IS NOT NULL ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, [u8; 32]>(1)?))
    })?;
    let mut paths = Vec::new();
    for row in rows {
        let (path, hash) = row?;
        if charts.contains(&hash) {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Writes `items` in one transaction, and `completed` when given.
fn commit(conn: &mut Connection, items: &[Item], completed: Option<i64>) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    let mut counted = 0;
    for item in items {
        match item {
            Item::Counted(c) => {
                if matches!(bests::ingest(&tx, c)?, bests::Ingested::Counted { .. }) {
                    counted += 1;
                }
            }
            Item::Restamp(id) => bests::restamp(&tx, id)?,
            Item::Refused {
                path,
                size,
                mtime_ns,
                reason,
                waits_for,
            } => {
                tx.execute(
                    "INSERT INTO refused (path, size, mtime_ns, reason, waits_for)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (path) DO UPDATE SET size = excluded.size,
                         mtime_ns = excluded.mtime_ns, reason = excluded.reason,
                         waits_for = excluded.waits_for",
                    params![path, size, mtime_ns, reason.number(), waits_for],
                )?;
            }
        }
    }
    if let Some(r) = completed {
        tx.execute("UPDATE meta SET completed = ?1 WHERE id = 1", [r])?;
    }
    tx.commit()?;
    Ok(counted)
}

/// A chart read for the records of one folder.
struct ChartFile {
    sha256: [u8; 32],
    file: Result<(PathBuf, Vec<u8>), Refusal>,
}

impl Scanner<'_> {
    fn count_bests(&self, f: impl FnOnce(&mut BestsReport)) {
        self.shared.update(|state| f(&mut state.report.bests));
    }

    /// Commits the bests batch, and `completed` when given.
    pub(super) fn commit_bests(&mut self, completed: Option<i64>) -> rusqlite::Result<()> {
        if self.items.is_empty() && completed.is_none() {
            return Ok(());
        }
        #[cfg(test)]
        self.options
            .test
            .bests(super::tests::Step::Commit(self.items.len()));
        let counted = commit(&mut self.conn, &self.items, completed)?;
        self.items.clear();
        self.count_bests(|r| {
            r.counted += counted;
            r.commits += 1;
        });
        Ok(())
    }

    fn push_item(&mut self, item: Item) -> rusqlite::Result<()> {
        self.items.push(item);
        if self.items.len() >= BEST_BATCH {
            self.commit_bests(None)?;
        }
        Ok(())
    }

    /// The bests phase, after the chart phase's last commit.
    pub(super) fn bests(&mut self, records: &Path) -> rusqlite::Result<()> {
        // A request whose write failed is written first, and forgotten only
        // once it is.
        if self.owner.catch_up.swap(false, Ordering::AcqRel) && db::request(&self.conn).is_err() {
            self.owner.catch_up.store(true, Ordering::Release);
        }
        let held = self.owner.catch_up.load(Ordering::Acquire);
        let (requested, completed) = db::progress(&self.conn)?;
        if requested > completed || held {
            self.count_bests(|r| {
                r.running = true;
                r.catch_up = true;
            });
            let ended = self.catch_up(records, requested);
            self.count_bests(|r| r.running = false);
            return ended;
        }
        if self.read.is_empty() {
            return Ok(());
        }
        let read = std::mem::take(&mut self.read);
        let paths = match waiting(&self.conn, &read) {
            Ok(paths) => paths,
            Err(e) => {
                let _ = self.owner.request(&self.conn);
                return Err(e);
            }
        };
        if paths.is_empty() {
            return Ok(());
        }
        self.count_bests(|r| r.running = true);
        let ended = self.retry(records, paths);
        self.count_bests(|r| r.running = false);
        // The charts read are forgotten with the pass: a retry that failed
        // leaves its records to a catch-up, written or held in memory.
        if ended.is_err() {
            let _ = self.owner.request(&self.conn);
        }
        ended
    }

    /// Retries the records at `paths` (relative to `records`).
    fn retry(&mut self, records: &Path, paths: Vec<String>) -> rusqlite::Result<()> {
        let mut chart = None;
        for rel in paths {
            self.count_bests(|r| r.retried += 1);
            let path = records.join(&rel);
            if let Flow::Stop = self.record(&path, rel, &mut chart)? {
                return self.commit_bests(None);
            }
        }
        self.commit_bests(None)
    }

    /// The catch-up: every record under `records`, then `completed` set to
    /// `requested` when the listing was complete.
    fn catch_up(&mut self, records: &Path, requested: i64) -> rusqlite::Result<()> {
        let mut complete = true;
        let top = match fs::read_dir(records) {
            Ok(top) => Some(top),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(_) => {
                complete = false;
                None
            }
        };
        for entry in top.into_iter().flatten() {
            let Ok(entry) = entry else {
                complete = false;
                break;
            };
            let folder = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(_) => {
                    complete = false;
                    continue;
                }
            }
            let (files, listed) = record_files(&folder, &self.options);
            complete &= listed;
            let Some(folder_name) = folder.file_name().and_then(|n| n.to_str()) else {
                for _ in &files {
                    self.count_bests(|r| r.refused.count(Refusal::Unreadable));
                }
                continue;
            };
            let folder_name = folder_name.to_owned();
            let mut chart = None;
            for file in files {
                let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                    self.count_bests(|r| r.refused.count(Refusal::Unreadable));
                    continue;
                };
                let rel = format!("{folder_name}/{name}");
                if let Flow::Stop = self.record(&file, rel, &mut chart)? {
                    return self.commit_bests(None);
                }
            }
        }
        self.count_bests(|r| r.enumeration_complete = complete);
        self.commit_bests(complete.then_some(requested))
    }

    /// Between two steps of a record: on a pause, commits and waits for
    /// the release.
    fn step(&mut self) -> rusqlite::Result<Flow> {
        Ok(if self.checkpoint()? {
            Flow::Go
        } else {
            Flow::Stop
        })
    }

    /// Processes one record file, `rel` under `records`.
    fn record(
        &mut self,
        path: &Path,
        rel: String,
        chart: &mut Option<ChartFile>,
    ) -> rusqlite::Result<Flow> {
        if let Flow::Stop = self.step()? {
            return Ok(Flow::Stop);
        }
        let meta = match fs::metadata(path) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => return self.refuse(rel, (0, 0), Refusal::Unreadable, None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return self.refuse(rel, (0, 0), Refusal::Missing, None);
            }
            Err(_) => return self.refuse(rel, (0, 0), Refusal::Unreadable, None),
        };
        let (size, mtime) = stamp(&meta);
        if counted_unchanged(&self.conn, &rel, size, mtime)? {
            self.count_bests(|r| r.skipped_unchanged += 1);
            return Ok(Flow::Go);
        }
        if refused_unchanged(&self.conn, &rel, size, mtime)? {
            self.count_bests(|r| r.skipped_refused += 1);
            return Ok(Flow::Go);
        }
        let decoded = match read_capped(path, DecodeLimits::STANDARD.max_compressed) {
            Ok(bytes) => guarded(|| {
                #[cfg(test)]
                self.options.test.bests(super::tests::Step::Decode(path));
                let record =
                    replay::decode(&bytes, &DecodeLimits::STANDARD).map_err(|_| Refusal::Decode)?;
                let id = PlayId::of(rel.clone(), &record, size, mtime);
                Ok((record, id))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Refusal::Missing),
            Err(_) => Err(Refusal::Unreadable),
        };
        let (record, id) = match decoded {
            Ok(decoded) => decoded,
            Err(reason) => return self.refuse(rel, (size, mtime), reason, None),
        };
        if is_counted(&self.conn, &id.path, &id.body_sha256)? {
            self.count_bests(|r| r.already += 1);
            self.push_item(Item::Restamp(id))?;
            return Ok(Flow::Go);
        }
        match self.verify(path, &record, id, chart)? {
            (Flow::Stop, _) => Ok(Flow::Stop),
            (Flow::Go, Outcome::Item(item)) => {
                self.push_item(item)?;
                Ok(Flow::Go)
            }
            (Flow::Go, Outcome::Skipped) => Ok(Flow::Go),
            (Flow::Go, Outcome::Refused(reason)) => {
                let waits = matches!(
                    reason,
                    Refusal::ChartNotInLibrary | Refusal::ChartUnreadable
                )
                .then_some(record.chart.identity.sha256);
                self.refuse(rel, (size, mtime), reason, waits)
            }
        }
    }

    /// Counts a refusal, storing it when it is terminal or waits for a
    /// chart.
    fn refuse(
        &mut self,
        path: String,
        (size, mtime_ns): (i64, i64),
        reason: Refusal,
        waits_for: Option<[u8; 32]>,
    ) -> rusqlite::Result<Flow> {
        self.count_bests(|r| r.refused.count(reason));
        if reason.terminal() || waits_for.is_some() {
            self.push_item(Item::Refused {
                path,
                size,
                mtime_ns,
                reason,
                waits_for,
            })?;
        }
        Ok(Flow::Go)
    }

    /// Checks, loads, admits and evaluates a decoded record: its
    /// contribution, or why not.
    fn verify(
        &mut self,
        path: &Path,
        record: &PlayRecord,
        id: PlayId,
        chart: &mut Option<ChartFile>,
    ) -> rusqlite::Result<(Flow, Outcome)> {
        macro_rules! step {
            () => {
                if let Flow::Stop = self.step()? {
                    return Ok((Flow::Stop, Outcome::Skipped));
                }
            };
        }
        macro_rules! or_refuse {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(reason) => return Ok((Flow::Go, Outcome::Refused(reason))),
                }
            };
        }
        step!();
        let preset = or_refuse!(guarded(|| {
            verify::check_record(record).map_err(|e| Refusal::of(&e))
        }));
        step!();
        let sha = record.chart.identity.sha256;
        if chart.as_ref().is_none_or(|c| c.sha256 != sha) {
            let file = match chart_path(&self.conn, &sha)? {
                None => Err(Refusal::ChartNotInLibrary),
                Some(p) => {
                    let p = PathBuf::from(p);
                    let cap = Limits::default().max_file_bytes;
                    match read_capped(&p, cap) {
                        Ok(bytes) if bytes.len() <= cap => Ok((p, bytes)),
                        _ => Err(Refusal::ChartUnreadable),
                    }
                }
            };
            *chart = Some(ChartFile { sha256: sha, file });
        }
        let (chart_file, bytes) = match chart.as_ref().map(|c| &c.file) {
            Some(Ok((p, bytes))) => (p.clone(), bytes),
            Some(Err(reason)) => return Ok((Flow::Go, Outcome::Refused(*reason))),
            None => return Ok((Flow::Go, Outcome::Refused(Refusal::ChartUnreadable))),
        };
        step!();
        #[cfg(test)]
        self.options.test.bests(super::tests::Step::Load(path));
        #[cfg(not(test))]
        let _ = path;
        let dir = chart_file.parent().unwrap_or_else(|| Path::new("."));
        let loaded = or_refuse!(guarded(|| {
            verify::load_recorded_chart(record, bytes, dir).map_err(|e| Refusal::of(&e))
        }));
        step!();
        let budget = self.options.rebuild_memory.unwrap_or(REBUILD_MEMORY);
        or_refuse!(guarded(|| {
            verify::admit(record, &loaded, &preset, budget).map_err(|e| Refusal::of(&e))
        }));
        step!();
        #[cfg(test)]
        self.options.test.bests(super::tests::Step::Evaluate(path));
        let contribution = or_refuse!(guarded(|| {
            let re = verify::evaluate_record(record, &loaded, None).map_err(|e| Refusal::of(&e))?;
            let verified = Verified {
                completed: re.evaluation.completed,
                random: !loaded.identity.random_results.is_empty(),
            };
            Ok(Contribution::of(
                id,
                record,
                &verified,
                &re.evaluation,
                &preset,
            ))
        }));
        Ok((
            Flow::Go,
            Outcome::Item(Item::Counted(Box::new(contribution))),
        ))
    }
}

/// The record files (`*.tprl`) of a chart folder, by name, and whether it
/// listed to its end.
fn record_files(folder: &Path, options: &super::ScanOptions) -> (Vec<PathBuf>, bool) {
    #[cfg(test)]
    if options.test.fault(folder) == Some(super::tests::Fault::Listing) {
        return (Vec::new(), false);
    }
    #[cfg(not(test))]
    let _ = options;
    let Ok(entries) = fs::read_dir(folder) else {
        return (Vec::new(), false);
    };
    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return (files, false);
        };
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tprl") {
            files.push(path);
        }
    }
    files.sort();
    (files, true)
}

#[cfg(test)]
mod tests;
