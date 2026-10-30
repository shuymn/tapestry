//! The background scan (design.md "Loading and Song Selection"): a walker
//! lists the chart files, [`PARSERS`] parser threads read the ones whose size
//! or modification time changed, and a writer commits their rows in batches
//! of [`BATCH`] and removes the rows of files that are gone.
//!
//! **Pausing.** [`Scan::request_pause`] stops every thread from starting a
//! read; the parsers finish the file in hand, the writer commits everything
//! they handed it, and then [`Scan::wait_paused`] reports the pause. Nothing
//! is read or committed until [`Scan::release`].
//!
//! **Endings.** A scan ends when the walk is done and every changed file is
//! committed, when its writer fails (the others stop at their next send), or
//! when the [`Scan`] is dropped (each thread stops at its next step and the
//! writer commits what it holds). Every queue is bounded, and every thread is
//! joined before [`Scan::finish`] or the drop returns.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use bms_parser::{Limits, read_header};
use md5::Md5;
use rusqlite::Connection;
use sha2::{Digest as _, Sha256};

use crate::db::{self, Prepared, Row};

/// Parser threads.
pub const PARSERS: usize = 2;
/// Rows and removals per commit.
pub const BATCH: usize = 256;
/// Files listed ahead of the parsers.
const JOB_QUEUE: usize = 64;
/// Messages queued for the writer.
const RESULT_QUEUE: usize = BATCH;

/// A scan thread, as [`ScanOptions::on_thread_start`] is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Worker {
    /// The thread listing the chart files.
    Walker,
    /// A thread reading chart files, by index below [`PARSERS`].
    Parser(usize),
    /// The thread owning the writer connection.
    Writer,
}

/// Called first on each scan thread, before it does any work.
pub type ThreadStart = Arc<dyn Fn(Worker) + Send + Sync>;

/// How a scan runs.
#[derive(Clone, Default)]
pub struct ScanOptions {
    /// Run first on each scan thread, for example to put it in background
    /// mode. This crate holds no platform code, so the caller sets the
    /// priority.
    pub on_thread_start: Option<ThreadStart>,
}

impl fmt::Debug for ScanOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanOptions")
            .field("on_thread_start", &self.on_thread_start.is_some())
            .finish()
    }
}

/// What a scan did, so far or in all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Chart files listed under the roots.
    pub found: u64,
    /// Listed files whose size and modification time match their row, so
    /// they were not read.
    pub unchanged: u64,
    /// Files read and handed to the writer.
    pub read: u64,
    /// Files that could not be read: over the loader's size cap, refused by
    /// the file system, or with a path that is not Unicode.
    pub skipped: u64,
    /// Rows removed: their file is gone, or their root is no longer one of
    /// the roots. The rows under a folder that could not be listed stay.
    pub removed: u64,
}

/// Why a scan stopped short.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The writer connection refused a statement.
    #[error("the library scan cannot write the database")]
    Database(#[source] rusqlite::Error),
    /// A scan thread panicked.
    #[error("a library scan thread stopped unexpectedly")]
    Panicked,
}

/// The hook a unit test holds a parser in.
#[cfg(test)]
pub type InHand = Arc<dyn Fn(&Path) + Send + Sync>;

/// Seams for the unit tests; empty in every other build.
#[derive(Clone, Default)]
pub struct Hooks {
    /// Called by a parser holding a file it has read, before handing it on.
    #[cfg(test)]
    pub in_hand: Option<InHand>,
}

/// A changed chart file for a parser.
struct Job {
    root: i64,
    path: String,
}

/// A message for the writer.
enum Msg {
    /// A parser read a file.
    Parsed(usize, Box<Row>),
    /// A parser could not read a file.
    Skipped(usize),
    /// Rows whose files are gone.
    Gone(Vec<String>),
    /// A walker or parser thread ended.
    Done,
    /// Look at the state again.
    Wake,
}

#[derive(Debug, Default)]
struct State {
    /// A pause is requested and not released.
    pause: bool,
    /// The writer acknowledged the pause.
    acked: bool,
    /// The scan is being dropped.
    shutdown: bool,
    /// The writer has ended.
    finished: bool,
    /// The walker is reading a folder.
    walker_busy: bool,
    /// Each parser holds a file whose result the writer has not received.
    holding: [bool; PARSERS],
    report: ScanReport,
    error: Option<ScanError>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits while a pause holds, then marks `worker` as reading. `false`
    /// when the scan is being dropped.
    fn enter(&self, worker: Worker) -> bool {
        let mut state = self.lock();
        while state.pause && !state.shutdown {
            state = self.wait(state);
        }
        if state.shutdown {
            return false;
        }
        match worker {
            Worker::Walker => state.walker_busy = true,
            Worker::Parser(i) => {
                if let Some(held) = state.holding.get_mut(i) {
                    *held = true;
                }
            }
            Worker::Writer => {}
        }
        true
    }

    fn leave_walker(&self) {
        self.lock().walker_busy = false;
        self.changed.notify_all();
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }
}

/// Marks a walker or parser as idle and tells the writer it ended, however
/// its thread ends.
struct ExitGuard {
    shared: Arc<Shared>,
    worker: Worker,
    results: SyncSender<Msg>,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.shared.update(|state| match self.worker {
            Worker::Walker => state.walker_busy = false,
            Worker::Parser(i) => {
                if let Some(held) = state.holding.get_mut(i) {
                    *held = false;
                }
            }
            Worker::Writer => {}
        });
        let _ = self.results.send(Msg::Done);
    }
}

/// A running scan. Dropping it stops the scan and joins its threads.
pub struct Scan {
    shared: Arc<Shared>,
    wake: Option<SyncSender<Msg>>,
    threads: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Scan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scan")
            .field("state", &*self.shared.lock())
            .finish_non_exhaustive()
    }
}

/// Starts the threads of a scan of `roots` writing through `conn`.
pub fn start(
    conn: Connection,
    roots: Vec<PathBuf>,
    options: ScanOptions,
    hooks: Hooks,
) -> io::Result<Scan> {
    let shared = Arc::new(Shared::default());
    // Declared before the channel ends, so a failed spawn drops them first
    // and then `scan`, whose drop joins the threads already started.
    let mut scan = Scan {
        shared: Arc::clone(&shared),
        wake: None,
        threads: Vec::new(),
    };
    let (results, results_rx) = mpsc::sync_channel(RESULT_QUEUE);
    let (setup, setup_rx) = mpsc::sync_channel(1);
    let (jobs, jobs_rx) = mpsc::sync_channel(JOB_QUEUE);
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    scan.wake = Some(results.clone());
    let spawn = |name: String, worker: Worker, body: Box<dyn FnOnce() + Send>| {
        let hook = options.on_thread_start.clone();
        thread::Builder::new().name(name).spawn(move || {
            if let Some(hook) = hook {
                hook(worker);
            }
            body();
        })
    };

    let writer = {
        let shared = Arc::clone(&shared);
        Box::new(move || write(&shared, conn, &roots, setup, &results_rx))
    };
    scan.threads
        .push(spawn("library-writer".to_owned(), Worker::Writer, writer)?);

    let walker = {
        let guard = ExitGuard {
            shared: Arc::clone(&shared),
            worker: Worker::Walker,
            results: results.clone(),
        };
        Box::new(move || {
            if let Ok(prepared) = setup_rx.recv() {
                walk(&guard, prepared, &jobs);
            }
        })
    };
    scan.threads
        .push(spawn("library-walker".to_owned(), Worker::Walker, walker)?);

    for i in 0..PARSERS {
        let guard = ExitGuard {
            shared: Arc::clone(&shared),
            worker: Worker::Parser(i),
            results: results.clone(),
        };
        let jobs = Arc::clone(&jobs_rx);
        let hooks = hooks.clone();
        let parser = Box::new(move || parse(&guard, i, &jobs, &hooks));
        scan.threads.push(spawn(
            format!("library-parser-{i}"),
            Worker::Parser(i),
            parser,
        )?);
    }
    Ok(scan)
}

impl Scan {
    /// Asks the scan to pause: no thread starts another read, the parsers
    /// finish the file in hand and the writer commits it. Returns at once;
    /// [`wait_paused`](Self::wait_paused) waits for the acknowledgment.
    pub fn request_pause(&self) {
        self.shared.update(|state| state.pause = true);
        self.wake();
    }

    /// Waits up to `timeout` for the pause to be acknowledged. `true` once
    /// the scan holds still: paused, or ended.
    #[must_use]
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, |state| {
            (state.pause && state.acked) || state.finished
        })
    }

    /// Releases a pause; the scan goes on.
    pub fn release(&self) {
        self.shared.update(|state| {
            state.pause = false;
            state.acked = false;
        });
    }

    /// Waits up to `timeout` for the scan to end. `true` once it has.
    #[must_use]
    pub fn wait_finished(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, |state| state.finished)
    }

    /// What the scan has done so far.
    #[must_use]
    pub fn report(&self) -> ScanReport {
        self.shared.lock().report
    }

    /// Releases any pause, waits for the scan to end and joins its threads.
    ///
    /// # Errors
    ///
    /// [`ScanError::Database`] when the writer failed, and
    /// [`ScanError::Panicked`] when a thread panicked. What was committed
    /// before stays.
    pub fn finish(mut self) -> Result<ScanReport, ScanError> {
        self.release();
        let panicked = self.join();
        let mut state = self.shared.lock();
        match state.error.take() {
            Some(e) => Err(e),
            None if panicked => Err(ScanError::Panicked),
            None => Ok(state.report),
        }
    }

    fn wait_for(&self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if done(&state) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Wakes the writer if it waits for a message. A full queue means it
    /// has messages to take, after which it looks at the state anyway.
    fn wake(&self) {
        if let Some(wake) = &self.wake {
            // Full: the writer has messages to take. Disconnected: it ended.
            let _ = wake.try_send(Msg::Wake);
        }
    }

    /// Joins every thread; `true` if one panicked.
    fn join(&mut self) -> bool {
        self.wake = None;
        let mut panicked = false;
        for thread in self.threads.drain(..) {
            panicked |= thread.join().is_err();
        }
        panicked
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.shared.update(|state| state.shutdown = true);
        self.wake();
        self.join();
    }
}

/// Whether `path` names a BMS chart file by its extension.
fn is_chart(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        ["bms", "bme", "bml"]
            .iter()
            .any(|c| ext.eq_ignore_ascii_case(c))
    })
}

/// Nanoseconds since the Unix epoch, negative before it, saturating.
fn mtime_ns(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |ns| -ns),
    }
}

/// An entry of a listed folder.
enum Entry {
    Folder(PathBuf),
    Chart(PathBuf, i64, i64),
}

/// Lists a folder: its subfolders, not following links, and its chart files
/// with their size and modification time. An entry that cannot be read is
/// left out, so it counts as gone only once its folder lists without it.
fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            entries.push(Entry::Folder(path));
        } else if is_chart(&path)
            && let Ok(meta) = fs::metadata(&path)
            && meta.is_file()
        {
            let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
            let mtime = meta.modified().map_or(0, mtime_ns);
            entries.push(Entry::Chart(path, size, mtime));
        }
    }
    Ok(entries)
}

/// The walker: lists every root, hands the changed files to the parsers, and
/// then the writer the rows whose files are gone.
fn walk(guard: &ExitGuard, prepared: Prepared, jobs: &SyncSender<Job>) {
    let shared = &guard.shared;
    let Prepared {
        roots, mut known, ..
    } = prepared;
    let mut seen = HashSet::new();
    let mut unlisted = Vec::new();
    for (root, root_path) in roots {
        let mut folders = vec![root_path];
        while let Some(folder) = folders.pop() {
            if !shared.enter(Worker::Walker) {
                return;
            }
            let listing = list(&folder);
            shared.leave_walker();
            let Ok(entries) = listing else {
                unlisted.push(folder);
                continue;
            };
            for entry in entries {
                let (path, size, mtime) = match entry {
                    Entry::Folder(path) => {
                        folders.push(path);
                        continue;
                    }
                    Entry::Chart(path, size, mtime) => (path, size, mtime),
                };
                let Some(path) = path.to_str().map(str::to_owned) else {
                    shared.update(|state| state.report.skipped += 1);
                    continue;
                };
                if !seen.insert(path.clone()) {
                    continue;
                }
                let unchanged = known.remove(&path) == Some((size, mtime));
                shared.update(|state| {
                    state.report.found += 1;
                    if unchanged {
                        state.report.unchanged += 1;
                    }
                });
                if !unchanged && jobs.send(Job { root, path }).is_err() {
                    return;
                }
            }
        }
    }
    let mut gone: Vec<String> = known
        .into_keys()
        .filter(|path| !unlisted.iter().any(|dir| Path::new(path).starts_with(dir)))
        .collect();
    while !gone.is_empty() {
        let chunk = gone.split_off(gone.len().saturating_sub(BATCH));
        if guard.results.send(Msg::Gone(chunk)).is_err() {
            return;
        }
    }
}

/// Reads a chart file: its size and time as it is opened, then at most the
/// loader's cap and one byte more.
fn read_chart(root: i64, path: String, limits: &Limits) -> Option<Row> {
    let file = File::open(&path).ok()?;
    let meta = file.metadata().ok()?;
    let cap = u64::try_from(limits.max_file_bytes).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let header = read_header(&bytes, limits).ok()?;
    Some(Row {
        root,
        path,
        size: i64::try_from(meta.len()).unwrap_or(i64::MAX),
        mtime_ns: meta.modified().map_or(0, mtime_ns),
        sha256: Sha256::digest(&bytes).into(),
        md5: Md5::digest(&bytes).into(),
        header,
    })
}

/// A parser: reads each changed file it is handed.
fn parse(guard: &ExitGuard, index: usize, jobs: &Mutex<Receiver<Job>>, hooks: &Hooks) {
    let limits = Limits::default();
    loop {
        let job = jobs.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(job) = job else { return };
        if !guard.shared.enter(Worker::Parser(index)) {
            return;
        }
        let row = read_chart(job.root, job.path, &limits);
        #[cfg(test)]
        if let (Some(in_hand), Some(row)) = (&hooks.in_hand, &row) {
            in_hand(Path::new(&row.path));
        }
        #[cfg(not(test))]
        let _ = hooks;
        let msg = row.map_or(Msg::Skipped(index), |row| Msg::Parsed(index, Box::new(row)));
        if guard.results.send(msg).is_err() {
            return;
        }
    }
}

/// What the writer holds for its next commit.
#[derive(Default)]
struct Batch {
    upserts: Vec<Row>,
    removals: Vec<String>,
}

impl Batch {
    fn len(&self) -> usize {
        self.upserts.len() + self.removals.len()
    }
}

/// The writer's side of the scan.
struct Writer<'a> {
    shared: &'a Shared,
    conn: Connection,
    rx: &'a Receiver<Msg>,
    batch: Batch,
    /// Walker and parser threads that have ended.
    ended: usize,
}

impl Writer<'_> {
    /// Takes one message; commits once the batch is full.
    fn take(&mut self, msg: Msg) -> rusqlite::Result<()> {
        match msg {
            Msg::Parsed(i, row) => {
                self.batch.upserts.push(*row);
                self.received(i, |report| report.read += 1);
            }
            Msg::Skipped(i) => self.received(i, |report| report.skipped += 1),
            Msg::Gone(paths) => self.batch.removals.extend(paths),
            Msg::Done => self.ended += 1,
            Msg::Wake => {}
        }
        if self.batch.len() >= BATCH {
            self.commit()?;
        }
        Ok(())
    }

    fn received(&self, parser: usize, count: impl FnOnce(&mut ScanReport)) {
        self.shared.update(|state| {
            if let Some(held) = state.holding.get_mut(parser) {
                *held = false;
            }
            count(&mut state.report);
        });
    }

    fn commit(&mut self) -> rusqlite::Result<()> {
        if self.batch.len() == 0 {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let removed = db::commit(&mut self.conn, &batch.upserts, &batch.removals)?;
        self.shared.update(|state| state.report.removed += removed);
        Ok(())
    }

    /// Takes every result of the files in hand, commits, acknowledges the
    /// pause and waits for its release. `false` when the scan is being
    /// dropped.
    fn pause(&mut self) -> rusqlite::Result<bool> {
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.take(msg)?;
            }
            let mut state = self.shared.lock();
            if state.shutdown {
                return Ok(false);
            }
            if state.holding.iter().any(|&held| held) {
                // A parser holding a file sends its result next, or ends.
                drop(state);
                match self.rx.recv() {
                    Ok(msg) => self.take(msg)?,
                    Err(_) => return Ok(false),
                }
                continue;
            }
            if state.walker_busy {
                // The walker only reads the file system; it sends nothing
                // while it is busy.
                while state.walker_busy && !state.shutdown {
                    state = self.shared.wait(state);
                }
                continue;
            }
            break;
        }
        self.commit()?;
        let mut state = self.shared.lock();
        if state.pause {
            state.acked = true;
            self.shared.changed.notify_all();
        }
        while state.pause && !state.shutdown {
            state = self.shared.wait(state);
        }
        state.acked = false;
        Ok(!state.shutdown)
    }

    /// Takes messages until every walker and parser thread has ended.
    fn run(&mut self) -> rusqlite::Result<()> {
        while self.ended < PARSERS + 1 {
            let (pause, shutdown) = {
                let state = self.shared.lock();
                (state.pause, state.shutdown)
            };
            if shutdown {
                break;
            }
            if pause {
                if !self.pause()? {
                    break;
                }
                continue;
            }
            match self.rx.recv() {
                Ok(msg) => self.take(msg)?,
                Err(_) => break,
            }
        }
        self.commit()
    }
}

/// The writer thread: prepares the roots, hands the walker what is indexed,
/// and commits what the parsers read.
fn write(
    shared: &Shared,
    mut conn: Connection,
    roots: &[PathBuf],
    setup: SyncSender<Prepared>,
    rx: &Receiver<Msg>,
) {
    let result = (|| {
        let mut prepared = db::prepare(&mut conn, roots)?;
        let removed = std::mem::take(&mut prepared.removed);
        shared.update(|state| state.report.removed += removed);
        let _ = setup.send(prepared);
        drop(setup);
        Writer {
            shared,
            conn,
            rx,
            batch: Batch::default(),
            ended: 0,
        }
        .run()
    })();
    shared.update(|state| {
        state.finished = true;
        if let Err(e) = result {
            state.error = Some(ScanError::Database(e));
        }
    });
}

#[cfg(test)]
mod tests;
