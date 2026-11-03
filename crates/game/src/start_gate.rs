//! The start gate in a headless session (design.md "Play Lifecycle", the
//! start gate): the scripted rig's input and audio sides of the
//! generation-checked handshake, and the adjustment controls' driver fed
//! by the session's tap, so F1, F2, START and SELECT reach the reducer
//! and its snapshot through the same driver as the player's.
//!
//! The input side is the session's [`SourceSet`] with its adjustment tap:
//! it opens a candidate only where START and SELECT are verifiably
//! released and cancels the armed candidate as it accepts a hold, marking
//! both in the tap's stream, which the driver turns into the head's
//! phases. The audio side's engine holds the candidate's transport, and
//! the pass whose block reaches the commit point (chart time 0, or a
//! round's lead-in start), by its own wake, commits it by generation
//! before anything of the schedule starts; a commit that finds the
//! candidate cancelled stops its audio there, so its scheduled chart start
//! and restore never run.
//!
//! [`SourceSet`]: input::SourceSet

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use ::audio::AudioEngine;
use bms_model::TimeUs;
use input::{GateState, SourceSet, StartGate};

use crate::driver::{AdjustDriver, InputPump};
use crate::live::InputWaitError;

/// The session's input side as the driver's waits need it: the input
/// thread is the rig's own, so a fence is answered at once.
pub(crate) struct SessionPump<'a> {
    pub(crate) sources: &'a mut SourceSet,
    /// The batch clock now: the script's time.
    pub(crate) now: TimeUs,
    pub(crate) display_offset_us: i64,
}

impl InputPump for SessionPump<'_> {
    fn request_fence(&mut self, n: u64) {
        let at = self.sources.presentation_time(self.now);
        if let Some(tap) = self.sources.adjustment_mut() {
            tap.fence(n, at);
        }
    }

    fn wake(&mut self) {
        let at = self.sources.presentation_time(self.now);
        self.sources.tick_presentation(at);
    }

    fn display_offset_us(&self) -> i64 {
        self.display_offset_us
    }

    fn park(&mut self) -> Result<(), InputWaitError> {
        // Nothing runs while the rig waits: what is not answered now never
        // is.
        Err(InputWaitError::Timeout)
    }
}

/// The scripted rig's gate: the tap's start gate and the candidate whose
/// transport the audio side has still to commit.
#[derive(Debug)]
pub(crate) struct ScriptedGate<'a> {
    gate: StartGate,
    /// Where the transport commits, in chart time.
    commit_at: TimeUs,
    /// The armed candidate's generation, until the audio side commits
    /// or drops it.
    pending: Option<u32>,
    pub(crate) adjust: &'a mut AdjustDriver,
}

impl<'a> ScriptedGate<'a> {
    pub(crate) const fn new(
        gate: StartGate,
        commit_at: TimeUs,
        adjust: &'a mut AdjustDriver,
    ) -> Self {
        Self {
            gate,
            commit_at,
            pending: None,
            adjust,
        }
    }

    /// The gate's state.
    pub(crate) fn state(&self) -> GateState {
        self.gate.state()
    }

    /// Candidate `generation` opened: its transport waits for the commit.
    /// The head follows it once the driver takes the tap's marker.
    pub(crate) const fn opened(&mut self, generation: u32) {
        self.pending = Some(generation);
    }

    /// Candidate `generation` was cancelled: nothing of it is left to
    /// commit.
    pub(crate) fn cancelled(&mut self, generation: u32) {
        if self.pending == Some(generation) {
            self.pending = None;
        }
    }

    /// Cancels candidate `generation` unless its transport committed
    /// first: `true` when it committed.
    pub(crate) fn settle(&self, generation: u32) -> bool {
        let _ = self.gate.cancel();
        self.gate.state()
            == GateState {
                generation,
                phase: input::GatePhase::Committed,
            }
    }

    /// After the engine restarted for the pending candidate: its
    /// transport holds at the commit point
    /// ([`AudioEngine::hold_transport`]).
    pub(crate) fn hold(&self, engine: &mut AudioEngine) {
        if self.pending.is_some() {
            engine.hold_transport(self.commit_at);
        }
    }

    /// The admission the engine pass reaching the commit point asks:
    /// commits the pending candidate by generation, `false` when the
    /// commit finds it cancelled or stale.
    pub(crate) fn admit(&mut self) -> bool {
        self.pending
            .take()
            .is_some_and(|generation| self.gate.commit(generation))
    }

    /// The driver's side, after the input side's batch.
    pub(crate) fn drain(&mut self, input: &mut SessionPump<'_>) {
        self.adjust.pump(input);
    }
}
