//! Pausing, the endings of a scan, and listing failures, with the scan
//! held mid-file or mid-commit and faults injected through [`Hooks`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{
    BATCH, HEADER_FIELD_BYTES, Scan, ScanError, ScanOptions, ThreadStart, cap, read_chart,
};
use crate::{Library, LibraryError};

const LONG: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(300);

type PathHook = Arc<dyn Fn(&Path) + Send + Sync>;
type FaultHook = Arc<dyn Fn(&Path) -> Option<Fault> + Send + Sync>;
type CommitHook = Arc<dyn Fn(usize) + Send + Sync>;

/// A failure injected into a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Listing stops with an error at this entry.
    Listing,
    /// This entry's type and metadata cannot be read.
    Metadata,
}

/// The unit tests' seams in a scan.
#[derive(Clone, Default)]
pub struct Hooks {
    /// The scan is about to read this file.
    in_hand: Option<PathHook>,
    /// The scan read this file, before it checks it again.
    after_read: Option<PathHook>,
    /// The scan is at this entry of a listing.
    fault: Option<FaultHook>,
    /// The scan commits this many rows and removals.
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

/// Lets every held file go until the scan ends.
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
    write_charts(&root, 4);
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

    // A is read and waits in the batch; B is in hand.
    let a = gate.next();
    gate.open(1);
    let b = gate.next();
    scan.request_pause();
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment while a file is in hand"
    );
    gate.open(1);
    assert!(scan.wait_paused(LONG));

    // Everything read is committed, and nothing else is read.
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(indexed(&lib), expected);
    assert_eq!(scan.report().read, 2);
    assert!(gate.quiet(), "no file is read during the pause");
    assert_eq!(indexed(&lib).len(), 2);

    scan.release();
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 4);
    assert_eq!(indexed(&lib).len(), 4);
}

#[test]
fn release_and_pause_again_during_a_commit_acknowledge_once_idle() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
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
    scan.request_pause();
    gate.open(1);
    // The scan commits for the pause; meanwhile the pause is released and
    // requested again.
    commits.next();
    scan.release();
    assert!(gate.quiet(), "nothing is read during the commit");
    scan.request_pause();
    commits.open(1);
    assert!(scan.wait_paused(LONG));
    assert_eq!(indexed(&lib).len(), 1);
    assert!(gate.quiet(), "nothing is read during the pause");

    scan.release();
    while !scan.wait_finished(Duration::from_millis(10)) {
        while gate.held.try_recv().is_ok() {
            gate.open(1);
        }
        while commits.held.try_recv().is_ok() {
            commits.open(1);
        }
    }
    assert_eq!(scan.finish().unwrap().read, 3);
}

#[test]
fn a_pause_requested_again_right_after_a_release_is_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
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
    gate.next();
    scan.request_pause();
    gate.open(1);
    assert!(scan.wait_paused(LONG));
    // Released and paused again before the scan can see the release.
    scan.shared.update(|state| {
        state.release();
        state.request_pause();
    });
    assert!(scan.wait_paused(LONG), "the new request is acknowledged");
    assert!(gate.quiet(), "nothing is read between");
    // Released and paused again from outside, one call after the other.
    scan.release();
    scan.request_pause();
    assert!(scan.wait_paused(LONG));
    scan.release();
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 3);
}

#[test]
fn a_failed_commit_ends_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, BATCH + 20);
    let lib = library(dir.path());
    lib.reader()
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON charts
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let scan = lib
        .scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap();
    assert!(scan.wait_finished(LONG));
    scan.request_pause();
    assert!(scan.wait_paused(Duration::ZERO), "an ended scan holds");
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

/// Runs a scan whose hooks panic, and checks that it publishes its end,
/// reports the panic and releases its lease, within the bound.
fn panicking_scan(options: ScanOptions) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 5);
    let lib = library(dir.path());
    let scan = lib.scan(std::slice::from_ref(&root), options).unwrap();
    assert!(scan.wait_finished(LONG), "the end is published");
    assert!(scan.wait_paused(Duration::ZERO));
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Panicked)), "{result:?}");
    drop(lib.scan(&[root], ScanOptions::default()).unwrap());
}

#[test]
fn a_panic_in_the_start_hook_ends_the_scan() {
    panicking_scan(ScanOptions {
        on_thread_start: Some(Arc::new(|| panic!("injected start hook panic"))),
        ..ScanOptions::default()
    });
}

#[test]
fn a_panic_mid_file_ends_the_scan() {
    panicking_scan(with(Hooks {
        in_hand: Some(Arc::new(|_: &Path| panic!("injected panic mid-file"))),
        ..Hooks::default()
    }));
}

#[test]
fn a_panic_in_the_commit_of_a_pause_ends_the_scan() {
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
                before_commit: Some(Arc::new(|_| panic!("injected commit panic"))),
                ..Hooks::default()
            }),
        )
        .unwrap();
    gate.next();
    scan.request_pause();
    gate.open(1);
    assert!(scan.wait_finished(LONG), "the end is published");
    assert!(scan.wait_paused(Duration::ZERO));
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Panicked)), "{result:?}");
    drop(lib.scan(&[root], ScanOptions::default()).unwrap());
}

/// Options whose hook counts the thread starts; the hook's own count of
/// owners falls back to one once the thread has ended.
fn counted() -> (ScanOptions, Arc<AtomicUsize>, ThreadStart) {
    let started = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&started);
    let hook: ThreadStart = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
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
fn dropping_a_paused_scan_joins_its_thread() {
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
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&hook), 1, "the thread has ended");
    assert_eq!(indexed(&lib).len(), committed, "the drop commits nothing");
}

#[test]
fn dropping_a_running_scan_joins_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 700);
    let lib = library(dir.path());
    let (options, started, hook) = counted();
    let scan = lib.scan(std::slice::from_ref(&root), options).unwrap();
    bounded(move || drop(scan));
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&hook), 1, "the thread has ended");
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
fn a_capped_header_value_keeps_no_long_allocation() {
    let mut short = Some("short".to_owned());
    cap(&mut short);
    assert_eq!(short.as_deref(), Some("short"));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("long.bms");
    let title = "あ".repeat(4 << 20);
    fs::write(&path, format!("#TITLE {title}\n#ARTIST a\n")).unwrap();
    let row = read_chart(
        0,
        path.to_str().unwrap().to_owned(),
        &bms_parser::Limits::default(),
        &ScanOptions::default(),
    )
    .unwrap();
    let kept = row.header.title.unwrap();
    assert_eq!(kept.len(), HEADER_FIELD_BYTES / 3 * 3);
    assert!(
        kept.capacity() <= HEADER_FIELD_BYTES,
        "capacity {}",
        kept.capacity()
    );
}

/// Links file `link` to `target`, or `None` when this system does not let
/// the test create links.
fn link_file(target: &Path, link: &Path) -> Option<()> {
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    match made {
        Ok(()) => Some(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
        // ERROR_PRIVILEGE_NOT_HELD: links need Developer Mode or elevation.
        Err(e) if cfg!(windows) && e.raw_os_error() == Some(1314) => None,
        Err(e) => panic!("cannot create a file link: {e}"),
    }
}

#[test]
fn a_linked_name_of_the_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("library.db");
    let (lib, _) = Library::open(&real).unwrap();
    let alias = dir.path().join("alias.db");
    if link_file(&real, &alias).is_none() {
        println!("skipped: this system does not allow creating file links");
        return;
    }
    assert!(matches!(
        Library::open(&alias),
        Err(LibraryError::AlreadyOpen { .. })
    ));
    drop(lib);
    let (lib, opened) = Library::open(&alias).unwrap();
    assert_eq!(opened, crate::Opened::Current, "the linked file is opened");
    assert!(matches!(
        Library::open(&real),
        Err(LibraryError::AlreadyOpen { .. })
    ));
    drop(lib);
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

#[cfg(windows)]
#[test]
fn a_case_alias_of_the_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (lib, _) = Library::open(&dir.path().join("library.db")).unwrap();
    assert!(matches!(
        Library::open(&dir.path().join("LIBRARY.DB")),
        Err(LibraryError::AlreadyOpen { .. })
    ));
    drop(lib);
    drop(Library::open(&dir.path().join("LIBRARY.DB")).unwrap());
}
