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
//!   goes on, and the thread ends after it, its result kept. A mode call
//!   that unwinds leaves the thread's mode unknown: the job ends failed (or,
//!   after it ended, its result is kept) and the thread ends.
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

use std::collections::{HashSet, VecDeque};
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
    /// Charts the practice page measures itself ([`Worker::claim`]):
    /// never queued, never taken, until [`Worker::unclaim`].
    claimed: HashSet<PathBuf>,
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

    /// Takes the next waiting job as the one in hand, when none is held
    /// and no pause is requested; a claimed chart's job is never taken.
    fn pop(&mut self) {
        if self.current.is_none() && !self.pause {
            while let Some(job) = self.waiting.pop_front() {
                if !self.claimed.contains(job.path()) {
                    self.current = Some(job);
                    break;
                }
            }
        }
    }

    /// What [`Worker::claim`] finds for `path`, without changing anything.
    fn decide(&self, path: &Path, key: &LoudnessKey) -> Decision {
        if let Some(i) = self
            .results
            .iter()
            .position(|f| f.path == path && f.measured.is_some_and(|(k, _)| k == *key))
        {
            return Decision::Reuse(i);
        }
        if self.holds(path) {
            return if self.may_run(path) {
                Decision::Running
            } else if self.promoter.is_some() {
                Decision::Settling
            } else {
                Decision::TakeCurrent
            };
        }
        if self.waiting.iter().any(|j| j.path() == path) {
            Decision::TakeWaiting
        } else {
            Decision::Free
        }
    }
}

/// [`State::decide`]'s finding, taken by [`Worker::claim`] in the same
/// critical section.
#[derive(Clone, Copy, Debug)]
enum Decision {
    Reuse(usize),
    Running,
    Settling,
    TakeCurrent,
    TakeWaiting,
    Free,
}

/// What became of a chart the practice page measures ([`Worker::claim`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Claim {
    /// The thread measured it, under the page's key: its row, taken.
    Reused(LoudnessRow),
    /// The thread measures it now and goes on: claim again later.
    Running,
    /// The thread was told to stop it for a pause and has not returned:
    /// claim again later.
    Settling,
    /// The thread had it, kept or waiting, and gave it up: the page
    /// measures it.
    Taken,
    /// The thread had nothing of it: the page measures it.
    Free,
}

#[derive(Debug, Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Where a claim and the thread's pop wait while a test holds them.
    #[cfg(test)]
    hooks: tests::Hooks,
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
            if s.holds(job.path())
                || s.claimed.contains(job.path())
                || s.waiting.iter().any(|j| j.path() == job.path())
            {
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

    /// The practice page measures the chart at `path`, its 1× bank keyed
    /// `key`, unless this thread has or does: decided and taken in one
    /// critical section, so the thread's pop, its stop and its release
    /// each come wholly before or after. A result under `key` is taken
    /// ([`Claim::Reused`]); one under another key stays for the shelf. On
    /// [`Claim::Taken`] and [`Claim::Free`] the chart is claimed: a job
    /// kept for a pause or waiting is dropped, and none of it is queued or
    /// taken until [`unclaim`](Self::unclaim).
    pub fn claim(&self, path: &Path, key: &LoudnessKey) -> Claim {
        self.shared.update(|s| {
            let decision = s.decide(path, key);
            #[cfg(test)]
            self.shared.hooks.decided(&self.shared);
            match decision {
                Decision::Reuse(i) => {
                    let finished = s.results.remove(i);
                    match finished.measured {
                        Some((_, row)) => Claim::Reused(row),
                        None => Claim::Free,
                    }
                }
                Decision::Running => Claim::Running,
                Decision::Settling => Claim::Settling,
                Decision::TakeCurrent => {
                    s.current = None;
                    s.claimed.insert(path.to_owned());
                    Claim::Taken
                }
                Decision::TakeWaiting => {
                    s.waiting.retain(|j| j.path() != path);
                    s.claimed.insert(path.to_owned());
                    Claim::Taken
                }
                Decision::Free => {
                    s.claimed.insert(path.to_owned());
                    Claim::Free
                }
            }
        })
    }

    /// The practice page no longer measures `path`: it may be queued again.
    pub fn unclaim(&self, path: &Path) {
        self.shared.update(|s| {
            s.claimed.remove(path);
        });
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
                #[cfg(test)]
                if s.current.is_none() && !s.pause && !s.waiting.is_empty() {
                    s = shared.hooks.before_pop(shared, s);
                }
                s.pop();
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
        // A mode call that has not returned: set before each and cleared
        // after it, so one that unwound stays set, even when the unwinding
        // is caught inside the measurement (a decoder's own catch). From
        // then on the thread's mode is unknown: every later check says
        // stop, no other mode call is made, and the job is failed whatever
        // the measurement returns.
        let in_call = AtomicBool::new(false);
        let stop = || {
            if in_call.load(Ordering::Relaxed) {
                return true;
            }
            let s = shared.lock();
            let stop = s.shutdown || !s.may_run(&path);
            if stop {
                told.store(true, Ordering::Relaxed);
            }
            stop
        };
        // Whether the thread is out of background mode (promoted), cleared
        // only once an entry succeeded, and whether an entry was refused.
        let (left, refused) = (AtomicBool::new(false), AtomicBool::new(false));
        let on_class = |class: Class| {
            if in_call.load(Ordering::Relaxed) {
                return;
            }
            in_call.store(true, Ordering::Relaxed);
            match class {
                Class::Foreground => match mode.leave_background() {
                    Ok(()) => left.store(true, Ordering::Relaxed),
                    Err(e) => warn(&format!(
                        "loudness: {}: background mode could not be left ({e}); measured at background priority",
                        job.name
                    )),
                },
                _ if left.load(Ordering::Relaxed) => {
                    if entered(mode) {
                        left.store(false, Ordering::Relaxed);
                    } else {
                        refused.store(true, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
            in_call.store(false, Ordering::Relaxed);
        };
        let mut outcome =
            match task.run(&stop, &on_class, |ctx| measure(&job, &|| ctx.checkpoint())) {
                coord::Outcome::Done(outcome) => outcome,
                // The wait for its permit stopped: as a stop it was told.
                coord::Outcome::Cancelled if told.load(Ordering::Relaxed) => Outcome::Stopped,
                coord::Outcome::Cancelled => Outcome::Failed("no speculative permit".to_owned()),
                coord::Outcome::Panicked(p) => Outcome::Failed(format!(
                    "the background measurement stopped unexpectedly: {p}"
                )),
            };
        // A mode call that unwound during the job is not the measurement's
        // failure, but the measurement cannot be trusted: it is failed,
        // and the thread, its mode unknown, runs no further job.
        let unknown = in_call.load(Ordering::Relaxed);
        if unknown {
            warn(
                "loudness: a background mode call failed unexpectedly; the background measurement is off for this run",
            );
            outcome =
                Outcome::Failed("a background mode call failed during the measurement".to_owned());
        }
        // Back in background mode before the next job, or no more jobs. An
        // entry that unwinds here counts as refused, after the job's result
        // is kept.
        let resumed = !unknown
            && !refused.load(Ordering::Relaxed)
            && (!left.load(Ordering::Relaxed)
                || coord::catch(|| entered(mode)).unwrap_or_else(|p| {
                    warn(&format!(
                        "loudness: entering background mode failed unexpectedly ({p}); the background measurement is off for this run"
                    ));
                    false
                }));
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

    /// A claim's test hook, run between its decision and its take with the
    /// state's lock held.
    type Decided = Box<dyn Fn(&super::Shared) + Send + Sync>;

    /// Where a claim and the thread's pop wait while a test holds them.
    #[derive(Default)]
    pub(crate) struct Hooks {
        /// Run by every claim between its decision and its take.
        decided: Mutex<Option<Decided>>,
        /// Run once, the lock let go, before the thread's next pop of a
        /// waiting job.
        pop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl std::fmt::Debug for Hooks {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Hooks").finish_non_exhaustive()
        }
    }

    impl Hooks {
        pub(super) fn decided(&self, shared: &super::Shared) {
            if let Some(hook) = &*self.decided.lock().unwrap() {
                hook(shared);
            }
        }

        /// The pop's hold: the guard let go while the hook runs, then
        /// taken again.
        pub(super) fn before_pop<'a>(
            &self,
            shared: &'a super::Shared,
            guard: std::sync::MutexGuard<'a, super::State>,
        ) -> std::sync::MutexGuard<'a, super::State> {
            let Some(hook) = self.pop.lock().unwrap().take() else {
                return guard;
            };
            drop(guard);
            hook();
            shared.lock()
        }
    }

    impl Worker {
        /// Runs `hook` in every claim between its decision and its take.
        pub(crate) fn on_decided(&self, hook: impl Fn(&super::Shared) + Send + Sync + 'static) {
            *self.shared.hooks.decided.lock().unwrap() = Some(Box::new(hook));
        }

        /// Runs `hook` once before the thread's next pop of a waiting job,
        /// the state's lock let go.
        pub(crate) fn before_pop(&self, hook: impl FnOnce() + Send + 'static) {
            *self.shared.hooks.pop.lock().unwrap() = Some(Box::new(hook));
        }
    }

    /// The thread's pop, attempted from a test: `None` when the state's
    /// lock is held elsewhere (observed, never waited for), else the job
    /// held after it.
    pub(crate) fn try_pop(shared: &super::Shared) -> Option<Option<PathBuf>> {
        let mut s = match shared.state.try_lock() {
            Ok(s) => s,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        s.pop();
        let held = s.current.as_ref().map(|j| j.path().to_owned());
        drop(s);
        shared.changed.notify_all();
        Some(held)
    }

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
        /// The next entry panics (once).
        pub(crate) panics: AtomicBool,
        /// The next leave panics (once).
        pub(crate) leave_panics: AtomicBool,
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
                !self.panics.swap(false, Ordering::SeqCst),
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
            assert!(
                !self.leave_panics.swap(false, Ordering::SeqCst),
                "the leave failed (test)"
            );
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

    /// A mode call that unwinds (the entry at a demotion) is not a
    /// measurement's failure: the job is published failed, the thread's
    /// mode is unknown, so the worker ends and takes no further job.
    #[test]
    fn an_unwinding_entry_at_the_demotion_ends_the_worker() {
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
        mode.panics.store(true, Ordering::SeqCst);
        worker.demote(Path::new("a"));
        stepper.step();
        assert_eq!(
            results(&worker, 1),
            vec![Finished {
                path: PathBuf::from("a"),
                measured: None,
            }]
        );
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// A leave that unwinds at a promotion leaves the thread's mode
    /// unknown: the job is published failed, and the worker ends.
    #[test]
    fn an_unwinding_leave_at_the_promotion_ends_the_worker() {
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let (worker, stepper) = stepped(&coord, &mode);
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.queue(job("a"));
        assert_eq!(stepper.said(), Said::Block(0));
        mode.leave_panics.store(true, Ordering::SeqCst);
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(
            results(&worker, 1),
            vec![Finished {
                path: PathBuf::from("a"),
                measured: None,
            }]
        );
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// Through the production measurement: an entry that unwinds at a
    /// demotion, at a check inside a sound's decoding, is caught by the
    /// decoder's own boundary, which would take the sound for undecodable
    /// and measure the rest. The fault is the job's: every later check
    /// stops it, the job is published failed, and the worker ends.
    #[test]
    fn an_unwinding_entry_caught_inside_decoding_fails_the_job() {
        use crate::loudness::fixture::{RATE, square, wav};
        let (dir, loaded) = crate::imports::facts::tests::loaded_kept(
            "#WAV01 a.wav\n#WAV02 b.wav\n#00101:02\n",
            "beatoraja-style",
        );
        std::fs::write(dir.path().join("a.wav"), wav(&square(1.0, 3_000))).unwrap();
        std::fs::write(dir.path().join("b.wav"), wav(&square(1.0, 6_000))).unwrap();
        let job = Job {
            source: loaded.source(),
            rate: RATE,
            name: "a.bms".to_owned(),
        };
        let path = job.path().to_owned();
        // The measurement's check 6 is the first packet's, inside the
        // first sound's decoding (before it: the read, then the first file,
        // its read's return, the probe and the codec): held there until the
        // test has demoted.
        let (at_tx, at) = mpsc::channel::<()>();
        let (go, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let checks = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&checks);
        let measure: Measure = Box::new(move |job, stop| {
            let checks = &counted;
            let gated = || {
                if checks.fetch_add(1, Ordering::SeqCst) + 1 == 6 {
                    let _ = at_tx.send(());
                    let _ = go_rx.lock().unwrap().recv();
                }
                stop()
            };
            super::measure(job, &gated)
        });
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let worker = Worker::start(measure, coord.clone(), Arc::clone(&mode) as _).unwrap();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.promote(&path);
        worker.queue(job.clone());
        assert_eq!(calls.next(), ModeCall::Leave(true));
        at.recv_timeout(*WATCHDOG)
            .expect("the check inside decoding");
        mode.panics.store(true, Ordering::SeqCst);
        worker.demote(&path);
        go.send(()).unwrap();
        assert_eq!(
            results(&worker, 1),
            vec![Finished {
                path: path.clone(),
                measured: None,
            }]
        );
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job), Queued::Ended);
        // The next check (before the second sound) stops the measurement:
        // nothing more is decoded or analyzed.
        assert_eq!(checks.load(Ordering::SeqCst), 7);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// A measurement that catches the unwinding of a mode call itself and
    /// returns a result is still failed: the fault stays the job's through
    /// later class changes (no mode call is made after it, so none can
    /// clear it), its checks say stop, and the worker ends.
    #[test]
    fn a_fault_caught_by_the_measurement_itself_still_fails_the_job() {
        // Four checks, each held until the test lets it go, each catching
        // what unwinds from it; then a result, whatever the checks said.
        let (at_tx, at) = mpsc::channel::<bool>();
        let (go, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let measure: Measure = Box::new(move |job, stop| {
            for _ in 0..4 {
                let _ = go_rx.lock().unwrap().recv();
                let said = game::coord::catch(stop).unwrap_or(true);
                let _ = at_tx.send(said);
            }
            Outcome::Measured(key(job.path()), ROW)
        });
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let worker = Worker::start(measure, coord.clone(), Arc::clone(&mode) as _).unwrap();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        let path = Path::new("a");
        worker.promote(path);
        worker.queue(job("a"));
        assert_eq!(calls.next(), ModeCall::Leave(true));
        let check = |what: &str| {
            go.send(()).unwrap();
            at.recv_timeout(*WATCHDOG).expect(what)
        };
        assert!(!check("the first check"));
        // The demotion's entry unwinds, caught by the measurement.
        mode.panics.store(true, Ordering::SeqCst);
        worker.demote(path);
        assert!(check("the check that unwinds"));
        // A promotion and a demotion after it: the permit moves, and the
        // demotion makes no mode call.
        worker.promote(path);
        assert!(check("the check after the promotion"));
        worker.demote(path);
        assert!(check("the check after the demotion"));
        assert_eq!(
            results(&worker, 1),
            vec![Finished {
                path: PathBuf::from("a"),
                measured: None,
            }]
        );
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// A measurement whose last check is the one that unwinds, caught by
    /// the measurement, returns a result with no check after it: the job is
    /// still failed.
    #[test]
    fn a_fault_at_the_last_check_still_fails_the_job() {
        let (at_tx, at) = mpsc::channel::<()>();
        let (go, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let measure: Measure = Box::new(move |job, stop| {
            let _ = at_tx.send(());
            let _ = go_rx.lock().unwrap().recv();
            let _ = game::coord::catch(stop);
            Outcome::Measured(key(job.path()), ROW)
        });
        let coord = Coordinator::new(CAPS);
        let mode = Arc::new(Mode::default());
        let calls = mode.calls();
        let worker = Worker::start(measure, coord.clone(), Arc::clone(&mode) as _).unwrap();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        worker.promote(Path::new("a"));
        worker.queue(job("a"));
        assert_eq!(calls.next(), ModeCall::Leave(true));
        at.recv_timeout(*WATCHDOG).expect("the measurement's start");
        mode.panics.store(true, Ordering::SeqCst);
        worker.demote(Path::new("a"));
        go.send(()).unwrap();
        assert_eq!(
            results(&worker, 1),
            vec![Finished {
                path: PathBuf::from("a"),
                measured: None,
            }]
        );
        assert!(worker.wait_ended(*WATCHDOG));
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// After a refused entry at a demotion the measurement goes on, still
    /// out of background mode: a later wait promotes it again (its leave
    /// refused, the thread being out already) and that wait's end demotes
    /// it, entering background mode again. The result is kept and the
    /// worker ends after it.
    #[test]
    fn a_demotion_after_a_refused_entry_enters_again() {
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
        mode.refuse_entry_from.store(0, Ordering::SeqCst);
        mode.refuse_leave.store(true, Ordering::SeqCst);
        worker.promote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Leave(false));
        assert_eq!(stepper.said(), Said::Block(3));
        worker.demote(Path::new("a"));
        stepper.step();
        assert_eq!(calls.next(), ModeCall::Enter(true));
        assert_eq!(stepper.said(), Said::Block(4));
        assert_eq!(held(&coord), (1, 0));
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(worker.wait_ended(*WATCHDOG));
        assert!(calls.none());
        assert_eq!(held(&coord), (0, 0));
    }

    /// An entry that unwinds after a measurement that ended promoted keeps
    /// its result, and the worker ends.
    #[test]
    fn an_unwinding_entry_after_a_promoted_job_keeps_its_result() {
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
        mode.panics.store(true, Ordering::SeqCst);
        stepper.finish();
        assert_eq!(results(&worker, 1), vec![measured("a")]);
        assert!(worker.wait_ended(*WATCHDOG));
        assert_eq!(worker.queue(job("b")), Queued::Ended);
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

    /// The practice page's claim against the background (P16), through the
    /// production measurement: every interleaving a test asserts is held at
    /// a meeting point (the thread's pop, a check inside its analysis, the
    /// claim between its decision and its take), never timed. Analyses are
    /// counted where every real one begins ([`crate::loudness::analyses`]),
    /// each test on a chart of its own.
    mod claims {
        use std::sync::mpsc::{self, Receiver, Sender};
        use std::sync::{Arc, Mutex};

        use game::coord::Coordinator;
        use library::LoudnessKey;

        use super::super::{Claim, Finished, Job, Measure, Worker};
        use super::{CAPS, Mode, WATCHDOG, results, try_pop};
        use crate::loudness::analyses;
        use crate::loudness::fixture::{RATE, square, wav};

        /// A chart of its own (`tag` makes its hash), its sounds written:
        /// the background's job of it, its 1× key and its resolved hash.
        struct Song {
            _dir: tempfile::TempDir,
            job: Job,
            key: LoudnessKey,
            hash: [u8; 32],
            chart: bms_model::Chart,
        }

        fn song(tag: &str) -> Song {
            let (dir, loaded) = crate::imports::facts::tests::loaded_kept(
                &format!("#TITLE {tag}\n#WAV01 a.wav\n#00101:01\n"),
                "beatoraja-style",
            );
            std::fs::write(dir.path().join("a.wav"), wav(&square(1.0, 4_000))).unwrap();
            // Read as the thread reads it, so the keys are the thread's.
            let chart = loaded
                .source()
                .read(
                    &game::Parts::unsaved(&game::PlayerSettings::default()),
                    bms_model::Speed::NORMAL,
                )
                .unwrap();
            let bank = audio::load_bank_at(
                &chart,
                RATE,
                audio::Speed::NORMAL,
                &audio::BankLimits::default(),
            )
            .unwrap();
            let key = crate::loudness::key(&chart, &bank).unwrap();
            Song {
                _dir: dir,
                job: Job {
                    source: loaded.source(),
                    rate: RATE,
                    name: tag.to_owned(),
                },
                key,
                hash: chart.identity.resolved_hash,
                chart,
            }
        }

        /// The page's own measurement of `song`: one real analysis.
        fn page_measures(song: &Song) {
            let bank = Arc::new(
                audio::load_bank_at(
                    &song.chart,
                    RATE,
                    audio::Speed::NORMAL,
                    &audio::BankLimits::default(),
                )
                .unwrap(),
            );
            let mut cancelled = || false;
            let _ = crate::loudness::analyze(&song.chart, bank, &mut cancelled);
        }

        /// A hold at the first check inside an analysis of a chart: armed
        /// with its hash, it says when the thread is there and waits for
        /// `go`.
        #[derive(Default)]
        struct Hold {
            armed: Mutex<Option<[u8; 32]>>,
            at: Mutex<Option<Sender<()>>>,
            go: Mutex<Option<Receiver<()>>>,
        }

        impl Hold {
            /// Arms it for the next analysis of the chart `hash`; the
            /// channel says when it holds, the sender lets it go.
            fn arm(&self, hash: [u8; 32]) -> (Receiver<()>, Sender<()>) {
                let (at_tx, at) = mpsc::channel();
                let (go, go_rx) = mpsc::channel();
                *self.at.lock().unwrap() = Some(at_tx);
                *self.go.lock().unwrap() = Some(go_rx);
                *self.armed.lock().unwrap() = Some(hash);
                (at, go)
            }

            fn check(&self, hash: [u8; 32], began: usize) {
                if analyses::of(hash) <= began {
                    return;
                }
                if self.armed.lock().unwrap().take_if(|h| *h == hash).is_none() {
                    return;
                }
                if let Some(at) = self.at.lock().unwrap().take() {
                    let _ = at.send(());
                }
                if let Some(go) = self.go.lock().unwrap().take() {
                    let _ = go.recv();
                }
            }
        }

        /// A worker on the production measurement, each job held by `hold`
        /// at its first check inside its analysis when armed.
        fn started(hold: &Arc<Hold>) -> (Worker, Coordinator) {
            let hold = Arc::clone(hold);
            let measure: Measure = Box::new(move |job, stop| {
                let hash = job
                    .source
                    .read(
                        &game::Parts::unsaved(&game::PlayerSettings::default()),
                        bms_model::Speed::NORMAL,
                    )
                    .map(|c| c.identity.resolved_hash)
                    .unwrap_or_default();
                let began = analyses::of(hash);
                let gated = || {
                    hold.check(hash, began);
                    stop()
                };
                super::super::measure(job, &gated)
            });
            let coord = Coordinator::new(CAPS);
            let mode = Arc::new(Mode::default());
            let worker = Worker::start(measure, coord.clone(), mode as _).unwrap();
            (worker, coord)
        }

        /// A wait for the thread to be past the claimed `claimed`: `other`
        /// queued after it is measured, and nothing of `claimed` ended on
        /// the thread meanwhile.
        fn past(worker: &Worker, claimed: &Song, other: &Song) {
            worker.queue(other.job.clone());
            let mut done = Vec::new();
            while !done.iter().any(|f: &Finished| f.path == other.job.path()) {
                done.extend(results(worker, 1));
            }
            assert!(
                !done.iter().any(|f| f.path == claimed.job.path()),
                "the thread measured the claimed chart: {done:?}"
            );
        }

        /// The thread held before its pop of `song`, waiting: the pop's
        /// channels.
        fn held_before_pop(worker: &Worker, song: &Song) -> (Receiver<()>, Sender<()>) {
            let (at_tx, at) = mpsc::channel();
            let (go, go_rx) = mpsc::channel::<()>();
            worker.before_pop(move || {
                let _ = at_tx.send(());
                let _ = go_rx.recv();
            });
            worker.queue(song.job.clone());
            at.recv_timeout(*WATCHDOG).expect("the pop held");
            (at, go)
        }

        /// The counter sees every real analysis: with no claim the
        /// background and the page both measure the chart, two.
        #[test]
        fn the_analysis_count_sees_both_measurements() {
            let song = song("p16-control");
            let (worker, _coord) = started(&Arc::default());
            worker.queue(song.job.clone());
            assert_eq!(results(&worker, 1).len(), 1);
            page_measures(&song);
            assert_eq!(analyses::of(song.hash), 2);
        }

        /// P16b: the chart waits in the queue, the thread held at its pop.
        /// A claim first takes it, and the thread never measures it; the
        /// pop first measures it, the claim finds it running and then its
        /// result, and the page measures nothing. One analysis either way.
        #[test]
        fn the_claim_and_the_dequeue_take_the_chart_once() {
            let other = song("p16b-other");
            // Claim first.
            let s = song("p16b-claim-first");
            let hold = Arc::new(Hold::default());
            let (worker, _coord) = started(&hold);
            let (_at, go) = held_before_pop(&worker, &s);
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Taken);
            go.send(()).unwrap();
            page_measures(&s);
            past(&worker, &s, &other);
            assert_eq!(analyses::of(s.hash), 1, "claim first");
            drop(worker);
            // The pop first.
            let s = song("p16b-pop-first");
            let (worker, _coord) = started(&hold);
            let (at, go_analysis) = hold.arm(s.hash);
            let (_pop, go) = held_before_pop(&worker, &s);
            go.send(()).unwrap();
            at.recv_timeout(*WATCHDOG).expect("inside the analysis");
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Running);
            go_analysis.send(()).unwrap();
            assert!(worker.wait_done(s.job.path(), *WATCHDOG));
            assert!(matches!(
                worker.claim(s.job.path(), &s.key),
                Claim::Reused(_)
            ));
            assert_eq!(analyses::of(s.hash), 1, "the pop first");
        }

        /// P16f: the claim's decision and its take are one critical
        /// section. Between them the test attempts the thread's pop with a
        /// `try_lock`, which finds the lock held: observed, not timed. The
        /// claim then takes the waiting chart, the thread never measures
        /// it, and the page's is the one analysis.
        #[test]
        fn the_claims_decision_and_take_hold_the_lock() {
            let other = song("p16f-other");
            let s = song("p16f");
            let (worker, _coord) = started(&Arc::default());
            let attempts = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&attempts);
            worker.on_decided(move |shared| seen.lock().unwrap().push(try_pop(shared)));
            let (_at, go) = held_before_pop(&worker, &s);
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Taken);
            assert_eq!(
                *attempts.lock().unwrap(),
                [None],
                "the pop found the lock free between the decision and the take"
            );
            go.send(()).unwrap();
            page_measures(&s);
            past(&worker, &s, &other);
            assert_eq!(analyses::of(s.hash), 1);
        }

        /// The chart measured on the thread, held inside its analysis, and
        /// a pause requested: told to stop, not returned.
        fn told_to_stop(worker: &Worker, hold: &Hold, s: &Song) -> Sender<()> {
            let (at, go) = hold.arm(s.hash);
            worker.queue(s.job.clone());
            at.recv_timeout(*WATCHDOG).expect("inside the analysis");
            worker.request_pause(None);
            go
        }

        /// P16c: the measurement held inside its analysis, a pause
        /// requested: the claim finds it settling. Once it returned, kept
        /// for the pause, the claim takes it, and after the release the
        /// thread does not measure it again: the stopped analysis and the
        /// page's, two.
        #[test]
        fn the_claim_takes_a_paused_measurement_once() {
            let other = song("p16c-other");
            let s = song("p16c");
            let hold = Arc::new(Hold::default());
            let (worker, _coord) = started(&hold);
            let go = told_to_stop(&worker, &hold, &s);
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Settling);
            go.send(()).unwrap();
            assert!(worker.wait_paused(*WATCHDOG));
            assert!(worker.holds(s.job.path()), "kept for the pause");
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Taken);
            worker.release();
            page_measures(&s);
            past(&worker, &s, &other);
            assert_eq!(analyses::of(s.hash), 2, "the stopped one and the page's");
        }

        /// P16d: the measurement kept for a pause. Released first, the
        /// claim finds it running again and then its result: the stopped
        /// analysis and the thread's again, two, none of the page. Claimed
        /// first, the release measures nothing: the stopped one and the
        /// page's, two.
        #[test]
        fn the_claim_and_the_release_measure_once() {
            let other = song("p16d-other");
            let hold = Arc::new(Hold::default());
            // Released first.
            let s = song("p16d-release-first");
            let (worker, _coord) = started(&hold);
            let go = told_to_stop(&worker, &hold, &s);
            go.send(()).unwrap();
            assert!(worker.wait_paused(*WATCHDOG));
            let (at, go) = hold.arm(s.hash);
            worker.release();
            at.recv_timeout(*WATCHDOG).expect("measured again");
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Running);
            go.send(()).unwrap();
            assert!(worker.wait_done(s.job.path(), *WATCHDOG));
            assert!(matches!(
                worker.claim(s.job.path(), &s.key),
                Claim::Reused(_)
            ));
            assert_eq!(analyses::of(s.hash), 2, "released first");
            drop(worker);
            // Claimed first.
            let s = song("p16d-claim-first");
            let (worker, _coord) = started(&hold);
            let go = told_to_stop(&worker, &hold, &s);
            go.send(()).unwrap();
            assert!(worker.wait_paused(*WATCHDOG));
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Taken);
            worker.release();
            page_measures(&s);
            past(&worker, &s, &other);
            assert_eq!(analyses::of(s.hash), 2, "claimed first");
        }

        /// P16e: a finished measurement of the chart is reused only under
        /// the page's key. Under it: taken, no analysis of the page. Under
        /// another key (another rate, not the default): left, and the page
        /// measures. With no measurement (its file gone, which begins no
        /// analysis): the page measures.
        #[test]
        fn a_finished_measurement_is_reused_only_with_the_pages_key() {
            // The page's key.
            let s = song("p16e-same");
            let (worker, _coord) = started(&Arc::default());
            worker.queue(s.job.clone());
            assert!(worker.wait_results(1, *WATCHDOG));
            assert!(matches!(
                worker.claim(s.job.path(), &s.key),
                Claim::Reused(_)
            ));
            assert_eq!(analyses::of(s.hash), 1, "reused: none of the page");
            // Another key.
            let s = song("p16e-other-key");
            worker.queue(s.job.clone());
            assert!(worker.wait_results(1, *WATCHDOG));
            let other_key = LoudnessKey {
                rate: s.key.rate + 1,
                ..s.key
            };
            assert_eq!(worker.claim(s.job.path(), &other_key), Claim::Free);
            page_measures(&s);
            assert_eq!(analyses::of(s.hash), 2, "another key: the page's too");
            let left = worker.take_results();
            assert!(
                left.iter()
                    .any(|f| f.path == s.job.path() && f.measured.is_some()),
                "the other key's result stays for the shelf: {left:?}"
            );
            // No measurement: the chart's file is gone before the thread
            // reads it.
            let s = song("p16e-failed");
            std::fs::remove_file(s.job.path()).unwrap();
            worker.queue(s.job.clone());
            assert!(worker.wait_results(1, *WATCHDOG));
            assert_eq!(analyses::of(s.hash), 0, "the failure began no analysis");
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Free);
            page_measures(&s);
            assert_eq!(analyses::of(s.hash), 1);
        }

        /// A claimed chart is not queued again until the page gives it
        /// back.
        #[test]
        fn a_claimed_chart_is_not_queued_until_unclaimed() {
            let s = song("p16-unclaim");
            let (worker, _coord) = started(&Arc::default());
            worker.request_pause(None);
            assert_eq!(worker.claim(s.job.path(), &s.key), Claim::Free);
            assert_eq!(worker.queue(s.job.clone()), super::Queued::Already);
            worker.unclaim(s.job.path());
            assert_eq!(worker.queue(s.job.clone()), super::Queued::Added);
        }
    }
}
