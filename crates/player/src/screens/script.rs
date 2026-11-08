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
//!   runtime, and a retirement on the render thread only moves the runtime
//!   aside to be destroyed later.
//! - **Destruction.** Only a retirement destroys a VM: the render
//!   thread's, outside the interval, of a runtime superseded or removed;
//!   or [`Scripts::retire`] on the game thread, with a [`Quiet`] token.
//!   Retired is terminal for its generation. When the cell's last handle
//!   goes without that retirement (the game thread unwinding, or the link
//!   dropped unretired), its runtimes are leaked until the process exits,
//!   never dropped there, where the interval might be open.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use skin_beatoraja::lua::runtime::ScreenRuntime;
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

/// Every kind's entry, and the runtimes retired while the interval was
/// open, destroyed at the next retirement outside it.
#[derive(Debug, Default)]
pub(in crate::screens) struct ScriptHost {
    pub(in crate::screens) entries: [Entry; 2],
    pub(in crate::screens) doomed: Vec<ScreenRuntime>,
}

/// The cell the render thread and `RenderLink` share.
#[derive(Debug, Default)]
pub(in crate::screens) struct ScriptCell {
    pub(in crate::screens) host: Mutex<ScriptHost>,
    /// A play's interval is open: from its start to its end.
    pub(in crate::screens) open: AtomicBool,
    /// The render thread has ended, however it ended.
    pub(in crate::screens) orphaned: AtomicBool,
}

impl ScriptCell {
    /// The host, a poisoned lock read through: every write to it is one
    /// assignment, so a panic leaves it whole.
    pub(in crate::screens) fn host(&self) -> MutexGuard<'_, ScriptHost> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(in crate::screens) fn interval_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// Destroys `rt` here, on the render thread outside the interval, or
    /// keeps it to be destroyed by the next retirement outside it.
    pub(in crate::screens) fn retire_runtime(&self, host: &mut ScriptHost, rt: Box<ScreenRuntime>) {
        if self.interval_open() {
            host.doomed.push(*rt);
        } else {
            drop(rt);
            host.doomed.clear();
        }
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
                let mut host = self.0.host();
                self.0.retire_runtime(&mut host, rt);
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
            self.0.retire_runtime(&mut host, old);
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
            self.0.retire_runtime(&mut host, rt);
        }
    }

    /// Opens a play's interval, after the render thread acknowledged its
    /// runtimes parked: no visit takes one until it closes.
    pub(crate) fn open_interval(&self) -> PlayInterval {
        self.0.open.store(true, Ordering::Release);
        PlayInterval(Arc::clone(&self.0))
    }

    /// The token that no play's interval is open: before the first play,
    /// and between plays.
    #[must_use]
    pub(crate) fn inactive(&self) -> Option<Inactive> {
        (!self.0.interval_open()).then_some(Inactive(()))
    }

    /// Destroys every parked runtime, and those kept for later, on the
    /// game thread: `token` proves the interval closed or never opened.
    /// Each entry is retired, terminal for its generation; a runtime a
    /// visit holds is retired when it parks.
    pub(crate) fn retire(&self, token: Quiet) {
        let _ = token;
        let mut host = self.0.host();
        let mut retired = std::mem::take(&mut host.doomed);
        for entry in &mut host.entries {
            if let Held::Parked(rt) = std::mem::replace(&mut entry.held, Held::Retired) {
                retired.push(*rt);
            }
        }
        drop(host);
        drop(retired);
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

    /// An empty slot for `kind`'s screen of the installation keyed `key`.
    #[must_use]
    pub(crate) fn slot(&self, kind: ScreenKind, key: usize) -> ScriptSlot {
        ScriptSlot(Slot::Bound {
            cell: Arc::clone(&self.0),
            kind,
            key,
            running: None,
        })
    }
}

/// A play's open interval, held by the game thread until the play's end.
#[derive(Debug)]
#[must_use]
pub(crate) struct PlayInterval(pub(in crate::screens) Arc<ScriptCell>);

impl PlayInterval {
    /// Closes the interval: the play has ended (its audio tail, Closing or
    /// abort).
    pub(crate) fn close(self) -> IntervalClosed {
        self.0.open.store(false, Ordering::Release);
        IntervalClosed(())
    }
}

/// The token a play's interval closed with.
#[derive(Debug)]
pub(crate) struct IntervalClosed(pub(in crate::screens) ());

/// The token that no play's interval is open.
#[derive(Debug)]
pub(crate) struct Inactive(pub(in crate::screens) ());

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
    },
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
        } = &mut self.0
        else {
            return None;
        };
        if running.is_none() && !cell.interval_open() {
            let mut host = cell.host();
            if let Some(entry) = index(*kind).and_then(|i| host.entries.get_mut(i))
                && entry.key == *key
                && matches!(entry.held, Held::Parked(_))
                && let Held::Parked(rt) = std::mem::replace(&mut entry.held, Held::Running)
            {
                *running = Some((rt, entry.generation));
            }
        }
        running.as_mut().map(|(rt, _)| &mut **rt)
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
            None => cell.retire_runtime(&mut host, rt),
        }
    }
}

#[cfg(all(test, feature = "dev-tools"))]
impl ScriptSlot {
    /// Sets `gone` when the VM of the runtime it holds is destroyed.
    pub(crate) fn on_vm_drop(&self, gone: &Arc<AtomicBool>) {
        let gone = Arc::clone(gone);
        let rt = match &self.0 {
            Slot::Staged(rt) => Some(rt),
            Slot::Bound { running, .. } => running.as_ref().map(|(rt, _)| rt),
            Slot::Empty => None,
        };
        if let Some(rt) = rt {
            rt.on_vm_drop(move || gone.store(true, Ordering::Release));
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
