//! The headless runner: the whole system advanced without threads, in the
//! order production runs it (design.md "Mechanisms for Testing").

use bms_model::{Chart, TimeSource, TimeUs, VirtualClock};
use eval::{DeviceProfile, EvalInput, JudgeEvent};
use input::{InputAdapter, ScriptedInput, SourceSet};
use rules::RulePreset;

use crate::logic::{EventSink, Logic, LogicConfig, LogicError};
use crate::render::{PlayView, RenderSide};

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Evaluation completed.
    Complete,
    /// The play was aborted: the focus gate tripped, or every source was
    /// retired. It ends at the watermark already finalized.
    Aborted,
}

/// Runs a scripted play on a [`VirtualClock`]. Each step moves the clock,
/// then feeds the script (which publishes), advances the logic, runs the
/// audio step and drains the render side. Every buffer is reserved at
/// construction, so the steps allocate nothing.
pub struct Runner<'a> {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    logic: Logic<'a>,
    render: RenderSide,
    stream: Vec<JudgeEvent>,
    effects: Vec<JudgeEvent>,
}

impl std::fmt::Debug for Runner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("now", &self.clock.now())
            .field("logic", &self.logic)
            .field("stream", &self.stream.len())
            .field("effects", &self.effects.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Runner<'a> {
    /// Feeds the script up to `start`, which connects its devices, opens
    /// recording there, and prepares the logic for the participating
    /// sources. `device` is indexed by their source ids.
    ///
    /// # Errors
    ///
    /// What [`Logic::new`] refuses.
    pub fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        mut sources: SourceSet,
        mut script: ScriptedInput,
        config: LogicConfig,
        start: TimeUs,
    ) -> Result<Self, LogicError> {
        let clock = VirtualClock::new(start);
        script.poll(start, &mut sources);
        let input_start = sources.open_recording(start);
        let readers = sources.take_readers();
        let sinks: Vec<Box<dyn EventSink + Send + 'a>> = Vec::new();
        let (logic, render) =
            Logic::new(chart, rule, device, &input_start, readers, config, sinks)?;
        let capacity = logic.max_events();
        Ok(Self {
            clock,
            sources,
            script,
            logic,
            render,
            stream: Vec::with_capacity(capacity),
            effects: Vec::with_capacity(capacity),
        })
    }

    /// Steps the clock by `step` until evaluation completes or the play is
    /// aborted.
    ///
    /// # Panics
    ///
    /// If `step` is not positive, since the clock would never reach the end.
    pub fn run_to_end(&mut self, step: TimeUs) -> RunOutcome {
        assert!(step.0 > 0, "Runner::run_to_end needs a positive step");
        loop {
            self.clock.advance(step);
            let now = self.clock.now();
            self.script.poll(now, &mut self.sources);
            if self.sources.gate().is_tripped() {
                // The play ends at what was already finalized.
                self.logic.abort();
                self.render_step();
                return RunOutcome::Aborted;
            }
            let advance = self.logic.advance_to(now);
            self.stream.extend_from_slice(self.logic.events());
            Self::audio_step(now);
            self.render_step();
            if advance.complete {
                return RunOutcome::Complete;
            }
            if advance.watermark.is_none() {
                self.logic.abort();
                return RunOutcome::Aborted;
            }
        }
    }

    /// The audio step: a hook that does nothing until the mixer exists.
    const fn audio_step(_now: TimeUs) {}

    /// The render step: drains the effects and reads the newest view.
    fn render_step(&mut self) {
        let effects = &mut self.effects;
        self.render.drain_effects(|ev| effects.push(*ev));
        let _ = self.render.view();
    }

    /// Every event the logic emitted, in order.
    #[must_use]
    pub fn stream(&self) -> &[JudgeEvent] {
        &self.stream
    }

    /// Every effect the render side received, in order.
    #[must_use]
    pub fn effects(&self) -> &[JudgeEvent] {
        &self.effects
    }

    /// Effects dropped because the render queue was full.
    #[must_use]
    pub fn dropped_effects(&self) -> u64 {
        self.render.dropped()
    }

    /// The newest play view.
    pub fn view(&mut self) -> &PlayView {
        self.render.view()
    }

    /// The collected record, for re-evaluation.
    #[must_use]
    pub fn record(&self) -> EvalInput {
        self.logic.record()
    }

    /// Whether the record is incomplete: an input queue or the record
    /// overflowed, or an input could not be evaluated in order.
    #[must_use]
    pub fn incomplete(&self) -> bool {
        self.logic.incomplete() || self.sources.overflowed()
    }

    /// The logic step.
    #[must_use]
    pub const fn logic(&self) -> &Logic<'a> {
        &self.logic
    }
}
