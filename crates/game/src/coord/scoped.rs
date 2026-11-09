//! Foreground jobs on scoped threads (design.md "Play Lifecycle", who
//! loads): the chosen chart's read, its bank and its loudness, each on a
//! thread of its own that a game thread waits for in slices.
//!
//! A job's result comes back through its thread's join, never through
//! [`Coordinator::take`], so it needs neither `Clone` nor a shared slot;
//! the coordinator keeps only the outcome's kind. Waiting
//! ([`ScopedJob::wait`]) asks the waiter each slice whether to stop. At the
//! first yes it stops the job through its owner's stop, which its admission
//! and every checkpoint read (a seen stop is terminal: the job ends
//! cancelled), and then **settles**: it waits for the thread's end in
//! parked slices, asking nothing more, joins it and decides from what the
//! join returned. A panic stays fatal after a stop, and a result the job
//! made after the stop is dropped. The choice's scope is not advanced: that
//! would cancel the other subscribers of the choice too, the stop is this
//! job's own.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Scope as ThreadScope, ScopedJoinHandle};
use std::time::Duration;

use bms_model::Speed;

use super::{Class, Coordinator, Ctx, Handle, JobKey, JobKind, JobPanic, Outcome, Scope};

/// Why a foreground job ended the run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the chart's {} {cause}", kind_name(*.job))]
pub struct LoadFailure {
    /// The job.
    pub job: JobKind,
    /// What happened to it.
    pub cause: Failure,
}

/// What ended a foreground job.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// Its thread could not start.
    #[error("cannot start its thread: {0}")]
    ThreadStart(String),
    /// It panicked.
    #[error("stopped unexpectedly: {0}")]
    Panicked(JobPanic),
}

const fn kind_name(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Loudness | JobKind::Measure => "loudness measurement",
        JobKind::Ingest => "read",
        JobKind::Bank => "sound decode",
    }
}

/// Starts each foreground job's thread; tests name kinds whose threads are
/// refused, as a system out of threads would refuse them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spawner {
    refused: &'static [JobKind],
}

impl Spawner {
    /// The system's threads.
    pub const SYSTEM: Self = Self { refused: &[] };

    /// The system's threads, except for `refused`'s jobs (tests).
    #[doc(hidden)]
    #[must_use]
    pub const fn refusing(refused: &'static [JobKind]) -> Self {
        Self { refused }
    }

    fn spawn<'scope, 'env, T: Send + 'scope>(
        self,
        scope: &'scope ThreadScope<'scope, 'env>,
        kind: JobKind,
        f: impl FnOnce() -> T + Send + 'scope,
    ) -> io::Result<ScopedJoinHandle<'scope, T>> {
        if self.refused.contains(&kind) {
            return Err(io::Error::other("refused (test)"));
        }
        thread::Builder::new()
            .name(format!("load-{}", kind_name(kind).replace(' ', "-")))
            .spawn_scoped(scope, f)
    }
}

/// A foreground job running on its own scoped thread.
#[derive(Debug)]
pub struct ScopedJob<'scope, R> {
    kind: JobKind,
    /// Kept until the join: dropping the last handle would cancel it.
    handle: Handle<()>,
    stop: Arc<AtomicBool>,
    thread: ScopedJoinHandle<'scope, (Outcome<()>, Option<R>)>,
}

/// How a waited job ended for its waiter.
#[derive(Debug, PartialEq, Eq)]
pub enum Joined<R> {
    /// Its result.
    Done(R),
    /// It stopped, or the waiter stopped it: no result. What its work
    /// returned anyway, if it returned, says only how it stopped (a
    /// decode's own cancel, a result made after the stop); it is never
    /// installed.
    Cancelled(Option<R>),
    /// It ends the run.
    Failed(LoadFailure),
}

impl Coordinator {
    /// Begins the foreground job `kind` of `path` at `speed` for the
    /// current choice and runs `work` on a scoped thread of `scope`, on a
    /// foreground permit. `stop` is also asked while it waits for its
    /// permit and at each checkpoint.
    ///
    /// # Errors
    ///
    /// The thread could not start: the job never ran and holds nothing.
    pub fn run_scoped<'scope, 'env, R: Send + 'scope>(
        &self,
        scope: &'scope ThreadScope<'scope, 'env>,
        spawner: Spawner,
        (kind, path, speed): (JobKind, PathBuf, Speed),
        work: impl FnOnce(&Ctx<'_>) -> R + Send + 'scope,
    ) -> Result<ScopedJob<'scope, R>, LoadFailure> {
        let failed = |e: String| LoadFailure {
            job: kind,
            cause: Failure::ThreadStart(e),
        };
        let key = JobKey { kind, path, speed };
        let (handle, runner) =
            self.begin::<()>(key, Class::Foreground, self.subscriber(Scope::Choice));
        let runner = runner.ok_or_else(|| failed("the job already runs".to_owned()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = spawner
            .spawn(scope, kind, move || {
                let mut out = None;
                let outcome = runner.run(&|| stopped.load(Ordering::Acquire), &|_| {}, |ctx| {
                    out = Some(work(ctx))
                });
                (outcome, out)
            })
            .map_err(|e| failed(e.to_string()))?;
        Ok(ScopedJob {
            kind,
            handle,
            stop,
            thread,
        })
    }
}

impl<R> ScopedJob<'_, R> {
    /// Waits for the job, asking `stop` each `slice` until it ends; at the
    /// first yes the job is asked to stop and settled (module doc).
    pub fn wait(self, slice: Duration, mut stop: impl FnMut() -> bool) -> Joined<R> {
        let mut settling = false;
        while !self.thread.is_finished() {
            if !settling && stop() {
                settling = true;
                self.stop.store(true, Ordering::Release);
                continue;
            }
            thread::park_timeout(slice);
        }
        let joined = self.thread.join();
        drop(self.handle);
        let fail = |p: JobPanic| {
            Joined::Failed(LoadFailure {
                job: self.kind,
                cause: Failure::Panicked(p),
            })
        };
        match joined {
            Err(_) => fail(JobPanic("its thread ended unexpectedly".to_owned())),
            Ok((Outcome::Panicked(p), _)) => fail(p),
            Ok((Outcome::Done(()), Some(r))) if !settling => Joined::Done(r),
            Ok((_, r)) => Joined::Cancelled(r),
        }
    }

    /// Whether the job's thread has ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

#[cfg(test)]
mod tests {
    //! A foreground job's waiter: its result, a refused thread, a stop
    //! while it waits for its permit, while it works and after it worked,
    //! and a panic after a stop. Threads meet at channels; a watchdog
    //! (`TAPESTRY_TEST_WATCHDOG`, seconds) panics a wait that never ends.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use bms_model::Speed;

    use super::{Failure, Joined, LoadFailure, Spawner};
    use crate::coord::{Caps, Class, Coordinator, FOREGROUND_PERMITS, JobKind, JobPanic};

    const SLICE: Duration = Duration::from_millis(1);

    fn watchdog() -> Duration {
        let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
        Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
    }

    fn coord() -> Coordinator {
        Coordinator::new(Caps {
            foreground: FOREGROUND_PERMITS,
            speculative: 1,
        })
    }

    fn job(kind: JobKind) -> (JobKind, PathBuf, Speed) {
        (kind, PathBuf::from("a.bms"), Speed::NORMAL)
    }

    #[test]
    fn a_job_returns_its_result_on_a_foreground_permit() {
        let coord = coord();
        let joined = thread::scope(|s| {
            coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Bank), |ctx| {
                    (ctx.class(), coord.held(Class::Foreground))
                })
                .unwrap()
                .wait(SLICE, || false)
        });
        assert_eq!(joined, Joined::Done((Some(Class::Foreground), 1)));
        assert_eq!(coord.held(Class::Foreground), 0);
    }

    /// A refused thread is a failure naming the job; the job never ran,
    /// holds nothing and a later one of its key runs.
    #[test]
    fn a_refused_thread_fails_and_holds_nothing() {
        let coord = coord();
        let ran = AtomicUsize::new(0);
        thread::scope(|s| {
            let e = coord
                .run_scoped(
                    s,
                    Spawner::refusing(&[JobKind::Ingest]),
                    job(JobKind::Ingest),
                    |_| {
                        ran.fetch_add(1, Ordering::SeqCst);
                    },
                )
                .unwrap_err();
            assert_eq!(
                e,
                LoadFailure {
                    job: JobKind::Ingest,
                    cause: Failure::ThreadStart("refused (test)".to_owned()),
                }
            );
            assert_eq!(
                e.to_string(),
                "the chart's read cannot start its thread: refused (test)"
            );
        });
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(coord.held(Class::Foreground), 0);
        let again = thread::scope(|s| {
            coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Ingest), |_| 7)
                .unwrap()
                .wait(SLICE, || false)
        });
        assert_eq!(again, Joined::Done(7));
    }

    /// A stop asked while the work runs, with no checkpoint in it: the
    /// waiter settles, asking nothing more, until the thread ends, and the
    /// result the work still made is dropped.
    #[test]
    fn a_stop_during_the_work_settles_and_drops_its_result() {
        let coord = coord();
        let (go, gate) = mpsc::channel::<()>();
        let (started_tx, started) = mpsc::channel::<()>();
        let (asked_tx, asked) = mpsc::channel::<()>();
        let stops = AtomicUsize::new(0);
        let joined = thread::scope(|s| {
            let job = coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Ingest), move |_| {
                    let _ = started_tx.send(());
                    gate.recv_timeout(watchdog()).expect("the gate opens");
                    "parsed"
                })
                .unwrap();
            s.spawn(move || {
                asked.recv_timeout(watchdog()).expect("the waiter stops it");
                go.send(()).unwrap();
            });
            job.wait(SLICE, || {
                if started.try_recv().is_err() && stops.load(Ordering::SeqCst) == 0 {
                    return false;
                }
                stops.fetch_add(1, Ordering::SeqCst);
                let _ = asked_tx.send(());
                true
            })
        });
        assert_eq!(
            joined,
            Joined::Cancelled(Some("parsed")),
            "made, then dropped"
        );
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "settling asks nothing more"
        );
        assert_eq!(coord.held(Class::Foreground), 0);
    }

    /// A panic after the stop is still the run's end.
    #[test]
    fn a_panic_after_a_stop_still_fails() {
        let coord = coord();
        let (go, gate) = mpsc::channel::<()>();
        let (started_tx, started) = mpsc::channel::<()>();
        let joined = thread::scope(|s| {
            let job = coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Bank), move |_| {
                    let _ = started_tx.send(());
                    gate.recv_timeout(watchdog()).expect("the gate opens");
                    panic!("injected decode panic");
                })
                .unwrap();
            job.wait(SLICE, || {
                if started.try_recv().is_err() {
                    return false;
                }
                let _ = go.send(());
                true
            })
        });
        assert_eq!(
            joined,
            Joined::<()>::Failed(LoadFailure {
                job: JobKind::Bank,
                cause: Failure::Panicked(JobPanic("injected decode panic".to_owned())),
            })
        );
        assert_eq!(coord.held(Class::Foreground), 0);
    }

    /// A stop seen at a checkpoint ends the work, cancelled.
    #[test]
    fn a_stop_seen_at_a_checkpoint_cancels_the_job() {
        let coord = coord();
        let (started_tx, started) = mpsc::channel::<()>();
        let joined = thread::scope(|s| {
            let job = coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Measure), move |ctx| {
                    let _ = started_tx.send(());
                    let began = std::time::Instant::now();
                    while !ctx.checkpoint() {
                        assert!(began.elapsed() < watchdog(), "the stop is never seen");
                        thread::yield_now();
                    }
                    "measured"
                })
                .unwrap();
            job.wait(SLICE, || started.try_recv().is_ok())
        });
        assert_eq!(
            joined,
            Joined::Cancelled(Some("measured")),
            "returned after the stop"
        );
        assert_eq!(coord.held(Class::Foreground), 0);
    }

    /// A stop while the job waits for its foreground permit (every one held
    /// elsewhere) ends it before it computes.
    #[test]
    fn a_stop_while_waiting_for_a_permit_runs_nothing() {
        let coord = coord();
        let held: Vec<_> = (0..FOREGROUND_PERMITS)
            .map(|_| coord.try_lease(Class::Foreground).unwrap())
            .collect();
        let ran = AtomicUsize::new(0);
        let expired = std::sync::atomic::AtomicBool::new(false);
        let (done, waited) = mpsc::channel::<()>();
        let joined = thread::scope(|s| {
            // A hang guard: a stop never seen would wait for a permit
            // forever, so the permits are given back at the watchdog and
            // the test fails.
            let expired = &expired;
            s.spawn(move || {
                if waited.recv_timeout(watchdog()).is_err() {
                    expired.store(true, Ordering::SeqCst);
                }
                drop(held);
            });
            let joined = coord
                .run_scoped(s, Spawner::SYSTEM, job(JobKind::Bank), |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap()
                .wait(SLICE, || true);
            let _ = done.send(());
            joined
        });
        assert!(!expired.load(Ordering::SeqCst), "the stop was never seen");
        assert_eq!(joined, Joined::Cancelled(None));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(coord.held(Class::Foreground), 0);
    }
}
