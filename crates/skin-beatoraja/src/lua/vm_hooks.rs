//! Hooks on the sandbox's VMs for the callers' tests (dev-tools): how
//! many VMs are open in the process, how many passes ran (in the process
//! and on the calling thread), how many instruction hook steps they
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

thread_local! {
    /// [`PASSES`]'s share of this thread, for callers' tests that run a
    /// pass on their own thread beside others.
    static THREAD_PASSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The Lua passes that have created their VM on this thread so far.
#[must_use]
pub fn thread_passes() -> u64 {
    THREAD_PASSES.with(std::cell::Cell::get)
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
    THREAD_PASSES.with(|n| n.set(n.get() + 1));
    #[cfg(test)]
    THREAD_LIVE.with(|n| n.set(n.get() + 1));
    Open
}

/// A pass's VM counted open; its drop counts it closed.
pub(crate) struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
        #[cfg(test)]
        THREAD_LIVE.with(|n| n.set(n.get() - 1));
    }
}

thread_local! {
    /// Whether this thread's next host call panics ([`panic_next_host_call`]).
    static PANIC_HOST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes this thread's next host call panic, as a defect in a host
/// function would: the tests of a runtime's panic stop.
pub fn panic_next_host_call() {
    PANIC_HOST.with(|p| p.set(true));
}

/// A host call starts: panics when [`panic_next_host_call`] asked.
pub(super) fn host_call() {
    assert!(
        !PANIC_HOST.with(|p| p.replace(false)),
        "an injected host panic"
    );
}

thread_local! {
    /// Whether this thread's next runtime read panics holding the host
    /// context's lock ([`panic_holding_context`]).
    static PANIC_HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread's runtime frame time, in place of
    /// [`super::limits::RUNTIME_FRAME_TIME`] ([`set_frame_time`]).
    static FRAME_TIME: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Makes this thread's next runtime `number` read panic while it holds
/// the host context's lock, poisoning it: the tests of a poisoned context.
pub fn panic_holding_context() {
    PANIC_HOLDING.with(|p| p.set(true));
}

/// A runtime read holds the context's lock: panics when
/// [`panic_holding_context`] asked.
pub(super) fn holding_context() {
    assert!(
        !PANIC_HOLDING.with(|p| p.replace(false)),
        "an injected panic holding the context"
    );
}

/// Sets this thread's runtime frame time (`None`: the limit's), so a test
/// tells the time cap from the instruction and host-call caps.
pub fn set_frame_time(t: Option<std::time::Duration>) {
    FRAME_TIME.with(|f| f.set(t));
}

/// This thread's runtime frame time.
pub(super) fn frame_time() -> std::time::Duration {
    FRAME_TIME
        .with(std::cell::Cell::get)
        .unwrap_or(super::limits::RUNTIME_FRAME_TIME)
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
