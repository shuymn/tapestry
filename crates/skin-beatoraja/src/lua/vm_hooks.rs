//! Hooks on the sandbox's VMs for the callers' tests (dev-tools): how
//! many VMs are open in the process, how many instruction hook steps they
//! have run, and a barrier that holds each VM's destruction until
//! released, so a test can observe a pass midway between its cancellation
//! and the end of its VM.
//!
//! A pass's VM counts from its creation until it is closed. The barrier
//! waits before the VM is dropped, so while a destruction is held the VM
//! is still open (`lua_close` has not run); the count goes down only once
//! it is closed. Each instruction hook step counts one step, before the
//! hook's own checks, so a test sees Lua executing.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

/// The VMs open.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The instruction hook steps run.
static STEPS: AtomicU64 = AtomicU64::new(0);

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

/// A pass's VM was created.
pub(super) fn opened() {
    LIVE.fetch_add(1, Ordering::SeqCst);
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

/// A pass's VM was dropped, `lua_close` run.
pub(super) fn closed() {
    LIVE.fetch_sub(1, Ordering::SeqCst);
}
