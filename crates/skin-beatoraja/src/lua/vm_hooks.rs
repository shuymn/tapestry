//! Hooks on the sandbox's VMs for the callers' tests (dev-tools): how
//! many VMs are alive in the process, and a barrier that holds each VM's
//! destruction until released, so a test can observe a pass midway
//! between its cancellation and the end of its VM.
//!
//! Every VM the sandbox creates carries a [`Live`] in its app data: it
//! counts one while the VM lives, and its drop, run as the VM is
//! destroyed, waits while the barrier is held before it counts the VM
//! gone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

/// The VMs alive.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Whether drops are held, and how many wait at the barrier.
static BARRIER: Mutex<Barrier> = Mutex::new(Barrier {
    held: false,
    waiting: 0,
});
static RELEASED: Condvar = Condvar::new();

struct Barrier {
    held: bool,
    waiting: usize,
}

/// The VMs alive in the process now.
#[must_use]
pub fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
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

/// One VM's presence, in its app data.
pub(super) struct Live;

impl Live {
    pub(super) fn new() -> Self {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let mut b = BARRIER.lock().unwrap_or_else(PoisonError::into_inner);
        b.waiting += 1;
        while b.held {
            b = RELEASED.wait(b).unwrap_or_else(PoisonError::into_inner);
        }
        b.waiting -= 1;
        drop(b);
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}
