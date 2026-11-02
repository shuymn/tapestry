//! The library on the game thread (design.md "Loading and Song
//! Selection"): `library.db` under the data root, kept current by scan
//! passes in the background while the select screen shows, and paused from
//! the choice of a chart until READY of each play is over.
//!
//! - **One process per data root.** The [`Library`] holds its lock on
//!   `library.db.lock` for the whole run; a second player process on the
//!   same data root is refused by that lock, the first thing it takes,
//!   before it reads the settings, opens a window or a connection to the
//!   database ([`open`]).
//! - **Passes.** A pass starts when the run starts, and again once the last
//!   one has been over for [`RESCAN_AFTER`] while nothing pauses the scan:
//!   a chart added while the game runs is listed without a restart. Only
//!   files whose size or modification time changed are read again.
//! - **Pausing.** Choosing a chart requests a pause, and each READY state
//!   waits until the scan acknowledges it ([`Shelf::wait_paused`]); every
//!   entry into selection and every result screen releases it.
//! - **Priority.** The scan thread runs in background mode (lowered CPU and
//!   I/O priority), set by its start hook; when the system refuses it, at
//!   the lowest CPU priority, the refusal reported once on the console.
//! - **Loudness.** The shelf also holds the loaded chart's loudness
//!   ([`Loudness`]): a miss is measured during the chart's load, and its
//!   measurements are stored only while the scan is quiet. A load
//!   cancelled during its measurement leaves the chart to the background
//!   measurement ([`Worker`]), which pauses and is released with the scan;
//!   its results are kept and stored as a load's own.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audio::{Gain, SoundBank};
use bms_model::Chart;
use game::{Chain, DataRoot};
use library::{
    BestWriter, ChartEntry, Library, LibraryError, Opened, Scan, ScanOptions, ScanReport, Score,
    Search,
};

use crate::background::{Job, Queued, Worker};
use crate::loudness::{Host, Loudness};

/// How often a load's wait for its chart's background measurement looks
/// whether it was cancelled: a measurement asks about as often, before
/// each block.
pub const BACKGROUND_SLICE: Duration = Duration::from_millis(10);

/// How long the last pass is over before the next starts, at the select
/// screen.
pub const RESCAN_AFTER: Duration = Duration::from_secs(10);
/// How long the end of a run waits for a running pass to stop.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a READY state's wait for the scan's pause looks whether it
/// was cancelled.
pub const PAUSE_SLICE: Duration = Duration::from_millis(100);
/// How long the wait for the pause runs before the select screen says so:
/// the file in hand (at most the loader's 16 MiB cap) and the batch's
/// commit take far less on a local disk.
pub const PAUSE_NOTICE: Duration = Duration::from_secs(2);

/// The database file under the data root.
pub fn database(root: &DataRoot) -> PathBuf {
    root.path().join("library.db")
}

/// Why the library cannot be opened: another process holds it, or the
/// library refused.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Another player process runs on this data root.
    #[error(
        "another tapestry is running on the data folder {}; close it first",
        root.display()
    )]
    Running {
        /// The data root.
        root: PathBuf,
    },
    /// The data folder cannot be created.
    #[error("the data folder cannot be created: {}", root.display())]
    Folder {
        /// The data root.
        root: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The library refused.
    #[error(transparent)]
    Library(#[from] LibraryError),
}

/// Opens the library of the data root: the first thing a run with song
/// selection does.
///
/// # Errors
///
/// [`OpenError::Running`] while another process holds the library of this
/// data root; otherwise why the library cannot be opened.
pub fn open(root: &DataRoot) -> Result<(Library, Opened), OpenError> {
    std::fs::create_dir_all(root.path()).map_err(|source| OpenError::Folder {
        root: root.path().to_owned(),
        source,
    })?;
    let (library, opened) = Library::open(&database(root)).map_err(|e| match e {
        LibraryError::AlreadyOpen { .. } => OpenError::Running {
            root: root.path().to_owned(),
        },
        e => OpenError::Library(e),
    })?;
    // Each launch catches the bests up with the records, so plays saved
    // without a library (`tapestry play`) are counted (design.md
    // "Personal Bests"). A request that cannot be written is held by the
    // library for its next pass.
    if let Err(e) = library.request_bests_pass() {
        let line = format!(
            "library: a bests catch-up cannot be requested: {}",
            Chain(&e)
        );
        eprintln!("{line}");
        log::warn!("{line}");
    }
    Ok((library, opened))
}

/// What a READY state's wait for the pause found: the scan's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paused {
    /// The scan acknowledged the pause.
    Acknowledged,
    /// No scan runs.
    Idle,
    /// The wait was cancelled: the window is closing, or the player left
    /// the chart. No READY state follows.
    Cancelled,
}

/// The library and its scan passes.
pub struct Shelf {
    library: Library,
    roots: Vec<PathBuf>,
    options: ScanOptions,
    scan: Option<Scan>,
    /// A pause is requested and not released: no pass starts.
    paused: bool,
    /// When the last pass ended.
    ended: Option<Instant>,
    rescan_after: Duration,
    /// The running pass's progress last seen, in batches.
    seen: u64,
    /// The latest pass's outcome, for the select screen.
    status: String,
    /// The loaded chart's loudness.
    loudness: Loudness,
    /// The background measurement of charts whose load was cancelled
    /// during theirs; none when its thread could not start.
    background: Option<Worker>,
    /// Passes completed (dev-tools).
    #[cfg(feature = "dev-tools")]
    passes: u64,
    /// When the running pass started (dev-tools).
    #[cfg(feature = "dev-tools")]
    pass_started: Option<Instant>,
}

impl Shelf {
    /// Keeps `library` current over `roots`, and its bests over the
    /// records in `records`, if given, starting the first pass.
    pub fn new(
        library: Library,
        roots: Vec<PathBuf>,
        rescan_after: Duration,
        records: Option<PathBuf>,
    ) -> Self {
        let options = ScanOptions {
            on_thread_start: Some(Arc::new(background_mode)),
            records,
            ..ScanOptions::default()
        };
        // In background mode as the scan.
        let hook = options.on_thread_start.clone();
        Self::with_worker(library, roots, rescan_after, options, || {
            Worker::start(Box::new(crate::background::measure), hook)
        })
    }

    /// [`new`](Self::new), each pass's thread starting with `options`, the
    /// background measurement's with nothing.
    #[cfg(test)]
    pub(crate) fn with_options(
        library: Library,
        roots: Vec<PathBuf>,
        rescan_after: Duration,
        options: ScanOptions,
    ) -> Self {
        Self::with_worker(library, roots, rescan_after, options, || {
            Worker::start(Box::new(crate::background::measure), None)
        })
    }

    /// [`with_options`](Self::with_options), the background measurement
    /// started by `start`: a shelf whose background cannot start leaves a
    /// cancelled load's chart to its next choice.
    fn with_worker(
        library: Library,
        roots: Vec<PathBuf>,
        rescan_after: Duration,
        options: ScanOptions,
        start: impl FnOnce() -> std::io::Result<Worker>,
    ) -> Self {
        let background = match start() {
            Ok(worker) => Some(worker),
            Err(e) => {
                let line = format!("loudness: the background measurement cannot start: {e}");
                eprintln!("{line}");
                log::warn!("{line}");
                None
            }
        };
        let mut shelf = Self {
            library,
            roots,
            options,
            scan: None,
            paused: false,
            ended: None,
            rescan_after,
            seen: 0,
            status: String::new(),
            loudness: Loudness::default(),
            background,
            #[cfg(feature = "dev-tools")]
            passes: 0,
            #[cfg(feature = "dev-tools")]
            pass_started: None,
        };
        shelf.start_pass();
        shelf
    }

    /// What the library is doing, for the select screen: scanning, or
    /// rebuilding the bests with the plays counted so far.
    pub fn status(&self) -> Cow<'_, str> {
        match &self.scan {
            Some(scan) => {
                let bests = scan.report().bests;
                if bests.running {
                    Cow::Owned(format!("ベスト再構築中 {}", bests.counted))
                } else {
                    Cow::Borrowed("スキャン中")
                }
            }
            None => Cow::Borrowed(&self.status),
        }
    }

    /// Every chart's score under the preset of SHA-256 `preset`.
    ///
    /// # Errors
    ///
    /// Why the database refused the query.
    pub fn bests(&self, preset: &[u8; 32]) -> Result<HashMap<[u8; 32], Score>, LibraryError> {
        self.library.bests(preset, library::LN_MODE)
    }

    /// The connection that counts live plays.
    ///
    /// # Errors
    ///
    /// Why it cannot be opened.
    pub fn best_writer(&self) -> Result<BestWriter, LibraryError> {
        self.library.best_writer()
    }

    /// The charts `search` finds.
    ///
    /// # Errors
    ///
    /// Why the database refused the query.
    pub fn search(&self, search: &Search<'_>) -> Result<Vec<ChartEntry>, LibraryError> {
        self.library.search(search)
    }

    fn start_pass(&mut self) {
        if self.roots.is_empty() {
            self.status = "device.toml に [library] roots がありません".to_owned();
            return;
        }
        match self.library.scan(&self.roots, self.options.clone()) {
            Ok(scan) => {
                self.scan = Some(scan);
                self.seen = 0;
                #[cfg(feature = "dev-tools")]
                {
                    println!("dev: scan: pass {} started", self.passes + 1);
                    self.pass_started = Some(Instant::now());
                }
            }
            Err(e) => {
                let line = format!("library: the scan cannot start: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
                self.status = "スキャンできません".to_owned();
                self.ended = Some(Instant::now());
            }
        }
    }

    /// Looks at the scan at `now`: a pass's end is reported and logged, the
    /// next starts when it is due. `true` when the listed charts may have
    /// changed: a batch was committed or a pass ended.
    pub fn poll(&mut self, now: Instant) -> bool {
        self.adopt();
        if let Some(scan) = &self.scan {
            if !scan.wait_finished(Duration::ZERO) {
                let r = scan.report();
                // Chart batches, and the bests phase's commits.
                let batches = (r.read + r.removed) / library::BATCH as u64 + r.bests.commits;
                let changed = batches != self.seen;
                self.seen = batches;
                #[cfg(feature = "dev-tools")]
                if changed {
                    println!(
                        "dev: scan: pass {} committed {} read, {} removed",
                        self.passes + 1,
                        r.read,
                        r.removed
                    );
                }
                return changed;
            }
            if let Some(scan) = self.scan.take() {
                self.finished(scan.finish());
            }
            self.ended = Some(now);
            return true;
        }
        let due = self
            .ended
            .is_none_or(|t| now.saturating_duration_since(t) >= self.rescan_after);
        if !self.paused && due && !self.roots.is_empty() {
            self.start_pass();
        }
        false
    }

    fn finished(&mut self, done: Result<ScanReport, library::ScanError>) {
        #[cfg(feature = "dev-tools")]
        if let Some(started) = self.pass_started.take() {
            let outcome = match &done {
                Ok(r) => format!("{} found, {} read", r.found, r.read),
                Err(_) => "failed".to_owned(),
            };
            println!(
                "dev: scan: pass {} ended after {} ms: {outcome}",
                self.passes + 1,
                started.elapsed().as_millis()
            );
        }
        match done {
            Ok(r) => {
                // Logged only when it found something to do.
                if r.read + r.removed + r.skipped > 0 {
                    let line = format!(
                        "library: {} charts found, {} read, {} unchanged, {} skipped, {} removed",
                        r.found, r.read, r.unchanged, r.skipped, r.removed
                    );
                    println!("{line}");
                    log::info!("{line}");
                }
                let b = r.bests;
                if b.catch_up || b.retried > 0 {
                    let refused = b.refused.to_string();
                    let line = format!(
                        "library: bests: {} plays counted, {} counted before, {} unchanged, \
                         {} refused before, {} retried, {} refused ({}), listing {}",
                        b.counted,
                        b.already,
                        b.skipped_unchanged,
                        b.skipped_refused,
                        b.retried,
                        b.refused.total(),
                        if refused.is_empty() { "none" } else { &refused },
                        if b.enumeration_complete || !b.catch_up {
                            "complete"
                        } else {
                            "incomplete"
                        },
                    );
                    println!("{line}");
                    log::info!("{line}");
                }
                self.status = String::new();
                #[cfg(feature = "dev-tools")]
                {
                    self.passes += 1;
                }
            }
            Err(e) => {
                let line = format!("library: the scan failed: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
                self.status = "スキャンに失敗しました".to_owned();
            }
        }
    }

    /// The scan pauses after the file in hand, the background measurement
    /// at its next check, and no pass starts until the release.
    pub fn pause(&mut self) {
        self.request_pause(None);
    }

    /// `chart` was chosen: [`pause`](Self::pause), except that a
    /// background measurement of `chart` goes on, since its load waits for
    /// it ([`wait_background`](Self::wait_background)).
    pub fn choose(&mut self, chart: &Path) {
        self.request_pause(Some(chart));
    }

    fn request_pause(&mut self, chosen: Option<&Path>) {
        if !self.paused {
            self.paused = true;
            if let Some(scan) = &self.scan {
                scan.request_pause();
            }
            if let Some(worker) = &self.background {
                worker.request_pause(chosen);
            }
        }
    }

    /// Before a READY state: requests the pause if none is, and waits for the
    /// scan and the background measurement to acknowledge it, however long
    /// that takes (design.md "Loading and Song Selection": READY
    /// starts only after it); without a scan it waits for the background
    /// measurement alone. Every `slice` it asks `cancelled` whether to give
    /// up; once the wait has run `notice_after`, it calls `notice` once.
    pub fn wait_paused(
        &mut self,
        slice: Duration,
        mut cancelled: impl FnMut() -> bool,
        notice_after: Duration,
        notice: impl FnOnce(),
    ) -> Paused {
        self.pause();
        let started = Instant::now();
        let mut notice = Some(notice);
        loop {
            let scan = match &self.scan {
                None => Some(Paused::Idle),
                Some(scan) if scan.wait_paused(slice) => Some(Paused::Acknowledged),
                Some(_) => None,
            };
            if let Some(paused) = scan
                && self
                    .background
                    .as_ref()
                    .is_none_or(|w| w.wait_paused(slice))
            {
                return paused;
            }
            if cancelled() {
                return Paused::Cancelled;
            }
            if started.elapsed() >= notice_after
                && let Some(notice) = notice.take()
            {
                notice();
            }
        }
    }

    /// Selection was entered, or a result shown: the scan goes on, and
    /// passes start again when due.
    pub fn release(&mut self) {
        self.paused = false;
        if let Some(scan) = &self.scan {
            scan.release();
        }
        if let Some(worker) = &self.background {
            worker.release();
        }
    }

    /// The chart `name` was loaded with `bank`: its plays' gain is looked
    /// up, or measured now with `host` ([`Loudness::load`]). A job of the
    /// chart (its file `path`) waiting in the background is this load's
    /// own now. [`game::Loading::Cancelled`] when the load was cancelled
    /// during the measurement, which the quit key then leaves to the
    /// background ([`leave_to_background`](Self::leave_to_background)).
    pub fn loaded(
        &mut self,
        chart: &Chart,
        bank: &Arc<SoundBank>,
        name: &str,
        path: &Path,
        host: &mut dyn Host,
    ) -> game::Loading {
        self.adopt();
        if let Some(worker) = &self.background
            && worker.take_waiting(path)
        {
            println!("loudness: {name} is no longer waiting in the background");
        }
        self.loudness.load(&self.library, chart, bank, name, host)
    }

    /// A load the quit key cancelled during its measurement: `job` is
    /// measured in the background, unless the thread is gone. A close
    /// leaves nothing to it, since the run ends.
    pub fn leave_to_background(&self, job: Job) {
        let Some(worker) = &self.background else {
            return;
        };
        let name = job.name.clone();
        let line = match worker.queue(job) {
            Queued::Added => format!("loudness: {name} is left to the background"),
            Queued::Already => return,
            Queued::Dropped(old) => format!(
                "loudness: {name} is left to the background, and {} is dropped from it",
                file_name(&old)
            ),
            Queued::Ended => {
                format!("loudness: {name} cannot be left to the background, which has ended")
            }
        };
        println!("{line}");
        log::info!("{line}");
    }

    /// Before its sounds decode, the load of `chart` (`name` in the log)
    /// waits for the chart's background measurement if one runs, saying
    /// so with `measuring`; the result is then kept, so the load finds it.
    /// A measurement of another chart is waited for until it has stopped
    /// for the choice's pause and dropped its bank, so this load's bank,
    /// the engine's and the background's are never held together. Every
    /// [`BACKGROUND_SLICE`] it asks `cancelled` whether to give up:
    /// [`game::Loading::Cancelled`] then, and the measurement goes on.
    pub fn wait_background(
        &mut self,
        chart: &Path,
        name: &str,
        mut cancelled: impl FnMut() -> bool,
        measuring: impl FnOnce(),
    ) -> game::Loading {
        // Requested at the choice; here again for a load without one.
        self.choose(chart);
        let Some(worker) = &self.background else {
            return game::Loading::Ready;
        };
        if !worker.holds(chart) {
            while !worker.wait_paused(BACKGROUND_SLICE) {
                if cancelled() {
                    return game::Loading::Cancelled;
                }
            }
            return game::Loading::Ready;
        }
        measuring();
        let line = format!("loudness: waiting for {name}'s measurement in the background");
        println!("{line}");
        log::info!("{line}");
        while !worker.wait_done(chart, BACKGROUND_SLICE) {
            if cancelled() {
                return game::Loading::Cancelled;
            }
        }
        self.adopt();
        game::Loading::Ready
    }

    /// Keeps the background's measurements in memory, those still wanted.
    fn adopt(&mut self) {
        if let Some(worker) = &self.background {
            let results = worker.take_results();
            self.keep(results);
        }
    }

    /// Keeps the measurements of `results` still wanted.
    fn keep(&mut self, results: Vec<crate::background::Finished>) {
        for (key, row) in results.into_iter().filter_map(|f| f.measured) {
            self.loudness.adopt(&self.library, key, row);
        }
    }

    /// Before a READY state, once [`wait_paused`](Self::wait_paused) found
    /// the scan quiet: stores the measurements taken, the background's
    /// among them.
    pub fn store_loudness(&mut self) {
        self.adopt();
        self.loudness.store(&mut self.library);
    }

    /// Whether the background measurement holds a job, and whether it has
    /// none at all, held or waiting (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn background(&self) -> (bool, bool) {
        self.background.as_ref().map_or((false, true), Worker::busy)
    }

    /// The gain of the loaded chart's plays.
    pub const fn gain(&self) -> Gain {
        self.loudness.gain()
    }

    /// How many passes have completed (dev-tools): what a search lists is
    /// the whole library's once one has.
    #[cfg(feature = "dev-tools")]
    pub const fn passes(&self) -> u64 {
        self.passes
    }

    /// Whether a pause is requested and not released.
    #[cfg(test)]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    /// Whether a pass runs (dev-tools): a script's wait for a listing
    /// stands still meanwhile.
    #[cfg(any(test, feature = "dev-tools"))]
    pub const fn has_scan(&self) -> bool {
        self.scan.is_some()
    }
}

impl Drop for Shelf {
    /// Stops the running pass and the background measurement, waiting at
    /// most [`STOP_TIMEOUT`] for both together: a pass stuck on its file
    /// system is left to end on its own, keeping the library's lock until
    /// it does or the process exits, and so is a stuck measurement, whose
    /// result nobody takes. The measurements the background completed are
    /// stored with the load's.
    fn drop(&mut self) {
        let background = self.background.take();
        // Both stop at once, and share one deadline.
        let deadline = Instant::now() + STOP_TIMEOUT;
        if let Some(worker) = &background {
            worker.stop();
        }
        let (quiet, stopped) = stop_together(
            deadline,
            Instant::now,
            self.scan
                .take()
                .map(|scan| move |left| scan.stop_within(left)),
            background.map(|worker| move |left| worker.stop_within(left)),
        );
        if !quiet {
            let line = format!(
                "library: the scan did not stop within {} s; it ends with the process",
                STOP_TIMEOUT.as_secs()
            );
            eprintln!("{line}");
            log::warn!("{line}");
        }
        if let Some((ended, results)) = stopped {
            if !ended {
                let line = format!(
                    "loudness: the background measurement did not stop within {} s; it ends with the process",
                    STOP_TIMEOUT.as_secs()
                );
                eprintln!("{line}");
                log::warn!("{line}");
            }
            self.keep(results);
        }
        // A measurement is stored only when no scan may still commit.
        self.loudness.close(&mut self.library, quiet);
    }
}

/// Waits for the scan, then the background measurement, both already told
/// to stop, until `deadline` on the clock `now`: each wait gets only the
/// time left, so the two share one bound. Whether the scan stopped (or
/// none ran), and what the background's wait gave.
fn stop_together<W>(
    deadline: Instant,
    now: impl Fn() -> Instant,
    scan: Option<impl FnOnce(Duration) -> bool>,
    background: Option<impl FnOnce(Duration) -> W>,
) -> (bool, Option<W>) {
    let quiet = scan.is_none_or(|stop| stop(deadline.saturating_duration_since(now())));
    let stopped = background.map(|stop| stop(deadline.saturating_duration_since(now())));
    (quiet, stopped)
}

/// The scan thread's start hook: background mode, which lowers its CPU,
/// I/O and memory priority (design.md "Coexisting with Other Software").
/// When the system refuses it, the thread falls back to the lowest CPU
/// priority, and the first refusal of the run is reported on the console.
fn background_mode() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY_LOWEST,
    };
    static REPORTED: AtomicBool = AtomicBool::new(false);
    // SAFETY: the pseudo handle of the calling thread, always valid.
    let Err(e) = (unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) })
    else {
        return;
    };
    // SAFETY: as above.
    let lowest = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
    if !REPORTED.swap(true, Ordering::Relaxed) {
        match lowest {
            Ok(()) => {
                eprintln!("library: background mode refused ({e}); lowest CPU priority instead")
            }
            Err(l) => {
                eprintln!("library: background mode refused ({e}); lowest CPU priority too ({l})")
            }
        }
    }
}

/// The file name of `path`, for console lines.
pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use game::DataRoot;
    use library::{Search, Sort};

    use super::{OpenError, PAUSE_SLICE, Paused, Shelf, open};

    fn chart(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let text = format!("#TITLE {name}\n#BPM 120\n#00111:01\n");
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn listed(shelf: &Shelf) -> Vec<String> {
        let all = Search {
            text: "",
            sort: Sort::Title,
            by_folder: false,
            limit: 100,
        };
        shelf
            .search(&all)
            .unwrap()
            .into_iter()
            .map(|e| super::file_name(&e.path))
            .collect()
    }

    /// The wait a READY state makes, never cancelled.
    fn wait(shelf: &mut Shelf) -> Paused {
        shelf.wait_paused(PAUSE_SLICE, || false, Duration::MAX, || {})
    }

    /// How long a test waits for something that must happen however late a
    /// loaded machine runs it: it only catches what never happens.
    const WATCHDOG: Duration = Duration::from_secs(600);

    /// Polls until the running pass has ended. The pass runs in background
    /// mode, which a loaded machine can starve for seconds.
    fn pass_ends(shelf: &mut Shelf) {
        let begun = Instant::now();
        while shelf.has_scan() {
            assert!(begun.elapsed() < WATCHDOG, "the pass never ended");
            shelf.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn holds(shelf: &Shelf, name: &str) -> bool {
        listed(shelf).iter().any(|n| n == name)
    }

    /// While a chart's choice pauses the scan no pass starts, however long
    /// the select screen waits; once released, a chart added meanwhile is
    /// listed by the next pass.
    #[test]
    fn a_paused_shelf_starts_no_pass_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let charts = dir.path().join("charts");
        chart(&charts, "refused.bms");
        let (library, _) = open(&root).unwrap();
        let mut shelf = Shelf::new(
            library,
            vec![charts.clone()],
            Duration::from_millis(20),
            None,
        );
        pass_ends(&mut shelf);
        assert!(holds(&shelf, "refused.bms"));
        // A chart is chosen.
        shelf.pause();
        assert_ne!(wait(&mut shelf), Paused::Cancelled);
        chart(&charts, "added.bms");
        // Paused: no pass starts however long selection waits, though one
        // is due every 20 ms.
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            shelf.poll(Instant::now());
            assert!(!shelf.has_scan(), "a pass started while paused");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!holds(&shelf, "added.bms"));
        shelf.release();
        // Due long since: the next poll starts the pass.
        shelf.poll(Instant::now());
        assert!(shelf.has_scan());
        pass_ends(&mut shelf);
        assert!(holds(&shelf, "added.bms"));
    }

    /// The pause a READY state waits for is acknowledged by a running pass
    /// before the wait returns, and requested by the wait itself when no
    /// choice did.
    #[test]
    fn the_pause_is_acknowledged_before_each_ready_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let charts = dir.path().join("charts");
        for k in 0..600 {
            chart(&charts.join(format!("f{}", k / 50)), &format!("{k}.bms"));
        }
        let (library, _) = open(&root).unwrap();
        let mut shelf = Shelf::new(library, vec![charts], Duration::from_secs(60), None);
        assert!(shelf.has_scan());
        assert_eq!(wait(&mut shelf), Paused::Acknowledged);
        assert!(shelf.paused());
        // While paused nothing more is committed.
        let before = listed(&shelf).len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(listed(&shelf).len(), before);
        shelf.release();
        // A second wait, as a retry's, is acknowledged again.
        assert_ne!(wait(&mut shelf), Paused::Cancelled);
        shelf.release();
    }

    /// Each launch's opening requests a bests catch-up, which the first
    /// pass completes: a play saved by a run without a library is counted
    /// at the next launch.
    #[test]
    fn each_launch_requests_a_bests_catch_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let charts = dir.path().join("charts");
        chart(&charts, "a.bms");
        let launch = |root: &DataRoot| {
            let (library, _) = open(root).unwrap();
            assert!(library.bests_pending().unwrap(), "no catch-up requested");
            let mut shelf = Shelf::new(
                library,
                vec![charts.clone()],
                Duration::from_secs(60),
                Some(root.records()),
            );
            pass_ends(&mut shelf);
            assert!(!shelf.library.bests_pending().unwrap());
        };
        launch(&root);
        // The second launch finds the database current, its last catch-up
        // complete.
        launch(&root);
    }

    /// The library lock refuses a second opening of the data root with an
    /// error naming the folder.
    #[test]
    fn a_second_opening_names_the_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let _first = open(&root).unwrap();
        let e = open(&root).unwrap_err();
        assert!(matches!(e, OpenError::Running { .. }), "{e:?}");
        assert!(e.to_string().contains(&root.path().display().to_string()));
    }

    /// A gate the scan thread waits at before any work, so the pass cannot
    /// acknowledge a pause until the test opens it.
    type Gate = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

    fn gated(dir: &Path) -> (Shelf, Gate) {
        gated_with(dir, || {
            Worker::start(Box::new(crate::background::measure), None)
        })
    }

    /// [`gated`], the background started by `start`.
    fn gated_with(dir: &Path, start: impl FnOnce() -> std::io::Result<Worker>) -> (Shelf, Gate) {
        let root = DataRoot::new(dir.join("data"));
        let charts = dir.join("charts");
        chart(&charts, "a.bms");
        let (library, _) = open(&root).unwrap();
        let gate: Gate = std::sync::Arc::default();
        let held = std::sync::Arc::clone(&gate);
        let options = library::ScanOptions {
            on_thread_start: Some(std::sync::Arc::new(move || {
                let (open, changed) = &*held;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = changed.wait(open).unwrap();
                }
            })),
            ..library::ScanOptions::default()
        };
        let shelf = Shelf::with_worker(
            library,
            vec![charts],
            Duration::from_secs(60),
            options,
            start,
        );
        (shelf, gate)
    }

    fn open_gate(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    /// READY's wait lasts until the scan acknowledges, however many
    /// slices that takes: it never gives up on its own. Past its notice
    /// time it says so once.
    #[test]
    fn ready_waits_for_the_acknowledgment() {
        let dir = tempfile::tempdir().unwrap();
        let (mut shelf, gate) = gated(dir.path());
        let opener = std::sync::Arc::clone(&gate);
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            open_gate(&opener);
        });
        let mut notices = 0;
        let paused = shelf.wait_paused(
            Duration::from_millis(20),
            || false,
            Duration::from_millis(100),
            || notices += 1,
        );
        assert_eq!(paused, Paused::Acknowledged);
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert_eq!(notices, 1);
        thread.join().unwrap();
        shelf.release();
    }

    /// Closing the window, or the quit key, cancels the wait: no READY state
    /// follows, and the pause stays requested.
    #[test]
    fn closing_cancels_the_wait_for_the_pause() {
        let dir = tempfile::tempdir().unwrap();
        let (mut shelf, gate) = gated(dir.path());
        let mut asked = 0;
        let paused = shelf.wait_paused(
            Duration::from_millis(20),
            || {
                asked += 1;
                asked == 5
            },
            Duration::MAX,
            || {},
        );
        assert_eq!(paused, Paused::Cancelled);
        assert_eq!(asked, 5);
        assert!(shelf.paused());
        // The scan thread ends once it can.
        open_gate(&gate);
    }

    /// The end of a run whose pass is stuck (its gate never opens) is
    /// bounded: the shelf drops after its stop bound without waiting for
    /// the pass, and the stuck pass keeps the library refused to another
    /// opening until it ends.
    #[test]
    fn a_stuck_pass_bounds_the_shutdown_and_keeps_the_library_locked() {
        let dir = tempfile::tempdir().unwrap();
        let (shelf, gate) = gated(dir.path());
        let (dropped_tx, dropped) = std::sync::mpsc::channel();
        let dropping = std::thread::spawn(move || {
            let started = Instant::now();
            drop(shelf);
            let _ = dropped_tx.send(started.elapsed());
        });
        // The gate stays shut until the drop returns, so the pass cannot
        // end first: only the stop bound ends the drop, which never waits
        // for the pass.
        let Ok(took) = dropped.recv_timeout(WATCHDOG) else {
            // Let the pass and the drop end before failing.
            open_gate(&gate);
            let _ = dropping.join();
            panic!("the drop waited for the stuck pass");
        };
        assert!(took >= super::STOP_TIMEOUT, "{took:?}");
        let root = DataRoot::new(dir.path().join("data"));
        assert!(matches!(open(&root), Err(OpenError::Running { .. })));
        open_gate(&gate);
        // However long the machine takes to run the released pass to its
        // end.
        let begun = Instant::now();
        while open(&root).is_err() {
            assert!(begun.elapsed() < WATCHDOG, "the lock stayed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::background::{Job, Queued, Worker};
    use crate::loudness::fixture::{self as sound, Host, bank, square, write_sound};

    /// A shelf without roots (no scan).
    fn unscanned(dir: &Path) -> Shelf {
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        Shelf::with_options(
            library,
            Vec::new(),
            Duration::from_secs(60),
            library::ScanOptions::default(),
        )
    }

    /// The background job of the chart at `path`.
    fn job(path: &Path) -> Job {
        Job {
            source: game::ChartSource::new(path.to_owned(), 0, [0; 32]),
            rate: sound::RATE,
            name: "a.bms".to_owned(),
        }
    }

    /// Loads a chart of a two-second sound into `shelf`, measured during
    /// the load; its key.
    fn load(shelf: &mut Shelf, dir: &Path) -> library::LoudnessKey {
        let songs = dir.join("song");
        write_sound(&songs, &square(2.0, 3_000));
        let c = sound::chart(&songs, 1);
        let b = bank(&c);
        let key = crate::loudness::key(&c, &b).unwrap();
        let mut host = Host::default();
        shelf.loaded(&c, &b, "a.bms", &songs.join("a.bms"), &mut host);
        assert_eq!(host.measuring, 1);
        key
    }

    /// The row a fake background measurement gives.
    const ROW: library::LoudnessRow = library::LoudnessRow {
        integrated: Some(-30.0),
        true_peak: 0.25,
        over_range: false,
    };

    /// A background measurement as the real one is to the shelf: block
    /// after block of 10 ms, asking `stop` before each and keeping nothing
    /// once told to; it gives [`ROW`] under `key` after `blocks`, and not
    /// before `open` is set.
    fn fake(
        key: library::LoudnessKey,
        blocks: usize,
        open: &Arc<std::sync::atomic::AtomicBool>,
    ) -> crate::background::Measure {
        let open = Arc::clone(open);
        Box::new(move |_, stop| {
            let mut n = 0;
            loop {
                if stop() {
                    return crate::background::Outcome::Stopped;
                }
                if n >= blocks && open.load(std::sync::atomic::Ordering::SeqCst) {
                    return crate::background::Outcome::Measured(key, ROW);
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    }

    /// A shelf without roots whose background measures with `measure`.
    fn with_background(dir: &Path, measure: crate::background::Measure) -> Shelf {
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        Shelf::with_worker(
            library,
            Vec::new(),
            Duration::from_secs(60),
            library::ScanOptions::default(),
            || Worker::start(measure, None),
        )
    }

    /// A chart of a two-second sound, its bank, key and file.
    fn song(
        dir: &Path,
    ) -> (
        bms_model::Chart,
        Arc<audio::SoundBank>,
        library::LoudnessKey,
        PathBuf,
    ) {
        let songs = dir.join("song");
        write_sound(&songs, &square(2.0, 3_000));
        let c = sound::chart(&songs, 1);
        let b = bank(&c);
        let key = crate::loudness::key(&c, &b).unwrap();
        (c, b, key, songs.join("a.bms"))
    }

    /// The load of `c` chosen as selection does it, cancelled by the quit
    /// key at its measurement's first block, which leaves the chart to the
    /// background; back to selection.
    fn cancelled_load(
        shelf: &mut Shelf,
        c: &bms_model::Chart,
        b: &Arc<audio::SoundBank>,
        path: &Path,
    ) {
        shelf.choose(path);
        let mut host = Host {
            cancel_at: Some(0),
            ..Host::default()
        };
        assert_eq!(
            shelf.loaded(c, b, "a.bms", path, &mut host),
            game::Loading::Cancelled
        );
        shelf.leave_to_background(job(path));
        shelf.release();
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let begun = Instant::now();
        while !done() {
            assert!(begun.elapsed() < WATCHDOG, "{what} never happened");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn worker(shelf: &Shelf) -> &Worker {
        shelf.background.as_ref().unwrap()
    }

    /// A load cancelled during its measurement leaves the chart to the
    /// background, which measures it while selection shows; the result is
    /// kept and stored at the next READY state, and the chart's next load
    /// measures nothing.
    #[test]
    fn a_cancelled_measurement_is_taken_in_the_background_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (c, b, key, path) = song(dir.path());
        let mut shelf = with_background(dir.path(), fake(key, 5, &open));
        cancelled_load(&mut shelf, &c, &b, &path);
        until("the background's end", || {
            shelf.poll(Instant::now());
            worker(&shelf).busy().1
        });
        shelf.poll(Instant::now());
        assert_eq!(wait(&mut shelf), Paused::Idle);
        shelf.store_loudness();
        assert_eq!(shelf.library.loudness(&key).unwrap(), Some(ROW));
        let mut host = Host::default();
        shelf.choose(&path);
        assert_eq!(
            shelf.loaded(&c, &b, "a.bms", &path, &mut host),
            game::Loading::Ready
        );
        assert_eq!(host.measuring, 0);
        assert_eq!(shelf.gain(), crate::loudness::gain(&ROW));
    }

    /// The chart's load waits for its running background measurement,
    /// which the choice's pause lets go on; the quit key cancels the wait,
    /// not the measurement, and the next load takes its result without
    /// measuring.
    #[test]
    fn the_load_waits_for_its_charts_background_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (c, b, key, path) = song(dir.path());
        let mut shelf = with_background(dir.path(), fake(key, 0, &open));
        cancelled_load(&mut shelf, &c, &b, &path);
        until("the background's start", || worker(&shelf).holds(&path));
        shelf.choose(&path);
        let (mut asked, mut said) = (0, 0);
        let waited = shelf.wait_background(
            &path,
            "a.bms",
            || {
                asked += 1;
                asked == 3
            },
            || said += 1,
        );
        assert_eq!(waited, game::Loading::Cancelled);
        assert_eq!(said, 1);
        assert!(worker(&shelf).holds(&path));
        shelf.release();
        // Chosen again: the measurement goes on through the pause.
        shelf.choose(&path);
        open.store(true, std::sync::atomic::Ordering::SeqCst);
        let waited = shelf.wait_background(&path, "a.bms", || false, || {});
        assert_eq!(waited, game::Loading::Ready);
        let mut host = Host::default();
        shelf.loaded(&c, &b, "a.bms", &path, &mut host);
        assert_eq!(host.measuring, 0);
        assert_eq!(shelf.gain(), crate::loudness::gain(&ROW));
        assert_eq!(wait(&mut shelf), Paused::Idle);
    }

    /// Another chart's choice pauses the background measurement, and
    /// READY starts only once it has stopped at a block (200 ms long
    /// here): nothing is measured during the play, until the result
    /// releases it.
    #[test]
    fn ready_waits_for_the_background_to_pause() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let (_, _, key, _) = song(dir.path());
        let inside = Arc::new(AtomicBool::new(false));
        let blocks = Arc::new(AtomicUsize::new(0));
        let (i, n) = (Arc::clone(&inside), Arc::clone(&blocks));
        let measure: crate::background::Measure = Box::new(move |_, stop| {
            i.store(true, Ordering::SeqCst);
            let outcome = loop {
                if stop() {
                    break crate::background::Outcome::Stopped;
                }
                if n.fetch_add(1, Ordering::SeqCst) > 1_000 {
                    break crate::background::Outcome::Measured(key, ROW);
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            i.store(false, Ordering::SeqCst);
            outcome
        });
        let mut shelf = with_background(dir.path(), measure);
        let other = dir.path().join("other.bms");
        worker(&shelf).queue(job(&other));
        until("the background's block", || {
            blocks.load(Ordering::SeqCst) > 0
        });
        shelf.choose(&dir.path().join("a.bms"));
        // A pause that never comes fails the test rather than hanging it.
        let begun = Instant::now();
        let paused = shelf.wait_paused(
            PAUSE_SLICE,
            || begun.elapsed() > Duration::from_secs(30),
            Duration::MAX,
            || {},
        );
        assert_eq!(paused, Paused::Idle);
        assert!(!inside.load(Ordering::SeqCst));
        let paused_at = blocks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(blocks.load(Ordering::SeqCst), paused_at);
        // The result releases it: measured again from its start.
        shelf.release();
        until("the next block", || {
            blocks.load(Ordering::SeqCst) > paused_at
        });
    }

    /// Another chart's load waits, before its sounds decode, until the
    /// background has stopped for the pause and dropped its bank: the
    /// background thread is held (descheduled) before its next check
    /// until the load's wait has begun, and the wait returns only once the
    /// bank is gone.
    #[test]
    fn another_charts_load_waits_for_the_background_bank_to_go() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let (_, _, _, path) = song(dir.path());
        let other = dir.path().join("other.bms");
        let bank = Arc::new(AtomicBool::new(false));
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let held = Arc::clone(&bank);
        let measure: crate::background::Measure = Box::new(move |_, stop| {
            /// Drops the "bank" however the measurement ends.
            struct Bank(Arc<AtomicBool>);
            impl Drop for Bank {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            held.store(true, Ordering::SeqCst);
            let _bank = Bank(Arc::clone(&held));
            // Descheduled before its next check, until the test lets it on.
            let _ = go.recv();
            loop {
                if stop() {
                    return crate::background::Outcome::Stopped;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let mut shelf = with_background(dir.path(), measure);
        assert_eq!(worker(&shelf).queue(job(&other)), Queued::Added);
        until("the background's bank", || bank.load(Ordering::SeqCst));
        shelf.choose(&path);
        let mut asked = 0;
        let begun = Instant::now();
        let waited = shelf.wait_background(
            &path,
            "a.bms",
            || {
                // The load waits: only now may the background go on.
                asked += 1;
                if asked == 1 {
                    let _ = go_tx.send(());
                }
                // A pause never acknowledged fails the test, not hangs it.
                begun.elapsed() > WATCHDOG
            },
            || panic!("another chart's measurement is not this load's"),
        );
        let held_at_return = bank.load(Ordering::SeqCst);
        // Lets a background the wait did not wait for end too.
        let _ = go_tx.send(());
        assert_eq!(waited, game::Loading::Ready);
        assert!(
            !held_at_return,
            "the load went on beside the background's bank"
        );
        assert!(asked > 0);
    }

    /// The scan's and the background's stops share one deadline: on a
    /// clock the waits move, the background's wait gets only what the
    /// scan's left, and nothing once the scan used it all.
    #[test]
    fn the_scan_and_the_background_share_one_stop_deadline() {
        use std::cell::Cell;
        let start = Instant::now();
        let deadline = start + super::STOP_TIMEOUT;
        for (scan_took, left) in [
            (Duration::ZERO, super::STOP_TIMEOUT),
            (
                Duration::from_millis(1_500),
                super::STOP_TIMEOUT - Duration::from_millis(1_500),
            ),
            (super::STOP_TIMEOUT * 2, Duration::ZERO),
        ] {
            let clock = Cell::new(start);
            let budgets = Cell::new((None, None));
            let (quiet, stopped) = super::stop_together(
                deadline,
                || clock.get(),
                Some(|budget| {
                    budgets.set((Some(budget), None));
                    clock.set(clock.get() + scan_took);
                    false
                }),
                Some(|budget| {
                    budgets.set((budgets.get().0, Some(budget)));
                    "stopped"
                }),
            );
            assert!(!quiet);
            assert_eq!(stopped, Some("stopped"));
            assert_eq!(
                budgets.get(),
                (Some(super::STOP_TIMEOUT), Some(left)),
                "after a scan wait of {scan_took:?}"
            );
        }
        // Without a scan the background has the whole bound.
        let (quiet, stopped) = super::stop_together(
            deadline,
            || start,
            None::<fn(Duration) -> bool>,
            Some(|budget| budget),
        );
        assert!(quiet);
        assert_eq!(stopped, Some(super::STOP_TIMEOUT));
    }

    /// The end of a run with both the scan and the background stuck ends:
    /// neither is joined, so the drop returns (the bound itself is
    /// [`the_scan_and_the_background_share_one_stop_deadline`]'s).
    #[test]
    fn a_stuck_scan_and_background_do_not_hold_the_end() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let (_, _, key, path) = song(dir.path());
        let (started, open) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (s, o) = (Arc::clone(&started), Arc::clone(&open));
        // Stuck on its file system: it asks nothing until opened.
        let measure: crate::background::Measure = Box::new(move |_, _| {
            s.store(true, Ordering::SeqCst);
            while !o.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            crate::background::Outcome::Measured(key, ROW)
        });
        let (shelf, gate) = gated_with(dir.path(), || Worker::start(measure, None));
        worker(&shelf).queue(job(&path));
        until("the background's start", || started.load(Ordering::SeqCst));
        let (dropped_tx, dropped) = std::sync::mpsc::channel();
        let begun = Instant::now();
        let dropping = std::thread::spawn(move || {
            drop(shelf);
            let _ = dropped_tx.send(());
        });
        // A join that waits for the stuck ones fails here, not hangs.
        let ended = dropped.recv_timeout(WATCHDOG);
        let took = begun.elapsed();
        open.store(true, Ordering::SeqCst);
        open_gate(&gate);
        dropping.join().unwrap();
        assert!(
            ended.is_ok(),
            "the end waited for the stuck scan or background"
        );
        assert!(took >= super::STOP_TIMEOUT, "{took:?}");
    }

    /// A chart whose background job only waits is the load's own: taken
    /// from the queue and measured by the load.
    #[test]
    fn a_waiting_background_job_is_taken_by_its_load() {
        let dir = tempfile::tempdir().unwrap();
        let open = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (c, b, key, path) = song(dir.path());
        let other = dir.path().join("other.bms");
        let mut shelf = with_background(dir.path(), fake(key, 0, &open));
        // Another chart runs, this one waits behind it.
        assert_eq!(worker(&shelf).queue(job(&other)), Queued::Added);
        until("the other's start", || worker(&shelf).holds(&other));
        cancelled_load(&mut shelf, &c, &b, &path);
        shelf.choose(&path);
        assert_eq!(
            shelf.wait_background(&path, "a.bms", || false, || {}),
            game::Loading::Ready
        );
        let mut host = Host::default();
        shelf.loaded(&c, &b, "a.bms", &path, &mut host);
        assert_eq!(host.measuring, 1);
        assert!(!worker(&shelf).take_waiting(&path));
        open.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The end of a run while the background measures stops it at its
    /// next block: the drop returns before its stop bound, long before the
    /// measurement could end, and nothing of it is stored.
    #[test]
    fn exit_while_measuring_in_the_background_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (c, b, key, path) = song(dir.path());
        // 3 s of blocks.
        let mut shelf = with_background(dir.path(), fake(key, 300, &open));
        cancelled_load(&mut shelf, &c, &b, &path);
        until("the background's start", || worker(&shelf).holds(&path));
        let begun = Instant::now();
        drop(shelf);
        assert!(
            begun.elapsed() < super::STOP_TIMEOUT,
            "{:?}",
            begun.elapsed()
        );
        let (library, _) = open_library(dir.path());
        assert_eq!(library.loudness(&key).unwrap(), None);
    }

    fn open_library(dir: &Path) -> (library::Library, library::Opened) {
        open(&DataRoot::new(dir.join("data"))).unwrap()
    }

    /// A shelf whose background could not start leaves a cancelled load's
    /// chart to its next choice, which measures it.
    #[test]
    fn without_its_background_a_cancelled_chart_is_measured_at_its_next_choice() {
        let dir = tempfile::tempdir().unwrap();
        let (c, b, _, path) = song(dir.path());
        let (library, _) = open_library(dir.path());
        let mut shelf = Shelf::with_worker(
            library,
            Vec::new(),
            Duration::from_secs(60),
            library::ScanOptions::default(),
            || Err(std::io::Error::other("no thread (test)")),
        );
        cancelled_load(&mut shelf, &c, &b, &path);
        assert_eq!(
            shelf.wait_background(&path, "a.bms", || false, || {}),
            game::Loading::Ready
        );
        let mut host = Host::default();
        shelf.loaded(&c, &b, "a.bms", &path, &mut host);
        assert_eq!(host.measuring, 1);
        assert_eq!(wait(&mut shelf), Paused::Idle);
    }

    /// The wait for the pause does not wait for the loudness, which the
    /// load measured: without a scan it returns at once.
    #[test]
    fn ready_does_not_wait_for_the_loudness() {
        let dir = tempfile::tempdir().unwrap();
        let mut shelf = unscanned(dir.path());
        load(&mut shelf, dir.path());
        assert_eq!(wait(&mut shelf), Paused::Idle);
    }

    /// A measurement not yet stored (the run ended before a READY state) is
    /// stored when the run ends with its scan stopped: the next run (a
    /// second opening of the library) finds it.
    #[test]
    fn a_measurement_is_stored_at_exit() {
        let dir = tempfile::tempdir().unwrap();
        let mut shelf = unscanned(dir.path());
        let key = load(&mut shelf, dir.path());
        drop(shelf);
        let (library, _) = open(&DataRoot::new(dir.path().join("data"))).unwrap();
        assert!(library.loudness(&key).unwrap().is_some());
    }

    /// At an exit whose scan does not stop (it may still commit), a
    /// measurement is not stored.
    #[test]
    fn exit_stores_nothing_with_a_stuck_scan() {
        let dir = tempfile::tempdir().unwrap();
        let (mut shelf, gate) = gated(dir.path());
        let key = load(&mut shelf, dir.path());
        let dropping = std::thread::spawn(move || drop(shelf));
        let begun = Instant::now();
        while !dropping.is_finished() {
            assert!(begun.elapsed() < WATCHDOG);
            std::thread::sleep(Duration::from_millis(10));
        }
        open_gate(&gate);
        dropping.join().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let begun = Instant::now();
        let library = loop {
            if let Ok((library, _)) = open(&root) {
                break library;
            }
            assert!(begun.elapsed() < WATCHDOG, "the lock stayed");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(library.loudness(&key).unwrap(), None);
    }
}
