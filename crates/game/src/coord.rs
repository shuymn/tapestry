//! The coordinator's portable core (design.md "Load Pipeline"): permits by
//! class, request scopes with their generations, jobs shared by key, and
//! the commit of a current generation's result. The priority calls are the
//! player's, made through the hook a run is given.
//!
//! - **Permits** come in three classes. Maintenance (the scan) is counted
//!   and never refused. Foreground has [`FOREGROUND_PERMITS`]. Speculative
//!   has `min(setting, SPECULATIVE_MAX, cores − RESERVED_CORES)`, none on
//!   two cores or fewer or when the setting is 0 ([`Caps::for_machine`]). A
//!   [`Lease`] holds one while its work computes and gives it back when
//!   dropped; a job drops it across each wait ([`Ctx::idle`]).
//! - **Scopes** ([`Scope`]) each count their [`Generation`]. A keyed job
//!   ([`Coordinator::begin`]) has subscribers, each of a scope and a
//!   generation; once none of them is current its cancel is fired, and the
//!   job sees it at its next [`Ctx::checkpoint`]. Beginning a job of the
//!   same key joins it while it runs (A→B→A), and clears a cancel it has
//!   not yet acted on.
//! - **Promotion** ([`Handle::promote`], [`Promoter::promote`]) moves a
//!   speculative job to the foreground: at its next checkpoint, on its own
//!   thread, its permit moves to the foreground class and the run's
//!   `on_promote` hook is called once (the player leaves background mode
//!   there).
//! - **Outcomes** ([`Outcome`]) carry no policy: done, cancelled, or
//!   panicked with the panic caught at the job's boundary. The caller
//!   decides what a panic means.
//! - **Commit**: [`Coordinator::take`] gives a result only to a subscriber
//!   whose generation is still current; a superseded one gets
//!   [`Taken::Stale`] and nothing else. Workers install nothing.

use std::any::Any;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

/// Foreground permits: the chosen chart's decode threads, from a choice to
/// its READY state (design.md "Load Pipeline", system friendliness).
pub const FOREGROUND_PERMITS: usize = 4;
/// The most speculative permits, whatever the setting.
pub const SPECULATIVE_MAX: usize = 2;
/// The speculative permits by default.
pub const SPECULATIVE_DEFAULT: usize = 1;
/// The cores speculation leaves to the rest of the machine.
pub const RESERVED_CORES: usize = 2;

/// How often a wait for a permit asks whether to give up: a release wakes
/// it at once.
const PERMIT_SLICE: Duration = Duration::from_millis(10);

/// A permit's class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Catalog maintenance (the scan): counted, never refused.
    Maintenance,
    /// The chosen chart's work.
    Foreground,
    /// Work for what may be chosen.
    Speculative,
}

impl Class {
    const fn index(self) -> usize {
        match self {
            Self::Maintenance => 0,
            Self::Foreground => 1,
            Self::Speculative => 2,
        }
    }
}

/// How many permits each bounded class has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Foreground permits.
    pub foreground: usize,
    /// Speculative permits: none disables speculation.
    pub speculative: usize,
}

impl Caps {
    /// The caps for a machine of `cores` cores with `setting` speculative
    /// permits asked for: `min(setting, SPECULATIVE_MAX, cores −
    /// RESERVED_CORES)` speculative, none on two cores or fewer.
    #[must_use]
    pub const fn for_machine(setting: usize, cores: usize) -> Self {
        let mut speculative = cores.saturating_sub(RESERVED_CORES);
        if setting < speculative {
            speculative = setting;
        }
        if SPECULATIVE_MAX < speculative {
            speculative = SPECULATIVE_MAX;
        }
        Self {
            foreground: FOREGROUND_PERMITS,
            speculative,
        }
    }

    /// The caps for this machine with the default setting.
    #[must_use]
    pub fn this_machine() -> Self {
        let cores = thread::available_parallelism().map_or(1, usize::from);
        Self::for_machine(SPECULATIVE_DEFAULT, cores)
    }

    /// The cap of `class`; none for maintenance.
    const fn of(self, class: Class) -> Option<usize> {
        match class {
            Class::Maintenance => None,
            Class::Foreground => Some(self.foreground),
            Class::Speculative => Some(self.speculative),
        }
    }
}

/// A request scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// A song folder, a table's level or a search result.
    Folder,
    /// The cursor's chart.
    Cursor,
    /// The chosen chart.
    Choice,
}

impl Scope {
    const fn index(self) -> usize {
        match self {
            Self::Folder => 0,
            Self::Cursor => 1,
            Self::Choice => 2,
        }
    }
}

/// A scope's generation: each request of the scope makes a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// Who wants a job's result: a scope's generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Subscriber {
    /// The scope.
    pub scope: Scope,
    /// Its generation when it subscribed.
    pub generation: Generation,
}

/// What a job computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// A chart's loudness at 1×.
    Loudness,
}

/// What tells jobs apart: two of one key are one job.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobKey {
    /// What the job computes.
    pub kind: JobKind,
    /// The chart's file.
    pub path: PathBuf,
}

/// A panic caught at a job's boundary, with its message when it had one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobPanic(pub String);

impl std::fmt::Display for JobPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a job ended. The caller decides what each means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// Its result.
    Done(T),
    /// Cancelled before or while it computed: no result.
    Cancelled,
    /// It panicked.
    Panicked(JobPanic),
}

/// Runs `work`, catching a panic.
///
/// # Errors
///
/// The panic, when `work` panicked.
pub fn catch<T>(work: impl FnOnce() -> T) -> Result<T, JobPanic> {
    panic::catch_unwind(AssertUnwindSafe(work)).map_err(|payload| {
        let payload: &(dyn Any + Send) = &*payload;
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        JobPanic(message)
    })
}

/// A job's flags, shared by its runner and whoever may cancel or promote
/// it.
#[derive(Debug, Default)]
struct Flags {
    cancel: AtomicBool,
    promote: AtomicBool,
}

/// Where a keyed job's outcome waits for its subscribers.
struct Slot<T> {
    outcome: Mutex<Option<Outcome<T>>>,
}

struct Entry {
    flags: Arc<Flags>,
    subscribers: Vec<(u64, Subscriber)>,
    /// The job's `Arc<Slot<T>>`.
    slot: Arc<dyn Any + Send + Sync>,
}

#[derive(Debug)]
struct State {
    caps: Caps,
    held: [usize; 3],
    generations: [u64; 3],
    next_subscription: u64,
}

struct Shared {
    state: Mutex<State>,
    jobs: Mutex<HashMap<JobKey, Entry>>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn jobs(&self) -> MutexGuard<'_, HashMap<JobKey, Entry>> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, class: Class) {
        let mut state = self.lock();
        if let Some(held) = state.held.get_mut(class.index()) {
            *held = held.saturating_sub(1);
        }
        drop(state);
        self.changed.notify_all();
    }
}

/// The coordinator: permits, scopes and jobs. Clones share it.
#[derive(Clone)]
pub struct Coordinator {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator")
            .field("state", &*self.shared.lock())
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    /// A coordinator with `caps`.
    #[must_use]
    pub fn new(caps: Caps) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    caps,
                    held: [0; 3],
                    generations: [0; 3],
                    next_subscription: 0,
                }),
                jobs: Mutex::new(HashMap::new()),
                changed: Condvar::new(),
            }),
        }
    }

    /// Its caps.
    #[must_use]
    pub fn caps(&self) -> Caps {
        self.shared.lock().caps
    }

    /// How many permits of `class` are held.
    #[must_use]
    pub fn held(&self, class: Class) -> usize {
        self.shared
            .lock()
            .held
            .get(class.index())
            .copied()
            .unwrap_or(0)
    }

    /// A permit of `class` if one is free; maintenance always gets one.
    #[must_use]
    pub fn try_lease(&self, class: Class) -> Option<Lease> {
        let mut state = self.shared.lock();
        let caps = state.caps;
        let held = state.held.get_mut(class.index())?;
        if caps.of(class).is_some_and(|cap| *held >= cap) {
            return None;
        }
        *held += 1;
        Some(Lease {
            shared: Arc::clone(&self.shared),
            class,
        })
    }

    /// Waits for a permit of `class`, asking `give_up` between waits;
    /// none once it says so, or at once when the class has no permit at
    /// all.
    pub fn lease(&self, class: Class, give_up: &dyn Fn() -> bool) -> Option<Lease> {
        if self.caps().of(class) == Some(0) {
            return None;
        }
        loop {
            if give_up() {
                return None;
            }
            if let Some(lease) = self.try_lease(class) {
                return Some(lease);
            }
            let state = self.shared.lock();
            let full = state
                .caps
                .of(class)
                .zip(state.held.get(class.index()))
                .is_some_and(|(cap, held)| *held >= cap);
            if full {
                drop(
                    self.shared
                        .changed
                        .wait_timeout(state, PERMIT_SLICE)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            }
        }
    }

    /// The current generation of `scope`.
    #[must_use]
    pub fn current(&self, scope: Scope) -> Generation {
        Generation(
            self.shared
                .lock()
                .generations
                .get(scope.index())
                .copied()
                .unwrap_or(0),
        )
    }

    /// A subscriber of `scope`'s current generation.
    #[must_use]
    pub fn subscriber(&self, scope: Scope) -> Subscriber {
        Subscriber {
            scope,
            generation: self.current(scope),
        }
    }

    /// Makes a new generation of `scope` and fires the cancel of every
    /// job left with no current subscriber.
    pub fn advance(&self, scope: Scope) -> Generation {
        let generations = {
            let mut state = self.shared.lock();
            if let Some(g) = state.generations.get_mut(scope.index()) {
                *g += 1;
            }
            state.generations
        };
        for entry in self.shared.jobs().values() {
            if !any_current(&entry.subscribers, generations) {
                entry.flags.cancel.store(true, Ordering::SeqCst);
            }
        }
        self.shared.changed.notify_all();
        Generation(generations.get(scope.index()).copied().unwrap_or(0))
    }

    /// Subscribes `subscriber` to the job of `key`: it joins the job when
    /// one runs, clearing a cancel the job has not acted on; otherwise the
    /// job begins, and its [`Runner`] is returned with the handle, for the
    /// caller to run on a permit of `class`.
    #[must_use]
    pub fn begin<T: Clone + Send + 'static>(
        &self,
        key: JobKey,
        class: Class,
        subscriber: Subscriber,
    ) -> (Handle<T>, Option<Runner<T>>) {
        let id = {
            let mut state = self.shared.lock();
            state.next_subscription += 1;
            state.next_subscription
        };
        let mut jobs = self.shared.jobs();
        if let Some(entry) = jobs.get_mut(&key)
            && let Ok(slot) = Arc::clone(&entry.slot).downcast::<Slot<T>>()
        {
            entry.subscribers.push((id, subscriber));
            entry.flags.cancel.store(false, Ordering::SeqCst);
            let handle = Handle {
                shared: Arc::clone(&self.shared),
                key,
                id,
                subscriber,
                slot,
                flags: Arc::clone(&entry.flags),
            };
            return (handle, None);
        }
        let flags = Arc::new(Flags::default());
        let slot = Arc::new(Slot {
            outcome: Mutex::new(None),
        });
        let any_slot: Arc<dyn Any + Send + Sync> = Arc::clone(&slot) as _;
        jobs.insert(
            key.clone(),
            Entry {
                flags: Arc::clone(&flags),
                subscribers: vec![(id, subscriber)],
                slot: any_slot,
            },
        );
        drop(jobs);
        let handle = Handle {
            shared: Arc::clone(&self.shared),
            key: key.clone(),
            id,
            subscriber,
            slot: Arc::clone(&slot),
            flags: Arc::clone(&flags),
        };
        let runner = Runner {
            coord: self.clone(),
            class,
            flags,
            publish: Some(Publish { key, slot }),
        };
        (handle, Some(runner))
    }

    /// A job of its owner's alone, with no key and no subscriber: only its
    /// owner's `stop` cancels it, and its [`Promoter`] promotes it.
    #[must_use]
    pub fn task<T>(&self, class: Class) -> Runner<T> {
        Runner {
            coord: self.clone(),
            class,
            flags: Arc::new(Flags::default()),
            publish: None,
        }
    }

    /// The outcome of `handle`'s job for its subscriber: only while the
    /// subscriber's generation is current.
    #[must_use]
    pub fn take<T: Clone>(&self, handle: &Handle<T>) -> Taken<T> {
        if self.current(handle.subscriber.scope) != handle.subscriber.generation {
            return Taken::Stale;
        }
        handle
            .slot
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .map_or(Taken::Pending, Taken::Ready)
    }
}

fn any_current(subscribers: &[(u64, Subscriber)], generations: [u64; 3]) -> bool {
    subscribers
        .iter()
        .any(|(_, s)| generations.get(s.scope.index()) == Some(&s.generation.0))
}

/// What [`Coordinator::take`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken<T> {
    /// The job has not ended.
    Pending,
    /// How it ended.
    Ready(Outcome<T>),
    /// The subscriber's generation is superseded: its result is not given.
    Stale,
}

/// A permit, given back when dropped.
pub struct Lease {
    shared: Arc<Shared>,
    class: Class,
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl Lease {
    /// Its class.
    #[must_use]
    pub const fn class(&self) -> Class {
        self.class
    }

    /// Moves it to the foreground class if a foreground permit is free;
    /// whether it is a foreground permit now.
    pub fn promote(&mut self) -> bool {
        if self.class == Class::Foreground {
            return true;
        }
        let mut state = self.shared.lock();
        let caps = state.caps;
        let fg = Class::Foreground.index();
        if state.held.get(fg).is_some_and(|h| *h >= caps.foreground) {
            return false;
        }
        if let Some(held) = state.held.get_mut(fg) {
            *held += 1;
        }
        if let Some(held) = state.held.get_mut(self.class.index()) {
            *held = held.saturating_sub(1);
        }
        drop(state);
        self.class = Class::Foreground;
        self.shared.changed.notify_all();
        true
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.shared.release(self.class);
    }
}

/// A subscriber's handle on a keyed job. Dropping it unsubscribes, and
/// the job's cancel is fired once no subscriber is current.
pub struct Handle<T> {
    shared: Arc<Shared>,
    key: JobKey,
    id: u64,
    subscriber: Subscriber,
    slot: Arc<Slot<T>>,
    flags: Arc<Flags>,
}

impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("key", &self.key)
            .field("subscriber", &self.subscriber)
            .finish_non_exhaustive()
    }
}

impl<T> Handle<T> {
    /// Its subscriber.
    #[must_use]
    pub const fn subscriber(&self) -> Subscriber {
        self.subscriber
    }

    /// Promotes its job to the foreground (at the job's next checkpoint).
    pub fn promote(&self) {
        self.flags.promote.store(true, Ordering::SeqCst);
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        let generations = self.shared.lock().generations;
        let mut jobs = self.shared.jobs();
        if let Some(entry) = jobs.get_mut(&self.key)
            && Arc::ptr_eq(&entry.flags, &self.flags)
        {
            entry.subscribers.retain(|(id, _)| *id != self.id);
            if !any_current(&entry.subscribers, generations) {
                entry.flags.cancel.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Promotes a task ([`Coordinator::task`]) from another thread.
#[derive(Clone, Debug)]
pub struct Promoter(Arc<Flags>);

impl Promoter {
    /// Promotes the task at its next checkpoint, or when it starts.
    pub fn promote(&self) {
        self.0.promote.store(true, Ordering::SeqCst);
    }
}

struct Publish<T> {
    key: JobKey,
    slot: Arc<Slot<T>>,
}

/// A job to run on the caller's thread ([`Runner::run`]). Dropped without
/// running, a keyed job ends cancelled.
pub struct Runner<T> {
    coord: Coordinator,
    class: Class,
    flags: Arc<Flags>,
    publish: Option<Publish<T>>,
}

impl<T> std::fmt::Debug for Runner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl<T> Runner<T> {
    /// Its promoter.
    #[must_use]
    pub fn promoter(&self) -> Promoter {
        Promoter(Arc::clone(&self.flags))
    }

    /// Runs `work` on this thread on a permit of its class (the foreground
    /// once promoted), waiting for one while `stop` and the job's cancel
    /// allow: [`Outcome::Cancelled`] when either stops the wait, or when
    /// the cancel was fired by the time `work` returned. A panic in `work`
    /// is caught. `on_promote` is called once, on this thread, when the
    /// job is promoted.
    pub fn run(
        mut self,
        stop: &(dyn Fn() -> bool + Sync),
        on_promote: &(dyn Fn() + Sync),
        work: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Outcome<T>
    where
        T: Clone,
    {
        let outcome = self.compute(stop, on_promote, work);
        if let Some(publish) = self.publish.take() {
            self.finish(&publish, outcome.clone());
        }
        outcome
    }

    fn compute(
        &self,
        stop: &(dyn Fn() -> bool + Sync),
        on_promote: &(dyn Fn() + Sync),
        work: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Outcome<T> {
        let ctx = Ctx {
            coord: &self.coord,
            flags: &self.flags,
            stop,
            on_promote,
            thread: thread::current().id(),
            lease: Mutex::new(None),
            promoted: AtomicBool::new(false),
        };
        if !ctx.acquire(self.class) {
            return Outcome::Cancelled;
        }
        match catch(|| work(&ctx)) {
            Err(panic) => Outcome::Panicked(panic),
            Ok(_) if self.flags.cancel.load(Ordering::SeqCst) => Outcome::Cancelled,
            Ok(value) => Outcome::Done(value),
        }
    }

    fn finish(&self, publish: &Publish<T>, outcome: Outcome<T>) {
        // Out of the registry first: a later begin of the key starts anew.
        let mut jobs = self.coord.shared.jobs();
        if jobs
            .get(&publish.key)
            .is_some_and(|e| Arc::ptr_eq(&e.flags, &self.flags))
        {
            jobs.remove(&publish.key);
        }
        drop(jobs);
        *publish
            .slot
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        self.coord.shared.changed.notify_all();
    }
}

impl<T> Drop for Runner<T> {
    fn drop(&mut self) {
        if let Some(publish) = self.publish.take() {
            self.finish(&publish, Outcome::Cancelled);
        }
    }
}

/// What a running job sees: its cancel, its checkpoints, and its permit.
pub struct Ctx<'a> {
    coord: &'a Coordinator,
    flags: &'a Flags,
    stop: &'a (dyn Fn() -> bool + Sync),
    on_promote: &'a (dyn Fn() + Sync),
    thread: ThreadId,
    lease: Mutex<Option<Lease>>,
    promoted: AtomicBool,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("class", &self.class())
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    fn lease(&self) -> MutexGuard<'_, Option<Lease>> {
        self.lease.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the job is cancelled, or its owner's `stop` says to stop.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.flags.cancel.load(Ordering::SeqCst) || (self.stop)()
    }

    /// The class of its permit; none while it waits without one.
    #[must_use]
    pub fn class(&self) -> Option<Class> {
        self.lease().as_ref().map(Lease::class)
    }

    /// Whether it was promoted.
    #[must_use]
    pub fn promoted(&self) -> bool {
        self.promoted.load(Ordering::SeqCst)
    }

    /// A checkpoint: on the job's own thread, applies a promotion asked
    /// for (the permit moved, or retaken as a foreground one when none is
    /// free, and `on_promote` called once). Whether the job should stop.
    pub fn checkpoint(&self) -> bool {
        if self.flags.promote.load(Ordering::SeqCst)
            && !self.promoted()
            && thread::current().id() == self.thread
        {
            let moved = self.lease().as_mut().is_some_and(Lease::promote);
            if moved {
                self.promoted.store(true, Ordering::SeqCst);
                (self.on_promote)();
            } else {
                // Its permit given back, a foreground one taken; `acquire`
                // records the promotion.
                *self.lease() = None;
                if !self.acquire(Class::Foreground) {
                    return true;
                }
            }
        }
        self.cancelled()
    }

    /// Runs `wait` without its permit, retaken after: none when the job
    /// is cancelled while it waits for it again.
    pub fn idle<R>(&self, wait: impl FnOnce() -> R) -> Option<R> {
        let class = self.lease().take().map(|l| l.class())?;
        let out = wait();
        self.acquire(class).then_some(out)
    }

    /// Takes a permit of `class`, the foreground once promoted; `false`
    /// when the wait was stopped.
    fn acquire(&self, class: Class) -> bool {
        let promote = self.flags.promote.load(Ordering::SeqCst);
        let class = if promote { Class::Foreground } else { class };
        let Some(lease) = self.coord.lease(class, &|| self.cancelled()) else {
            return false;
        };
        *self.lease() = Some(lease);
        if promote && !self.promoted() && thread::current().id() == self.thread {
            self.promoted.store(true, Ordering::SeqCst);
            (self.on_promote)();
        }
        true
    }
}

#[cfg(test)]
mod tests;
