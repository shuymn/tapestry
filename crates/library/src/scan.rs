//! The background scan (design.md "Loading and Song Selection"): one thread
//! that lists each folder, reads and hashes each changed chart file, reads
//! its header and commits the rows in batches of [`BATCH`], in turn. Once
//! every folder is listed it removes the rows of files that are gone.
//!
//! **Pausing.** The thread looks for a pause before each folder, each file
//! and each removal of a gone file's row. On one it commits its batch, acknowledges the latest request and
//! waits until the release or the drop. A file in hand is finished first.
//!
//! **Endings.** Whatever ends the thread, an error, a panic in the start
//! hook or anywhere after it, or the drop, a guard publishes the end and
//! wakes every waiter; [`Scan::finish`] or the drop joins the thread.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use bms_parser::{Header, Limits, read_header};
use md5::Md5;
use rusqlite::Connection;
use sha2::{Digest as _, Sha256};

use crate::ScanLease;
use crate::db::{self, Prepared, Row};

/// Rows and removals per commit.
pub const BATCH: usize = 256;
/// Bytes kept of each header value; a longer value is cut at a character
/// boundary at or below it (design.md "Loading and Song Selection").
pub const HEADER_FIELD_BYTES: usize = 1024;

/// Called first on the scan thread, before it does any work.
pub type ThreadStart = Arc<dyn Fn() + Send + Sync>;

/// How a scan runs.
#[derive(Clone, Default)]
pub struct ScanOptions {
    /// Run first on the scan thread, for example to put it in background
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
    /// Files read and added to the batch.
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
    /// The scan thread panicked.
    #[error("the library scan stopped unexpectedly")]
    Panicked,
}

#[derive(Debug, Default)]
struct State {
    /// A pause is requested and not released.
    pause: bool,
    /// Counts pause requests, so each is acknowledged on its own.
    pause_gen: u64,
    /// The latest request the thread acknowledged.
    acked_gen: u64,
    /// The scan is being dropped.
    shutdown: bool,
    /// The thread has ended, or is ending and will do nothing more.
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

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }
}

/// Publishes the end of the scan thread however it ends, a panic included,
/// after releasing the lease, and wakes every waiter.
struct Ending {
    shared: Arc<Shared>,
    lease: Option<ScanLease>,
}

impl Drop for Ending {
    fn drop(&mut self) {
        drop(self.lease.take());
        let panicked = thread::panicking();
        self.shared.update(|state| {
            if panicked && state.error.is_none() {
                state.error = Some(ScanError::Panicked);
            }
            state.finished = true;
        });
    }
}

/// A running scan. Dropping it stops the scan and joins its thread;
/// [`Scan::stop_within`] bounds that wait.
pub struct Scan {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Scan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scan")
            .field("state", &*self.shared.lock())
            .finish_non_exhaustive()
    }
}

/// Starts the thread of a scan of `roots` writing through `conn`. `lease`
/// is released when the thread ends, or with this call if it fails.
pub fn start(
    conn: Connection,
    roots: Vec<PathBuf>,
    options: ScanOptions,
    lease: ScanLease,
) -> io::Result<Scan> {
    let shared = Arc::new(Shared::default());
    let ending = Ending {
        shared: Arc::clone(&shared),
        lease: Some(lease),
    };
    let thread = thread::Builder::new()
        .name("library-scan".to_owned())
        .spawn(move || {
            // `ending` drops last, after the connection, and sees any panic.
            let (ending, conn, roots, options) = (ending, conn, roots, options);
            if let Some(hook) = &options.on_thread_start {
                hook();
            }
            let result = Scanner::new(&ending.shared, conn, options).run(&roots);
            if let Err(e) = result {
                ending
                    .shared
                    .update(|state| state.error = Some(ScanError::Database(e)));
            }
        })?;
    Ok(Scan {
        shared,
        thread: Some(thread),
    })
}

impl Scan {
    /// Asks the scan to pause: it finishes the file in hand, commits and
    /// acknowledges. Returns at once; [`wait_paused`](Self::wait_paused)
    /// waits for the acknowledgment.
    pub fn request_pause(&self) {
        self.shared.update(State::request_pause);
    }

    /// Waits up to `timeout` for the latest pause request to be
    /// acknowledged. `true` once the scan does nothing: paused, or
    /// finished.
    #[must_use]
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, State::paused)
    }

    /// Releases a pause; the scan goes on.
    pub fn release(&self) {
        self.shared.update(State::release);
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

    /// Releases any pause, waits for the scan to end and joins its thread.
    ///
    /// # Errors
    ///
    /// [`ScanError::Database`] when a statement failed, and
    /// [`ScanError::Panicked`] when the thread panicked. What was committed
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

    /// Stops the scan, as dropping it does, but waits at most `timeout` for
    /// its work to end, never for its thread: `true` once the work ended
    /// (the thread is left to exit on its own), `false` when it had not by
    /// then, and the thread is left to finish it (a read stalled on its file
    /// system). Until the work ends the thread keeps its connection and the
    /// lock beside the database, so the library stays refused to any other
    /// opening until then, or until the process exits.
    pub fn stop_within(mut self, timeout: Duration) -> bool {
        self.shared.update(|state| state.shutdown = true);
        let finished = self.wait_finished(timeout);
        // Detached either way: once the scan's work is done, what is left
        // of the thread (its thread-local destructors) may still block, and
        // the drop finds no thread to join.
        drop(self.thread.take());
        finished
    }

    /// Joins the thread; `true` if it panicked.
    fn join(&mut self) -> bool {
        self.thread.take().is_some_and(|t| t.join().is_err())
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.shared.update(|state| state.shutdown = true);
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

/// Reads a chart file of root `root`: its size and time as it is opened, at
/// most the loader's cap and one byte more, then its size and time again.
/// `None` when it cannot be read, is over the cap, or changed while being
/// read.
fn read_chart(root: i64, path: String, limits: &Limits, options: &ScanOptions) -> Option<Row> {
    let mut file = File::open(&path).ok()?;
    let before = stamp(&file.metadata().ok()?);
    let cap_bytes = u64::try_from(limits.max_file_bytes).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    (&mut file)
        .take(cap_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    #[cfg(test)]
    options.test.after_read(Path::new(&path));
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
        root,
        path,
        size: before.0,
        mtime_ns: before.1,
        sha256: Sha256::digest(&bytes).into(),
        md5: Md5::digest(&bytes).into(),
        header,
    })
}

/// The scan thread's work: the walk's state and the batch.
struct Scanner<'a> {
    shared: &'a Shared,
    conn: Connection,
    options: ScanOptions,
    limits: Limits,
    /// What is indexed and not yet listed.
    known: HashMap<String, (i64, i64)>,
    seen: HashSet<String>,
    /// Entries whose rows stay, with every row below them.
    kept: Vec<PathBuf>,
    upserts: Vec<Row>,
    removals: Vec<String>,
}

impl<'a> Scanner<'a> {
    fn new(shared: &'a Shared, conn: Connection, options: ScanOptions) -> Self {
        Self {
            shared,
            conn,
            options,
            limits: Limits::default(),
            known: HashMap::new(),
            seen: HashSet::new(),
            kept: Vec::new(),
            upserts: Vec::new(),
            removals: Vec::new(),
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

    /// Between two folders or files: on a pause, commits, acknowledges every
    /// request made of it and waits for the release. `false` when the scan
    /// is being dropped.
    fn checkpoint(&mut self) -> rusqlite::Result<bool> {
        {
            let state = self.shared.lock();
            if state.shutdown {
                return Ok(false);
            }
            if !state.pause {
                return Ok(true);
            }
        }
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
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Takes a folder's listing: queues its subfolders and returns its
    /// changed chart files.
    fn listed(
        &mut self,
        root: i64,
        folder: PathBuf,
        listing: Listing,
        folders: &mut Vec<(i64, PathBuf)>,
    ) -> Vec<String> {
        let mut changed = Vec::new();
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
                changed.push(path);
            }
        }
        folders.extend(listing.folders.into_iter().map(|f| (root, f)));
        self.kept.extend(listing.unread);
        if !listing.complete {
            self.kept.push(folder);
        }
        self.count(|report| {
            report.found += found;
            report.unchanged += unchanged;
            report.skipped += skipped;
        });
        changed
    }

    /// Runs the scan to its end or the drop, committing what it holds.
    fn run(mut self, roots: &[PathBuf]) -> rusqlite::Result<()> {
        let Prepared {
            roots,
            known,
            removed,
        } = db::prepare(&mut self.conn, roots)?;
        self.count(|report| report.removed += removed);
        self.known = known;
        // Popped from the end: the first root is listed first.
        let mut folders: Vec<(i64, PathBuf)> = roots.into_iter().rev().collect();
        while let Some((root, folder)) = folders.pop() {
            if !self.checkpoint()? {
                return self.commit();
            }
            let listing = list(&folder, &self.options);
            for path in self.listed(root, folder, listing, &mut folders) {
                if !self.checkpoint()? {
                    return self.commit();
                }
                #[cfg(test)]
                self.options.test.in_hand(Path::new(&path));
                match read_chart(root, path, &self.limits, &self.options) {
                    Some(row) => {
                        self.count(|report| report.read += 1);
                        self.push(Ok(row))?;
                    }
                    None => self.count(|report| report.skipped += 1),
                }
            }
        }
        let kept = std::mem::take(&mut self.kept);
        let gone: Vec<String> = std::mem::take(&mut self.known)
            .into_keys()
            .filter(|path| !kept.iter().any(|dir| Path::new(path).starts_with(dir)))
            .collect();
        for path in gone {
            if !self.checkpoint()? {
                return self.commit();
            }
            self.push(Err(path))?;
        }
        self.commit()
    }
}

#[cfg(test)]
mod tests;
