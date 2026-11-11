//! WARP's turn among the tests that share one process (design.md "Testing
//! Strategy"): a test that draws on WARP alongside others in its process, as
//! the release allocation guards do, takes the turn first and holds it until
//! its renderer is gone, so one WARP device at a time is alive there.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// The turn, held for its drop: take it before anything the test makes
/// on WARP, and before `crate::imports::tests::serial` when the test
/// takes both, so the two are always taken in that order.
pub(crate) struct Turn(#[expect(dead_code, reason = "held for its drop")] MutexGuard<'static, ()>);

/// Waits for WARP's turn; a test that failed holding it passes it on.
pub(crate) fn take() -> Turn {
    static TURN: Mutex<()> = Mutex::new(());
    Turn(TURN.lock().unwrap_or_else(PoisonError::into_inner))
}
