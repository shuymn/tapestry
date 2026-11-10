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
//!   withdraws it (A→B→A joins the running job). An end the job has seen
//!   (a cancel, its owner's stop, or a refused admission) is terminal: the
//!   job can no longer be joined, a later begin of its key starts a fresh
//!   job, and it ends [`Outcome::Cancelled`], never done with what it had
//!   when it stopped. The admission reads the cancel under the lock that
//!   takes its permit, so a job cancelled before it has one never
//!   computes.
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

use bms_model::Speed;

mod owned;
mod scoped;

pub use owned::{Handed, OwnedJob, Promotion, Settled, Waited, WorkerLease, WorkerLeases};
pub use scoped::{Failure, Joined, LoadFailure, ScopedJob, Spawner};

/// Foreground permits: the chosen chart's decode threads, from a choice to
/// its READY state (design.md "Load Pipeline", system friendliness).
pub const FOREGROUND_PERMITS: usize = 4;
/// The most speculative permits, whatever the setting.
pub const SPECULATIVE_MAX: usize = 2;
/// The speculative permits by default.
pub const SPECULATIVE_DEFAULT: usize = 1;
/// The cores speculation leaves to the rest of the machine.
pub const RESERVED_CORES: usize = 2;
/// The cores a machine needs for speculation to run when the setting
/// leaves it to the machine.
pub const AUTO_MIN_CORES: usize = 4;
/// The physical memory a machine needs for speculation to run when the
/// setting leaves it to the machine: 8 GiB.
pub const AUTO_MIN_MEMORY: u64 = 8 << 30;

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

    /// The caps for `device.toml`'s `speculation` on a machine of `cores`
    /// cores and `memory` bytes of physical memory (`None` when it cannot
    /// be read): on or off as set, and when unset, on only with at least
    /// [`AUTO_MIN_CORES`] cores and [`AUTO_MIN_MEMORY`] (design.md "Load
    /// Pipeline", the setting). On gives [`SPECULATIVE_DEFAULT`] within
    /// [`for_machine`](Self::for_machine)'s bounds.
    #[must_use]
    pub fn from_setting(speculation: Option<bool>, cores: usize, memory: Option<u64>) -> Self {
        let on = speculation.unwrap_or_else(|| {
            cores >= AUTO_MIN_CORES && memory.is_some_and(|m| m >= AUTO_MIN_MEMORY)
        });
        Self::for_machine(if on { SPECULATIVE_DEFAULT } else { 0 }, cores)
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
    /// The chosen chart read, hashed and parsed.
    Ingest,
    /// The chosen chart's sound bank decoded.
    Bank,
    /// The chosen chart's loudness measured, its cache missed.
    Measure,
}

/// What tells jobs apart: two of one key are one job. The speed is
/// groundwork: no two foreground loads overlap yet, and reuse by key needs
/// the other freshness inputs of design.md "Load Pipeline" first.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobKey {
    /// What the job computes.
    pub kind: JobKind,
    /// The chart's file.
    pub path: PathBuf,
    /// The speed it is read or decoded at.
    pub speed: Speed,
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
    panic::catch_unwind(AssertUnwindSafe(work)).map_err(|payload| panic_of(&*payload))
}

/// A caught panic's message.
fn panic_of(payload: &(dyn Any + Send)) -> JobPanic {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned());
    JobPanic(message)
}

/// Who answers for a job's failure as it publishes its outcome: the
/// chosen chart's foreground work, or speculative work. The coordinator
/// records it and decides nothing by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    /// Foreground work: its failure is the run's.
    Foreground,
    /// Speculative work: its failure is recoverable unless a promotion
    /// made a waiter responsible for it.
    Speculative,
}

impl Owner {
    /// The owner a job begun in `class` has until told otherwise.
    const fn of(class: Class) -> Self {
        match class {
            Class::Foreground => Self::Foreground,
            Class::Maintenance | Class::Speculative => Self::Speculative,
        }
    }
}

/// A job's published outcome, with who answered for it as it published:
/// its owner then, and whether a promotion of the live job had made a
/// waiter responsible for it (nothing withdraws that). The callers'
/// rule: a panic is fatal when the owner was [`Owner::Foreground`] or the
/// job `responsible`, otherwise recoverable; a cancellation is no failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed<T> {
    /// How it ended.
    pub outcome: Outcome<T>,
    /// Its owner as it published.
    pub owner: Owner,
    /// A promotion of the live job made a waiter responsible for it.
    pub responsible: bool,
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
    /// Who answers for it as it publishes.
    owner: Owner,
    /// A promotion of it while live made a waiter responsible for it.
    responsible: bool,
    /// An owned job ([`Coordinator::spawn_owned`]): its owner's is its only
    /// subscriber, never joined by a `begin`, so once that subscriber is
    /// stale nothing withdraws its cancel.
    owned: bool,
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
            // An owned job's requested cancel is final: it ends cancelled
            // whatever subscriber turns current afterwards.
            (Cancel::Requested, true) if !job.owned => Cancel::No,
            (cancel, _) => cancel,
        };
    }

    /// Whether job `id` is cancelled; a request is latched as seen
    /// ([`terminate`](Self::terminate)).
    fn observe(&mut self, id: u64) -> bool {
        let cancel = self.jobs.get(&id).map_or(Cancel::No, |job| job.cancel);
        if cancel == Cancel::No {
            return false;
        }
        self.terminate(id);
        true
    }

    /// Job `id` has seen its end (a cancel, its owner's stop, or a refused
    /// admission): terminal, and the job can no longer be joined.
    fn terminate(&mut self, id: u64) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        job.cancel = Cancel::Observed;
        if let Some(key) = job.key.take()
            && self.keys.get(&key) == Some(&id)
        {
            self.keys.remove(&key);
        }
    }

    /// Removes a finished job, returned so that it drops after the lock is
    /// let go.
    fn finish(&mut self, id: u64) -> Option<Job> {
        let job = self.jobs.remove(&id)?;
        if let Some(key) = &job.key
            && self.keys.get(key) == Some(&id)
        {
            self.keys.remove(key);
        }
        Some(job)
    }

    fn set_promoted(&mut self, id: u64, promoted: bool) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.promoted = promoted;
        }
    }
}

/// Where a keyed job's outcome waits for its subscribers: published once,
/// under the state lock, behind an `Arc`, so a reader clones the `Arc`
/// under the slot's mutex and the payload only after letting it go.
struct Slot<T> {
    published: Mutex<Option<Arc<Observed<T>>>>,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            published: Mutex::new(None),
        }
    }

    /// The published record, if any; no payload is cloned under the lock.
    fn record(&self) -> Option<Arc<Observed<T>>> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Shared {
    caps: Caps,
    state: Mutex<State>,
    changed: Condvar,
    /// Owned jobs dropped without being settled (tests).
    #[cfg(test)]
    unsettled: std::sync::atomic::AtomicUsize,
    /// Where tests hold a job's finish.
    #[cfg(test)]
    cuts: Mutex<Cuts>,
}

/// Where a test holds a job: as its admission begins, between its
/// computation and its finish, and inside the finish between the registry
/// removal and the publication, the state lock held (tests).
#[cfg(test)]
#[derive(Default)]
struct Cuts {
    before_finish: Option<Arc<Gate>>,
    in_finish: Option<Arc<Gate>>,
    admission: Option<Arc<Gate>>,
}

/// A point threads stop at until a test opens it (tests).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Gate {
    /// Someone arrived; it is open.
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl Gate {
    /// Arrives, and waits until it is open.
    pub(crate) fn pass(&self) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.0 = true;
        self.changed.notify_all();
        while !s.1 {
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Arrives without waiting: a signal.
    pub(crate) fn arrive(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).0 = true;
        self.changed.notify_all();
    }

    /// Waits until a thread arrived; `false` after `timeout`.
    pub(crate) fn reached(&self, timeout: Duration) -> bool {
        let s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (s, _) = self
            .changed
            .wait_timeout_while(s, timeout, |s| !s.0)
            .unwrap_or_else(PoisonError::into_inner);
        s.0
    }

    /// Lets every thread through, now and later.
    pub(crate) fn open(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.changed.notify_all();
    }
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
                #[cfg(test)]
                unsettled: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(test)]
                cuts: Mutex::new(Cuts::default()),
            }),
        }
    }

    /// Holds each job from now on as its admission begins, before it reads
    /// its cancel (tests).
    #[cfg(test)]
    pub(crate) fn cut_admission(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.shared
            .cuts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admission = Some(Arc::clone(&gate));
        gate
    }

    /// Holds each job's end from now on: before its finish, or inside it
    /// between the registry removal and the publication (tests).
    #[cfg(test)]
    pub(crate) fn cut(&self, in_finish: bool) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        let mut cuts = self
            .shared
            .cuts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_finish {
            cuts.in_finish = Some(Arc::clone(&gate));
        } else {
            cuts.before_finish = Some(Arc::clone(&gate));
        }
        gate
    }

    /// Whether the state lock is free now, found without waiting (tests).
    #[cfg(test)]
    pub(crate) fn state_free(&self) -> bool {
        !matches!(
            self.shared.state.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        )
    }

    /// Owned jobs dropped without being settled (tests).
    #[cfg(test)]
    pub(crate) fn unsettled_drops(&self) -> usize {
        self.shared
            .unsettled
            .load(std::sync::atomic::Ordering::SeqCst)
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
    /// withdraws its cancel. An owned job of `key` is never joined: a job
    /// of its own begins beside it, and takes the key.
    #[must_use]
    pub fn begin<T: Clone + Send + Sync + 'static>(
        &self,
        key: JobKey,
        class: Class,
        subscriber: Subscriber,
    ) -> (Handle<T>, Option<Runner<T>>) {
        let mut state = self.shared.lock();
        let sub = state.next_id();
        let joined = state.keys.get(&key).copied().and_then(|id| {
            let job = state.jobs.get_mut(&id).filter(|job| !job.owned)?;
            let slot = Arc::clone(job.slot.as_ref()?).downcast::<Slot<T>>().ok()?;
            job.subscribers.push((sub, subscriber));
            Some((id, slot))
        });
        let (id, slot, runner) = if let Some((id, slot)) = joined {
            (id, slot, false)
        } else {
            let id = state.next_id();
            let slot = Arc::new(Slot::new());
            let any_slot: Arc<dyn Any + Send + Sync> = Arc::clone(&slot) as _;
            state.jobs.insert(
                id,
                Job {
                    key: Some(key.clone()),
                    subscribers: vec![(sub, subscriber)],
                    cancel: Cancel::No,
                    base: class,
                    promoted: false,
                    owner: Owner::of(class),
                    responsible: false,
                    owned: false,
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
                    owner: Owner::of(class),
                    responsible: false,
                    owned: false,
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
        // The payload is cloned after the slot's lock is let go.
        handle
            .slot
            .record()
            .map_or(Taken::Pending, |r| Taken::Ready(r.outcome.clone()))
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
        let computed = self.compute(stop, on_class, work);
        // The published outcome, cloned after every lock is let go.
        match self.finish(computed) {
            Final::Kept(outcome) => outcome,
            Final::Published(record) => record.outcome.clone(),
        }
    }

    fn compute(
        &self,
        stop: &(dyn Fn() -> bool + Sync),
        on_class: &(dyn Fn(Class) + Sync),
        work: impl FnOnce(&Ctx<'_>) -> T,
    ) -> Computed<T> {
        let ctx = Ctx {
            shared: &self.coord.shared,
            id: self.id,
            base: self.base,
            stop,
            on_class,
            thread: thread::current().id(),
            lease: Mutex::new(None),
            mode: Mutex::new(self.base),
            fault: Mutex::new(None),
        };
        let computed = catch(|| ctx.admit().then(|| work(&ctx)));
        // A hook's panic stays the job's even when its work caught it.
        let fault = ctx
            .fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // Its permit is given back before the outcome is published.
        drop(ctx);
        match (computed, fault) {
            // A panic comes before any cancel.
            (Err(panic), _) | (Ok(_), Some(panic)) => Computed::Panicked(panic),
            (Ok(None), None) => Computed::Stopped,
            (Ok(Some(value)), None) => Computed::Value(value),
        }
    }

    /// Out of the registry and published in one critical section, so an
    /// observer under the lock finds the job live or published, never
    /// neither; a later begin of the key starts anew. The final cancel is
    /// read in the same section, so a job an observer found ending never
    /// publishes a result. Only prepared values move under the lock; the
    /// removed entry and a discarded result drop after it.
    fn finish(&mut self, computed: Computed<T>) -> Final<T> {
        self.done = true;
        let id = self.id;
        #[cfg(test)]
        let cut = |in_finish: bool| {
            let cuts = self
                .coord
                .shared
                .cuts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if in_finish {
                cuts.in_finish.clone()
            } else {
                cuts.before_finish.clone()
            }
        };
        #[cfg(test)]
        if let Some(gate) = cut(false) {
            gate.pass();
        }
        let mut state = self.coord.shared.lock();
        let cancelled = state.observe(id);
        let removed = state.finish(id);
        let (owner, responsible) = removed
            .as_ref()
            .map_or((Owner::of(self.base), false), |j| (j.owner, j.responsible));
        let (outcome, discarded) = match computed {
            Computed::Panicked(panic) => (Outcome::Panicked(panic), None),
            Computed::Stopped => (Outcome::Cancelled, None),
            Computed::Value(value) if cancelled => (Outcome::Cancelled, Some(value)),
            Computed::Value(value) => (Outcome::Done(value), None),
        };
        #[cfg(test)]
        if let Some(gate) = cut(true) {
            gate.pass();
        }
        let left = match &self.slot {
            Some(slot) => {
                let record = Arc::new(Observed {
                    outcome,
                    owner,
                    responsible,
                });
                *slot
                    .published
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&record));
                Final::Published(record)
            }
            None => Final::Kept(outcome),
        };
        drop(state);
        drop(removed);
        drop(discarded);
        self.coord.shared.changed.notify_all();
        left
    }
}

/// What a job's computation came to, before its final cancel is read.
enum Computed<T> {
    /// It, or its hook, panicked: before any cancel.
    Panicked(JobPanic),
    /// Its admission or a checkpoint stopped it.
    Stopped,
    /// Its work returned.
    Value(T),
}

/// The outcome a finish settled on: published in the slot, or kept for
/// a job without one.
enum Final<T> {
    Published(Arc<Observed<T>>),
    Kept(Outcome<T>),
}

impl<T> Drop for Runner<T> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.finish(Computed::Stopped);
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
    /// A hook's panic, kept even when the work caught its unwinding.
    fault: Mutex<Option<JobPanic>>,
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

    /// Whether the job is cancelled, or its owner's `stop` says to stop.
    /// Either, once seen, is terminal: the job can no longer be joined and
    /// it ends [`Outcome::Cancelled`] whatever its work returns.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        if self.shared.lock().observe(self.id) {
            return true;
        }
        if (self.stop)() {
            self.shared.lock().terminate(self.id);
            return true;
        }
        false
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
    /// or stopped meanwhile. On another thread, the permit is not touched:
    /// none when the job is cancelled or stopped, otherwise `wait` runs
    /// with the permit kept.
    pub fn idle<R>(&self, wait: impl FnOnce() -> R) -> Option<R> {
        if !self.owner() {
            return (!self.cancelled()).then(wait);
        }
        *self.lease() = None;
        let out = wait();
        self.admit().then_some(out)
    }

    /// The class the job should run in now: foreground while promoted,
    /// else the class it was begun in.
    #[must_use]
    pub fn wanted(&self) -> Class {
        self.want()
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
    /// at each wake (a promotion or a demotion meanwhile); `false`, and
    /// terminal, once the job is cancelled or stopped, or when that class
    /// has no permit. The cancel is read under the same lock that picks
    /// the class and takes the permit, so a job cancelled before it takes
    /// one never computes.
    fn admit(&self) -> bool {
        #[cfg(test)]
        {
            let gate = self
                .shared
                .cuts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .admission
                .clone();
            if let Some(gate) = gate {
                gate.pass();
            }
        }
        loop {
            if (self.stop)() {
                self.shared.lock().terminate(self.id);
                return false;
            }
            let mut state = self.shared.lock();
            if state.observe(self.id) {
                return false;
            }
            let want = state.jobs.get(&self.id).map_or(self.base, Job::want);
            if self.shared.caps.of(want) == Some(0) {
                state.terminate(self.id);
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
    /// mode was last set for, and records it once the hook returned: a
    /// hook that unwinds leaves the record as it was, and its panic is
    /// kept as the job's fault, which the job ends with even when its work
    /// catches the unwinding.
    fn apply(&self, class: Class) {
        let changed = *self.mode.lock().unwrap_or_else(PoisonError::into_inner) != class;
        if changed {
            if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| (self.on_class)(class))) {
                let mut fault = self.fault.lock().unwrap_or_else(PoisonError::into_inner);
                if fault.is_none() {
                    *fault = Some(panic_of(&*payload));
                }
                drop(fault);
                panic::resume_unwind(payload);
            }
            *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = class;
        }
    }
}

#[cfg(test)]
mod tests;
