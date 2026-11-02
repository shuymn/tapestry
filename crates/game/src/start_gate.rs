//! The start gate in a headless session (design.md "Play Lifecycle", the
//! start gate): the scripted rig's input and audio sides of the
//! generation-checked handshake, and the adjustment controls' reducer fed
//! by the session's tap, so F1, F2, START and SELECT reach the reducer
//! and its snapshot, never the visible-time ladder.
//!
//! The input side is the session's [`SourceSet`] with its adjustment tap:
//! it opens a candidate only where START and SELECT are verifiably
//! released and cancels the armed candidate as it accepts a hold. The
//! audio side commits the candidate's transport by generation before the
//! engine pass whose block reaches the commit point (chart time 0, or a
//! round's lead-in start); a commit that finds the candidate cancelled
//! stops its audio there, so its scheduled chart start and restore never
//! run.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use ::audio::AudioEngine;
use bms_model::TimeUs;
use input::{AdjustmentReceiver, AdjustmentTap, GateState, StartGate, TapError, adjustment_queue};

use crate::adjust_keys::AdjustmentPlan;
use crate::adjustment::{
    Adjustment, Event, Head, NoTempo, Observation, PresentationReader, PresentationWriter,
    presentation_link,
};
use crate::hispeed::PlayOptions;

/// The adjustment controls of a gated session: the tap's receiving end,
/// the plan that turns its observations into the reducer's, the reducer
/// and its publication.
#[derive(Debug)]
pub(crate) struct SessionAdjust {
    receiver: AdjustmentReceiver,
    plan: AdjustmentPlan,
    reducer: Adjustment,
    writer: PresentationWriter,
    /// The latest observation's sequence, for the phases the rig adds.
    seq: u64,
}

impl SessionAdjust {
    /// The tap for `plan`, to install in the session's source set, and
    /// the reducer with `options`, whose snapshots `reader` reads.
    pub(crate) fn new(
        plan: &AdjustmentPlan,
        options: PlayOptions,
        opens_at: TimeUs,
        display_offset_us: i64,
    ) -> Result<(Self, AdjustmentTap, PresentationReader), TapError> {
        let (mut tap, receiver) = adjustment_queue(plan.bindings.clone())?;
        tap.set_active(true);
        let reducer = Adjustment::new(options, plan.roles, opens_at, display_offset_us);
        let (writer, reader) = presentation_link(&reducer.snapshot());
        Ok((
            Self {
                receiver,
                plan: plan.clone(),
                reducer,
                writer,
                seq: 0,
            },
            tap,
            reader,
        ))
    }

    /// Feeds what the tap observed to the reducer and publishes one
    /// snapshot.
    fn drain(&mut self) {
        let Self {
            receiver,
            plan,
            reducer,
            writer,
            seq,
        } = self;
        let observations = std::iter::from_fn(|| receiver.pop()).filter_map(|o| {
            *seq = o.seq;
            plan.observation(&o)
        });
        reducer.drain(observations, &NoTempo, writer);
    }

    /// The lifecycle's phase from receive time `at` on.
    fn phase(&mut self, at: TimeUs, head: Head, generation: u32) {
        let o = Observation {
            time: at,
            seq: self.seq,
            event: Event::Phase { head, generation },
        };
        self.reducer.observe(o, &NoTempo);
    }

    /// The settings with every accepted change.
    pub(crate) const fn options(&self) -> PlayOptions {
        self.reducer.options()
    }

    /// A new attempt ([`Adjustment::begin_attempt`]).
    pub(crate) fn begin_attempt(
        &mut self,
        options: PlayOptions,
        opens_at: TimeUs,
        display_offset_us: i64,
    ) {
        self.reducer
            .begin_attempt(options, opens_at, display_offset_us);
    }
}

/// The scripted rig's gate: the tap's start gate and the candidate whose
/// transport the audio side has still to commit.
#[derive(Debug)]
pub(crate) struct ScriptedGate<'a> {
    gate: StartGate,
    /// Where the transport commits, in chart time.
    commit_at: TimeUs,
    /// Where the attempt opens, in chart time: the frozen head's base.
    opens_at: TimeUs,
    /// The armed candidate's generation and its chart time 0 on the
    /// session clock, until the audio side commits or drops it.
    pending: Option<(u32, TimeUs)>,
    adjust: Option<&'a mut SessionAdjust>,
}

impl<'a> ScriptedGate<'a> {
    pub(crate) const fn new(
        gate: StartGate,
        commit_at: TimeUs,
        opens_at: TimeUs,
        adjust: Option<&'a mut SessionAdjust>,
    ) -> Self {
        Self {
            gate,
            commit_at,
            opens_at,
            pending: None,
            adjust,
        }
    }

    /// The gate's state.
    pub(crate) fn state(&self) -> GateState {
        self.gate.state()
    }

    /// Candidate `generation` opened with chart time 0 at `origin` on the
    /// session clock, at script time `at`, its chart time 0 at `chart0` on
    /// the script's timeline.
    pub(crate) fn opened(&mut self, generation: u32, origin: TimeUs, at: TimeUs, chart0: TimeUs) {
        self.pending = Some((generation, origin));
        if let Some(a) = &mut self.adjust {
            a.phase(
                at,
                Head::Clock {
                    at,
                    chart: TimeUs(at.0.saturating_sub(chart0.0)),
                },
                generation,
            );
        }
    }

    /// Candidate `generation` was cancelled at script time `at`: nothing
    /// of it is left to commit, and the presentation head freezes at the
    /// attempt's `opens_at()` until the rearm.
    pub(crate) fn cancelled(&mut self, generation: u32, at: TimeUs) {
        if self.pending.is_some_and(|(g, _)| g == generation) {
            self.pending = None;
        }
        let opens_at = self.opens_at;
        if let Some(a) = &mut self.adjust {
            a.phase(at, Head::Frozen { opens_at }, generation);
        }
    }

    /// Before an engine pass: commits the pending candidate when the
    /// pass's block reaches its commit point. `false` when the commit
    /// found it cancelled or stale: its audio must stop before the pass.
    pub(crate) fn before_pass(&mut self, engine: &AudioEngine) -> bool {
        let Some((generation, origin)) = self.pending else {
            return true;
        };
        let line = engine.frame_line_at(origin);
        let end = i64::try_from(line.block_start)
            .unwrap_or(i64::MAX)
            .saturating_add(i64::from(line.period));
        if line.frame(self.commit_at) >= end {
            return true;
        }
        self.pending = None;
        self.gate.commit(generation)
    }

    /// The reducer's side, after the input side's batch.
    pub(crate) fn drain(&mut self) {
        if let Some(a) = &mut self.adjust {
            a.drain();
        }
    }
}
