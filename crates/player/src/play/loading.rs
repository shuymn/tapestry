//! The player's rig in the loading state before a play (design.md "Play
//! Lifecycle", the loading state, tuning while loading, the opening):
//! what [`game::load_and_open`] reads of the render thread's loading
//! lifetime and of tuning acceptance, and how acceptance closes and
//! reopens on the window side (the tuning's step lock) and the reducer's
//! (a fence on the input thread).
//!
//! The reducer's count is read after a fence only where it must be
//! current: when the opening takes its starting settings
//! ([`apply_snapshot`](game::LoadingRig::apply_snapshot), which keeps it
//! as [`PlayerRig::accepted_at_o1`]) and at the revalidation. Every other
//! copy reads the count the driver last drained, and the input side's
//! atomics as the input thread published them.

use bms_model::{TimeSource as _, TimeUs};
use game::{InputWaitError, LoadingRig, Revalidated, Rig as _, Settings, TuningSnapshot};

use super::PlayerRig;

impl PlayerRig<'_> {
    /// The reducer's count of accepted adjustments as last drained; 0
    /// without the adjustments.
    fn accepted(&self) -> u64 {
        self.adjust.as_ref().map_or(0, game::AdjustDriver::accepted)
    }

    /// Closes the reducer's acceptance at a fence: its count, then
    /// current; `None` without the adjustments.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn close_reducer(&mut self) -> Result<Option<u64>, InputWaitError> {
        if let Some(e) = self.adjustments_failure() {
            return Err(e);
        }
        match self.adjustments() {
            Some((driver, mut pump)) => {
                driver.close_acceptance(&mut pump)?;
                Ok(Some(driver.accepted()))
            }
            None => Ok(None),
        }
    }

    /// Reopens the reducer's acceptance at a fence.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn reopen_reducer(&mut self) -> Result<(), InputWaitError> {
        if let Some(e) = self.adjustments_failure() {
            return Err(e);
        }
        if let Some((driver, mut pump)) = self.adjustments() {
            driver.reopen_acceptance(&mut pump)?;
        }
        Ok(())
    }
}

impl LoadingRig for PlayerRig<'_> {
    fn loading_lifetime(&self) -> u64 {
        self.render.loading_lifetime()
    }

    fn loading_ack(&self) -> Option<(u64, TimeUs)> {
        self.render.loading_ack()
    }

    fn loadend(&self) -> Option<TimeUs> {
        self.render.loading_loadend()
    }

    /// The tuning's copy under its step lock, with the reducer's count as
    /// last drained: no fence.
    fn tuning_snapshot(&mut self) -> Result<TuningSnapshot, InputWaitError> {
        if let Some(e) = self.input_failure() {
            return Err(e);
        }
        let mut snapshot = self.controls.tuning().snapshot();
        snapshot.revision.reducer = self.accepted();
        Ok(snapshot)
    }

    /// What [`Rig::before_play`](game::Rig::before_play) does, from
    /// `snapshot`'s values and without the tuning's step lock: the input
    /// offset keys were revoked when the result was left. The input thread
    /// is fenced first, so the adjustments' settings and the reducer's
    /// count kept for the revalidation are as of the same instant.
    fn apply_snapshot(&mut self, snapshot: &TuningSnapshot, settings: &mut Settings) {
        debug_assert!(
            !self.controls.tuning().end_screen_shown(),
            "the input offset keys are revoked before a loading state's opening"
        );
        self.result_input.release();
        let t = snapshot
            .values
            .unwrap_or_else(|| self.controls.tuning().get());
        t.apply_to(settings);
        // A failed fence is latched in the driver and ends the run.
        if let Some((driver, mut pump)) = self.adjustments() {
            let _ = driver.fence(&mut pump);
        }
        if let Some(driver) = &self.adjust {
            settings.player.set_options(&driver.options());
        }
        self.accepted_at_o1 = self.adjust.as_ref().map(game::AdjustDriver::accepted);
        self.input_offset_us = t.input_offset_us;
    }

    /// The reducer closes at a fence; then, under the tuning's step lock,
    /// the window side closes too if nothing moved since `at_o1`.
    /// Otherwise the reducer's close is reversed at the same instant.
    fn revalidate_and_close(
        &mut self,
        at_o1: &TuningSnapshot,
        generation: u32,
    ) -> Result<Revalidated, InputWaitError> {
        let reducer = self.close_reducer()?;
        let at = self.accepted_at_o1.unwrap_or(at_o1.revision.reducer);
        let unchanged = reducer.is_none_or(|n| n == at)
            && self.controls.tuning().close_unchanged(at_o1, generation);
        if unchanged {
            return Ok(Revalidated::Closed);
        }
        if reducer.is_some() {
            self.reopen_reducer()?;
        }
        Ok(Revalidated::Changed)
    }

    fn close_acceptance(&mut self, generation: u32) -> Result<TuningSnapshot, InputWaitError> {
        let reducer = self.close_reducer()?;
        let mut snapshot = self.controls.tuning().close(generation);
        snapshot.revision.reducer = reducer.unwrap_or(0);
        Ok(snapshot)
    }

    fn reopen_acceptance(&mut self) -> Result<(), InputWaitError> {
        self.reopen_reducer()?;
        self.controls.tuning().reopen();
        Ok(())
    }

    /// Focus away now, or lost since acceptance last reopened: an opening
    /// deferred for focus is one of these, read before the deferral
    /// reopens acceptance.
    fn focus_deferred(&self) -> bool {
        !self.controls.focused() || self.controls.tuning().focus_lost_since_reopen()
    }
}

impl PlayerRig<'_> {
    /// A loading state begins (design.md "Play Lifecycle", the loading
    /// state): tuning's epoch starts now, every settings key down held from
    /// before it, and the opening's reducer count is forgotten.
    pub(crate) fn begin_loading_epoch(&mut self) {
        self.controls.tuning().enter_loading(self.clock.now());
        self.accepted_at_o1 = None;
    }
}

#[cfg(test)]
#[cfg(feature = "dev-tools")]
mod tests;
