//! Development builds: the start stamps of the S10-09i design's §8.1
//! intervals that begin on the game thread (G3's, G5's), read where each
//! interval ends. A stamp is the time since the process's first stamp read
//! or taken, in ns, plus one, so 0 means none; storing and taking one
//! allocates nothing.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// G3's start: the save phase's drain ended and the result's preparation
/// began.
pub static RESULT_BEGAN: AtomicU64 = AtomicU64::new(0);
/// G5's start: a preset step accepted on the result.
pub static STEP_ACCEPTED: AtomicU64 = AtomicU64::new(0);

/// The instant every stamp counts from.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// `at` as a stamp.
#[must_use]
pub fn stamp_of(at: Instant) -> u64 {
    let ns = at.saturating_duration_since(epoch()).as_nanos();
    u64::try_from(ns).unwrap_or(u64::MAX - 1).saturating_add(1)
}

/// Stamps `slot` now.
pub fn stamp(slot: &AtomicU64) {
    slot.store(stamp_of(Instant::now()), Ordering::SeqCst);
}

/// Drops `slot`'s stamp, if any.
pub fn clear(slot: &AtomicU64) {
    slot.store(0, Ordering::SeqCst);
}

/// Takes `slot`'s stamp, leaving none: the µs from it to `at`, `None`
/// when there was none.
#[must_use]
pub fn take_since(slot: &AtomicU64, at: Instant) -> Option<u64> {
    let began = slot.swap(0, Ordering::SeqCst);
    (began != 0).then(|| stamp_of(at).saturating_sub(began) / 1_000)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    /// A stamp is taken once: the interval to a later instant, then none.
    #[test]
    fn a_stamp_is_taken_once() {
        let slot = AtomicU64::new(0);
        assert_eq!(super::take_since(&slot, Instant::now()), None);
        super::stamp(&slot);
        let later = Instant::now() + Duration::from_millis(5);
        let us = super::take_since(&slot, later).unwrap();
        assert!((5_000..1_000_000).contains(&us), "{us}");
        assert_eq!(super::take_since(&slot, later), None);
    }
}
