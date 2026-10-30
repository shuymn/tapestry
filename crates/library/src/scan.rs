//! The background scan (design.md "Loading and Song Selection").
//!
//! The writer thread coordinates. It hands the walker one folder at a time
//! to list, takes the listing, hands each changed chart file to one of
//! [`PARSERS`] parser threads, and commits their rows in batches of
//! [`BATCH`]. Once every folder is listed it removes the rows of files that
//! are gone. The walker and the parsers only do what the writer hands them,
//! so the writer always knows what is outstanding.
//!
//! **Pausing.** [`Scan::request_pause`] stops the writer from handing out
//! work. Once no listing or file is outstanding and the batch is committed,
//! the writer acknowledges, which [`Scan::wait_paused`] reports. Nothing is
//! handed out, so nothing is read or committed, until [`Scan::release`].
//!
//! **Endings.** A scan ends when every folder is listed and every changed
//! file committed, when its writer fails, or when the [`Scan`] is dropped.
//! In each case the writer stops handing out work, waits until the walker
//! and every parser have ended, and only then reports the scan finished.
//! Every queue is bounded, and every thread is joined before
//! [`Scan::finish`] or the drop returns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use bms_parser::{Header, Limits, read_header};
use md5::Md5;
use rusqlite::Connection;
use sha2::{Digest as _, Sha256};

use crate::ScanLease;
use crate::db::{self, Prepared, Row};

/// Parser threads.
pub const PARSERS: usize = 2;
/// Rows and removals per commit.
pub const BATCH: usize = 256;
/// Bytes kept of each header value; a longer value is cut at a character
/// boundary at or below it (design.md "Loading and Song Selection").
pub const HEADER_FIELD_BYTES: usize = 1024;
/// Files handed to the parsers and not yet answered, at most: one for each,
/// in its hand or queued for it.
const OUTSTANDING: usize = PARSERS;
/// Changed files waiting to be handed out before the walker lists another
/// folder.
const PENDING: usize = BATCH;
/// The writer's queue: every outstanding answer, the walker's listing, each
/// thread's end and a wake.
const INBOX: usize = OUTSTANDING + PARSERS + 3;

/// A scan thread, as [`ScanOptions::on_thread_start`] is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Worker {
    /// The thread listing folders.
    Walker,
    /// A thread reading chart files, by index below [`PARSERS`].
    Parser(usize),
    /// The thread owning the writer connection and coordinating the others.
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
    /// Seams for the unit tests.
    #[cfg(test)]
    pub(crate) test: tests::Hooks,
}

impl fmt::Debug for ScanOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanOptions")
            .field("on_thread_start", &self.on_thread_start.is_some())
            .finish_non_exhaustive()
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
    /// Chart files not read: over the loader's size cap, refused by the
    /// file system, changed while being read, or with a path that is not
    /// Unicode. Their rows stay as they were.
    pub skipped: u64,
    /// Rows removed: their file is gone, or their root is no longer one of
    /// the roots. Rows under a folder that did not list completely stay.
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

/// A changed chart file for a parser.
struct Job {
    root: i64,
    path: String,
}

/// What a folder held, as far as it could be listed.
#[derive(Debug, Default)]
struct Listing {
    folders: Vec<PathBuf>,
    /// Chart files with their size and modification time.
    charts: Vec<(PathBuf, i64, i64)>,
    /// Entries whose type or metadata could not be read: their rows, and
    /// any row below them, stay.
    unread: Vec<PathBuf>,
    /// Chart files among [`unread`](Self::unread).
    unread_charts: u64,
    /// The folder listed to its end.
    complete: bool,
}

/// A message for the writer.
enum Msg {
    /// The walker listed the folder it was handed.
    Listed(Listing),
    /// A parser read a file.
    Parsed(Box<Row>),
    /// A parser could not read a file.
    Skipped,
    /// A walker or parser thread ended.
    Done,
    /// Look at the state again.
    Wake,
}

#[derive(Debug, Default)]
struct State {
    /// A pause is requested and not released.
    pause: bool,
    /// Counts pause requests, so each is acknowledged on its own.
    pause_gen: u64,
    /// The latest request the writer acknowledged, with nothing outstanding.
    acked_gen: u64,
    /// The scan is being dropped.
    shutdown: bool,
    /// Every thread but the writer has ended, and the writer is ending.
    finished: bool,
    report: ScanReport,
    error: Option<ScanError>,
}

impl State {
    fn request_pause(&mut self) {
        self.pause = true;
        self.pause_gen += 1;
    }

    fn release(&mut self) {
        self.pause = false;
    }

    fn paused(&self) -> bool {
        (self.pause && self.acked_gen == self.pause_gen) || self.finished
    }
}

/// Why the writer stopped short.
enum Stop {
    /// The writer connection refused a statement.
    Database(rusqlite::Error),
    /// A walker or parser ended, or a channel closed, while the scan still
    /// needed it.
    Worker,
}

impl From<rusqlite::Error> for Stop {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e)
    }
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

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }
}

/// Tells the writer a walker or parser thread ended, however it ends.
struct ExitGuard(SyncSender<Msg>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::Done);
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

/// Starts the threads of a scan of `roots` writing through `conn`. `lease`
/// is released when the writer thread ends, or with this call if it fails.
pub fn start(
    conn: Connection,
    roots: Vec<PathBuf>,
    options: ScanOptions,
    lease: ScanLease,
) -> io::Result<Scan> {
    let shared = Arc::new(Shared::default());
    // Declared before the channel ends, so a failed spawn drops them first
    // and then `scan`, whose drop joins the threads already started.
    let mut scan = Scan {
        shared: Arc::clone(&shared),
        wake: None,
        threads: Vec::new(),
    };
    let (inbox, inbox_rx) = mpsc::sync_channel(INBOX);
    let (folders, folders_rx) = mpsc::sync_channel::<PathBuf>(1);
    let (jobs, jobs_rx) = mpsc::sync_channel::<Job>(OUTSTANDING);
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    scan.wake = Some(inbox.clone());
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
        let options = options.clone();
        Box::new(move || {
            write(
                &shared,
                Writer::new(&shared, conn, inbox_rx, folders, jobs, options),
                &roots,
                lease,
            );
        })
    };
    scan.threads
        .push(spawn("library-writer".to_owned(), Worker::Writer, writer)?);

    let walker = {
        let guard = ExitGuard(inbox.clone());
        let options = options.clone();
        Box::new(move || {
            while let Ok(folder) = folders_rx.recv() {
                let listing = list(&folder, &options);
                if guard.0.send(Msg::Listed(listing)).is_err() {
                    return;
                }
            }
        })
    };
    scan.threads
        .push(spawn("library-walker".to_owned(), Worker::Walker, walker)?);

    for i in 0..PARSERS {
        let guard = ExitGuard(inbox.clone());
        let jobs = Arc::clone(&jobs_rx);
        let options = options.clone();
        let parser = Box::new(move || parse(&guard, &jobs, &options));
        scan.threads.push(spawn(
            format!("library-parser-{i}"),
            Worker::Parser(i),
            parser,
        )?);
    }
    Ok(scan)
}

impl Scan {
    /// Asks the scan to pause: the writer hands out no more work, the
    /// parsers finish the files in hand, and the writer commits them.
    /// Returns at once; [`wait_paused`](Self::wait_paused) waits for the
    /// acknowledgment.
    pub fn request_pause(&self) {
        self.shared.update(State::request_pause);
        self.wake();
    }

    /// Waits up to `timeout` for the latest pause request to be
    /// acknowledged. `true` once no scan thread is working: paused with
    /// nothing outstanding, or finished.
    #[must_use]
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, State::paused)
    }

    /// Releases a pause; the scan goes on.
    pub fn release(&self) {
        self.shared.update(State::release);
        self.wake();
    }

    /// Waits up to `timeout` for the scan to end. `true` once the walker
    /// and every parser have ended.
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

    /// Wakes the writer if it waits for a message.
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

/// Size and modification time, as stored.
fn stamp(meta: &Metadata) -> (i64, i64) {
    (
        i64::try_from(meta.len()).unwrap_or(i64::MAX),
        meta.modified().map_or(0, mtime_ns),
    )
}

/// Lists a folder: its subfolders, not following links, and its chart files
/// with their size and modification time. An entry whose type or metadata
/// cannot be read is kept in [`Listing::unread`]; a failure partway leaves
/// the listing incomplete.
fn list(dir: &Path, options: &ScanOptions) -> Listing {
    let mut listing = Listing::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return listing;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return listing;
        };
        let path = entry.path();
        let kind = entry.file_type().ok();
        #[cfg(test)]
        let kind = match options.test.fault(&path) {
            Some(tests::Fault::Listing) => return listing,
            Some(tests::Fault::Metadata) => None,
            None => kind,
        };
        #[cfg(not(test))]
        let _ = options;
        match kind {
            Some(kind) if kind.is_dir() => listing.folders.push(path),
            Some(_) if !is_chart(&path) => {}
            Some(_) => match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    let (size, mtime) = stamp(&meta);
                    listing.charts.push((path, size, mtime));
                }
                Ok(_) => {}
                Err(_) => {
                    listing.unread_charts += 1;
                    listing.unread.push(path);
                }
            },
            None => {
                listing.unread_charts += u64::from(is_chart(&path));
                listing.unread.push(path);
            }
        }
    }
    listing.complete = true;
    listing
}

/// Cuts a header value to [`HEADER_FIELD_BYTES`] at a character boundary,
/// into a new string, so the long one's allocation is not kept.
fn cap(value: &mut Option<String>) {
    if let Some(text) = value
        && text.len() > HEADER_FIELD_BYTES
    {
        let end = text.floor_char_boundary(HEADER_FIELD_BYTES);
        *text = text[..end].to_owned();
    }
}

/// Reads a chart file: its size and time as it is opened, at most the
/// loader's cap and one byte more, then its size and time again. `None`
/// when it cannot be read, is over the cap, or changed while being read.
fn read_chart(job: Job, limits: &Limits, options: &ScanOptions) -> Option<Row> {
    let mut file = File::open(&job.path).ok()?;
    let before = stamp(&file.metadata().ok()?);
    let cap_bytes = u64::try_from(limits.max_file_bytes).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    (&mut file)
        .take(cap_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    #[cfg(test)]
    options.test.after_read(Path::new(&job.path));
    #[cfg(not(test))]
    let _ = options;
    if stamp(&file.metadata().ok()?) != before {
        return None;
    }
    let mut header = read_header(&bytes, limits).ok()?;
    let Header {
        title,
        subtitle,
        artist,
        subartist,
        genre,
        playlevel,
        difficulty,
        bpm,
    } = &mut header;
    for field in [
        title, subtitle, artist, subartist, genre, playlevel, difficulty, bpm,
    ] {
        cap(field);
    }
    Some(Row {
        root: job.root,
        path: job.path,
        size: before.0,
        mtime_ns: before.1,
        sha256: Sha256::digest(&bytes).into(),
        md5: Md5::digest(&bytes).into(),
        header,
    })
}

/// A parser: reads each file the writer hands it.
fn parse(guard: &ExitGuard, jobs: &Mutex<Receiver<Job>>, options: &ScanOptions) {
    let limits = Limits::default();
    loop {
        let job = jobs.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok(job) = job else { return };
        #[cfg(test)]
        options.test.in_hand(Path::new(&job.path));
        let msg = read_chart(job, &limits, options)
            .map_or(Msg::Skipped, |row| Msg::Parsed(Box::new(row)));
        if guard.0.send(msg).is_err() {
            return;
        }
    }
}

/// The writer: the walk's state, what is outstanding, and the batch.
struct Writer<'a> {
    shared: &'a Shared,
    conn: Connection,
    rx: Receiver<Msg>,
    /// Closed when the writer stops handing out work.
    folders_tx: Option<SyncSender<PathBuf>>,
    jobs_tx: Option<SyncSender<Job>>,
    #[cfg_attr(not(test), expect(dead_code, reason = "test seams only"))]
    options: ScanOptions,
    /// Folders still to list, with their root.
    folders: Vec<(i64, PathBuf)>,
    /// The folder the walker is listing, with its root, if it is.
    listing: Option<(i64, PathBuf)>,
    /// Changed files not yet handed out.
    pending: VecDeque<Job>,
    /// Files handed out and not yet answered.
    outstanding: usize,
    /// What is indexed and not yet listed.
    known: HashMap<String, (i64, i64)>,
    seen: HashSet<String>,
    /// Entries whose rows stay, with every row below them.
    kept: Vec<PathBuf>,
    walked: bool,
    upserts: Vec<Row>,
    removals: Vec<String>,
    /// Walker and parser threads that have ended.
    ended: usize,
}

impl<'a> Writer<'a> {
    fn new(
        shared: &'a Shared,
        conn: Connection,
        rx: Receiver<Msg>,
        folders: SyncSender<PathBuf>,
        jobs: SyncSender<Job>,
        options: ScanOptions,
    ) -> Self {
        Self {
            shared,
            conn,
            rx,
            folders_tx: Some(folders),
            jobs_tx: Some(jobs),
            options,
            folders: Vec::new(),
            listing: None,
            pending: VecDeque::new(),
            outstanding: 0,
            known: HashMap::new(),
            seen: HashSet::new(),
            kept: Vec::new(),
            walked: false,
            upserts: Vec::new(),
            removals: Vec::new(),
            ended: 0,
        }
    }

    fn count(&self, f: impl FnOnce(&mut ScanReport)) {
        self.shared.update(|state| f(&mut state.report));
    }

    /// Adds a row or a removal; commits once [`BATCH`] are held.
    fn push(&mut self, item: Result<Row, String>) -> rusqlite::Result<()> {
        match item {
            Ok(row) => self.upserts.push(row),
            Err(path) => self.removals.push(path),
        }
        if self.upserts.len() + self.removals.len() >= BATCH {
            self.commit()?;
        }
        Ok(())
    }

    fn commit(&mut self) -> rusqlite::Result<()> {
        if self.upserts.is_empty() && self.removals.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        self.options
            .test
            .before_commit(self.upserts.len() + self.removals.len());
        let removed = db::commit(&mut self.conn, &self.upserts, &self.removals)?;
        self.upserts.clear();
        self.removals.clear();
        self.count(|report| report.removed += removed);
        Ok(())
    }

    /// Takes one message. A walker or parser that ends while the writer
    /// still hands out work is fatal.
    fn take(&mut self, msg: Msg) -> Result<(), Stop> {
        match msg {
            Msg::Listed(listing) => self.listed(listing),
            Msg::Parsed(row) => {
                self.outstanding = self.outstanding.saturating_sub(1);
                self.count(|report| report.read += 1);
                self.push(Ok(*row))?;
            }
            Msg::Skipped => {
                self.outstanding = self.outstanding.saturating_sub(1);
                self.count(|report| report.skipped += 1);
            }
            Msg::Done => {
                self.ended += 1;
                return Err(Stop::Worker);
            }
            Msg::Wake => {}
        }
        Ok(())
    }

    /// Takes the walker's listing of a folder of root `root`.
    fn listed(&mut self, listing: Listing) {
        let Some((root, folder)) = self.listing.take() else {
            return;
        };
        let mut found = 0;
        let mut unchanged = 0;
        let mut skipped = listing.unread_charts;
        for (path, size, mtime) in listing.charts {
            let Some(path) = path.to_str().map(str::to_owned) else {
                skipped += 1;
                continue;
            };
            if !self.seen.insert(path.clone()) {
                continue;
            }
            found += 1;
            if self.known.remove(&path) == Some((size, mtime)) {
                unchanged += 1;
            } else {
                self.pending.push_back(Job { root, path });
            }
        }
        self.folders
            .extend(listing.folders.into_iter().map(|folder| (root, folder)));
        self.kept.extend(listing.unread);
        if !listing.complete {
            self.kept.push(folder);
        }
        self.count(|report| {
            report.found += found;
            report.unchanged += unchanged;
            report.skipped += skipped;
        });
    }

    /// Hands out work: a folder to the walker while few files wait, and
    /// waiting files to the parsers up to [`OUTSTANDING`]. Once every
    /// folder is listed, queues the removals of the files gone. A closed
    /// channel means its threads ended, which is fatal.
    fn dispatch(&mut self) -> Result<(), Stop> {
        while self.outstanding < OUTSTANDING
            && let Some(job) = self.pending.pop_front()
        {
            let jobs = self.jobs_tx.as_ref().ok_or(Stop::Worker)?;
            match jobs.try_send(job) {
                Ok(()) => self.outstanding += 1,
                Err(TrySendError::Full(job)) => {
                    self.pending.push_front(job);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => return Err(Stop::Worker),
            }
        }
        if self.listing.is_none()
            && self.pending.len() < PENDING
            && let Some((root, folder)) = self.folders.pop()
        {
            let walker = self.folders_tx.as_ref().ok_or(Stop::Worker)?;
            match walker.try_send(folder.clone()) {
                Ok(()) => self.listing = Some((root, folder)),
                Err(TrySendError::Full(_)) => self.folders.push((root, folder)),
                Err(TrySendError::Disconnected(_)) => return Err(Stop::Worker),
            }
        }
        if !self.walked && self.listing.is_none() && self.folders.is_empty() {
            self.walked = true;
            let kept = std::mem::take(&mut self.kept);
            let gone: Vec<String> = std::mem::take(&mut self.known)
                .into_keys()
                .filter(|path| !kept.iter().any(|dir| Path::new(path).starts_with(dir)))
                .collect();
            for path in gone {
                self.push(Err(path))?;
            }
        }
        Ok(())
    }

    /// Nothing is handed out and unanswered.
    const fn idle(&self) -> bool {
        self.outstanding == 0 && self.listing.is_none()
    }

    /// Commits, then, while the pause holds, acknowledges every request
    /// made of it, since nothing is handed out meanwhile. `false` when the
    /// scan is being dropped.
    fn hold(&mut self) -> Result<bool, Stop> {
        self.commit()?;
        let mut state = self.shared.lock();
        loop {
            if state.shutdown {
                return Ok(false);
            }
            if !state.pause {
                return Ok(true);
            }
            if state.acked_gen != state.pause_gen {
                state.acked_gen = state.pause_gen;
                self.shared.changed.notify_all();
            }
            state = self.shared.wait(state);
        }
    }

    /// Runs the scan to its end, a drop, or a failure.
    fn run(&mut self, roots: &[PathBuf]) -> Result<(), Stop> {
        let Prepared {
            roots,
            known,
            removed,
        } = db::prepare(&mut self.conn, roots)?;
        self.count(|report| report.removed += removed);
        self.known = known;
        // Popped from the end: the first root is listed first.
        self.folders = roots.into_iter().rev().collect();
        loop {
            let (pause, shutdown) = {
                let state = self.shared.lock();
                (state.pause, state.shutdown)
            };
            if shutdown {
                return Ok(self.commit()?);
            }
            if !pause {
                self.dispatch()?;
            } else if self.idle() {
                if !self.hold()? {
                    return Ok(self.commit()?);
                }
                continue;
            }
            if self.walked && self.pending.is_empty() && self.idle() {
                return Ok(self.commit()?);
            }
            // The scan holds a sender until its drop, which sets
            // `shutdown` first.
            let msg = self.rx.recv().map_err(|_| Stop::Worker)?;
            self.take(msg)?;
        }
    }

    /// Stops handing out work and waits until the walker and every parser
    /// have ended, dropping what they still send.
    fn close(&mut self) {
        self.folders_tx = None;
        self.jobs_tx = None;
        while self.ended < PARSERS + 1 {
            match self.rx.recv() {
                Ok(Msg::Done) => self.ended += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

/// The writer thread: runs the scan, then ends the others before the scan
/// counts as finished.
fn write(shared: &Shared, mut writer: Writer<'_>, roots: &[PathBuf], lease: ScanLease) {
    let error = match writer.run(roots) {
        Ok(()) => None,
        Err(Stop::Database(e)) => Some(ScanError::Database(e)),
        // A drop also ends the walker and the parsers; only an end the
        // scan did not ask for is an error.
        Err(Stop::Worker) if shared.lock().shutdown => None,
        Err(Stop::Worker) => Some(ScanError::Panicked),
    };
    if error.is_some() {
        shared.update(|state| state.error = error);
    }
    writer.close();
    drop(writer);
    drop(lease);
    shared.update(|state| state.finished = true);
}

#[cfg(test)]
mod tests;
