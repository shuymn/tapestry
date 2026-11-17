//! One allocation refused by the development build's allocator (tests): the
//! next allocation of a chosen size, on any thread, returns null as an
//! allocator out of memory does, so code that allocates fallibly can be
//! checked against a refusal it does not choose itself. Only an
//! allocation the code under test makes fallibly may be chosen: an
//! infallible one refused aborts the test process.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The size of the allocation to refuse; 0: none.
static SIZE: AtomicUsize = AtomicUsize::new(0);
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Refuses the next allocation of exactly `size` bytes, on any thread.
pub fn arm(size: usize) {
    REFUSED.store(false, Ordering::SeqCst);
    SIZE.store(size, Ordering::SeqCst);
}

/// Refuses nothing more; whether an allocation was refused since the last
/// [`arm`] or `disarm`.
pub fn disarm() -> bool {
    SIZE.store(0, Ordering::SeqCst);
    REFUSED.swap(false, Ordering::SeqCst)
}

/// Whether the allocator refuses an allocation of `size` bytes: the armed
/// size, once.
pub fn refuses(size: usize) -> bool {
    if size == 0
        || SIZE
            .compare_exchange(size, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
    {
        return false;
    }
    REFUSED.store(true, Ordering::SeqCst);
    true
}
