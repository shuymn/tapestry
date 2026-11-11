//! The rig's thread failures as one state (design.md "Play Lifecycle",
//! thread failures): the input link's handshake failure or its thread
//! gone, the adjustment driver's first failed wait on the input thread,
//! the audio output's latched failure or its thread gone, and a
//! cancellation the audio thread never acknowledged. Every reader asks it
//! here, from the game thread or from a wait's stop on another thread:
//! [`PlayerRig::input_failure`](game::Rig::input_failure) and
//! [`audio_failure`](game::Rig::audio_failure), the page's settles and the
//! decision screen's watcher in a chart's load.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use audio::OutputError;
use game::InputWaitError;

/// The thread failures, shared: cheap to clone, read without a lock.
#[derive(Clone, Debug)]
pub struct Failures {
    input: crate::input_side::InputFailure,
    /// The adjustment driver's latched wait, once installed: read here
    /// also while the driver is lent to a wait.
    driver: Option<game::InputFailureCell>,
    output: game::OutputProbe,
    /// The start gate's unacknowledged cancellation, once installed.
    stalled: Option<Arc<AtomicBool>>,
    /// A test's lost output.
    #[cfg(test)]
    lost: Arc<AtomicBool>,
}

impl Failures {
    /// The failures of `input` and `output`; the driver and the gate come
    /// with [`install`](Self::install).
    pub fn new(
        input: crate::input_side::InputFailure,
        output: game::OutputProbe,
        #[cfg(test)] lost: Arc<AtomicBool>,
    ) -> Self {
        Self {
            input,
            driver: None,
            output,
            stalled: None,
            #[cfg(test)]
            lost,
        }
    }

    /// The adjustment driver's latch and the start gate's stall join it.
    pub fn install(&mut self, driver: game::InputFailureCell, stalled: Arc<AtomicBool>) {
        self.driver = Some(driver);
        self.stalled = Some(stalled);
    }

    /// The input thread's failure: the link's, else the driver's.
    pub fn input(&self) -> Option<InputWaitError> {
        self.input
            .get()
            .or_else(|| self.driver.as_ref().and_then(game::InputFailureCell::get))
    }

    /// The audio output's failure: latched or its thread gone, else a
    /// cancellation it never acknowledged.
    pub fn output(&self) -> Option<OutputError> {
        #[cfg(test)]
        if self.lost.load(Ordering::Acquire) {
            return Some(OutputError::DeviceLost);
        }
        self.output.failure().or_else(|| {
            self.stalled
                .as_ref()
                .is_some_and(|s| s.load(Ordering::Acquire))
                .then_some(OutputError::Stalled)
        })
    }

    /// Whether either thread failed.
    pub fn any(&self) -> bool {
        self.input().is_some() || self.output().is_some()
    }
}
