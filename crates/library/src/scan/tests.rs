//! Pausing, the endings of a scan, and listing failures, with the scan
//! held mid-file or mid-commit and faults injected through [`Hooks`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{
    BATCH, HEADER_FIELD_BYTES, PassGuard, Scan, ScanError, ScanOptions, ThreadStart, cap,
    read_chart,
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

/// A step of the bests phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// This entry of `records` is about to be looked at and listed.
    List(&'a Path),
    /// This record is about to be decoded.
    Decode(&'a Path),
    /// This record's chart is about to be loaded.
    Load(&'a Path),
    /// This record is about to be evaluated.
    Evaluate(&'a Path),
    /// The phase commits this many items.
    Commit(usize),
}

pub type BestsHook = Arc<dyn Fn(Step<'_>) + Send + Sync>;

/// The unit tests' seams in a scan.
#[derive(Clone, Default)]
pub struct Hooks {
    /// The scan is about to read this file.
    pub(crate) in_hand: Option<PathHook>,
    /// The scan read this file, before it checks it again.
    pub(crate) after_read: Option<PathHook>,
    /// The scan is at this entry of a listing.
    pub(crate) fault: Option<FaultHook>,
    /// The scan commits this many rows and removals.
    pub(crate) before_commit: Option<CommitHook>,
    /// The bests phase is at this step.
    pub(crate) bests: Option<BestsHook>,
    /// The scan's thread is refused, as a system out of threads would.
    pub(crate) refuse_spawn: bool,
}

impl Hooks {
    pub fn bests(&self, step: Step<'_>) {
        if let Some(hook) = &self.bests {
            hook(step);
        }
    }

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

/// A row another loader version wrote is read again, its size and time
/// unchanged, and only such a row.
#[test]
fn a_row_of_another_loader_version_is_read_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let pass = |lib: &Library| {
        lib.scan(std::slice::from_ref(&root), ScanOptions::default())
            .unwrap()
            .finish()
            .unwrap()
    };
    assert_eq!(pass(&lib).read, 3);
    assert_eq!(pass(&lib).read, 0);
    lib.reader()
        .execute(
            "UPDATE charts SET loader = loader - 1, taps = 99
             WHERE path = (SELECT MIN(path) FROM charts)",
            [],
        )
        .unwrap();
    assert_eq!(lib.charts().unwrap()[0].summary.unwrap().taps, 99);
    let again = pass(&lib);
    assert_eq!((again.read, again.unchanged), (1, 2));
    assert_eq!(lib.charts().unwrap()[0].summary.unwrap().taps, 1);
    assert_eq!(pass(&lib).read, 0);
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
        // Disconnected: the scan has ended and holds the hook no more.
        self.held.recv_timeout(SHORT).is_err()
    }

    fn open(&self, n: usize) {
        for _ in 0..n {
            self.go.send(()).unwrap();
        }
    }
}

/// What a pass's guard was told, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Told {
    /// The guard was made.
    Start,
    /// The pass stopped for a pause.
    Paused,
    /// The pass goes on.
    Resumed,
    /// The guard was dropped: the pass ended.
    Dropped,
}

/// Which of a guard's calls wait for a go after saying so, and which
/// panic.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hold {
    pub paused: bool,
    pub resumed: bool,
    pub panic_paused: bool,
    pub panic_resumed: bool,
}

/// A pass guard saying each call from inside it.
struct Recorder {
    told: Sender<Told>,
    go: Arc<Mutex<Receiver<()>>>,
    hold: Hold,
}

impl Recorder {
    fn say(&self, told: Told, hold: bool, panic: bool) {
        let _ = self.told.send(told);
        if hold {
            let _ = self.go.lock().unwrap().recv_timeout(LONG);
        }
        assert!(!panic, "injected guard panic at {told:?}");
    }
}

impl PassGuard for Recorder {
    fn paused(&mut self) {
        self.say(Told::Paused, self.hold.paused, self.hold.panic_paused);
    }

    fn resumed(&mut self) {
        self.say(Told::Resumed, self.hold.resumed, self.hold.panic_resumed);
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.told.send(Told::Dropped);
    }
}

/// The guards of `options`' passes, as they are told.
pub struct Watch {
    told: Receiver<Told>,
    go: Sender<()>,
}

impl Watch {
    /// Gives `options` a guard that says every call, holding or panicking
    /// at the calls `hold` names.
    pub fn on(options: &mut ScanOptions, hold: Hold) -> Self {
        let (told_tx, told) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        let go_rx = Arc::new(Mutex::new(go_rx));
        let told_tx = Mutex::new(told_tx);
        options.on_pass = Some(Arc::new(move || {
            let told = told_tx.lock().unwrap().clone();
            let _ = told.send(Told::Start);
            Box::new(Recorder {
                told,
                go: Arc::clone(&go_rx),
                hold,
            })
        }));
        Self { told, go }
    }

    /// The next call, waiting for it.
    pub fn next(&self) -> Told {
        self.told.recv_timeout(LONG).unwrap()
    }

    /// The calls said so far, waiting for none.
    pub fn seen(&self) -> Vec<Told> {
        self.told.try_iter().collect()
    }

    /// Lets a held call return.
    pub fn open(&self) {
        self.go.send(()).unwrap();
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
    let mut options = with(Hooks {
        in_hand: Some(hook),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();

    // A is read and waits in the batch; B is in hand.
    let a = gate.next();
    gate.open(1);
    let b = gate.next();
    scan.request_pause();
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment while a file is in hand"
    );
    assert_eq!(
        watch.seen(),
        [Told::Start],
        "the guard is told at the pause"
    );
    gate.open(1);
    assert!(scan.wait_paused(LONG));
    // The guard was told before the acknowledgment was published.
    assert_eq!(watch.seen(), [Told::Paused]);

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
    assert_eq!(watch.seen(), [Told::Resumed, Told::Dropped]);
}

/// A pass's guard is made before the pass prepares and dropped before its
/// end is published, with nothing told between when no pause comes.
#[test]
fn the_pass_guard_spans_the_pass() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let mut options = ScanOptions::default();
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
    assert!(scan.wait_finished(LONG));
    assert_eq!(watch.seen(), [Told::Start, Told::Dropped]);
    assert_eq!(scan.finish().unwrap().read, 3);
}

/// A pause requested and withdrawn while the scan commits for it is never
/// waited for: the guard is told nothing, and the pass goes on.
#[test]
fn a_withdrawn_pause_never_tells_the_guard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let (gate, in_hand) = Gate::new();
    let (commits, before_commit) = Gate::new();
    let mut options = with(Hooks {
        in_hand: Some(in_hand),
        before_commit: Some(Arc::new(move |_| before_commit(Path::new("")))),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
    gate.next();
    scan.request_pause();
    gate.open(1);
    commits.next();
    scan.release();
    commits.open(1);
    gate.next();
    assert_eq!(
        watch.seen(),
        [Told::Start],
        "the pause was never waited for"
    );
    gate.open(1);
    while !scan.wait_finished(Duration::from_millis(10)) {
        while gate.held.try_recv().is_ok() {
            gate.open(1);
        }
        while commits.held.try_recv().is_ok() {
            commits.open(1);
        }
    }
    assert_eq!(watch.seen(), [Told::Dropped]);
    assert_eq!(scan.finish().unwrap().read, 3);
}

/// The scan held at a pause with its guard still being told (a file in
/// hand, then the guard's `paused` held), and its state changed meanwhile.
fn held_in_the_guards_pause(hold: Hold) -> (tempfile::TempDir, Library, Scan, Gate, Watch) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let (gate, hook) = Gate::new();
    let mut options = with(Hooks {
        in_hand: Some(hook),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, hold);
    let scan = lib.scan(&[root], options).unwrap();
    assert_eq!(watch.next(), Told::Start);
    gate.next();
    scan.request_pause();
    gate.open(1);
    (dir, lib, scan, gate, watch)
}

/// A release while the guard is being told of the pause: the scan tells
/// it the pause is over before it takes the next file, and never
/// acknowledges the withdrawn request.
#[test]
fn a_release_during_the_guards_pause_resumes_before_working() {
    let (_dir, _lib, scan, gate, watch) = held_in_the_guards_pause(Hold {
        paused: true,
        ..Hold::default()
    });
    assert_eq!(watch.next(), Told::Paused);
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment before the guard returns"
    );
    scan.release();
    watch.open();
    gate.next();
    assert_eq!(watch.seen(), [Told::Resumed], "told before the next file");
    gate.open(1);
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 3);
    assert_eq!(watch.seen(), [Told::Dropped]);
}

/// Another pause requested while the guard is being told of the first:
/// acknowledged only once the guard has returned, and the guard is told
/// once.
#[test]
fn a_second_pause_during_the_guards_pause_is_acknowledged_once_released() {
    let (_dir, _lib, scan, gate, watch) = held_in_the_guards_pause(Hold {
        paused: true,
        ..Hold::default()
    });
    assert_eq!(watch.next(), Told::Paused);
    scan.shared.update(|state| {
        state.release();
        state.request_pause();
    });
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment before the guard returns"
    );
    watch.open();
    assert!(scan.wait_paused(LONG));
    assert!(gate.quiet(), "nothing is read during the pause");
    assert_eq!(watch.seen(), [], "told once");
    scan.release();
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 3);
    assert_eq!(watch.seen(), [Told::Resumed, Told::Dropped]);
}

/// A shutdown while the guard is being told of the pause ends the pass
/// without telling it the pause is over.
#[test]
fn a_shutdown_during_the_guards_pause_ends_without_resuming() {
    let (_dir, _lib, scan, gate, watch) = held_in_the_guards_pause(Hold {
        paused: true,
        ..Hold::default()
    });
    assert_eq!(watch.next(), Told::Paused);
    scan.shared.update(|state| state.shutdown = true);
    watch.open();
    assert!(scan.wait_finished(LONG));
    assert_eq!(watch.seen(), [Told::Dropped]);
    assert!(gate.quiet(), "nothing more is read");
}

/// A shutdown while the guard is being told the pause is over ends the
/// pass before any further work.
#[test]
fn a_shutdown_during_the_guards_resume_ends_the_pass() {
    let (_dir, _lib, scan, gate, watch) = held_in_the_guards_pause(Hold {
        resumed: true,
        ..Hold::default()
    });
    assert!(scan.wait_paused(LONG));
    assert_eq!(watch.seen(), [Told::Paused]);
    scan.release();
    assert_eq!(watch.next(), Told::Resumed);
    scan.shared.update(|state| state.shutdown = true);
    watch.open();
    assert!(scan.wait_finished(LONG));
    assert_eq!(watch.seen(), [Told::Dropped]);
    assert!(gate.quiet(), "nothing more is read");
}

/// A guard that panics when told the pause is over ends the scan, its end
/// published and the guard dropped.
#[test]
fn a_panic_in_the_guards_resume_ends_the_scan() {
    let (_dir, _lib, scan, gate, watch) = held_in_the_guards_pause(Hold {
        panic_resumed: true,
        ..Hold::default()
    });
    assert!(scan.wait_paused(LONG));
    scan.release();
    assert!(scan.wait_finished(LONG), "the end is published");
    assert_eq!(watch.seen(), [Told::Paused, Told::Resumed, Told::Dropped]);
    assert!(gate.quiet(), "nothing more is read");
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Panicked)), "{result:?}");
}

/// A guard that panics when told of the pause ends the scan, its end
/// published and the guard dropped.
#[test]
fn a_panic_in_the_guards_pause_ends_the_scan() {
    let (_dir, _lib, scan, _gate, watch) = held_in_the_guards_pause(Hold {
        panic_paused: true,
        ..Hold::default()
    });
    assert!(scan.wait_finished(LONG), "the end is published");
    assert_eq!(watch.seen(), [Told::Paused, Told::Dropped]);
    let result = bounded(move || scan.finish());
    assert!(matches!(result, Err(ScanError::Panicked)), "{result:?}");
}

/// A scan detached by [`Scan::stop_within`] while its thread is in a step
/// keeps its pass's guard until that thread ends, and drops it then.
#[test]
fn a_detached_scan_drops_its_pass_guard_when_its_thread_ends() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 3);
    let lib = library(dir.path());
    let (gate, hook) = Gate::new();
    let mut options = with(Hooks {
        in_hand: Some(hook),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
    gate.next();
    assert!(!scan.stop_within(SHORT), "the step is still in hand");
    assert_eq!(watch.seen(), [Told::Start], "the guard is kept");
    gate.open(1);
    assert_eq!(watch.next(), Told::Dropped);
}

/// A scan whose thread cannot start returns the error, and its pass's
/// guard is never made.
#[test]
fn a_scan_that_cannot_start_never_calls_its_pass_hook() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    write_charts(&root, 1);
    let lib = library(dir.path());
    let mut options = with(Hooks {
        refuse_spawn: true,
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    assert!(lib.scan(std::slice::from_ref(&root), options).is_err());
    assert_eq!(watch.seen(), []);
    // Its lease was released with the call.
    drop(lib.scan(&[root], ScanOptions::default()).unwrap());
}

/// A pass guard's maker that panics ends the scan.
#[test]
fn a_panic_in_the_pass_hook_ends_the_scan() {
    panicking_scan(ScanOptions {
        on_pass: Some(Arc::new(|| panic!("injected pass hook panic"))),
        ..ScanOptions::default()
    });
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
    let mut options = with(Hooks {
        in_hand: Some(hook),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
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
    assert_eq!(
        watch.seen(),
        [Told::Start, Told::Paused],
        "the guard is told once, still paused"
    );
    // Released, and the scan takes the next file in hand before the new
    // request: the request is acknowledged once that file is done.
    scan.release();
    gate.next();
    assert_eq!(watch.seen(), [Told::Resumed], "told before the next file");
    scan.request_pause();
    assert!(
        !scan.wait_paused(SHORT),
        "no acknowledgment while a file is in hand"
    );
    gate.open(1);
    assert!(scan.wait_paused(LONG));
    assert!(gate.quiet(), "nothing is read during the pause");
    assert_eq!(watch.seen(), [Told::Paused]);
    scan.release();
    drain(&scan, &gate);
    assert_eq!(scan.finish().unwrap().read, 3);
    assert_eq!(watch.seen(), [Told::Resumed, Told::Dropped]);
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
    let mut options = ScanOptions::default();
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(std::slice::from_ref(&root), options).unwrap();
    assert!(scan.wait_finished(LONG));
    assert_eq!(watch.seen(), [Told::Start, Told::Dropped]);
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
    let mut options = with(Hooks {
        in_hand: Some(Arc::new(|_: &Path| panic!("injected panic mid-file"))),
        ..Hooks::default()
    });
    let watch = Watch::on(&mut options, Hold::default());
    panicking_scan(options);
    assert_eq!(watch.seen(), [Told::Start, Told::Dropped]);
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
    let (mut options, started, hook) = counted();
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
    scan.request_pause();
    assert!(scan.wait_paused(LONG));
    let committed = indexed(&lib).len();
    // Released and stopped at once, before the thread can see the release:
    // it ends without being told the pause is over.
    scan.shared.update(|state| {
        state.release();
        state.shutdown = true;
    });
    bounded(move || drop(scan));
    assert_eq!(watch.seen(), [Told::Start, Told::Paused, Told::Dropped]);
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
        .execute_batch("PRAGMA user_version = 8;")
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
    assert_eq!(opened, crate::Opened::Rebuilt { old_version: 8 });
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

#[test]
fn a_link_to_a_missing_file_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("alias.db");
    if link_file(&dir.path().join("missing.db"), &alias).is_none() {
        println!("skipped: this system does not allow creating file links");
        return;
    }
    // As written, and with a trailing separator, which makes resolution
    // follow the link while the file name drops the separator.
    let mut spellings = vec![alias.clone()];
    for separator in ["/", "\\"] {
        if cfg!(windows) || separator == "/" {
            let mut spelled = alias.clone().into_os_string();
            spelled.push(separator);
            spellings.push(PathBuf::from(spelled));
        }
    }
    for spelling in &spellings {
        let opened = Library::open(spelling);
        assert!(
            matches!(opened, Err(LibraryError::Resolve { .. })),
            "{}: {opened:?}",
            spelling.display()
        );
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{}: the link is still there",
            spelling.display()
        );
        assert!(!dir.path().join("missing.db").exists());
    }
}

/// A library indexing `BATCH + 44` files, all of them since deleted, so the
/// next scan removes their rows in two commits.
fn all_gone() -> (tempfile::TempDir, PathBuf, Library) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("songs");
    let charts = write_charts(&root, BATCH + 44);
    let lib = library(dir.path());
    lib.scan(std::slice::from_ref(&root), ScanOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    for path in charts {
        fs::remove_file(path).unwrap();
    }
    (dir, root, lib)
}

fn gated_commits(gate_hook: PathHook) -> ScanOptions {
    with(Hooks {
        before_commit: Some(Arc::new(move |_| gate_hook(Path::new("")))),
        ..Hooks::default()
    })
}

#[test]
fn a_pause_during_removals_holds_the_later_batches() {
    let (_dir, root, lib) = all_gone();
    let (commits, hook) = Gate::new();
    let mut options = gated_commits(hook);
    let watch = Watch::on(&mut options, Hold::default());
    let scan = lib.scan(&[root], options).unwrap();
    commits.next();
    scan.request_pause();
    commits.open(1);
    assert!(scan.wait_paused(LONG));
    assert_eq!(watch.seen(), [Told::Start, Told::Paused]);
    assert!(
        commits.quiet(),
        "no later batch is committed during the pause"
    );
    assert_eq!(indexed(&lib).len(), 44);
    scan.release();
    commits.next();
    commits.open(1);
    assert_eq!(
        scan.finish().unwrap().removed,
        u64::try_from(BATCH + 44).unwrap()
    );
    assert!(indexed(&lib).is_empty());
}

#[test]
fn a_drop_during_removals_runs_no_later_batch() {
    let (_dir, root, lib) = all_gone();
    let (commits, hook) = Gate::new();
    let scan = lib.scan(&[root], gated_commits(hook)).unwrap();
    commits.next();
    let shared = Arc::clone(&scan.shared);
    let dropped = thread::spawn(move || drop(scan));
    {
        let state = shared.lock();
        let (_state, timeout) = shared
            .changed
            .wait_timeout_while(state, LONG, |state| !state.shutdown)
            .unwrap();
        assert!(!timeout.timed_out(), "the drop is seen");
    }
    commits.open(1);
    assert!(commits.quiet(), "no later batch is committed");
    bounded(move || dropped.join().unwrap());
    assert_eq!(indexed(&lib).len(), 44);
}
