//! Pausing, the endings of a scan, and listing failures, with parsers held
//! mid-file, commits held, and faults injected through [`Hooks`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{BATCH, Fault, Scan, ScanError, ScanOptions, ThreadStart};
use crate::{Library, LibraryError};

const LONG: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(300);

type PathHook = Arc<dyn Fn(&Path) + Send + Sync>;
type FaultHook = Arc<dyn Fn(&Path) -> Option<Fault> + Send + Sync>;
type CommitHook = Arc<dyn Fn(usize) + Send + Sync>;

/// The unit tests' seams in a scan.
#[derive(Clone, Default)]
pub struct Hooks {
    /// A parser was handed this file, before it reads it.
    in_hand: Option<PathHook>,
    /// A parser read this file, before it checks it again.
    after_read: Option<PathHook>,
    /// The walker is at this entry of a listing.
    fault: Option<FaultHook>,
    /// The writer commits this many rows and removals.
    before_commit: Option<CommitHook>,
}

impl Hooks {
    pub fn in_hand(&self, path: &Path) {
        if let Some(hook) = &self.in_hand {
            hook(path);
        }
    }

    pub fn after_read(&self, path: &Path) {
        if let Some(hook) = &self.after_read {
            hook(path);
        }
    }

    pub fn fault(&self, path: &Path) -> Option<Fault> {
        self.fault.as_ref().and_then(|hook| hook(path))
    }

    pub fn before_commit(&self, len: usize) {
        if let Some(hook) = &self.before_commit {
            hook(len);
        }
    }
}

fn library(dir: &Path) -> Library {
    Library::open(&dir.join("library.db")).unwrap().0
}

fn chart(i: usize, title: &str) -> String {
    format!(
        "#PLAYER 1\n#TITLE {title}\n#BPM 120\n#00{:01}11:01\n",
        i % 10
    )
}

fn write_charts(root: &Path, count: usize) -> Vec<PathBuf> {
    fs::create_dir_all(root).unwrap();
    (0..count)
        .map(|i| {
            let path = root.join(format!("chart{i:04}.bms"));
            fs::write(&path, chart(i, &format!("Chart {i}"))).unwrap();
            path
        })
        .collect()
}

fn indexed(lib: &Library) -> Vec<PathBuf> {
    lib.charts().unwrap().into_iter().map(|c| c.path).collect()
}

fn with(test: Hooks) -> ScanOptions {
    ScanOptions {
        test,
        ..ScanOptions::default()
    }
}

/// Runs `f` on another thread and fails the test if it takes longer than
/// [`LONG`].
fn bounded<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(LONG).expect("returns in time")
}

/// A gate: each call reports its path and waits for one go.
struct Gate {
    held: Receiver<PathBuf>,
    go: Sender<()>,
}

impl Gate {
    fn new() -> (Self, PathHook) {
        let (held_tx, held) = mpsc::channel::<PathBuf>();
        let (go, go_rx) = mpsc::channel::<()>();
        let held_tx = Mutex::new(held_tx);
        let go_rx = Mutex::new(go_rx);
        let hook: PathHook = Arc::new(move |path: &Path| {
            let _ = held_tx.lock().unwrap().send(path.to_owned());
            let _ = go_rx.lock().unwrap().recv_timeout(LONG);
        });
        (Self { held, go }, hook)
    }

    fn next(&self) -> PathBuf {
        self.held.recv_timeout(LONG).unwrap()
    }

    fn quiet(&self) -> bool {
        self.held.recv_timeout(SHORT) == Err(RecvTimeoutError::Timeout)
    }

    fn open(&self, n: usize) {
        for _ in 0..n {
            self.go.send(()).unwrap();
        }
    }
}

/// Lets every held parser go until the scan ends.
fn drain(scan: &Scan, gate: &Gate) {
    while !scan.wait_finished(Duration::from_millis(10)) {
        while gate.held.try_recv().is_ok() {
            gate.open(1);
        }
    }
}

#[test]
fn pause_is_acknowledged_after_in_flight_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 5);
    let lib = library(dir.path());
    let (gate, hook) = Gate::new();
    let scan = lib
        .scan(
            &[root],
            with(Hooks {
                in_hand: Some(hook),
                ..Hooks::default()
            }),
        )
        .unwrap();

    // A is answered and waits in the batch; B and C are in the parsers'
    // hands.
    let a = gate.next();
    let b = gate.next();
    gate.open(1);
    let c = gate.next();
    scan.request_pause();
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment while files are in hand"
    );
    gate.open(2);
    assert!(scan.wait_paused(LONG));

    // Everything handed out is committed, and nothing else.
    let mut expected = vec![a, b, c];
    expected.sort();
    assert_eq!(indexed(&lib), expected);
    assert_eq!(scan.report().read, 3);
    assert!(gate.quiet(), "no file is handed out during the pause");
    assert_eq!(indexed(&lib).len(), 3);

    scan.release();
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 5);
    assert_eq!(indexed(&lib).len(), 5);
}

#[test]
fn release_and_pause_again_during_a_commit_acknowledge_once_idle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 4);
    let lib = library(dir.path());
    let (gate, in_hand) = Gate::new();
    let (commits, before_commit) = Gate::new();
    let scan = lib
        .scan(
            &[root],
            with(Hooks {
                in_hand: Some(in_hand),
                before_commit: Some(Arc::new(move |_| before_commit(Path::new("")))),
                ..Hooks::default()
            }),
        )
        .unwrap();
    gate.next();
    gate.next();
    scan.request_pause();
    gate.open(2);
    // The writer commits for the pause; meanwhile the pause is released
    // and requested again.
    commits.next();
    scan.release();
    assert!(gate.quiet(), "nothing is handed out during the commit");
    scan.request_pause();
    commits.open(1);
    assert!(scan.wait_paused(LONG));
    assert_eq!(indexed(&lib).len(), 2);
    assert!(gate.quiet(), "nothing is handed out during the pause");

    scan.release();
    while !scan.wait_finished(Duration::from_millis(10)) {
        while gate.held.try_recv().is_ok() {
            gate.open(1);
        }
        while commits.held.try_recv().is_ok() {
            commits.open(1);
        }
    }
    assert_eq!(scan.finish().unwrap().read, 4);
}

#[test]
fn a_failed_writer_ends_the_others_before_the_scan_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    // More files than one batch, so a commit fails while a parser still
    // holds the first file handed out.
    write_charts(&root, BATCH + 20);
    let lib = library(dir.path());
    lib.reader()
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON charts
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let (gate, hold) = Gate::new();
    let first = Arc::new(AtomicBool::new(true));
    let in_hand: PathHook = Arc::new(move |path: &Path| {
        if first.swap(false, Ordering::SeqCst) {
            hold(path);
        }
    });
    let scan = lib
        .scan(
            &[dir.path().join("songs")],
            with(Hooks {
                in_hand: Some(in_hand),
                ..Hooks::default()
            }),
        )
        .unwrap();
    gate.next();
    assert!(
        scan.wait_for(LONG, |state| state.error.is_some()),
        "the commit fails"
    );
    scan.request_pause();
    assert!(!scan.wait_finished(SHORT), "a parser still holds a file");
    assert!(!scan.wait_paused(SHORT), "a parser still holds a file");
    gate.open(1);
    assert!(scan.wait_finished(LONG));
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Database(_))), "{result:?}");
    assert!(indexed(&lib).is_empty());
    // The failed scan released its lease.
    drop(lib.scan(&[root], ScanOptions::default()).unwrap());
}

#[test]
fn one_scan_runs_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let (gate, hook) = Gate::new();
    let scan = lib
        .scan(
            std::slice::from_ref(&root),
            with(Hooks {
                in_hand: Some(hook),
                ..Hooks::default()
            }),
        )
        .unwrap();
    gate.next();
    assert!(matches!(
        lib.scan(std::slice::from_ref(&root), ScanOptions::default()),
        Err(LibraryError::ScanRunning)
    ));
    gate.open(1);
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 3);
    let report = lib
        .scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(report.unchanged, 3);

    // A dropped scan releases it too.
    let scan = lib
        .scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap();
    drop(scan);
    drop(lib.scan(&[root], ScanOptions::default()).unwrap());
}

#[test]
fn a_library_is_never_rebuilt_under_its_own_connections() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 2);
    let path = dir.path().join("library.db");
    let (lib, _) = Library::open(&path).unwrap();
    lib.scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    // The file now claims another version while the library holds it.
    lib.reader()
        .execute_batch("PRAGMA user_version = 7;")
        .unwrap();
    let again = dir.path().join(".").join("library.db");
    assert!(matches!(
        Library::open(&again),
        Err(LibraryError::AlreadyOpen { .. })
    ));
    assert_eq!(indexed(&lib).len(), 2, "the file is untouched");

    // A scan outliving its library still holds the file.
    let (gate, hook) = Gate::new();
    fs::write(root.join("new.bms"), chart(0, "New")).unwrap();
    let scan = lib
        .scan(
            std::slice::from_ref(&root),
            with(Hooks {
                in_hand: Some(hook),
                ..Hooks::default()
            }),
        )
        .unwrap();
    gate.next();
    drop(lib);
    assert!(matches!(
        Library::open(&path),
        Err(LibraryError::AlreadyOpen { .. })
    ));
    gate.open(1);
    drain(&scan, &gate);
    drop(scan);
    let (lib, opened) = Library::open(&path).unwrap();
    assert_eq!(opened, crate::Opened::Rebuilt { old_version: 7 });
    assert!(indexed(&lib).is_empty());
}

#[test]
fn incomplete_listing_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    let charts = write_charts(&root, 3);
    let lib = library(dir.path());
    lib.scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    fs::remove_file(&charts[2]).unwrap();

    // The listing fails at its second entry, after the first was listed.
    let entries = Arc::new(AtomicUsize::new(0));
    let fault: FaultHook = Arc::new(move |_: &Path| {
        (entries.fetch_add(1, Ordering::SeqCst) == 1).then_some(Fault::Listing)
    });
    let report = lib
        .scan(
            std::slice::from_ref(&root),
            with(Hooks {
                fault: Some(fault),
                ..Hooks::default()
            }),
        )
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!((report.found, report.removed), (1, 0));
    assert_eq!(indexed(&lib).len(), 3, "the unseen rows stay");

    let report = lib
        .scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(indexed(&lib).len(), 2);
}

#[test]
fn unreadable_metadata_keeps_the_row_and_counts_it_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    let charts = write_charts(&root, 2);
    let lib = library(dir.path());
    lib.scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    fs::write(&charts[0], chart(0, "Edited and longer")).unwrap();

    let target = charts[0].clone();
    let fault: FaultHook = Arc::new(move |path: &Path| (path == target).then_some(Fault::Metadata));
    let report = lib
        .scan(
            std::slice::from_ref(&root),
            with(Hooks {
                fault: Some(fault),
                ..Hooks::default()
            }),
        )
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(
        (
            report.found,
            report.unchanged,
            report.skipped,
            report.removed
        ),
        (1, 1, 1, 0)
    );
    let titles: Vec<_> = lib
        .charts()
        .unwrap()
        .into_iter()
        .map(|c| c.header.title.unwrap())
        .collect();
    assert_eq!(titles, ["Chart 0", "Chart 1"], "the old row stays");
}

#[test]
fn file_changed_while_read_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    let charts = write_charts(&root, 2);
    let lib = library(dir.path());
    let target = charts[0].clone();
    let after_read: PathHook = Arc::new(move |path: &Path| {
        if path == target {
            fs::write(path, chart(0, "Written during the read")).unwrap();
        }
    });
    let report = lib
        .scan(
            std::slice::from_ref(&root),
            with(Hooks {
                after_read: Some(after_read),
                ..Hooks::default()
            }),
        )
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!((report.read, report.skipped), (1, 1));
    assert_eq!(indexed(&lib), [charts[1].clone()]);
    // The next scan reads it.
    let report = lib
        .scan(&[root], ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(report.read, 1);
}

#[test]
fn commits_hold_at_most_one_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    let charts = write_charts(&root, BATCH + 100);
    let lib = library(dir.path());
    lib.scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    // Rows and removals together exceed one batch.
    for (i, path) in charts.iter().enumerate() {
        if i % 2 == 0 {
            fs::remove_file(path).unwrap();
        } else {
            fs::write(path, chart(i, "Changed and longer")).unwrap();
        }
    }
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&sizes);
    let report = lib
        .scan(
            &[root],
            with(Hooks {
                before_commit: Some(Arc::new(move |len| record.lock().unwrap().push(len))),
                ..Hooks::default()
            }),
        )
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(
        report.read + report.removed,
        u64::try_from(BATCH + 100).unwrap()
    );
    let sizes = sizes.lock().unwrap();
    assert!(sizes.contains(&BATCH), "{sizes:?}");
    assert!(sizes.iter().all(|&len| len <= BATCH), "{sizes:?}");
}

/// Options whose hook counts the threads started; the hook's own count of
/// owners falls back to one once every thread has ended.
fn counted() -> (ScanOptions, Arc<Mutex<usize>>, ThreadStart) {
    let started = Arc::new(Mutex::new(0_usize));
    let counter = Arc::clone(&started);
    let hook: ThreadStart = Arc::new(move |_| *counter.lock().unwrap() += 1);
    (
        ScanOptions {
            on_thread_start: Some(Arc::clone(&hook)),
            ..ScanOptions::default()
        },
        started,
        hook,
    )
}

#[test]
fn dropping_a_paused_scan_joins_its_threads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 300);
    let lib = library(dir.path());
    let (options, started, hook) = counted();
    let scan = lib.scan(&[root], options).unwrap();
    scan.request_pause();
    assert!(scan.wait_paused(LONG));
    let committed = indexed(&lib).len();
    bounded(move || drop(scan));
    assert_eq!(*started.lock().unwrap(), 4);
    assert_eq!(Arc::strong_count(&hook), 1, "every thread has ended");
    assert_eq!(indexed(&lib).len(), committed, "the drop commits nothing");
}

#[test]
fn dropping_a_running_scan_joins_its_threads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 700);
    let lib = library(dir.path());
    let (options, started, hook) = counted();
    let scan = lib.scan(std::slice::from_ref(&root), options).unwrap();
    bounded(move || drop(scan));
    assert_eq!(*started.lock().unwrap(), 4);
    assert_eq!(Arc::strong_count(&hook), 1, "every thread has ended");
    // What was committed is whole rows, and a later scan completes it.
    let report = lib
        .scan(&[root], ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(report.found, 700);
    assert_eq!(indexed(&lib).len(), 700);
}

#[test]
fn pausing_an_ended_scan_holds_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 2);
    let lib = library(dir.path());
    let scan = lib.scan(&[root], ScanOptions::default()).unwrap();
    assert!(scan.wait_finished(LONG));
    scan.request_pause();
    assert!(scan.wait_paused(Duration::ZERO));
    assert_eq!(scan.finish().unwrap().read, 2);
}
