//! The runtimes of the screens outside play, and who may destroy them
//! (design.md "beatoraja Lua Skins": Ownership, Quiescence acknowledged).
//!
//! - **The cell.** [`ScriptCell`] holds a [`ScriptHost`]: per screen kind
//!   that keeps a runtime, the entry of its installation committed last,
//!   with the installation's key and a generation, incremented at each
//!   commit of the kind. The render thread and `RenderLink` share it
//!   through [`Scripts`], so it outlives the render loop's state: a
//!   render-thread failure destroys none of the parked runtimes.
//! - **Visits.** A screen holds a [`ScriptSlot`]: a visit takes its
//!   installation's parked runtime of the current generation into the
//!   slot, and parking (or dropping the slot) puts it back. A runtime of a
//!   generation no longer current, parked so, is retired instead.
//! - **The play interval.** The game thread opens it at a play's start
//!   ([`Scripts::open_interval`]) and closes it at the play's end,
//!   getting [`IntervalClosed`]; while it is open no visit takes a
//!   runtime, and a retirement only moves the runtime aside to be
//!   destroyed later. Each opening is a new epoch of the cell.
//! - **Destruction.** Only a retirement destroys a VM: the render
//!   thread's, outside the interval, of a runtime superseded or removed;
//!   or [`Scripts::retire`] on the game thread, with a [`Quiet`] token.
//!   Retired is terminal for its generation. When the cell's last handle
//!   goes without that retirement (the game thread unwinding, or the link
//!   dropped unretired), its runtimes are leaked until the process exits,
//!   never dropped there, where the interval might be open.
//! - **One lock.** The interval's state lives in the host, so opening it,
//!   taking a runtime for a visit and every destruction are ordered by the
//!   host's lock, and a VM is destroyed while that lock is held. A token
//!   names its cell and the epoch it was made in; [`Scripts::retire`]
//!   destroys only when the token is this cell's, of the current epoch,
//!   and no interval is open, and otherwise retires the runtimes into the
//!   deferred set, as the render thread does during the interval.
//! - **Result generations.** The result kind keeps up to
//!   [`RESULT_VMS`] runtimes, each a generation ([`ScriptGen`]) unique in
//!   the cell, apart from the result's GPU installations (s7-07-design
//!   §3.7.1): one enters by an import's [`Scripts::offer_result`] and
//!   leaves by a retirement only: the game's eviction before a permit is
//!   taken ([`Scripts::result_permit`]), the run's disable, or the run's
//!   end. A view's slot names its generation; the presented view's slot
//!   takes the runtime at its frame and re-reads the entry at each use.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use skin_beatoraja::lua::runtime::{NoteBook, Permit, Pool, ScreenRuntime, Stopped};
use skin_model::ScreenKind;

/// The kinds that keep a runtime, by their index in [`ScriptHost`].
pub(in crate::screens) const fn index(kind: ScreenKind) -> Option<usize> {
    match kind {
        ScreenKind::Selection => Some(0),
        // The result's runtimes are generations apart from its GPU
        // installations ([`Scripts::offer_result`]).
        ScreenKind::Result | ScreenKind::Play | ScreenKind::Decision => None,
    }
}

/// What a kind's entry holds.
#[derive(Debug, Default)]
pub(in crate::screens) enum Held {
    /// No runtime: none committed, or an installation without one.
    #[default]
    None,
    /// Parked in the cell.
    Parked(Box<ScreenRuntime>),
    /// Taken by a visit into its slot.
    Running,
    /// Retired: terminal for this generation.
    Retired,
}

/// One kind's entry: the installation's key, its generation and its
/// runtime.
#[derive(Debug, Default)]
pub(in crate::screens) struct Entry {
    pub(in crate::screens) key: usize,
    pub(in crate::screens) generation: u64,
    pub(in crate::screens) held: Held,
}

/// The result runtimes the cell keeps at most: its permits.
pub(in crate::screens) const RESULT_VMS: usize = 2;

/// A result generation's entry.
#[derive(Debug)]
pub(in crate::screens) struct ResultEntry {
    pub(in crate::screens) generation: u64,
    pub(in crate::screens) held: Held,
    /// The fixed skin's: never evicted, only disabled or retired at the
    /// run's end.
    pub(in crate::screens) retained: bool,
    /// Its runtime stopped, as its slot last saw it while running.
    pub(in crate::screens) stopped: bool,
}

/// Every kind's entry, the runtimes retired while the interval was open
/// (destroyed at the next retirement outside it), and the interval's
/// state.
#[derive(Debug, Default)]
pub(in crate::screens) struct ScriptHost {
    pub(in crate::screens) entries: [Entry; 1],
    /// The result generations with a runtime, parked or running; a
    /// running one retired stays as `Retired` until its slot gives the
    /// runtime back.
    pub(in crate::screens) results: Vec<ResultEntry>,
    /// The generation of the view presented last and the result lifetime
    /// it presented in: protected from eviction, even while parked, only
    /// while that lifetime is the current one ([`ScriptHost::presented_now`]).
    pub(in crate::screens) presented: Option<(u64, u64)>,
    /// The current result lifetime, 0 between lifetimes.
    pub(in crate::screens) result_lifetime: u64,
    /// The generation the result skin's publication of the current
    /// attempt runs, if any ([`Scripts::set_published`]).
    pub(in crate::screens) published: Option<u64>,
    /// The last generation given: one counter for the cell.
    pub(in crate::screens) generations: u64,
    /// The run's result kind is disabled: no runtime is offered again.
    pub(in crate::screens) results_disabled: bool,
    pub(in crate::screens) doomed: Vec<ScreenRuntime>,
    /// The play intervals open: from a play's start to its end.
    pub(in crate::screens) open: u32,
    /// The intervals opened so far: a token is good only in its epoch.
    pub(in crate::screens) epoch: u64,
}

impl ScriptHost {
    /// Destroys `rt` now, outside the interval, or keeps it to be
    /// destroyed by the next retirement outside it. The caller holds the
    /// lock, so no interval opens during the destruction.
    pub(in crate::screens) fn retire_runtime(&mut self, rt: Box<ScreenRuntime>) {
        if self.open > 0 {
            self.doomed.push(*rt);
        } else {
            drop(rt);
            self.doomed.clear();
        }
    }

    /// The generation presented in the current result lifetime, if any:
    /// decided where it is used, so a mark of an ended lifetime protects
    /// nothing.
    pub(in crate::screens) fn presented_now(&self) -> Option<u64> {
        self.presented
            .filter(|&(_, l)| l != 0 && l == self.result_lifetime)
            .map(|(g, _)| g)
    }

    /// Whether result generation `generation` is protected now: the fixed
    /// skin's, presented in the current result lifetime, or published in
    /// the current attempt. The one predicate the eviction, the reuse and
    /// the commit point all ask, each time (s7-07-design §3.7.1 M6).
    pub(in crate::screens) fn protects(&self, generation: u64) -> bool {
        self.presented_now() == Some(generation)
            || self.published == Some(generation)
            || self
                .results
                .iter()
                .any(|e| e.generation == generation && e.retained)
    }

    /// The entry of result generation `generation`, if it has one.
    pub(in crate::screens) fn result(&mut self, generation: u64) -> Option<&mut ResultEntry> {
        self.results.iter_mut().find(|e| e.generation == generation)
    }

    /// Retires every result entry `doomed` picks among the parked ones,
    /// and marks the running ones it picks retired: their slots give
    /// their runtimes back.
    pub(in crate::screens) fn retire_results(
        &mut self,
        mut doomed: impl FnMut(&ResultEntry) -> bool,
    ) {
        let mut i = 0;
        while let Some(e) = self.results.get_mut(i) {
            if !doomed(e) {
                i += 1;
                continue;
            }
            if matches!(e.held, Held::Running | Held::Retired) {
                e.held = Held::Retired;
                i += 1;
                continue;
            }
            let e = self.results.swap_remove(i);
            if let Held::Parked(rt) = e.held {
                self.retire_runtime(rt);
            }
        }
    }

    /// A runtime of result generation `generation` given back by its
    /// slot: parked while its entry is running, else retired with its
    /// entry.
    pub(in crate::screens) fn give_back(&mut self, generation: u64, rt: Box<ScreenRuntime>) {
        match self.result(generation) {
            Some(e) if matches!(e.held, Held::Running) => e.held = Held::Parked(rt),
            _ => {
                self.results
                    .retain(|e| !(e.generation == generation && matches!(e.held, Held::Retired)));
                self.retire_runtime(rt);
            }
        }
    }
}

/// A result generation: its cell and the cell's generation number, unique
/// in the process together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScriptGen {
    pub(in crate::screens) cell: u64,
    pub(in crate::screens) generation: u64,
}

/// What a result generation is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GenState {
    /// Parked or running; `stopped` when its runtime stopped.
    Live { stopped: bool },
    /// Retired, or never this cell's: a view of it runs nothing and is
    /// not reused.
    Gone,
}

/// How [`Scripts::commit_view`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewCommit {
    /// The view's generation is gone: nothing was committed.
    Gone,
    /// The commit itself failed.
    Refused,
    /// Committed and marked presented.
    Committed,
}

/// A result permit, taken for a job that may keep a runtime: returned when
/// dropped, or with the runtime it is attached to.
#[derive(Debug)]
pub(crate) struct ScriptPermit(pub(in crate::screens) Permit);

/// The notes of one result lifetime, shared by every runtime that runs in
/// it (s7-07-design §3.7.1 M7).
#[derive(Clone, Debug, Default)]
pub(crate) struct ScriptNotes(pub(in crate::screens) NoteBook);

impl ScriptNotes {
    /// The notes kept, and the further ones counted (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn notes(&self) -> (Vec<skin_beatoraja::lua::runtime::RuntimeNote>, u64) {
        self.0.notes()
    }
}

/// The cells made so far: each takes the next number, so a token names
/// its cell.
static CELLS: AtomicU64 = AtomicU64::new(0);

/// The cell the render thread and `RenderLink` share.
#[derive(Debug)]
pub(in crate::screens) struct ScriptCell {
    /// This cell's number, unique in the process.
    pub(in crate::screens) id: u64,
    pub(in crate::screens) host: Mutex<ScriptHost>,
    /// The render thread has ended, however it ended.
    pub(in crate::screens) orphaned: AtomicBool,
    /// The result runtimes' permits.
    pub(in crate::screens) pool: Pool,
}

impl Default for ScriptCell {
    fn default() -> Self {
        Self {
            id: CELLS.fetch_add(1, Ordering::Relaxed),
            // Retirements during an interval wait here: room for every
            // runtime the cell can hold, so none allocates then.
            host: Mutex::new(ScriptHost {
                doomed: Vec::with_capacity(RESULT_VMS + 2),
                results: Vec::with_capacity(RESULT_VMS),
                ..ScriptHost::default()
            }),
            orphaned: AtomicBool::default(),
            pool: Pool::new(RESULT_VMS),
        }
    }
}

impl ScriptCell {
    /// The host, a poisoned lock read through: every write to it is one
    /// assignment, so a panic leaves it whole.
    pub(in crate::screens) fn host(&self) -> MutexGuard<'_, ScriptHost> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ScriptCell {
    /// The final owner never destroys a VM: what no token retired is
    /// leaked until the process exits.
    fn drop(&mut self) {
        let host = std::mem::take(self.host.get_mut().unwrap_or_else(PoisonError::into_inner));
        let _ = ManuallyDrop::new(host);
    }
}

/// The handle of the cell: the render thread's and `RenderLink`'s.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scripts(pub(in crate::screens) Arc<ScriptCell>);

impl Scripts {
    /// An empty cell.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Commits `kind`'s installation keyed `key` (its bundle's address)
    /// with the runtime `staged` carries, if any: a new generation, the
    /// runtime parked in it; the previous generation's runtime is
    /// retired. On the render thread, at the installation's commit.
    pub(crate) fn commit(&self, kind: ScreenKind, key: usize, staged: ScriptSlot) {
        let rt = staged.into_staged();
        let Some(i) = index(kind) else {
            if let Some(rt) = rt {
                self.0.host().retire_runtime(rt);
            }
            return;
        };
        let mut host = self.0.host();
        let Some(entry) = host.entries.get_mut(i) else {
            return;
        };
        let generation = entry.generation.wrapping_add(1);
        let old = std::mem::replace(
            entry,
            Entry {
                key,
                generation,
                held: rt.map_or(Held::None, Held::Parked),
            },
        );
        if let Held::Parked(old) = old.held {
            host.retire_runtime(old);
        }
    }

    /// Retires `kind`'s runtime: its installation was removed or refused.
    /// On the render thread; a runtime a visit holds is retired when it
    /// parks.
    pub(crate) fn retire_kind(&self, kind: ScreenKind) {
        let Some(i) = index(kind) else {
            return;
        };
        let mut host = self.0.host();
        let Some(entry) = host.entries.get_mut(i) else {
            return;
        };
        let held = std::mem::replace(&mut entry.held, Held::Retired);
        if let Held::Parked(rt) = held {
            host.retire_runtime(rt);
        }
    }

    /// Opens a play's interval, after the render thread acknowledged its
    /// runtimes parked: no visit takes one until it closes. A new epoch:
    /// every token made before it no longer destroys.
    pub(crate) fn open_interval(&self) -> PlayInterval {
        let mut host = self.0.host();
        host.open = host.open.saturating_add(1);
        host.epoch = host.epoch.wrapping_add(1);
        PlayInterval {
            cell: Arc::clone(&self.0),
            epoch: host.epoch,
        }
    }

    /// The token that no play's interval is open: before the first play,
    /// and between plays. It destroys only until the next interval opens.
    #[must_use]
    pub(crate) fn inactive(&self) -> Option<Inactive> {
        let host = self.0.host();
        (host.open == 0).then_some(Inactive(Stamp {
            cell: self.0.id,
            epoch: host.epoch,
        }))
    }

    /// Retires every parked runtime on the game thread, terminal for each
    /// entry's generation; a runtime a visit holds is retired when it
    /// parks. They and those kept for later are destroyed here when
    /// `token` is this cell's, of the current epoch, and no interval is
    /// open; otherwise they are kept for the next retirement outside the
    /// interval, or leaked with the cell. Returns whether it destroyed.
    pub(crate) fn retire(&self, token: Quiet) -> bool {
        let stamp = match token {
            Quiet::Closed(IntervalClosed(s)) | Quiet::Inactive(Inactive(s)) => s,
        };
        let mut guard = self.0.host();
        let host = &mut *guard;
        for entry in &mut host.entries {
            if let Held::Parked(rt) = std::mem::replace(&mut entry.held, Held::Retired) {
                host.doomed.push(*rt);
            }
        }
        let mut i = 0;
        while let Some(e) = host.results.get_mut(i) {
            if let Held::Parked(_) = e.held {
                let e = host.results.swap_remove(i);
                if let Held::Parked(rt) = e.held {
                    host.doomed.push(*rt);
                }
            } else {
                e.held = Held::Retired;
                i += 1;
            }
        }
        host.results_disabled = true;
        let quiet = stamp.cell == self.0.id && stamp.epoch == host.epoch && host.open == 0;
        if quiet {
            // Under the lock: no interval opens while they are destroyed.
            host.doomed.clear();
        }
        quiet
    }

    /// Whether the render thread has ended.
    #[must_use]
    pub(crate) fn orphaned(&self) -> bool {
        self.0.orphaned.load(Ordering::Acquire)
    }

    /// The guard the render thread holds: its drop, on the thread's
    /// return or as it unwinds, marks the cell orphaned, with no lock, no
    /// wait and no allocation.
    #[must_use]
    pub(crate) fn orphan_guard(&self) -> OrphanGuard {
        OrphanGuard(Arc::clone(&self.0))
    }

    /// The calls `kind`'s parked runtime made so far, and whether it
    /// stopped; `None` without a parked one (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn parked(&self, kind: ScreenKind) -> Option<(u64, bool)> {
        let host = self.0.host();
        match &host.entries.get(index(kind)?)?.held {
            Held::Parked(rt) => Some((rt.counters().calls, rt.stopped().is_some())),
            _ => None,
        }
    }

    /// Keeps the runtime `staged` carries, if any, as a new result
    /// generation, parked; `retained` for the fixed skin's, which no
    /// eviction retires. The runtime takes a permit if it holds none. On
    /// the game thread, when an import lands. `None`, the runtime retired,
    /// once the run's result kind is disabled or with no permit free.
    pub(crate) fn offer_result(&self, staged: ScriptSlot, retained: bool) -> Option<ScriptGen> {
        let mut rt = staged.into_staged()?;
        let mut host = self.0.host();
        if !host.results_disabled && !rt.holds_permit() {
            match self.0.pool.take() {
                Some(p) => {
                    let _ = rt.attach_permit(p);
                }
                None => {
                    host.retire_runtime(rt);
                    return None;
                }
            }
        }
        if host.results_disabled {
            host.retire_runtime(rt);
            return None;
        }
        host.generations = host.generations.wrapping_add(1);
        let generation = host.generations;
        host.results.push(ResultEntry {
            generation,
            held: Held::Parked(rt),
            retained,
            stopped: false,
        });
        Some(ScriptGen {
            cell: self.0.id,
            generation,
        })
    }

    /// Retires the runtime `staged` carries, if any: a landing no request
    /// keeps. Through the cell, so it waits for an open interval's end.
    pub(crate) fn discard(&self, staged: ScriptSlot) {
        if let Some(rt) = staged.into_staged() {
            self.0.host().retire_runtime(rt);
        }
    }

    /// What result generation `g` is now.
    #[must_use]
    pub(crate) fn result_state(&self, g: ScriptGen) -> GenState {
        if g.cell != self.0.id {
            return GenState::Gone;
        }
        let mut host = self.0.host();
        match host.result(g.generation) {
            Some(ResultEntry {
                held: Held::Parked(rt),
                ..
            }) => GenState::Live {
                stopped: rt.stopped().is_some(),
            },
            Some(ResultEntry {
                held: Held::Running,
                stopped,
                ..
            }) => GenState::Live { stopped: *stopped },
            _ => GenState::Gone,
        }
    }

    /// Whether result generation `g`, if any, can still run: `None` (no
    /// runtime needed) or live.
    #[must_use]
    pub(crate) fn result_live(&self, g: Option<ScriptGen>) -> bool {
        g.is_none_or(|g| matches!(self.result_state(g), GenState::Live { .. }))
    }

    /// A permit for a job that may keep a result runtime, after the
    /// eviction (s7-07-design §4 rule 2): every parked result generation
    /// but the presented one, the fixed skin's and those in `keep` (the
    /// published and the cached layouts', which a view may still present)
    /// is retired first. `None` when both permits stay held. On the game
    /// thread.
    pub(crate) fn result_permit(&self, keep: &[Option<ScriptGen>]) -> Option<ScriptPermit> {
        {
            let mut host = self.0.host();
            let presented = host.presented_now();
            let published = host.published;
            let id = self.0.id;
            host.retire_results(|e| {
                matches!(e.held, Held::Parked(_))
                    && !e.retained
                    && Some(e.generation) != presented
                    && Some(e.generation) != published
                    && !keep
                        .iter()
                        .flatten()
                        .any(|k| k.cell == id && k.generation == e.generation)
            });
        }
        self.0.pool.take().map(ScriptPermit)
    }

    /// The generation the view presented in the current result lifetime
    /// runs, if any (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub(crate) fn presented(&self) -> Option<ScriptGen> {
        self.0.host().presented_now().map(|generation| ScriptGen {
            cell: self.0.id,
            generation,
        })
    }

    /// Result lifetime `lifetime` begins (on the game thread, as the
    /// render link begins it): the presentations of earlier ones protect
    /// nothing from now on.
    pub(crate) fn enter_result(&self, lifetime: u64) {
        self.0.host().result_lifetime = lifetime;
    }

    /// The result skin published a layout running generation `g` (none
    /// for one without a runtime or the built-in result), or a new attempt
    /// began (`None`): the publication protecting a generation now.
    pub(crate) fn set_published(&self, g: Option<ScriptGen>) {
        self.0.host().published = g.filter(|g| g.cell == self.0.id).map(|g| g.generation);
    }

    /// Whether result generation `g` is protected now
    /// ([`ScriptHost::protects`]).
    #[must_use]
    pub(crate) fn protected(&self, g: ScriptGen) -> bool {
        g.cell == self.0.id && self.0.host().protects(g.generation)
    }

    /// The result lifetime ended: no presentation is current.
    pub(crate) fn leave_result(&self) {
        self.0.host().result_lifetime = 0;
    }

    /// The permits free now (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn permits_free(&self) -> usize {
        self.0.pool.free()
    }

    /// The calls result generation `g`'s runtime made, while parked
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn result_calls(&self, g: ScriptGen) -> Option<u64> {
        match &self.0.host().result(g.generation)?.held {
            Held::Parked(rt) => Some(rt.counters().calls),
            _ => None,
        }
    }

    /// Whether result generation `g` is parked in the cell (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn result_parked(&self, g: ScriptGen) -> bool {
        self.0
            .host()
            .result(g.generation)
            .is_some_and(|e| matches!(e.held, Held::Parked(_)))
    }

    /// The result generations with an entry (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn result_entries(&self) -> usize {
        self.0.host().results.len()
    }

    /// The run's result kind is disabled: every result generation retires,
    /// a running one as its slot gives it back, and none is offered again.
    pub(crate) fn disable_result(&self) {
        let mut host = self.0.host();
        host.results_disabled = true;
        host.presented = None;
        host.retire_results(|_| true);
    }

    /// Commits a view of result generation `next` (none for a view with no
    /// runtime) of result lifetime `lifetime` under one hold of the host's lock: its generation checked
    /// live, then `commit` (the GPU commit, which must neither park nor
    /// drop a slot), then the generation marked presented. A generation
    /// gone, or no longer protected, is not committed, so an evicted or
    /// superseded view never presents.
    pub(crate) fn commit_view(
        &self,
        next: Option<ScriptGen>,
        lifetime: u64,
        commit: impl FnOnce() -> bool,
    ) -> ViewCommit {
        let mut host = self.0.host();
        if let Some(g) = next {
            // Live and protected: a view whose generation no publication or
            // presentation wants any more is withdrawn, as the reuse
            // metadata of that generation is dropped (code r3).
            let live = g.cell == self.0.id
                && host
                    .result(g.generation)
                    .is_some_and(|e| matches!(e.held, Held::Parked(_) | Held::Running))
                && host.protects(g.generation);
            if !live {
                return ViewCommit::Gone;
            }
        }
        if !commit() {
            return ViewCommit::Refused;
        }
        host.presented = next.map(|g| (g.generation, lifetime));
        ViewCommit::Committed
    }

    /// A result view's slot for generation `g`; empty for none.
    #[must_use]
    pub(crate) fn result_slot(&self, g: Option<ScriptGen>) -> ScriptSlot {
        match g {
            Some(g) if g.cell == self.0.id => ScriptSlot(Slot::Result {
                cell: Arc::clone(&self.0),
                generation: g.generation,
                running: None,
                noticed: false,
            }),
            _ => ScriptSlot::default(),
        }
    }

    /// An empty slot for `kind`'s screen of the installation keyed `key`.
    #[must_use]
    pub(crate) fn slot(&self, kind: ScreenKind, key: usize) -> ScriptSlot {
        ScriptSlot(Slot::Bound {
            cell: Arc::clone(&self.0),
            kind,
            key,
            running: None,
            noticed: false,
        })
    }
}

/// A play's open interval, held by the game thread until the play's end.
/// Dropped without [`close`](Self::close) (the game thread unwinding), the
/// interval stays open: nothing is destroyed after it.
#[derive(Debug)]
#[must_use]
pub(crate) struct PlayInterval {
    pub(in crate::screens) cell: Arc<ScriptCell>,
    pub(in crate::screens) epoch: u64,
}

impl PlayInterval {
    /// Closes the interval: the play has ended (its audio tail, Closing or
    /// abort).
    pub(crate) fn close(self) -> IntervalClosed {
        let mut host = self.cell.host();
        host.open = host.open.saturating_sub(1);
        IntervalClosed(Stamp {
            cell: self.cell.id,
            epoch: self.epoch,
        })
    }
}

/// Where and when a token was made: its cell and the epoch.
#[derive(Debug)]
pub(in crate::screens) struct Stamp {
    pub(in crate::screens) cell: u64,
    pub(in crate::screens) epoch: u64,
}

/// The token a play's interval closed with.
#[derive(Debug)]
pub(crate) struct IntervalClosed(pub(in crate::screens) Stamp);

/// The token that no play's interval is open.
#[derive(Debug)]
pub(crate) struct Inactive(pub(in crate::screens) Stamp);

/// A token that no VM can be in use by a play.
#[derive(Debug)]
pub(crate) enum Quiet {
    Closed(IntervalClosed),
    Inactive(Inactive),
}

/// Marks the cell orphaned when dropped.
#[derive(Debug)]
pub(crate) struct OrphanGuard(pub(in crate::screens) Arc<ScriptCell>);

impl Drop for OrphanGuard {
    fn drop(&mut self) {
        self.0.orphaned.store(true, Ordering::Release);
    }
}

/// What a [`ScriptSlot`] holds.
#[derive(Debug, Default)]
pub(in crate::screens) enum Slot {
    /// Nothing.
    #[default]
    Empty,
    /// An import's runtime on its way to its installation's commit.
    Staged(Box<ScreenRuntime>),
    /// A screen's slot for its installation, with the runtime it took, if
    /// any, and that runtime's generation.
    Bound {
        cell: Arc<ScriptCell>,
        kind: ScreenKind,
        key: usize,
        running: Option<(Box<ScreenRuntime>, u64)>,
        /// Its runtime's stop was reported.
        noticed: bool,
    },
    /// A result view's slot for its generation, with the runtime it took
    /// when it is the presented view's.
    Result {
        cell: Arc<ScriptCell>,
        generation: u64,
        running: Option<Box<ScreenRuntime>>,
        /// Its runtime's stop was reported.
        noticed: bool,
    },
}

/// Why a screen's runtime stopped, reported once per screen
/// ([`ScriptSlot`]) for its notice on the strip (design.md "beatoraja Lua
/// Skins", failures).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScriptStop {
    /// It ran past its frame budget for frames in a row.
    Budget,
    /// It reached its memory cap.
    Memory,
    /// A call panicked.
    Panic,
}

impl ScriptStop {
    pub(in crate::screens) const fn of(stopped: Stopped) -> Self {
        match stopped {
            Stopped::Budget => Self::Budget,
            Stopped::Memory => Self::Memory,
            Stopped::Panic => Self::Panic,
        }
    }

    /// The strip's notice.
    #[must_use]
    pub(crate) const fn notice(self) -> &'static str {
        match self {
            Self::Budget => "The skin's script stopped: it ran past its time budget",
            Self::Memory => "The skin's script stopped: it ran out of memory",
            Self::Panic => "The skin's script stopped: it failed",
        }
    }
}

/// A screen's hold on a runtime, opaque outside `screens`: an import's
/// runtime travelling to its commit, or a screen's slot.
#[derive(Debug, Default)]
pub(crate) struct ScriptSlot(pub(in crate::screens) Slot);

impl ScriptSlot {
    /// The slot an import's `runtime`, if any, travels to its
    /// installation's commit in.
    #[must_use]
    pub(crate) fn staged(runtime: Option<ScreenRuntime>) -> Self {
        Self(runtime.map_or(Slot::Empty, |rt| Slot::Staged(Box::new(rt))))
    }

    /// [`staged`](Self::staged), the runtime holding `permit`; with no
    /// runtime the permit returns here.
    #[must_use]
    pub(crate) fn staged_with(
        runtime: Option<ScreenRuntime>,
        permit: Option<ScriptPermit>,
    ) -> Self {
        Self::staged(runtime.map(|mut rt| {
            if let Some(ScriptPermit(p)) = permit {
                let _ = rt.attach_permit(p);
            }
            rt
        }))
    }

    /// The runtime of the presented result view's slot this frame, its
    /// notes going to `notes` once taken: the one it holds while its
    /// generation is still running, else its generation's parked one,
    /// taken unless a play's interval is open; `None` without one. A
    /// runtime whose generation was retired is given back to be retired,
    /// and nothing runs.
    pub(in crate::screens) fn result_running(
        &mut self,
        notes: &ScriptNotes,
    ) -> Option<&mut ScreenRuntime> {
        let Slot::Result {
            cell,
            generation,
            running,
            ..
        } = &mut self.0
        else {
            return None;
        };
        let mut host = cell.host();
        let live = match host.result(*generation) {
            Some(e) if matches!(e.held, Held::Running) => {
                e.stopped = running.as_ref().is_some_and(|rt| rt.stopped().is_some());
                true
            }
            _ => false,
        };
        if let Some(rt) = running.take_if(|_| !live) {
            host.give_back(*generation, rt);
        }
        if running.is_none()
            && host.open == 0
            && let Some(e) = host.result(*generation)
            && matches!(e.held, Held::Parked(_))
            && let Held::Parked(rt) = std::mem::replace(&mut e.held, Held::Running)
        {
            rt.use_notes(&notes.0);
            *running = Some(rt);
        }
        drop(host);
        running.as_deref_mut()
    }

    /// Moves the runtime `old` holds into this slot when both are slots of
    /// one result generation: a view switch that keeps its layout, at its
    /// successful commit.
    pub(crate) fn transfer_from(&mut self, old: &mut Self) {
        if let (
            Slot::Result {
                generation: g,
                running,
                noticed,
                ..
            },
            Slot::Result {
                generation: og,
                running: orun,
                noticed: onoticed,
                ..
            },
        ) = (&mut self.0, &mut old.0)
            && g == og
            && running.is_none()
        {
            *running = orun.take();
            *noticed = *onoticed;
        }
    }

    /// Whether it holds a running runtime (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub(crate) fn runs(&self) -> bool {
        matches!(
            self.0,
            Slot::Result {
                running: Some(_),
                ..
            } | Slot::Bound {
                running: Some(_),
                ..
            }
        )
    }

    /// Whether it holds a runtime (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    #[must_use]
    pub(crate) fn holds(&self) -> bool {
        matches!(
            self.0,
            Slot::Staged(_)
                | Slot::Bound {
                    running: Some(_),
                    ..
                }
        )
    }

    pub(in crate::screens) fn into_staged(mut self) -> Option<Box<ScreenRuntime>> {
        match std::mem::take(&mut self.0) {
            Slot::Staged(rt) => Some(rt),
            other => {
                self.0 = other;
                None
            }
        }
    }

    /// The runtime this slot's screen runs this frame: the one it holds,
    /// or its installation's parked one of the current generation, taken
    /// unless a play's interval is open; `None` without one.
    pub(in crate::screens) fn running(&mut self) -> Option<&mut ScreenRuntime> {
        let Slot::Bound {
            cell,
            kind,
            key,
            running,
            ..
        } = &mut self.0
        else {
            return None;
        };
        if running.is_none() {
            let mut host = cell.host();
            if host.open == 0
                && let Some(entry) = index(*kind).and_then(|i| host.entries.get_mut(i))
                && entry.key == *key
                && matches!(entry.held, Held::Parked(_))
                && let Held::Parked(rt) = std::mem::replace(&mut entry.held, Held::Running)
            {
                *running = Some((rt, entry.generation));
            }
        }
        running.as_mut().map(|(rt, _)| &mut **rt)
    }

    /// The generation it runs or would run: a selection slot's once it
    /// holds its runtime, a result slot's always (dev-tools: the frame
    /// probe's samples).
    #[cfg(feature = "dev-tools")]
    pub(in crate::screens) const fn generation(&self) -> Option<u64> {
        match &self.0 {
            Slot::Bound {
                running: Some((_, g)),
                ..
            } => Some(*g),
            Slot::Result { generation, .. } => Some(*generation),
            _ => None,
        }
    }

    /// Why the runtime it holds stopped, the first time it is asked after
    /// the stop; `None` before, after, and without one.
    pub(in crate::screens) fn stop(&mut self) -> Option<ScriptStop> {
        let (rt, noticed) = match &mut self.0 {
            Slot::Bound {
                running: Some((rt, _)),
                noticed,
                ..
            }
            | Slot::Result {
                running: Some(rt),
                noticed,
                ..
            } => (rt, noticed),
            _ => return None,
        };
        let stopped = rt.stopped().filter(|_| !*noticed)?;
        *noticed = true;
        Some(ScriptStop::of(stopped))
    }

    /// Puts the runtime it holds back in the cell, parked when its
    /// generation is still current, else retired.
    pub(crate) fn park(&mut self) {
        if let Slot::Result {
            cell,
            generation,
            running,
            ..
        } = &mut self.0
        {
            if let Some(rt) = running.take() {
                cell.host().give_back(*generation, rt);
            }
            return;
        }
        let Slot::Bound {
            cell,
            kind,
            running,
            ..
        } = &mut self.0
        else {
            return;
        };
        let Some((rt, generation)) = running.take() else {
            return;
        };
        let mut host = cell.host();
        let current = index(*kind)
            .and_then(|i| host.entries.get_mut(i))
            .filter(|e| e.generation == generation && matches!(e.held, Held::Running));
        match current {
            Some(entry) => entry.held = Held::Parked(rt),
            None => host.retire_runtime(rt),
        }
    }
}

#[cfg(all(test, feature = "dev-tools"))]
impl ScriptSlot {
    /// Sets `gone` when the VM of the runtime it holds is destroyed.
    pub(crate) fn on_vm_drop(&self, gone: &Arc<AtomicBool>) {
        let gone = Arc::clone(gone);
        self.on_vm_teardown(move || gone.store(true, Ordering::Release));
    }

    /// Calls `f` from the teardown of the VM of the runtime it holds; it
    /// replaces an earlier [`on_vm_drop`](Self::on_vm_drop).
    pub(crate) fn on_vm_teardown(&self, f: impl FnOnce() + Send + 'static) {
        let rt = match &self.0 {
            Slot::Staged(rt)
            | Slot::Result {
                running: Some(rt), ..
            } => Some(rt),
            Slot::Bound { running, .. } => running.as_ref().map(|(rt, _)| rt),
            Slot::Empty | Slot::Result { .. } => None,
        };
        if let Some(rt) = rt {
            rt.on_vm_drop(f);
        }
    }
}

impl Drop for ScriptSlot {
    /// A screen's slot parks what it holds; a staged runtime never
    /// committed is dropped with it, on the thread that refused it.
    fn drop(&mut self) {
        self.park();
    }
}

#[cfg(all(test, feature = "dev-tools"))]
pub(in crate::screens) mod tests;

#[cfg(all(test, feature = "dev-tools"))]
impl ScriptSlot {
    /// Whether this result slot runs its runtime this frame, a frame run
    /// on it (tests outside `screens`).
    pub(crate) fn runs_result(&mut self, notes: &ScriptNotes) -> bool {
        self.result_running(notes).is_some()
    }
}
