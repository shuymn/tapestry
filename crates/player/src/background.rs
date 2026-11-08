//! The loudness of a chart whose load was cancelled during its
//! measurement, measured in the background (design.md "Algorithms",
//! per-chart volume normalization), so the chart's next choice finds it.
//!
//! - **One thread** in background mode measures the jobs one at a time in
//!   the order they came, each on a speculative permit of the coordinator
//!   ([`game::coord`]), held only while it measures. A job is the chart's
//!   file and what its 1× read needs; the thread reads the chart again at
//!   1×, decodes its own 1× bank on itself and analyzes it, asking before
//!   each file, packet, chunk and block whether to stop. It never keeps a
//!   cancelled load's bank.
//! - **Priority** is checked ([`crate::priority`]): when the thread cannot
//!   enter background mode, it runs no job and ends, said once, and a
//!   cancelled load's chart is measured at its next choice, as when the
//!   thread cannot start.
//! - **Promotion** follows foreground interest: only while a load waits for
//!   the chart's measurement ([`Worker::promote`], from the shelf's wait)
//!   does it run on a foreground permit, the thread out of background mode.
//!   A choice alone (a practice page before its Start) promotes nothing.
//!   When the wait ends without the measurement (quit), it is demoted
//!   ([`Worker::demote`]): the permit goes back to the speculative class
//!   and the thread enters background mode again, checked, while the
//!   measurement goes on. A refused leave is said and the measurement goes
//!   on at background priority; a refused entry is said, the measurement
//!   goes on, and the thread ends after it, its result kept.
//! - **The queue** holds at most [`WAITING`] jobs besides the running one,
//!   none of a chart already waiting or running. When full, the oldest
//!   waiting job is dropped, never the running one; a dropped chart is
//!   measured at its next choice, as one never queued.
//! - **Pausing** follows the scan's: a choice requests a pause, each
//!   READY state waits for its acknowledgment, and selection and each result
//!   release it, so the thread never measures during a play. A measurement
//!   stopped by a pause starts again from the beginning once released; the
//!   running measurement of the chart just chosen goes on through the
//!   pause, since that chart's load waits for it.
//! - **Results** wait in the shared state until the game thread takes
//!   them; the shelf keeps each in memory and stores it where it stores a
//!   load's own. A measurement that stops, fails or panics gives nothing.
//! - **The end** of the run stops the thread at its next check; nothing
//!   partial is kept, and the wait for it is bounded
//!   ([`Worker::stop_within`]). One guard publishes the thread's end
//!   however it ends, so no waiter is left behind.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audio::{BankError, BankLimits, Speed};
use game::coord::{self, Class, Coordinator, Promoter};
use game::{Chain, ChartSource};
use library::{LoudnessKey, LoudnessRow};

use crate::loudness::{self, Analyzed};
use crate::priority::ThreadMode;

/// The most jobs waiting besides the running one.
pub const WAITING: usize = 4;

/// A chart to measure: its file and what reading it again at 1× needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The chart's file, its seed and its SHA-256.
    pub source: ChartSource,
    /// The output's rate, which the bank is decoded at.
    pub rate: u32,
    /// The chart's name in the log.
    pub name: String,
}

impl Job {
    /// The chart's file: what tells jobs apart.
    pub fn path(&self) -> &Path {
        self.source.path()
    }
}

/// How a job's measurement ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Measured: the key and the row to keep.
    Measured(LoudnessKey, LoudnessRow),
    /// It was asked to stop (a pause, the end of the run) and keeps
    /// nothing.
    Stopped,
    /// Why it gave no measurement.
    Failed(String),
}

/// Measures a job, asking the check it is given before each block whether
/// to stop; [`Outcome::Stopped`] once it says so.
pub type Measure = Box<dyn FnMut(&Job, &(dyn Fn() -> bool + Sync)) -> Outcome + Send>;

/// A job that ended with or without a measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    /// The chart's file.
    pub path: PathBuf,
    /// Its key and measurement, if it gave one.
    pub measured: Option<(LoudnessKey, LoudnessRow)>,
}

/// What became of a job handed to the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Queued {
    /// It waits.
    Added,
    /// A job of the same chart waits or runs: this one is not queued.
    Already,
    /// It waits, and the oldest waiting job, of this chart, was dropped.
    Dropped(PathBuf),
    /// The thread has ended or is ending: nothing is queued.
    Ended,
}

#[derive(Debug, Default)]
struct State {
    waiting: VecDeque<Job>,
    /// The job taken from the queue: measured, or stopped by a pause and
    /// measured again from the beginning once it is released.
    current: Option<Job>,
    /// A pause is requested and not released.
    pause: bool,
    /// The chart chosen with the pause: its running measurement goes on.
    chosen: Option<PathBuf>,
    /// Counts pause requests, so each is acknowledged on its own.
    pause_gen: u64,
    /// The latest request the thread acknowledged.
    acked_gen: u64,
    /// The run is ending: stop at the next check.
    shutdown: bool,
    /// The thread has ended, or is ending and will do nothing more.
    finished: bool,
    /// Promotes and demotes the running measurement.
    promoter: Option<Promoter>,
    /// The chart a load waits for: its measurement runs in the foreground.
    wanted: Option<PathBuf>,
    /// Jobs ended and not yet taken.
    results: Vec<Finished>,
}

impl State {
    /// Whether the job of `path` may be measured now.
    fn may_run(&self, path: &Path) -> bool {
        !self.pause || self.chosen.as_deref() == Some(path)
    }

    /// Whether the thread measures nothing until the release: it
    /// acknowledged the latest request, or holds no job (it takes none
    /// while paused), or has ended.
    fn paused(&self) -> bool {
        self.finished
            || (self.pause && (self.acked_gen == self.pause_gen || self.current.is_none()))
    }

    fn holds(&self, path: &Path) -> bool {
        self.current.as_ref().is_some_and(|j| j.path() == path)
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

    fn update<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        let out = f(&mut self.lock());
        self.changed.notify_all();
        out
    }

    /// Waits up to `timeout` for `done`; whether it holds.
    fn wait_for(&self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.lock();
        loop {
            if done(&state) {
                return true;
            }
            let left = deadline.map_or(Duration::MAX, |d| {
                d.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Publishes the thread's end however it ends, a panic included, and
/// wakes every waiter.
struct Ending(Arc<Shared>);

impl Drop for Ending {
    fn drop(&mut self) {
        self.0.update(|state| {
            state.finished = true;
            state.current = None;
            state.waiting.clear();
        });
    }
}

/// The background measurement's thread. Dropping it stops the thread and
/// joins it; [`Worker::stop_within`] bounds that wait.
pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("state", &*self.shared.lock())
            .finish_non_exhaustive()
    }
}

impl Worker {
    /// Starts the thread, which enters background mode through `mode` and
    /// then measures each job with `measure`, on a speculative permit of
    /// `coord`.
    ///
    /// # Errors
    ///
    /// Why the thread cannot start.
    pub fn start(
        measure: Measure,
        coord: Coordinator,
        mode: Arc<dyn ThreadMode>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let ending = Ending(Arc::clone(&shared));
        let thread = thread::Builder::new()
            .name("loudness-background".to_owned())
            .spawn(move || {
                let (ending, mut measure) = (ending, measure);
                if entered(&*mode) {
                    run(&ending.0, &mut measure, &coord, &*mode);
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Queues `job` unless one of its chart waits or runs; when the queue
    /// is full, the oldest waiting job is dropped.
    pub fn queue(&self, job: Job) -> Queued {
        self.shared.update(|s| {
            if s.finished || s.shutdown {
                return Queued::Ended;
            }
            if s.holds(job.path()) || s.waiting.iter().any(|j| j.path() == job.path()) {
                return Queued::Already;
            }
            s.waiting.push_back(job);
            if s.waiting.len() > WAITING
                && let Some(old) = s.waiting.pop_front()
            {
                return Queued::Dropped(old.path().to_owned());
            }
            Queued::Added
        })
    }

    /// Takes the waiting job of `path` out of the queue, if one waits: its
    /// load measures the chart itself. `true` if one did.
    pub fn take_waiting(&self, path: &Path) -> bool {
        self.shared.update(|s| {
            let before = s.waiting.len();
            s.waiting.retain(|j| j.path() != path);
            s.waiting.len() != before
        })
    }

    /// Whether the thread holds the job of `path`: measuring it, or about
    /// to measure it again.
    pub fn holds(&self, path: &Path) -> bool {
        self.shared.lock().holds(path)
    }

    /// Waits up to `timeout` for the thread to be done with the job of
    /// `path`; `true` once it is (its result, if any, is then among
    /// [`take_results`](Self::take_results)).
    pub fn wait_done(&self, path: &Path, timeout: Duration) -> bool {
        self.shared.wait_for(timeout, |s| !s.holds(path))
    }

    /// The jobs ended since the last call.
    pub fn take_results(&self) -> Vec<Finished> {
        std::mem::take(&mut self.shared.lock().results)
    }

    /// Asks the thread to pause: it stops its measurement at the next
    /// check and acknowledges, unless the measurement is of `chosen`'s
    /// chart, which goes on to its end first. Returns at once.
    pub fn request_pause(&self, chosen: Option<&Path>) {
        self.shared.update(|s| {
            s.pause = true;
            s.pause_gen += 1;
            s.chosen = chosen.map(Path::to_owned);
        });
    }

    /// A load waits for the measurement of `path`: it runs on a foreground
    /// permit, the thread out of background mode, from its next check (or
    /// its start) until [`demote`](Self::demote).
    pub fn promote(&self, path: &Path) {
        self.shared.update(|s| {
            s.wanted = Some(path.to_owned());
            if s.holds(path)
                && let Some(promoter) = &s.promoter
            {
                promoter.promote();
            }
        });
    }

    /// The load no longer waits for `path`'s measurement: it goes on in
    /// the background.
    pub fn demote(&self, path: &Path) {
        self.shared.update(|s| {
            if s.wanted.as_deref() == Some(path) {
                s.wanted = None;
                if let Some(promoter) = &s.promoter {
                    promoter.demote();
                }
            }
        });
    }

    /// Waits up to `timeout` for the latest pause request to be
    /// acknowledged. `true` once the thread measures nothing.
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        self.shared.wait_for(timeout, State::paused)
    }

    /// Releases a pause: the thread goes on.
    pub fn release(&self) {
        self.shared.update(|s| {
            s.pause = false;
            s.chosen = None;
        });
    }

    /// Asks the thread to stop at its next check, keeping nothing of the
    /// measurement in hand. Returns at once.
    pub fn stop(&self) {
        self.shared.update(|s| s.shutdown = true);
    }

    /// Stops the thread, waiting at most `timeout` for it to end: `true`
    /// once it ended, `false` when it had not, and it is left to end on
    /// its own (a read stalled on its file system); with the jobs it
    /// completed and nobody took.
    pub fn stop_within(mut self, timeout: Duration) -> (bool, Vec<Finished>) {
        self.stop();
        let ended = self.shared.wait_for(timeout, |s| s.finished);
        // Detached either way, as the scan's: what is left of the thread
        // may still block, and the drop finds no thread to join.
        drop(self.thread.take());
        (ended, self.take_results())
    }

    /// Waits up to `timeout` until `n` results wait to be taken (tests).
    #[cfg(test)]
    pub fn wait_results(&self, n: usize, timeout: Duration) -> bool {
        self.shared.wait_for(timeout, |s| s.results.len() >= n)
    }

    /// Waits up to `timeout` for the thread's end (tests).
    #[cfg(test)]
    pub fn wait_ended(&self, timeout: Duration) -> bool {
        self.shared.wait_for(timeout, |s| s.finished)
    }

    /// Whether the thread holds a job, and whether it has none at all,
    /// held or waiting (dev-tools, tests).
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn busy(&self) -> (bool, bool) {
        let s = self.shared.lock();
        (
            s.current.is_some(),
            s.current.is_none() && s.waiting.is_empty(),
        )
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread's loop: takes the jobs in order while nothing pauses it,
/// acknowledges each pause, and ends at the shutdown.
fn run(shared: &Shared, measure: &mut Measure, coord: &Coordinator, mode: &dyn ThreadMode) {
    loop {
        let (job, task) = {
            let mut s = shared.lock();
            loop {
                if s.shutdown {
                    return;
                }
                if s.current.is_none() && !s.pause {
                    s.current = s.waiting.pop_front();
                }
                if let Some(job) = &s.current
                    && s.may_run(job.path())
                {
                    let job = job.clone();
                    let task = coord.task::<Outcome>(Class::Speculative);
                    let promoter = task.promoter();
                    // Waited for before it started: promoted from its start.
                    if s.wanted.as_deref() == Some(job.path()) {
                        promoter.promote();
                    }
                    s.promoter = Some(promoter);
                    break (job, task);
                }
                if s.pause && s.acked_gen != s.pause_gen {
                    s.acked_gen = s.pause_gen;
                    shared.changed.notify_all();
                }
                s = shared
                    .changed
                    .wait(s)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let path = job.path().to_owned();
        // Whether the measurement was told to stop: a stop it was told is
        // a pause's or the end's, whatever the state is when it returns.
        let told = AtomicBool::new(false);
        let stop = || {
            let s = shared.lock();
            let stop = s.shutdown || !s.may_run(&path);
            if stop {
                told.store(true, Ordering::Relaxed);
            }
            stop
        };
        // Whether the thread is out of background mode (promoted), and
        // whether entering it again was refused.
        let (left, refused) = (AtomicBool::new(false), AtomicBool::new(false));
        let on_class = |class: Class| match class {
            Class::Foreground => match mode.leave_background() {
                Ok(()) => left.store(true, Ordering::Relaxed),
                Err(e) => warn(&format!(
                    "loudness: {}: background mode could not be left ({e}); measured at background priority",
                    job.name
                )),
            },
            _ if left.load(Ordering::Relaxed) => {
                left.store(false, Ordering::Relaxed);
                if !entered(mode) {
                    refused.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        };
        let outcome = match task.run(&stop, &on_class, |ctx| measure(&job, &|| ctx.checkpoint())) {
            coord::Outcome::Done(outcome) => outcome,
            // The wait for its permit stopped: as a stop it was told.
            coord::Outcome::Cancelled if told.load(Ordering::Relaxed) => Outcome::Stopped,
            coord::Outcome::Cancelled => Outcome::Failed("no speculative permit".to_owned()),
            coord::Outcome::Panicked(p) => Outcome::Failed(format!(
                "the background measurement stopped unexpectedly: {p}"
            )),
        };
        // Back in background mode before the next job, or no more jobs.
        let resumed =
            !refused.load(Ordering::Relaxed) && (!left.load(Ordering::Relaxed) || entered(mode));
        let mut s = shared.lock();
        s.promoter = None;
        let measured = match outcome {
            // Kept as the job in hand, even when the pause was released
            // meanwhile: measured again from the beginning once it may be,
            // or dropped at the shutdown.
            Outcome::Stopped if told.load(Ordering::Relaxed) => {
                if resumed {
                    continue;
                }
                return;
            }
            Outcome::Measured(key, row) => Some((key, row)),
            Outcome::Stopped => {
                loudness::log_line(
                    &format!(
                        "loudness: {}: the background measurement stopped by itself",
                        job.name
                    ),
                    true,
                );
                None
            }
            Outcome::Failed(why) => {
                loudness::log_line(&format!("loudness: {}: {why}", job.name), true);
                None
            }
        };
        s.current = None;
        s.results.push(Finished { path, measured });
        drop(s);
        shared.changed.notify_all();
        if !resumed {
            return;
        }
    }
}

/// Enters background mode through `mode`; `false`, said once (the thread
/// then ends), when the system refuses it.
fn entered(mode: &dyn ThreadMode) -> bool {
    match mode.enter_background() {
        Ok(()) => true,
        Err(e) => {
            warn(&format!(
                "loudness: background mode refused ({e}); the background measurement is off for this run"
            ));
            false
        }
    }
}

/// A warning on the console and in the log.
fn warn(line: &str) {
    eprintln!("{line}");
    log::warn!("{line}");
}

/// The player's measurement of a job: the chart read again at 1× with the
/// player's parts, its 1× bank decoded on this thread, and the analysis,
/// `stop` asked before the read, each file, packet, chunk and block.
pub fn measure(job: &Job, stop: &(dyn Fn() -> bool + Sync)) -> Outcome {
    if stop() {
        return Outcome::Stopped;
    }
    loudness::log_line(
        &format!("loudness: measuring {} in the background", job.name),
        false,
    );
    // A script's gate (dev-tools).
    #[cfg(feature = "dev-tools")]
    if !crate::dev::background_gate(stop) {
        return Outcome::Stopped;
    }
    // The player's parts are the standard ones: what reads its charts.
    let parts = game::Parts::unsaved(&game::PlayerSettings::default());
    let chart = match job.source.read(&parts, Speed::NORMAL) {
        Ok(chart) => chart,
        Err(e) => return Outcome::Failed(format!("the chart cannot be read again: {}", Chain(&e))),
    };
    let limits = BankLimits {
        max_decode_threads: 1,
        ..BankLimits::default()
    };
    let bank = match audio::load_bank_reporting(
        &chart,
        job.rate,
        Speed::NORMAL,
        &limits,
        &|_, _| {},
        stop,
    ) {
        Ok(bank) => Arc::new(bank),
        Err(BankError::Cancelled) => return Outcome::Stopped,
        Err(e) => return Outcome::Failed(format!("the 1× sounds cannot be read: {e:#}")),
    };
    let Some(key) = loudness::key(&chart, &bank) else {
        return Outcome::Failed("the sounds have no digest".to_owned());
    };
    match loudness::analyze(&chart, bank, &mut || stop()) {
        Analyzed::Done(m, took) => {
            loudness::log_measured(&job.name, &m, took, " in the background");
            Outcome::Measured(key, loudness::row(&m))
        }
        Analyzed::Cancelled => Outcome::Stopped,
        Analyzed::Failed(why) => Outcome::Failed(why),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use game::ChartSource;
    use game::coord::{Caps, Class, Coordinator};
    use library::{LoudnessKey, LoudnessRow};

    use super::{Finished, Job, Measure, Outcome, Queued, Worker};
    use crate::priority::{PriorityError, ThreadMode, Unchanged};

    /// How long a test waits for what must happen however late a loaded
    /// machine runs it.
    static WATCHDOG: std::sync::LazyLock<Duration> = std::sync::LazyLock::new(|| {
        // `TAPESTRY_TEST_WATCHDOG` (seconds) shortens it for mutation runs.
        let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
        Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
    });

    pub(crate) fn job(name: &str) -> Job {
        Job {
            source: ChartSource::new(PathBuf::from(name), 0, [0; 32]),
            rate: 8_000,
            name: name.to_owned(),
        }
    }

    pub(crate) fn key(path: &Path) -> LoudnessKey {
        let id = path.to_string_lossy().bytes().next().unwrap_or(0);
        LoudnessKey {
            resolved_hash: [id; 32],
            sounds: [id; 32],
            mixer_version: audio::MIXER_VERSION,
            analysis_version: audio::ANALYSIS_VERSION,
            rate: 8_000,
        }
    }

    pub(crate) const ROW: LoudnessRow = LoudnessRow {
        integrated: Some(-20.0),
        true_peak: 0.5,
        over_range: false,
    };

    /// What the fake measurement saw and does.
    #[derive(Default)]
    struct Probe {
        /// A measurement is in hand.
        inside: AtomicBool,
        /// Blocks measured, all jobs.
        blocks: AtomicUsize,
        /// Each measurement begun, by its chart.
        starts: Mutex<Vec<PathBuf>>,
        /// Measurements end after 3 blocks once open; until then, they
        /// measure block after block.
        open: AtomicBool,
        /// A job of this chart panics.
        panic_on: Mutex<Option<PathBuf>>,
        /// Measurements ignore the stop (a read stalled on its file
        /// system) until open.
        stuck: AtomicBool,
    }

    impl Probe {
        fn starts(&self) -> Vec<PathBuf> {
            self.starts.lock().unwrap().clone()
        }
    }

    struct Inside<'a>(&'a AtomicBool);

    impl Drop for Inside<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// A measurement as the real one is to the worker: it asks `stop`
    /// before each block of 1 ms, and stops there once told to, keeping
    /// nothing.
    fn fake(probe: &Arc<Probe>) -> Measure {
        let probe = Arc::clone(probe);
        Box::new(move |job, stop| {
            probe.starts.lock().unwrap().push(job.path().to_owned());
            probe.inside.store(true, Ordering::SeqCst);
            let _inside = Inside(&probe.inside);
            let fails = probe.panic_on.lock().unwrap().as_deref() == Some(job.path());
            assert!(!fails, "a measurement failed (test)");
            let mut blocks = 0;
            loop {
                let open = probe.open.load(Ordering::SeqCst);
                let stuck = probe.stuck.load(Ordering::SeqCst) && !open;
                if !stuck && stop() {
                    return Outcome::Stopped;
                }
                if open && blocks >= 3 {
                    return Outcome::Measured(key(job.path()), ROW);
                }
                blocks += 1;
                probe.blocks.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    }

    fn until(what: &str, done: impl Fn() -> bool) {
        let begun = Instant::now();
        while !done() {
            assert!(begun.elapsed() < *WATCHDOG, "{what} never happened");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    const CAPS: Caps = Caps {
        foreground: 4,
        speculative: 1,
    };

    /// A worker on a coordinator of its own whose thread keeps its
    /// priority.
    fn start(measure: Measure) -> Worker {
        Worker::start(measure, Coordinator::new(CAPS), Arc::new(Unchanged)).unwrap()
    }

    /// A call to the background mode, and whether it was accepted.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum ModeCall {
        Enter(bool),
        Leave(bool),
    }

    /// A background mode that counts its calls, says each on a channel
    /// once it is made, and refuses what it is told to.
    #[derive(Default)]
    pub(crate) struct Mode {
        pub(crate) enters: AtomicUsize,
        pub(crate) leaves: AtomicUsize,
        /// Entries from this one on (counted from 1) are refused; 0 for
        /// none.
        pub(crate) refuse_entry_from: AtomicUsize,
        pub(crate) refuse_leave: AtomicBool,
        /// The entry panics.
        pub(crate) panics: AtomicBool,
        calls: Mutex<Option<Sender<ModeCall>>>,
    }

    impl Mode {
        /// The calls from now on, each once it is made.
        pub(crate) fn calls(&self) -> Calls {
            let (tx, rx) = mpsc::channel();
            *self.calls.lock().unwrap() = Some(tx);
            Calls(rx)
        }

        fn said(&self, call: ModeCall) {
            if let Some(tx) = &*self.calls.lock().unwrap() {
                let _ = tx.send(call);
            }
        }
    }

    impl ThreadMode for Mode {
        fn enter_background(&self) -> Result<(), PriorityError> {
            assert!(
                !self.panics.load(Ordering::SeqCst),
                "the entry failed (test)"
            );
            let n = self.enters.fetch_add(1, Ordering::SeqCst) + 1;
            let from = self.refuse_entry_from.load(Ordering::SeqCst);
            let refused = from != 0 && n >= from;
            self.said(ModeCall::Enter(!refused));
            if refused {
                return Err(PriorityError("refused (test)".to_owned()));
            }
            Ok(())
        }

        fn leave_background(&self) -> Result<(), PriorityError> {
            self.leaves.fetch_add(1, Ordering::SeqCst);
            let refused = self.refuse_leave.load(Ordering::SeqCst);
            self.said(ModeCall::Leave(!refused));
            if refused {
                return Err(PriorityError("refused (test)".to_owned()));
            }
            Ok(())
        }
    }

    /// The mode's calls, in order.
    pub(crate) struct Calls(Receiver<ModeCall>);

    impl Calls {
        /// The next call.
        pub(crate) fn next(&self) -> ModeCall {
            self.0
                .recv_timeout(*WATCHDOG)
                .expect("the background mode was never called")
        }

        /// No call made since the last one taken.
        pub(crate) fn none(&self) -> bool {
            self.0.try_recv().is_err()
        }
    }

    /// What a stepped measurement said.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Said {
        /// It passed a check (counted from 0) and waits for the next step.
        Block(usize),
        /// A check told it to stop.
        Stopped,
    }

    /// A measurement the test steps block by block: at each block it asks
    /// its check, says what it found, and waits for the test to step it
    /// or to finish it (then measured). A test that drops its stepper
    /// finishes it, so a failing test leaves no thread waiting.
    pub(crate) struct Stepper {
        go: Mutex<Option<Sender<bool>>>,
        said: Mutex<Receiver<Said>>,
    }

    impl Stepper {
        /// The stepper and the measurement it steps.
        pub(crate) fn new() -> (Self, Measure) {
            let (go_tx, go_rx) = mpsc::channel::<bool>();
            let (said_tx, said_rx) = mpsc::channel();
            let measure: Measure = Box::new(move |job, stop| {
                let mut n = 0;
                loop {
                    if stop() {
                        let _ = said_tx.send(Said::Stopped);
                        return Outcome::Stopped;
                    }
                    let _ = said_tx.send(Said::Block(n));
                    match go_rx.recv() {
                        Ok(true) => n += 1,
                        Ok(false) | Err(_) => return Outcome::Measured(key(job.path()), ROW),
                    }
                }
            });
            let stepper = Self {
                go: Mutex::new(Some(go_tx)),
                said: Mutex::new(said_rx),
            };
            (stepper, measure)
        }

        /// What the measurement said next.
        pub(crate) fn said(&self) -> Said {
            self.said
                .lock()
                .unwrap()
                .recv_timeout(*WATCHDOG)
                .expect("the measurement said nothing")
        }

        /// One more block, through its check.
        pub(crate) fn step(&self) {
            if let Some(go) = &*self.go.lock().unwrap() {
                let _ = go.send(true);
            }
        }

        /// The measurement ends, measured.
        pub(crate) fn finish(&self) {
            if let Some(go) = &*self.go.lock().unwrap() {
                let _ = go.send(false);
            }
        }
    }

    /// A worker measuring with a probe, on `coord`, with `mode`.
    fn moded(coord: &Coordinator, mode: &Arc<Mode>) -> (Worker, Arc<Probe>) {
        let probe = Arc::new(Probe::default());
        let mode: Arc<dyn ThreadMode> = Arc::clone(mode) as _;
        let worker = Worker::start(fake(&probe), coord.clone(), mode).unwrap();
        (worker, probe)
    }

    fn held(coord: &Coordinator) -> (usize, usize) {
        (
            coord.held(Class::Speculative),
            coord.held(Class::Foreground),
        )
    }

    fn started() -> (Worker, Arc<Probe>) {
        let probe = Arc::new(Probe::default());
        (start(fake(&probe)), probe)
    }

    fn paths(results: &[Finished]) -> Vec<PathBuf> {
        results.iter().map(|f| f.path.clone()).collect()
    }

    fn all(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// Waits until every job ended; their results in order.
    fn drain(worker: &Worker) -> Vec<Finished> {
        let mut results = Vec::new();
        let begun = Instant::now();
        loop {
            results.extend(worker.take_results());
            if worker.busy().1 {
                results.extend(worker.take_results());
                return results;
            }
            assert!(begun.elapsed() < *WATCHDOG, "the jobs never ended");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// With a job running and four waiting, a fifth drops the oldest
    /// waiting one, never the running one; a job of a chart waiting or
    /// running is not queued again. The jobs run one at a time in order.
    #[test]
    fn the_queue_drops_the_oldest_waiting_job_and_never_the_running_one() {
        let (worker, probe) = started();
        assert_eq!(worker.queue(job("a")), Queued::Added);
        until("a's start", || probe.starts() == all(&["a"]));
        for name in ["b", "c", "d", "e"] {
            assert_eq!(worker.queue(job(name)), Queued::Added);
        }
        assert_eq!(worker.queue(job("f")), Queued::Dropped(PathBuf::from("b")));
        assert_eq!(worker.queue(job("a")), Queued::Already);
        assert_eq!(worker.queue(job("c")), Queued::Already);
        assert_eq!(worker.queue(job("f")), Queued::Already);
        probe.open.store(true, Ordering::SeqCst);
        let results = drain(&worker);
        assert_eq!(paths(&results), all(&["a", "c", "d", "e", "f"]));
        assert!(
            results
                .iter()
                .all(|f| f.measured == Some((key(&f.path), ROW)))
        );
        assert_eq!(probe.starts(), all(&["a", "c", "d", "e", "f"]));
    }

    /// A pause stops the running measurement at its next block and is
    /// acknowledged (before a READY state): nothing is measured, and no
    /// waiting job starts, until the release (selection), after which the
    /// stopped job is measured again from the beginning.
    #[test]
    fn a_pause_stops_the_measurement_until_the_release() {
        let (worker, probe) = started();
        worker.queue(job("a"));
        until("a's start", || !probe.starts().is_empty());
        worker.request_pause(None);
        assert!(worker.wait_paused(*WATCHDOG));
        assert!(!probe.inside.load(Ordering::SeqCst));
        worker.queue(job("b"));
        let blocks = probe.blocks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(probe.blocks.load(Ordering::SeqCst), blocks);
        assert_eq!(probe.starts(), all(&["a"]));
        assert!(worker.take_results().is_empty());
        // A second request, as a retry's READY state makes, is acknowledged
        // too.
        worker.release();
        worker.request_pause(None);
        assert!(worker.wait_paused(*WATCHDOG));
        worker.release();
        until("a's second start", || probe.starts().len() >= 2);
        probe.open.store(true, Ordering::SeqCst);
        let results = drain(&worker);
        assert_eq!(paths(&results), all(&["a", "b"]));
        assert_eq!(probe.starts().last(), Some(&PathBuf::from("b")));
    }

    /// The running measurement of the chart chosen with the pause goes on
    /// through it, and the pause is acknowledged once it ends; a waiting
    /// job does not start meanwhile.
    #[test]
    fn the_chosen_charts_measurement_goes_on_through_the_pause() {
        let (worker, probe) = started();
        worker.queue(job("a"));
        until("a's start", || !probe.starts().is_empty());
        worker.queue(job("b"));
        worker.request_pause(Some(Path::new("a")));
        assert!(!worker.wait_paused(Duration::from_millis(100)));
        let blocks = probe.blocks.load(Ordering::SeqCst);
        until("a's next blocks", || {
            probe.blocks.load(Ordering::SeqCst) > blocks + 5
        });
        assert!(worker.holds(Path::new("a")));
        probe.open.store(true, Ordering::SeqCst);
        assert!(worker.wait_done(Path::new("a"), *WATCHDOG));
        assert!(worker.wait_paused(*WATCHDOG));
        assert_eq!(paths(&worker.take_results()), all(&["a"]));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(probe.starts(), all(&["a"]));
        worker.release();
        assert_eq!(paths(&drain(&worker)), all(&["b"]));
    }

    /// A load takes its chart's waiting job out of the queue; the running
    /// one is not taken.
    #[test]
    fn a_load_takes_its_charts_waiting_job() {
        let (worker, probe) = started();
        worker.queue(job("a"));
        until("a's start", || !probe.starts().is_empty());
        worker.queue(job("b"));
        assert!(!worker.take_waiting(Path::new("a")));
        assert!(worker.take_waiting(Path::new("b")));
        assert!(!worker.take_waiting(Path::new("b")));
        probe.open.store(true, Ordering::SeqCst);
        assert_eq!(paths(&drain(&worker)), all(&["a"]));
    }

    /// The end of the run stops the measurement at its next block and
    /// keeps nothing of it; a measurement that does not ask (stalled on
    /// its file system) bounds the wait instead.
    #[test]
    fn the_end_stops_the_measurement_within_its_bound() {
        let (worker, probe) = started();
        worker.queue(job("a"));
        worker.queue(job("b"));
        until("a's start", || !probe.starts().is_empty());
        let (ended, results) = worker.stop_within(*WATCHDOG);
        assert!(ended);
        assert!(results.is_empty());
        assert_eq!(probe.starts(), all(&["a"]));
        assert!(!probe.inside.load(Ordering::SeqCst));

        let (worker, probe) = started();
        probe.stuck.store(true, Ordering::SeqCst);
        worker.queue(job("a"));
        until("a's start", || !probe.starts().is_empty());
        let begun = Instant::now();
        let (ended, _) = worker.stop_within(Duration::from_millis(200));
        assert!(!ended);
        assert!(begun.elapsed() >= Duration::from_millis(200));
        // Unstuck, the thread sees the stop and ends on its own.
        probe.open.store(true, Ordering::SeqCst);
        until("the stuck thread's end", || {
            !probe.inside.load(Ordering::SeqCst)
        });
    }

    /// A measurement that panics gives nothing and the next job runs.
    #[test]
    fn a_panicking_measurement_gives_nothing() {
        let (worker, probe) = started();
        *probe.panic_on.lock().unwrap() = Some(PathBuf::from("a"));
        probe.open.store(true, Ordering::SeqCst);
        worker.queue(job("a"));
        worker.queue(job("b"));
        let results = drain(&worker);
        assert_eq!(paths(&results), all(&["a", "b"]));
        assert_eq!(results[0].measured, None);
        assert_eq!(results[1].measured, Some((key(Path::new("b")), ROW)));
    }

    /// A measurement told to stop by a pause, whose pause is released
    /// before the thread takes the lock again, keeps its job: measured
    /// again from the beginning, never dropped. The measurement waits at a
    /// barrier after its stop until the test has released the pause.
    #[test]
    fn a_pause_released_before_the_stop_returns_keeps_the_job() {
        let (stopping_tx, stopping) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let measure: Measure = Box::new(move |job, stop| {
            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                return Outcome::Measured(key(job.path()), ROW);
            }
            while !stop() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = stopping_tx.send(());
            let _ = go.recv();
            Outcome::Stopped
        });
        let worker = start(measure);
        worker.queue(job("a"));
        until("a's start", || runs.load(Ordering::SeqCst) == 1);
        worker.request_pause(None);
        stopping.recv_timeout(*WATCHDOG).unwrap();
        worker.release();
        go_tx.send(()).unwrap();
        let results = drain(&worker);
        assert_eq!(
            results,
            vec![Finished {
                path: PathBuf::from("a"),
                measured: Some((key(Path::new("a")), ROW)),
            }]
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// A measurement that stops without being told gives nothing and is
    /// not kept, even when a pause comes while it returns: it is held at
    /// a barrier, the pause is requested, and only then does it return
    /// its stop. The job ends once, with no result, and is not measured
    /// again after the release.
    #[test]
    fn a_stop_not_asked_for_gives_nothing() {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (go_tx, go) = std::sync::mpsc::channel::<()>();
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let measure: Measure = Box::new(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = started_tx.send(());
            let _ = go.recv();
            Outcome::Stopped
        });
        let worker = start(measure);
        worker.queue(job("a"));
        started.recv_timeout(*WATCHDOG).unwrap();
        worker.request_pause(None);
        go_tx.send(()).unwrap();
        // Paused either way: with the job ended, or kept and acknowledged.
        assert!(worker.wait_paused(*WATCHDOG));
        assert_eq!(
            worker.take_results(),
            vec![Finished {
                path: PathBuf::from("a"),
                measured: None,
            }]
        );
        assert!(!worker.holds(Path::new("a")));
        worker.release();
        assert!(drain(&worker).is_empty());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    /// A thread that ended (its entry into background mode was refused, or
    /// panicked) takes no job, holds none, and counts as paused, so no
    /// READY state waits for it; nothing is measured.
    #[test]
    fn an_ended_thread_takes_nothing() {
        for panics in [false, true] {
            let mode = Arc::new(Mode::default());
            mode.refuse_entry_from.store(1, Ordering::SeqCst);
            mode.panics.store(panics, Ordering::SeqCst);
            let (worker, probe) = moded(&Coordinator::new(CAPS), &mode);
            until("the end", || worker.queue(job("a")) == Queued::Ended);
            assert!(!worker.holds(Path::new("a")));
            worker.request_pause(None);
            assert!(worker.wait_paused(*WATCHDOG));
            assert!(worker.wait_done(Path::new("a"), Duration::ZERO));
            assert!(probe.starts().is_empty());
            assert!(worker.take_results().is_empty());
        }
    }

    /// The worker holds its speculative permit only while it measures: none
    /// while it waits for a job or is paused. With the permit taken, no
    /// measurement starts; a pause meanwhile is acknowledged, and the job
    /// runs once the permit is free and the pause released.
    #[test]
    fn the_permit_is_held_only_while_measuring() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let (worker, probe) = moded(&coord, &mode);
        assert_eq!(held(&coord), (0, 0));
        worker.queue(job("a"));
        until("a's start", || !probe.starts().is_empty());
        assert_eq!(held(&coord), (1, 0));
        worker.request_pause(None);
        assert!(worker.wait_paused(*WATCHDOG));
        assert_eq!(held(&coord), (0, 0));
        let blocker = coord.try_lease(Class::Speculative).unwrap();
        worker.release();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(probe.starts(), all(&["a"]));
        worker.request_pause(None);
        assert!(worker.wait_paused(*WATCHDOG));
        drop(blocker);
        worker.release();
        probe.open.store(true, Ordering::SeqCst);
        assert_eq!(paths(&drain(&worker)), all(&["a"]));
        assert_eq!(probe.starts(), all(&["a", "a"]));
        assert_eq!(held(&coord), (0, 0));
        assert_eq!(mode.leaves.load(Ordering::SeqCst), 0);
    }

    /// A stepped worker on `coord` with `mode`.
    fn stepped(coord: &Coordinator, mode: &Arc<Mode>) -> (Worker, Stepper) {
        let (stepper, measure) = Stepper::new();
        let mode: Arc<dyn ThreadMode> = Arc::clone(mode) as _;
        (
            Worker::start(measure, coord.clone(), mode).unwrap(),
            stepper,
        )
    }

    fn measured(name: &str) -> Finished {
        Finished {
            path: PathBuf::from(name),
            measured: Some((key(Path::new(name)), ROW)),
        }
    }

    /// Waits for `n` jobs to end; their results.
    fn results(worker: &Worker, n: usize) -> Vec<Finished> {
        assert!(worker.wait_results(n, *WATCHDOG), "the jobs never ended");
        worker.take_results()
    }

    /// A choice alone promotes nothing: the chosen chart's measurement goes
    /// on through the pause on its speculative permit, in background mode.
    #[test]
    fn a_choice_alone_promotes_nothing() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let (worker, stepper) = stepped(&coord, &mode);
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.request_pause(Some(Path::new("a")));
        stepper.step();
        assert_eq!(stepper.said(), Said::Block(1));
        assert_eq!(held(&coord), (1, 0));
        assert_eq!(mode.leaves.load(Ordering::SeqCst), 0);
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(worker.wait_paused(*WATCHDOG));
        assert_eq!(held(&coord), (0, 0));
        assert_eq!(mode.enters.load(Ordering::SeqCst), 1);
    }

    /// A waiting load promotes the measurement: at its next check its
    /// permit is a foreground one and the thread has left background mode.
    /// Its end demotes it: at the next check the permit is speculative
    /// again and the thread has entered background mode again, while the
    /// measurement goes on to its result. The next job runs speculative.
    #[test]
    fn a_waiting_load_promotes_and_its_end_demotes() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.request_pause(Some(Path::new("a")));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(true));
        assert_eq!(stepper.said(), Said::Block(1));
        assert_eq!(held(&coord), (0, 1));
        worker.demote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        assert_eq!(stepper.said(), Said::Block(2));
        assert_eq!(held(&coord), (1, 0));
        assert!(worker.holds(Path::new("a")));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(calls.none());
        worker.release();
        worker.queue(job("b"));
        assert_eq!(stepper.said(), Said::Block(0));
        assert_eq!(held(&coord), (1, 0));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("b")]);
        assert_eq!(held(&coord), (0, 0));
        assert_eq!(
            (
                mode.enters.load(Ordering::SeqCst),
                mode.leaves.load(Ordering::SeqCst)
            ),
            (2, 1)
        );
    }

    /// A promoted measurement that ends while promoted enters background
    /// mode again before the next job, which runs speculative.
    #[test]
    fn a_measurement_ending_promoted_enters_background_mode_again() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(true));
        assert_eq!(stepper.said(), Said::Block(1));
        stepper.finish();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        worker.demote(Path::new("a"));
        worker.queue(job("b"));
        assert_eq!(stepper.said(), Said::Block(0));
        assert_eq!(held(&coord), (1, 0));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("b")]);
        assert_eq!(held(&coord), (0, 0));
    }

    /// A job kept by a pause and then waited for is promoted from its
    /// start: the thread leaves background mode before its first check.
    #[test]
    fn a_job_waited_for_before_it_runs_again_is_promoted_from_its_start() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.request_pause(None);
        stepper.step();
        assert_eq!(stepper.said(), Said::Stopped);
        assert!(worker.wait_paused(*WATCHDOG));
        assert!(worker.holds(Path::new("a")));
        assert!(calls.none());
        worker.promote(Path::new("a"));
        worker.request_pause(Some(Path::new("a")));
        assert_eq!(calls.next(), ModeCall::Leave(true));
        assert_eq!(stepper.said(), Said::Block(0));
        assert_eq!(held(&coord), (0, 1));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        assert_eq!(held(&coord), (0, 0));
    }

    /// A refused leave is said and the measurement goes on, on its
    /// foreground permit at background priority, to its result; the
    /// thread, still in background mode, enters nothing again and measures
    /// the next job.
    #[test]
    fn a_refused_leave_measures_on_at_background_priority() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        mode.refuse_leave.store(true, Ordering::SeqCst);
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(false));
        assert_eq!(stepper.said(), Said::Block(1));
        assert_eq!(held(&coord), (0, 1));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        worker.demote(Path::new("a"));
        worker.queue(job("b"));
        assert_eq!(stepper.said(), Said::Block(0));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("b")]);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
        assert_eq!(mode.enters.load(Ordering::SeqCst), 1);
    }

    /// A refused entry at the demotion is said and the measurement goes on
    /// to its result, kept; then the thread ends and measures nothing more.
    #[test]
    fn a_refused_entry_at_the_demotion_ends_the_thread_after_the_job() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        mode.refuse_entry_from.store(2, Ordering::SeqCst);
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(true));
        assert_eq!(stepper.said(), Said::Block(1));
        worker.demote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Enter(false));
        assert_eq!(stepper.said(), Said::Block(2));
        assert_eq!(held(&coord), (1, 0));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// A refused entry after a promoted measurement's end keeps its result
    /// and ends the thread: no further job is measured.
    #[test]
    fn a_refused_entry_after_a_promotion_ends_the_thread() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        mode.refuse_entry_from.store(2, Ordering::SeqCst);
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(true));
        assert_eq!(stepper.said(), Said::Block(1));
        stepper.finish();
        assert_eq!(calls.next(), ModeCall::Enter(false));
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }
}
