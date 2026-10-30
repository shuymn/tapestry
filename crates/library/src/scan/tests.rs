//! Pausing and the endings of a scan, with a parser held mid-file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{Hooks, ScanError, ScanOptions, start};
use crate::{Library, db};

const LONG: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(300);

fn library(dir: &Path) -> Library {
    Library::open(&dir.join("library.db")).unwrap().0
}

fn write_charts(root: &Path, count: usize) -> Vec<PathBuf> {
    fs::create_dir_all(root).unwrap();
    (0..count)
        .map(|i| {
            let path = root.join(format!("chart{i:04}.bms"));
            fs::write(&path, format!("#PLAYER 1\n#TITLE Chart {i}\n#BPM 120\n")).unwrap();
            path
        })
        .collect()
}

fn indexed(lib: &Library) -> Vec<PathBuf> {
    lib.charts().unwrap().into_iter().map(|c| c.path).collect()
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

#[test]
fn pause_is_acknowledged_after_in_flight_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());

    // Each parser reports the file it holds and waits for a go.
    let (held_tx, held_rx) = mpsc::channel::<PathBuf>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let held_tx = Mutex::new(held_tx);
    let go_rx = Mutex::new(go_rx);
    let hooks = Hooks {
        in_hand: Some(Arc::new(move |path: &Path| {
            held_tx.lock().unwrap().send(path.to_owned()).unwrap();
            let _ = go_rx.lock().unwrap().recv_timeout(LONG);
        })),
    };
    let writer = db::connect(lib.path()).unwrap();
    let scan = start(writer, vec![root], ScanOptions::default(), hooks).unwrap();

    let mut in_hand = vec![
        held_rx.recv_timeout(LONG).unwrap(),
        held_rx.recv_timeout(LONG).unwrap(),
    ];
    in_hand.sort();
    scan.request_pause();
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment while the parsers hold their files"
    );
    go_tx.send(()).unwrap();
    go_tx.send(()).unwrap();
    assert!(scan.wait_paused(LONG));

    // The files in hand are committed, and nothing else.
    assert_eq!(in_hand.len(), 2);
    assert_eq!(indexed(&lib), in_hand);
    assert_eq!(scan.report().read, 2);
    // No file is read during the pause.
    assert_eq!(held_rx.recv_timeout(SHORT), Err(RecvTimeoutError::Timeout));
    assert_eq!(indexed(&lib).len(), 2);

    scan.release();
    held_rx.recv_timeout(LONG).unwrap();
    go_tx.send(()).unwrap();
    let report = scan.finish().unwrap();
    assert_eq!(report.read, 3);
    assert_eq!(indexed(&lib).len(), 3);
}

#[test]
fn scan_ends_when_the_writer_fails() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    // More files than one batch and both queues hold, so the walker and
    // the parsers are mid-scan when the first commit fails.
    write_charts(&root, 700);
    let lib = library(dir.path());
    lib.reader()
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON charts
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let scan = lib.scan(&[root], ScanOptions::default()).unwrap();
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Database(_))), "{result:?}");
    assert!(indexed(&lib).is_empty());
}

/// Options whose hook counts the threads started; the hook's own count
/// of owners falls back to one once every thread has ended.
fn counted() -> (ScanOptions, Arc<Mutex<usize>>, super::ThreadStart) {
    let started = Arc::new(Mutex::new(0_usize));
    let counter = Arc::clone(&started);
    let hook: super::ThreadStart = Arc::new(move |_| *counter.lock().unwrap() += 1);
    (
        ScanOptions {
            on_thread_start: Some(Arc::clone(&hook)),
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
    // The drop commits nothing the pause did not.
    assert_eq!(indexed(&lib).len(), committed);
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
