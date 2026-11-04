//! Hooks on the sandbox's VMs for the callers' tests (dev-tools): how
//! many VMs are open in the process, how many instruction hook steps they
//! have run, and a barrier that holds each VM's destruction until
//! released, so a test can observe a pass midway between its cancellation
//! and the end of its VM.
//!
//! A pass's VM counts from its creation until it is closed. The barrier
//! waits before the VM is dropped, so while a destruction is held the VM
//! is still open (`lua_close` has not run); the count goes down once it
//! is closed, or as a panic unwinds the pass. Each instruction hook step counts one step, before the
//! hook's own checks, so a test sees Lua executing.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

/// The VMs open.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The instruction hook steps run.
static STEPS: AtomicU64 = AtomicU64::new(0);
/// The passes whose VM was created.
static PASSES: AtomicU64 = AtomicU64::new(0);

/// Whether destructions are held, and how many wait at the barrier.
static BARRIER: Mutex<Barrier> = Mutex::new(Barrier {
    held: false,
    waiting: 0,
});
static RELEASED: Condvar = Condvar::new();

struct Barrier {
    held: bool,
    waiting: usize,
}

/// The VMs open in the process now.
#[must_use]
pub fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
}

/// The Lua passes that have created their VM in the process so far.
#[must_use]
pub fn passes() -> u64 {
    PASSES.load(Ordering::SeqCst)
}

/// The instruction hook steps every VM has run so far.
#[must_use]
pub fn steps() -> u64 {
    STEPS.load(Ordering::SeqCst)
}

/// Holds every VM's destruction from now on, until [`release`].
pub fn hold() {
    BARRIER.lock().unwrap_or_else(PoisonError::into_inner).held = true;
}

/// Releases the VMs held, and lets later ones go.
pub fn release() {
    BARRIER.lock().unwrap_or_else(PoisonError::into_inner).held = false;
    RELEASED.notify_all();
}

/// The VMs whose destruction waits at the barrier now.
#[must_use]
pub fn waiting() -> usize {
    BARRIER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .waiting
}

#[cfg(test)]
thread_local! {
    /// [`LIVE`]'s share of this thread, for the crate's own tests, which
    /// run beside others opening VMs.
    static THREAD_LIVE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's VMs open (the crate's tests).
#[cfg(test)]
pub(super) fn thread_live() -> usize {
    THREAD_LIVE.with(std::cell::Cell::get)
}

/// A pass's VM was created: it counts until the guard returned is
/// dropped, after the VM on the normal path and on a panic's unwinding
/// alike.
pub(super) fn opened() -> Open {
    LIVE.fetch_add(1, Ordering::SeqCst);
    PASSES.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    THREAD_LIVE.with(|n| n.set(n.get() + 1));
    Open
}

/// A pass's VM counted open; its drop counts it closed.
pub(super) struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
        #[cfg(test)]
        THREAD_LIVE.with(|n| n.set(n.get() - 1));
    }
}

/// One instruction hook step.
pub(super) fn step() {
    STEPS.fetch_add(1, Ordering::SeqCst);
}

/// A pass's VM is about to be dropped: waits while the barrier is held,
/// the VM still open.
pub(super) fn closing() {
    let mut b = BARRIER.lock().unwrap_or_else(PoisonError::into_inner);
    b.waiting += 1;
    while b.held {
        b = RELEASED.wait(b).unwrap_or_else(PoisonError::into_inner);
    }
    b.waiting -= 1;
}
