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

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use skin_beatoraja::lua::runtime::{ScreenRuntime, Stopped};
use skin_model::ScreenKind;

/// The kinds that keep a runtime, by their index in [`ScriptHost`].
pub(in crate::screens) const fn index(kind: ScreenKind) -> Option<usize> {
    match kind {
        ScreenKind::Selection => Some(0),
        ScreenKind::Result => Some(1),
        ScreenKind::Play | ScreenKind::Decision => None,
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

/// Every kind's entry, the runtimes retired while the interval was open
/// (destroyed at the next retirement outside it), and the interval's
/// state.
#[derive(Debug, Default)]
pub(in crate::screens) struct ScriptHost {
    pub(in crate::screens) entries: [Entry; 2],
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
}

impl Default for ScriptCell {
    fn default() -> Self {
        Self {
            id: CELLS.fetch_add(1, Ordering::Relaxed),
            host: Mutex::default(),
            orphaned: AtomicBool::default(),
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

    /// Why the runtime it holds stopped, the first time it is asked after
    /// the stop; `None` before, after, and without one.
    pub(in crate::screens) fn stop(&mut self) -> Option<ScriptStop> {
        let Slot::Bound {
            running: Some((rt, _)),
            noticed,
            ..
        } = &mut self.0
        else {
            return None;
        };
        let stopped = rt.stopped().filter(|_| !*noticed)?;
        *noticed = true;
        Some(ScriptStop::of(stopped))
    }

    /// Puts the runtime it holds back in the cell, parked when its
    /// generation is still current, else retired.
    pub(crate) fn park(&mut self) {
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
            Slot::Staged(rt) => Some(rt),
            Slot::Bound { running, .. } => running.as_ref().map(|(rt, _)| rt),
            Slot::Empty => None,
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
