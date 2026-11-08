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
//! - **One lock** holds the permits, the generations and every job's
//!   subscribers, cancel and class, so a job's fate is decided in one
//!   place, on the live generations.
//! - **Scopes** ([`Scope`]) each count their [`Generation`]. A keyed job
//!   ([`Coordinator::begin`]) has subscribers, each of a scope and a
//!   generation. Once none of them is current the job is asked to cancel;
//!   a current subscriber arriving before the job sees the request
//!   withdraws it (A→B→A joins the running job). A cancel the job has seen
//!   is terminal: the job can no longer be joined, a later begin of its
//!   key starts a fresh job, and it ends [`Outcome::Cancelled`], never
//!   done with what it had when it stopped.
//! - **Promotion** follows foreground interest ([`Handle::promote`],
//!   [`Promoter::promote`] and their `demote`): a promoted speculative job
//!   runs on a foreground permit, taken at its admission or moved at its
//!   next checkpoint, and returns to its own class when demoted. Only its
//!   own thread changes its permit, and the run's hook is called there at
//!   each change of class (the player changes the thread's mode).
//! - **Outcomes** ([`Outcome`]) carry no policy: done, cancelled, or
//!   panicked with the panic caught at the job's boundary, its admission
//!   and hooks included. The caller decides what a panic means.
//! - **Commit**: [`Coordinator::take`] gives a result only to a subscriber
//!   whose generation is still current; a superseded one gets
//!   [`Taken::Stale`] and nothing else. Workers install nothing.

use std::any::Any;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
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

/// Where a job is in its cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cancel {
    /// Not asked for, or withdrawn before the job saw it.
    No,
    /// Asked for (no subscriber is current); withdrawn if one becomes
    /// current before the job sees it.
    Requested,
    /// Seen by the job: terminal, and the job can no longer be joined.
    Observed,
}

/// A job not yet finished, keyed or not.
struct Job {
    /// Its key while it may be joined.
    key: Option<JobKey>,
    subscribers: Vec<(u64, Subscriber)>,
    cancel: Cancel,
    /// The class it was begun in.
    base: Class,
    /// Foreground interest: it should run on a foreground permit.
    promoted: bool,
    /// A keyed job's `Arc<Slot<T>>`.
    slot: Option<Arc<dyn Any + Send + Sync>>,
}

impl Job {
    /// The class it should run in now.
    const fn want(&self) -> Class {
        if self.promoted {
            Class::Foreground
        } else {
            self.base
        }
    }
}

/// Everything that decides a job's fate, under one lock.
struct State {
    held: [usize; 3],
    generations: [u64; 3],
    next_id: u64,
    jobs: HashMap<u64, Job>,
    /// The joinable job of each key.
    keys: HashMap<JobKey, u64>,
}

impl State {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Takes a permit of `class` if one is free.
    fn take_permit(&mut self, caps: Caps, class: Class) -> bool {
        let Some(held) = self.held.get_mut(class.index()) else {
            return false;
        };
        if caps.of(class).is_some_and(|cap| *held >= cap) {
            return false;
        }
        *held += 1;
        true
    }

    fn give_permit(&mut self, class: Class) {
        if let Some(held) = self.held.get_mut(class.index()) {
            *held = held.saturating_sub(1);
        }
    }

    /// The one cancel decision, made on the live generations: a keyed job
    /// with no current subscriber is asked to cancel; one with a current
    /// subscriber again has the request withdrawn, unless it saw it.
    fn decide(&mut self, id: u64) {
        let generations = self.generations;
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if job.key.is_none() && job.subscribers.is_empty() {
            // A task, or a detached job: only its owner stops it, or it
            // already saw its cancel.
            return;
        }
        let current = job
            .subscribers
            .iter()
            .any(|(_, s)| generations.get(s.scope.index()) == Some(&s.generation.0));
        job.cancel = match (job.cancel, current) {
            (Cancel::No, false) => Cancel::Requested,
            (Cancel::Requested, true) => Cancel::No,
            (cancel, _) => cancel,
        };
    }

    /// Whether job `id` is cancelled; a request is latched as seen, and the
    /// job can no longer be joined.
    fn observe(&mut self, id: u64) -> bool {
        let Some(job) = self.jobs.get_mut(&id) else {
            return false;
        };
        if job.cancel == Cancel::No {
            return false;
        }
        job.cancel = Cancel::Observed;
        if let Some(key) = job.key.take()
            && self.keys.get(&key) == Some(&id)
        {
            self.keys.remove(&key);
        }
        true
    }

    /// Removes a finished job.
    fn finish(&mut self, id: u64) {
        if let Some(job) = self.jobs.remove(&id)
            && let Some(key) = job.key
            && self.keys.get(&key) == Some(&id)
        {
            self.keys.remove(&key);
        }
    }

    fn set_promoted(&mut self, id: u64, promoted: bool) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.promoted = promoted;
        }
    }
}

/// Where a keyed job's outcome waits for its subscribers.
struct Slot<T> {
    outcome: Mutex<Option<Outcome<T>>>,
}

struct Shared {
    caps: Caps,
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` under the lock and wakes every waiter.
    fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let out = f(&mut self.lock());
        self.changed.notify_all();
        out
    }
}

/// The coordinator: permits, scopes and jobs. Clones share it.
#[derive(Clone)]
pub struct Coordinator {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("Coordinator")
            .field("caps", &self.shared.caps)
            .field("held", &state.held)
            .field("generations", &state.generations)
            .field("jobs", &state.jobs.len())
            .finish_non_exhaustive()
    }
}

impl Coordinator {
    /// A coordinator with `caps`.
    #[must_use]
    pub fn new(caps: Caps) -> Self {
        Self {
            shared: Arc::new(Shared {
                caps,
                state: Mutex::new(State {
                    held: [0; 3],
                    generations: [0; 3],
                    next_id: 0,
                    jobs: HashMap::new(),
                    keys: HashMap::new(),
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// Its caps.
    #[must_use]
    pub fn caps(&self) -> Caps {
        self.shared.caps
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
        self.shared
            .lock()
            .take_permit(self.shared.caps, class)
            .then(|| Lease {
                shared: Arc::clone(&self.shared),
                class,
            })
    }

    /// Waits for a permit of `class`, asking `give_up` between waits;
    /// none once it says so, or at once when the class has no permit at
    /// all.
    pub fn lease(&self, class: Class, give_up: &dyn Fn() -> bool) -> Option<Lease> {
        if self.shared.caps.of(class) == Some(0) {
            return None;
        }
        loop {
            if give_up() {
                return None;
            }
            let mut state = self.shared.lock();
            if state.take_permit(self.shared.caps, class) {
                return Some(Lease {
                    shared: Arc::clone(&self.shared),
                    class,
                });
            }
            drop(
                self.shared
                    .changed
                    .wait_timeout(state, PERMIT_SLICE)
                    .unwrap_or_else(PoisonError::into_inner),
            );
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

    /// Makes a new generation of `scope`, and asks every job left with no
    /// current subscriber to cancel, in the same step.
    pub fn advance(&self, scope: Scope) -> Generation {
        self.shared.update(|state| {
            if let Some(g) = state.generations.get_mut(scope.index()) {
                *g += 1;
            }
            let ids: Vec<u64> = state.jobs.keys().copied().collect();
            for id in ids {
                state.decide(id);
            }
            Generation(state.generations.get(scope.index()).copied().unwrap_or(0))
        })
    }

    /// Subscribes `subscriber` to the job of `key`: it joins the job when
    /// one runs that has not seen a cancel; otherwise a job begins, and its
    /// [`Runner`] is returned with the handle, for the caller to run on a
    /// permit of `class`. Either way the cancel is decided again on the
    /// live generations: a stale subscriber neither keeps a job alive nor
    /// withdraws its cancel.
    #[must_use]
    pub fn begin<T: Clone + Send + 'static>(
        &self,
        key: JobKey,
        class: Class,
        subscriber: Subscriber,
    ) -> (Handle<T>, Option<Runner<T>>) {
        let mut state = self.shared.lock();
        let sub = state.next_id();
        let joined = state.keys.get(&key).copied().and_then(|id| {
            let job = state.jobs.get_mut(&id)?;
            let slot = Arc::clone(job.slot.as_ref()?).downcast::<Slot<T>>().ok()?;
            job.subscribers.push((sub, subscriber));
            Some((id, slot))
        });
        let (id, slot, runner) = if let Some((id, slot)) = joined {
            (id, slot, false)
        } else {
            let id = state.next_id();
            let slot = Arc::new(Slot {
                outcome: Mutex::new(None),
            });
            let any_slot: Arc<dyn Any + Send + Sync> = Arc::clone(&slot) as _;
            state.jobs.insert(
                id,
                Job {
                    key: Some(key.clone()),
                    subscribers: vec![(sub, subscriber)],
                    cancel: Cancel::No,
                    base: class,
                    promoted: false,
                    slot: Some(any_slot),
                },
            );
            state.keys.insert(key, id);
            (id, slot, true)
        };
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
        let runner = runner.then(|| Runner {
            coord: self.clone(),
            id,
            base: class,
            slot: Some(slot),
            done: false,
        });
        (handle, runner)
    }

    /// A job of its owner's alone, with no key and no subscriber: only its
    /// owner's `stop` cancels it, and its [`Promoter`] promotes it.
    #[must_use]
    pub fn task<T>(&self, class: Class) -> Runner<T> {
        let id = self.shared.update(|state| {
            let id = state.next_id();
            state.jobs.insert(
                id,
                Job {
                    key: None,
                    subscribers: Vec::new(),
                    cancel: Cancel::No,
                    base: class,
                    promoted: false,
                    slot: None,
                },
            );
            id
        });
        Runner {
            coord: self.clone(),
            id,
            base: class,
            slot: None,
            done: false,
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

    /// Moves it to `class` if a permit of `class` is free; whether it is
    /// of `class` now.
    pub fn move_to(&mut self, class: Class) -> bool {
        if self.class == class {
            return true;
        }
        let from = self.class;
        let moved = self.shared.update(|state| {
            let moved = state.take_permit(self.shared.caps, class);
            if moved {
                state.give_permit(from);
            }
            moved
        });
        if moved {
            self.class = class;
        }
        moved
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let class = self.class;
        self.shared.update(|state| state.give_permit(class));
    }
}

/// A subscriber's handle on a keyed job. Dropping it unsubscribes, and the
/// job is asked to cancel once no subscriber is current.
pub struct Handle<T> {
    shared: Arc<Shared>,
    job: u64,
    sub: u64,
    subscriber: Subscriber,
    slot: Arc<Slot<T>>,
}

impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("job", &self.job)
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

    /// Foreground interest in its job: the job moves to a foreground permit
    /// (at its next checkpoint, or while it waits for admission).
    pub fn promote(&self) {
        self.shared
            .update(|state| state.set_promoted(self.job, true));
    }

    /// The interest ends: the job moves back to its own class.
    pub fn demote(&self) {
        self.shared
            .update(|state| state.set_promoted(self.job, false));
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        let (job, sub) = (self.job, self.sub);
        self.shared.update(|state| {
            if let Some(entry) = state.jobs.get_mut(&job) {
                entry.subscribers.retain(|(id, _)| *id != sub);
                state.decide(job);
            }
        });
    }
}

/// Promotes or demotes a task ([`Coordinator::task`]) from another thread.
#[derive(Clone)]
pub struct Promoter {
    shared: Arc<Shared>,
    job: u64,
}

impl std::fmt::Debug for Promoter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Promoter")
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

impl Promoter {
    /// Foreground interest: the task moves to a foreground permit at its
    /// next checkpoint, or while it waits for admission.
    pub fn promote(&self) {
        self.shared
            .update(|state| state.set_promoted(self.job, true));
    }

    /// The interest ends: the task moves back to its own class.
    pub fn demote(&self) {
        self.shared
            .update(|state| state.set_promoted(self.job, false));
    }
}

/// A job to run on the caller's thread ([`Runner::run`]). Dropped without
/// running, it ends cancelled.
pub struct Runner<T> {
    coord: Coordinator,
    id: u64,
    base: Class,
    slot: Option<Arc<Slot<T>>>,
    done: bool,
}

impl<T> std::fmt::Debug for Runner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("id", &self.id)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl<T> Runner<T> {
    /// Its promoter.
    #[must_use]
    pub fn promoter(&self) -> Promoter {
        Promoter {
            shared: Arc::clone(&self.coord.shared),
            job: self.id,
        }
    }

    /// Runs `work` on this thread on a permit of the class the job should
    /// run in, waiting for one while neither `stop` nor the job's cancel
    /// says to stop. [`Outcome::Cancelled`] when either stops the wait,
    /// or when the job was cancelled by the time `work` returned: a
    /// cancelled job never ends done. A panic in `work`, in its admission
    /// or in a hook is caught. `on_class` is called on this thread each
    /// time the class the job computes in changes, starting from the class
    /// it was begun in (the player changes its thread's mode there).
    pub fn run(
        mut self,
        stop: &(dyn Fn() -> bool + Sync),
        on_class: &(dyn Fn(Class) + Sync),
        work: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Outcome<T>
    where
        T: Clone,
    {
        let outcome = self.compute(stop, on_class, work);
        self.finish(outcome.clone());
        outcome
    }

    fn compute(
        &self,
        stop: &(dyn Fn() -> bool + Sync),
        on_class: &(dyn Fn(Class) + Sync),
        work: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Outcome<T> {
        let ctx = Ctx {
            shared: &self.coord.shared,
            id: self.id,
            base: self.base,
            stop,
            on_class,
            thread: thread::current().id(),
            lease: Mutex::new(None),
            mode: Mutex::new(self.base),
        };
        let computed = catch(|| ctx.admit().then(|| work(&ctx)));
        // Its permit is given back before the outcome is published.
        drop(ctx);
        let cancelled = self.coord.shared.lock().observe(self.id);
        match computed {
            Err(panic) => Outcome::Panicked(panic),
            Ok(None) => Outcome::Cancelled,
            Ok(Some(_)) if cancelled => Outcome::Cancelled,
            Ok(Some(value)) => Outcome::Done(value),
        }
    }

    fn finish(&mut self, outcome: Outcome<T>) {
        self.done = true;
        let id = self.id;
        // Out of the registry first: a later begin of the key starts anew.
        self.coord.shared.update(|state| state.finish(id));
        if let Some(slot) = &self.slot {
            *slot.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        }
        self.coord.shared.changed.notify_all();
    }
}

impl<T> Drop for Runner<T> {
    fn drop(&mut self) {
        if !self.done {
            self.finish(Outcome::Cancelled);
        }
    }
}

/// What a running job sees: its cancel, its checkpoints and its permit.
/// Only its own thread changes its permit or calls its hook.
pub struct Ctx<'a> {
    shared: &'a Arc<Shared>,
    id: u64,
    base: Class,
    stop: &'a (dyn Fn() -> bool + Sync),
    on_class: &'a (dyn Fn(Class) + Sync),
    thread: ThreadId,
    lease: Mutex<Option<Lease>>,
    /// The class the thread's mode was last set for.
    mode: Mutex<Class>,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("id", &self.id)
            .field("class", &self.class())
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    fn lease(&self) -> MutexGuard<'_, Option<Lease>> {
        self.lease.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn owner(&self) -> bool {
        thread::current().id() == self.thread
    }

    /// Whether the job is cancelled (latched once seen), or its owner's
    /// `stop` says to stop.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.shared.lock().observe(self.id) || (self.stop)()
    }

    /// The class of its permit; none while it waits without one.
    #[must_use]
    pub fn class(&self) -> Option<Class> {
        self.lease().as_ref().map(Lease::class)
    }

    /// A checkpoint. On the job's own thread it first moves the permit to
    /// the class the job should run in (moved, or given back and a permit
    /// of that class waited for), calling the hook on a change. Whether
    /// the job should stop.
    pub fn checkpoint(&self) -> bool {
        if self.owner() && !self.retarget() {
            return true;
        }
        self.cancelled()
    }

    /// Runs `wait` without its permit and takes one again after, of the
    /// class the job should run in then: none when the job is cancelled
    /// or stopped meanwhile. On another thread, `wait` runs with the
    /// permit kept.
    pub fn idle<R>(&self, wait: impl FnOnce() -> R) -> Option<R> {
        if !self.owner() {
            return Some(wait());
        }
        *self.lease() = None;
        let out = wait();
        self.admit().then_some(out)
    }

    fn want(&self) -> Class {
        self.shared
            .lock()
            .jobs
            .get(&self.id)
            .map_or(self.base, Job::want)
    }

    /// Moves the permit to the class wanted now; `false` when the wait for
    /// one was stopped.
    fn retarget(&self) -> bool {
        let want = self.want();
        let mut lease = self.lease();
        let Some(held) = lease.as_mut() else {
            drop(lease);
            return self.admit();
        };
        if held.class() == want {
            return true;
        }
        if held.move_to(want) {
            drop(lease);
            self.apply(want);
            return true;
        }
        *lease = None;
        drop(lease);
        self.admit()
    }

    /// Waits, holding no permit, for one of the class wanted, read again
    /// at each wake (a promotion or a demotion meanwhile); `false` once
    /// the job is cancelled or stopped, or when that class has no permit.
    fn admit(&self) -> bool {
        loop {
            if self.cancelled() {
                return false;
            }
            let mut state = self.shared.lock();
            let want = state.jobs.get(&self.id).map_or(self.base, Job::want);
            if self.shared.caps.of(want) == Some(0) {
                return false;
            }
            if state.take_permit(self.shared.caps, want) {
                drop(state);
                *self.lease() = Some(Lease {
                    shared: Arc::clone(self.shared),
                    class: want,
                });
                self.apply(want);
                return true;
            }
            drop(
                self.shared
                    .changed
                    .wait_timeout(state, PERMIT_SLICE)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }

    /// Calls the hook when `class` differs from the class the thread's
    /// mode was last set for.
    fn apply(&self, class: Class) {
        let changed = {
            let mut mode = self.mode.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *mode != class;
            *mode = class;
            changed
        };
        if changed {
            (self.on_class)(class);
        }
    }
}

#[cfg(test)]
mod tests;
