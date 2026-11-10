//! Owned jobs (design.md "Load Pipeline"): a job on a thread of its own
//! that outlives any one waiter, held by one owner that can promote it,
//! wait for it, stop it and settle it within a bound.
//!
//! - **Lifecycle under one lock.** A job is live (queued or computing,
//!   promotable), ending (its cancel requested or already seen: it ends
//!   cancelled and cannot be revived) or published (out of the registry,
//!   its outcome in its slot). `Runner::finish` removes it, reads its
//!   final cancel and publishes in one critical section, so the
//!   coordinator's lock never sees it as neither. Its thread may still run
//!   after publication (unwinding).
//! - **Ending is final, by construction.** The one place a requested
//!   cancel is withdrawn, the cancel decision, never withdraws an owned
//!   job's, whatever subscriber turns current (a later generation
//!   included); a seen cancel is never withdrawn for any job. No `begin`
//!   joins an owned job either (a same-key `begin` starts a job of its own
//!   and takes the key), so its owner's stays its only subscriber. No entry
//!   point (`begin`, `spawn_owned`, `advance`, a promotion or demotion, a
//!   checkpoint, a wait or a read) brings an ending owned job back. Keyed
//!   jobs keep their resubscription (design.md "Load Pipeline", A→B→A).
//! - **Who answers for it.** The outcome is published with its owner then
//!   ([`Owner`]) and whether a promotion of the live job made a waiter
//!   responsible for it; nothing withdraws responsibility, a stopped wait
//!   or a demotion included. The coordinator decides nothing by them: the
//!   callers' rule is in [`Observed`].
//! - **Locks.** The lifecycle and the owner are decided under the state
//!   lock; a published payload is cloned after every coordinator lock is
//!   let go, and no callback (`stop`, `terminal`, `clock`, `on_class`)
//!   runs under one.
//! - **Settling** stops the job and waits for its thread. Once a terminal
//!   cause is seen, the wait is bounded: past it the thread is detached,
//!   and the cause comes back either way. A [`Settled::Joined`] job's
//!   `after_panic` is a panic of its thread after publication: a published
//!   `Done` does not prove its thread ended cleanly.
//! - **Dropping** an unsettled job never blocks: it stops it and detaches
//!   the thread, a fallback that proves nothing about what the job held.
//!
//! **What the bound covers.** It is there so that work that hangs (a
//! decoder stuck on a crafted file, design.md "Threat Model") cannot
//! freeze the thread that settles it: the work and everything it calls
//! are covered. Everything else a settle runs, or waits for, is
//! first-party code that does not block, a precondition rather than
//! something a settle can bound:
//!
//! - the result type's `Clone` and `Drop`;
//! - the job thread's own exit after its work: the hook's captures
//!   dropping and its thread-local destructors, which a join waits for
//!   even once the thread reports finished;
//! - the caller's `terminal` and `clock`, and the drop of the cause they
//!   gave (also while a panic of theirs unwinds the settle);
//! - the drop of a panic payload taken at the join, or dropped with a
//!   finished thread's handle.
//!
//! An abandonment clones no result.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{
    Cancel, Class, Coordinator, Ctx, Failure, Handle, Job, JobKey, JobPanic, Lease, LoadFailure,
    Observed, Owner, PoisonError, Runner, Slot, Spawner, Subscriber, panic_of,
};

/// How often a settle looks at its thread and the terminal cause.
const SETTLE_SLICE: Duration = Duration::from_millis(1);

/// A job owned by one holder, on its own thread
/// ([`Coordinator::spawn_owned`]).
pub struct OwnedJob<T> {
    coord: Coordinator,
    id: u64,
    slot: Arc<Slot<T>>,
    /// Its one subscriber, until it is settled.
    handle: Option<Handle<T>>,
    /// The owner's stop, which its admission and checkpoints read.
    stop: Arc<AtomicBool>,
    /// Its thread, until joined or detached.
    thread: Option<JoinHandle<()>>,
}

impl<T> std::fmt::Debug for OwnedJob<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedJob")
            .field("id", &self.id)
            .field("settled", &self.thread.is_none())
            .finish_non_exhaustive()
    }
}

/// What [`OwnedJob::promote_unless_ended`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Promotion<T> {
    /// Live: promoted, and the caller is responsible for it.
    Live,
    /// Its cancel was requested or seen: it ends cancelled and cannot be
    /// revived. Settle it before beginning its key again.
    Ending,
    /// Published, with what was recorded then.
    Published(Observed<T>),
}

/// What [`OwnedJob::to_owner`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handed<T> {
    /// Unpublished: its outcome will carry the new owner.
    Changed,
    /// Already published, judged by the owner it published under.
    Published(Observed<T>),
}

/// How [`OwnedJob::wait`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited<T> {
    /// Published.
    Done(Observed<T>),
    /// The waiter stopped: the job was demoted and goes on; what had been
    /// published by then, if anything.
    Stopped(Option<Observed<T>>),
}

/// How [`OwnedJob::settle`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Settled<T, C> {
    /// Its thread was joined.
    Joined {
        /// What it published; none when its thread ended without.
        observed: Option<Observed<T>>,
        /// Its thread's panic after publication, or instead of it.
        after_panic: Option<JobPanic>,
        /// The terminal cause seen while settling, if any.
        cause: Option<C>,
    },
    /// The bound passed after a terminal cause: its thread is detached.
    Abandoned {
        /// What it had published by then, not cloned.
        observed: Option<Arc<Observed<T>>>,
        /// The terminal cause.
        cause: C,
    },
}

/// Where in its lifecycle a job is, under the lock.
enum Phase {
    Live,
    Ending,
    Published,
}

impl Coordinator {
    /// Begins the job of `key` on a thread of its own, owned by the
    /// returned [`OwnedJob`]: in `class`, promoted from the start when
    /// `promoted`, answered for by `owner`, for `subscriber`. `on_class`
    /// is called on the job's thread at each change of the class it
    /// computes in; it drops after the job publishes.
    ///
    /// Precondition (module doc, what the bound covers): `T`'s `Clone` and
    /// `Drop`, `on_class`'s captures' `Drop` and the thread-local
    /// destructors the job's thread runs at its exit do not block. `work`
    /// may. The job is never joined by a [`begin`](Self::begin) of its key.
    ///
    /// # Errors
    ///
    /// A job of `key` already runs (of any kind of result): nothing is
    /// replaced or joined. Or the thread could not start: the job ended
    /// cancelled and holds nothing.
    pub fn spawn_owned<T, W, C>(
        &self,
        spawner: Spawner,
        key: JobKey,
        (class, promoted, owner): (Class, bool, Owner),
        subscriber: Subscriber,
        on_class: C,
        work: W,
    ) -> Result<OwnedJob<T>, LoadFailure>
    where
        T: Clone + Send + Sync + 'static,
        W: FnOnce(&Ctx<'_>) -> T + Send + 'static,
        C: Fn(Class) + Send + Sync + 'static,
    {
        let kind = key.kind;
        let refused = |e: String| LoadFailure {
            job: kind,
            cause: Failure::ThreadStart(e),
        };
        let slot = Arc::new(Slot::<T>::new());
        let mut state = self.shared.lock();
        if state.keys.contains_key(&key) {
            return Err(refused("the job already runs".to_owned()));
        }
        let sub = state.next_id();
        let id = state.next_id();
        let any_slot: Arc<dyn Any + Send + Sync> = Arc::clone(&slot) as _;
        state.jobs.insert(
            id,
            Job {
                key: Some(key.clone()),
                subscribers: vec![(sub, subscriber)],
                cancel: Cancel::No,
                base: class,
                promoted,
                owner,
                responsible: false,
                owned: true,
                slot: Some(any_slot),
            },
        );
        state.keys.insert(key, id);
        state.decide(id);
        drop(state);
        self.shared.changed.notify_all();
        let handle = Handle {
            shared: Arc::clone(&self.shared),
            job: id,
            sub,
            subscriber,
            slot: Arc::clone(&slot),
        };
        let runner = Runner {
            coord: self.clone(),
            id,
            base: class,
            slot: Some(Arc::clone(&slot)),
            done: false,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        // A refused thread drops the runner, which ends the job cancelled.
        let thread = spawner
            .spawn_owned(kind, move || {
                let _ = runner.run(&|| stopped.load(Ordering::Acquire), &on_class, work);
            })
            .map_err(|e| refused(e.to_string()))?;
        Ok(OwnedJob {
            coord: self.clone(),
            id,
            slot,
            handle: Some(handle),
            stop,
            thread: Some(thread),
        })
    }
}

impl<T: Clone> OwnedJob<T> {
    /// Its lifecycle under the lock; promoted and made responsible when
    /// live and `promote`.
    fn phase(&self, promote: bool) -> Phase {
        self.coord.shared.update(|state| {
            state.decide(self.id);
            match state.jobs.get_mut(&self.id) {
                None => Phase::Published,
                Some(job) if job.cancel != Cancel::No => Phase::Ending,
                Some(job) => {
                    if promote {
                        job.promoted = true;
                        job.responsible = true;
                    }
                    Phase::Live
                }
            }
        })
    }

    /// What it published, if it has: the payload cloned outside every
    /// coordinator lock.
    #[must_use]
    pub fn poll(&self) -> Option<Observed<T>> {
        self.slot.record().map(|r| (*r).clone())
    }

    /// Promotes it if it is still live, and makes the caller responsible
    /// for it from then on (a later demotion or stopped wait keeps that).
    #[must_use]
    pub fn promote_unless_ended(&self) -> Promotion<T> {
        match self.phase(true) {
            Phase::Live => Promotion::Live,
            Phase::Ending => Promotion::Ending,
            // Out of the registry is published: one critical section.
            Phase::Published => self.poll().map_or(Promotion::Ending, Promotion::Published),
        }
    }

    /// Withdraws the foreground interest: it moves back to its own class
    /// at its next checkpoint. Responsibility stays.
    pub fn demote(&self) {
        self.coord
            .shared
            .update(|state| state.set_promoted(self.id, false));
    }

    /// Hands it to `owner` if it has not published; once published, it is
    /// judged by the owner it published under.
    #[must_use]
    pub fn to_owner(&self, owner: Owner) -> Handed<T> {
        let changed = self
            .coord
            .shared
            .update(|state| state.jobs.get_mut(&self.id).map(|job| job.owner = owner));
        match changed {
            Some(()) => Handed::Changed,
            None => self.poll().map_or(Handed::Changed, Handed::Published),
        }
    }

    /// Waits for its outcome, asking `stop` each `slice` (after a look at
    /// the slot, and with a look again after a yes). A stop demotes it and
    /// never cancels it.
    pub fn wait(&self, slice: Duration, mut stop: impl FnMut() -> bool) -> Waited<T> {
        loop {
            if let Some(o) = self.poll() {
                return Waited::Done(o);
            }
            if stop() {
                self.demote();
                return Waited::Stopped(self.poll());
            }
            let state = self.coord.shared.lock();
            if state.jobs.contains_key(&self.id) {
                drop(
                    self.coord
                        .shared
                        .changed
                        .wait_timeout(state, slice)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
    }

    /// Stops it and waits for its thread. `terminal` is asked each slice
    /// until it gives a cause (and once more at the join when it had not):
    /// from then on the wait lasts at most `bound` on `clock`, after which
    /// the thread is detached. Without a cause the wait is not bounded.
    /// The caller looks at terminal failures again before any return that
    /// does not end the run.
    ///
    /// The bound covers the job's work only. Precondition (module doc):
    /// `terminal` and `clock` return; the cause `C`'s `Drop` does not block,
    /// also while a panic of `terminal` or `clock` unwinds this settle; and
    /// so do `T`'s `Clone` and `Drop`, the job thread's exit, and the drop
    /// of a panic payload taken at the join.
    pub fn settle<C>(
        mut self,
        bound: Duration,
        clock: &dyn Fn() -> Instant,
        terminal: &mut dyn FnMut() -> Option<C>,
    ) -> Settled<T, C> {
        self.stop.store(true, Ordering::Release);
        drop(self.handle.take());
        let mut cause = None;
        let mut deadline = None;
        loop {
            if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                let after_panic = self
                    .thread
                    .take()
                    .and_then(|t| t.join().err())
                    .map(|p| panic_of(&*p));
                if cause.is_none() {
                    cause = terminal();
                }
                return Settled::Joined {
                    observed: self.poll(),
                    after_panic,
                    cause,
                };
            }
            if cause.is_none()
                && let Some(c) = terminal()
            {
                cause = Some(c);
                deadline = Some(clock() + bound);
            }
            if let Some(d) = deadline
                && clock() >= d
                && let Some(cause) = cause.take()
            {
                drop(self.thread.take());
                return Settled::Abandoned {
                    observed: self.slot.record(),
                    cause,
                };
            }
            thread::park_timeout(SETTLE_SLICE);
        }
    }
}

impl<T> OwnedJob<T> {
    /// Whether its thread has ended, as the settle would see it (tests).
    #[cfg(test)]
    pub(crate) fn thread_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl<T> Drop for OwnedJob<T> {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop.store(true, Ordering::Release);
            drop(self.handle.take());
            drop(self.thread.take());
            #[cfg(test)]
            self.coord.shared.unsettled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Foreground permits for a job's extra workers (a decode pool's beyond
/// the job's own thread).
#[derive(Debug)]
pub struct WorkerLeases<'a> {
    coord: &'a Coordinator,
}

/// One extra worker's foreground permit: a pending reservation until the
/// caller starts the worker (a failed start drops it, giving the permit
/// back), then the worker's until it retires.
#[derive(Debug)]
pub struct WorkerLease {
    /// Held for its drop, which gives the permit back.
    _permit: Lease,
}

impl<'a> WorkerLeases<'a> {
    /// Extra workers' permits of `coord`.
    #[must_use]
    pub const fn new(coord: &'a Coordinator) -> Self {
        Self { coord }
    }

    /// A permit for one more extra worker of `ctx`'s job, while the job
    /// wants the foreground and `extra` (its extra workers running or
    /// reserved) stays below the pool's own cap less the job's thread,
    /// `effective_cap.max(1) − 1`; none otherwise or when none is free. A
    /// demotion or a smaller cap stops new ones and takes none back.
    #[must_use]
    pub fn reserve(
        &self,
        ctx: &Ctx<'_>,
        effective_cap: usize,
        extra: usize,
    ) -> Option<WorkerLease> {
        if ctx.wanted() != Class::Foreground || extra >= effective_cap.max(1) - 1 {
            return None;
        }
        self.coord
            .try_lease(Class::Foreground)
            .map(|permit| WorkerLease { _permit: permit })
    }
}

impl WorkerLease {
    /// The worker retires: its permit goes back.
    pub fn retire(self) {}
}

#[cfg(test)]
mod tests;
