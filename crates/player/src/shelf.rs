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
/// How long a countdown waits for the scan to acknowledge its pause: the
/// file in hand (at most the loader's 16 MiB cap) and the batch's commit.
pub const PAUSE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The scan did not acknowledge within [`PAUSE_TIMEOUT`].
    TimedOut,
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

    /// Before a countdown: requests the pause if none is, and waits up to
    /// [`PAUSE_TIMEOUT`] for the scan to acknowledge it.
    pub fn wait_paused(&mut self) -> Paused {
        self.pause();
        match &self.scan {
            None => Paused::Idle,
            Some(scan) if scan.wait_paused(PAUSE_TIMEOUT) => Paused::Acknowledged,
            Some(_) => Paused::TimedOut,
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

    use super::{OpenError, Paused, Shelf, open};

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

    /// Polls until `listed` holds `name`, at most `within`.
    fn lists(shelf: &mut Shelf, name: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            shelf.poll(Instant::now());
            if listed(shelf).iter().any(|n| n == name) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
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
        assert!(lists(&mut shelf, "refused.bms", Duration::from_secs(10)));
        // A chart is chosen.
        shelf.pause();
        assert_ne!(shelf.wait_paused(), Paused::TimedOut);
        chart(&charts, "added.bms");
        // Paused: nothing is listed however long selection waits.
        assert!(!lists(&mut shelf, "added.bms", Duration::from_millis(300)));
        shelf.release();
        assert!(lists(&mut shelf, "added.bms", Duration::from_secs(10)));
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
        assert_eq!(shelf.wait_paused(), Paused::Acknowledged);
        assert!(shelf.paused());
        // While paused nothing more is committed.
        let before = listed(&shelf).len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(listed(&shelf).len(), before);
        shelf.release();
        // A second wait, as a retry's, is acknowledged again.
        assert_ne!(shelf.wait_paused(), Paused::TimedOut);
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
}
