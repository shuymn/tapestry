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
//! copy reads the count the driver last drained. Every copy reads the
//! activity record as one snapshot, the input thread's part written at
//! each event before the tap queues it, so before any fence's answer
//! after it. The revalidation's close is provisional on the reducer's
//! side until the comparison decides, and a close accepts the window's
//! focus-loss generation, which the opening asked for then carries
//! ([`PlayerRig::accepted_focus`]).

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
    /// current; `None` without the adjustments. With `provisionally`, what
    /// was observed after the fence waits until
    /// [`decide_reducer`](Self::decide_reducer).
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    fn close_reducer(&mut self, provisionally: bool) -> Result<Option<u64>, InputWaitError> {
        if let Some(e) = self.adjustments_failure() {
            return Err(e);
        }
        match self.adjustments() {
            Some((driver, mut pump)) => {
                if provisionally {
                    driver.close_provisionally(&mut pump)?;
                } else {
                    driver.close_acceptance(&mut pump)?;
                }
                Ok(Some(driver.accepted()))
            }
            None => Ok(None),
        }
    }

    /// Keeps the reducer's provisional close (`keep`) or reverses it at its
    /// fence: what was observed since is then taken closed, or open.
    fn decide_reducer(&mut self, keep: bool) {
        if let Some((driver, mut pump)) = self.adjustments() {
            if keep {
                driver.commit_close(&mut pump);
            } else {
                driver.reverse_close(&mut pump);
            }
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

    fn loading_wait(&mut self, until: Option<TimeUs>) {
        self.loading_park(until);
    }

    fn loading_refusal(&self, lifetime: u64) -> Result<(), crate::render_side::ScreenError> {
        self.render.loading_refused(lifetime).map_or(Ok(()), |why| {
            Err(crate::render_side::ScreenError::LoadingRefused(why))
        })
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
    /// The presentation names a new epoch for the attempt without entering
    /// it ([`game::AdjustDriver::refresh_presentation`]): holds, the
    /// selected cover and the window's scene stay. The loading script
    /// starts here as at [`Rig::begin_presentation`](game::Rig::begin_presentation).
    fn refresh_presentation(
        &mut self,
        loaded: &game::Loaded,
    ) -> Result<Option<game::PreparationRefusal>, InputWaitError> {
        #[cfg(feature = "dev-tools")]
        if std::mem::take(&mut self.loading_script) {
            self.input.loading_script(self.clock.now());
        }
        if let Some(e) = self.adjustments_failure() {
            return Err(e);
        }
        let refreshed = match self.adjustments() {
            Some((driver, mut pump)) => driver.refresh_presentation(loaded, &mut pump),
            None => Ok(None),
        };
        #[cfg(test)]
        self.opening_gates.refresh.pass();
        refreshed
    }

    /// The reducer closes at a fence, provisionally: what the input thread
    /// observed after it waits. Then, under the tuning's step lock, the
    /// window side closes too if nothing moved since `at_o1` and the
    /// tuning is quiet now, accepting `at_o1`'s focus-loss generation for
    /// the opening; the reducer's close is kept and what waited is taken
    /// closed. Otherwise the reducer's close is reversed at its fence and
    /// what waited is taken open.
    fn revalidate_and_close(
        &mut self,
        at_o1: &TuningSnapshot,
        generation: u32,
    ) -> Result<Revalidated, InputWaitError> {
        #[cfg(test)]
        self.opening_gates.revalidate.pass();
        let reducer = self.close_reducer(true)?;
        let at = self.accepted_at_o1.unwrap_or(at_o1.revision.reducer);
        let now = self.clock.now();
        let unchanged = reducer.is_none_or(|n| n == at)
            && self
                .controls
                .tuning()
                .close_unchanged(at_o1, generation, now);
        self.decide_reducer(unchanged);
        if unchanged {
            self.accepted_focus = Some(at_o1.focus_losses);
            return Ok(Revalidated::Closed);
        }
        Ok(Revalidated::Changed)
    }

    fn close_acceptance(&mut self, generation: u32) -> Result<TuningSnapshot, InputWaitError> {
        #[cfg(test)]
        self.opening_gates.closing.pass();
        let reducer = self.close_reducer(false)?;
        let mut snapshot = self.controls.tuning().close(generation);
        snapshot.revision.reducer = reducer.unwrap_or(0);
        self.accepted_focus = Some(snapshot.focus_losses);
        #[cfg(test)]
        self.opening_gates
            .closes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((self.clock.now(), snapshot));
        Ok(snapshot)
    }

    fn reopen_acceptance(&mut self) -> Result<(), InputWaitError> {
        self.reopen_reducer()?;
        self.controls.tuning().reopen();
        self.accepted_focus = None;
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

/// Where the loading opener's steps on the player's rig wait while a test
/// holds them (tests): after the epoch's refresh (O5) acted, before the
/// revalidation (O11) acts and before a close after a deferral or a
/// commit; and where a deferred opening passes.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct OpeningGates {
    pub refresh: crate::render_side::TestGate,
    pub revalidate: crate::render_side::TestGate,
    /// Passed at each opening the input thread answered `Deferred`.
    pub deferred: crate::render_side::TestGate,
    /// Passed before each close after a deferral or a commit, acceptance
    /// still open.
    pub closing: crate::render_side::TestGate,
    /// Each close after a deferral or a commit: the clock's time and the
    /// copy.
    pub closes: std::sync::Mutex<Vec<(TimeUs, TuningSnapshot)>>,
    /// The time the flow last read from the rig's clock
    /// ([`Rig::now`](game::Rig::now)).
    clock_read: std::sync::atomic::AtomicI64,
    /// Each end of loading asked: the time the flow last read before it,
    /// the one its decision to end the loading state was taken at.
    pub ends: std::sync::Mutex<Vec<TimeUs>>,
}

#[cfg(test)]
impl OpeningGates {
    /// The flow read `now` from the rig's clock.
    pub fn read_clock(&self, now: TimeUs) {
        self.clock_read
            .store(now.0, std::sync::atomic::Ordering::SeqCst);
    }

    /// The time the flow last read from the rig's clock; 0 before any.
    #[cfg(feature = "dev-tools")]
    pub fn clock_read(&self) -> TimeUs {
        TimeUs(self.clock_read.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// The flow asks the end of loading.
    pub fn ending(&self) {
        let at = TimeUs(self.clock_read.load(std::sync::atomic::Ordering::SeqCst));
        self.ends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(at);
    }
}

#[cfg(test)]
#[cfg(feature = "dev-tools")]
mod tests;
