//! The library on the game thread (design.md "Loading and Song
//! Selection"): `library.db` under the data root, kept current by scan
//! passes in the background while the select screen shows, and paused from
//! the choice of a chart until the countdown of each play is over.
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
//! - **Pausing.** Choosing a chart requests a pause, and each countdown
//!   waits until the scan acknowledges it ([`Shelf::wait_paused`]); every
//!   entry into selection and every result screen releases it.
//! - **Priority.** The scan thread runs in background mode (lowered CPU and
//!   I/O priority), set by its start hook.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use game::{Chain, DataRoot};
use library::{ChartEntry, Library, LibraryError, Opened, Scan, ScanOptions, ScanReport, Search};

/// How long the last pass is over before the next starts, at the select
/// screen.
pub const RESCAN_AFTER: Duration = Duration::from_secs(10);
/// How long the end of a run waits for a running pass to stop.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a countdown's wait for the scan's pause looks whether it
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
    Library::open(&database(root)).map_err(|e| match e {
        LibraryError::AlreadyOpen { .. } => OpenError::Running {
            root: root.path().to_owned(),
        },
        e => OpenError::Library(e),
    })
}

/// What a countdown's wait for the pause found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paused {
    /// The scan acknowledged the pause.
    Acknowledged,
    /// No scan runs.
    Idle,
    /// The wait was cancelled: the window is closing, or the player left
    /// the chart. No countdown follows.
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
}

impl Shelf {
    /// Keeps `library` current over `roots`, starting the first pass.
    pub fn new(library: Library, roots: Vec<PathBuf>, rescan_after: Duration) -> Self {
        let options = ScanOptions {
            on_thread_start: Some(Arc::new(background_mode)),
        };
        Self::with_options(library, roots, rescan_after, options)
    }

    /// [`new`](Self::new), each pass's thread starting with `options`.
    fn with_options(
        library: Library,
        roots: Vec<PathBuf>,
        rescan_after: Duration,
        options: ScanOptions,
    ) -> Self {
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
        };
        shelf.start_pass();
        shelf
    }

    /// What the library is doing, for the select screen.
    pub fn status(&self) -> &str {
        if self.scan.is_some() {
            "スキャン中"
        } else {
            &self.status
        }
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
        if let Some(scan) = &self.scan {
            if !scan.wait_finished(Duration::ZERO) {
                let r = scan.report();
                let batches = (r.read + r.removed) / library::BATCH as u64;
                let changed = batches != self.seen;
                self.seen = batches;
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
                self.status = String::new();
            }
            Err(e) => {
                let line = format!("library: the scan failed: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
                self.status = "スキャンに失敗しました".to_owned();
            }
        }
    }

    /// A chart was chosen: the scan pauses after the file in hand, and no
    /// pass starts until the release.
    pub fn pause(&mut self) {
        if !self.paused {
            self.paused = true;
            if let Some(scan) = &self.scan {
                scan.request_pause();
            }
        }
    }

    /// Before a countdown: requests the pause if none is, and waits for the
    /// scan to acknowledge it, however long that takes (design.md "Loading
    /// and Song Selection": the countdown starts only after it). Every
    /// `slice` it asks `cancelled` whether to give up; once the wait has
    /// run `notice_after`, it calls `notice` once.
    pub fn wait_paused(
        &mut self,
        slice: Duration,
        mut cancelled: impl FnMut() -> bool,
        notice_after: Duration,
        notice: impl FnOnce(),
    ) -> Paused {
        self.pause();
        let Some(scan) = &self.scan else {
            return Paused::Idle;
        };
        let started = Instant::now();
        let mut notice = Some(notice);
        loop {
            if scan.wait_paused(slice) {
                return Paused::Acknowledged;
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
    }

    /// Whether a pause is requested and not released.
    #[cfg(test)]
    const fn paused(&self) -> bool {
        self.paused
    }

    /// Whether a pass runs.
    #[cfg(test)]
    fn has_scan(&self) -> bool {
        self.scan.is_some()
    }
}

impl Drop for Shelf {
    /// Stops the running pass, waiting at most [`STOP_TIMEOUT`]: a pass
    /// stuck on its file system is left to end on its own, keeping the
    /// library's lock until it does or the process exits.
    fn drop(&mut self) {
        if let Some(scan) = self.scan.take()
            && !scan.stop_within(STOP_TIMEOUT)
        {
            let line = format!(
                "library: the scan did not stop within {} s; it ends with the process",
                STOP_TIMEOUT.as_secs()
            );
            eprintln!("{line}");
            log::warn!("{line}");
        }
    }
}

/// The scan thread's start hook: background mode, which lowers its CPU,
/// I/O and memory priority (design.md "Coexisting with Other Software").
fn background_mode() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: the pseudo handle of the calling thread, always valid.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
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

    /// The wait a countdown makes, never cancelled.
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
        let mut shelf = Shelf::new(library, vec![charts.clone()], Duration::from_millis(20));
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

    /// The pause a countdown waits for is acknowledged by a running pass
    /// before the wait returns, and requested by the wait itself when no
    /// choice did.
    #[test]
    fn the_pause_is_acknowledged_before_each_countdown() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("data"));
        let charts = dir.path().join("charts");
        for k in 0..600 {
            chart(&charts.join(format!("f{}", k / 50)), &format!("{k}.bms"));
        }
        let (library, _) = open(&root).unwrap();
        let mut shelf = Shelf::new(library, vec![charts], Duration::from_secs(60));
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
        };
        let shelf = Shelf::with_options(library, vec![charts], Duration::from_secs(60), options);
        (shelf, gate)
    }

    fn open_gate(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    /// The countdown's wait lasts until the scan acknowledges, however many
    /// slices that takes: it never gives up on its own. Past its notice
    /// time it says so once.
    #[test]
    fn the_countdown_waits_for_the_acknowledgment() {
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

    /// Closing the window, or the quit key, cancels the wait: no countdown
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
}
