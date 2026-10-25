//! The headless runner: the whole system advanced without threads, in the
//! order production runs it (design.md "Mechanisms for Testing").

use ::audio::{AudioOutput, OutputError, RecordingOutput, SimDevice};
use bms_model::{Chart, TimeSource, TimeUs, VirtualClock};
use eval::{DeviceProfile, JudgeEvent};
use input::{InputAdapter, ScriptedInput, SourceSet};
use replay::Incomplete;
use rules::RulePreset;

use crate::logic::{AbortCause, EventSink, Logic, LogicConfig, LogicError, Record};
use crate::render::{PlayView, RenderSide};
use crate::sound::{AudioQueue, RunnerAudio, Wakes, audio_link};

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Evaluation completed.
    Complete,
    /// The play was aborted. It ends at the watermark already finalized.
    Aborted(AbortCause),
    /// The audio output failed; the run stopped at that step.
    OutputFailed(OutputError),
    /// Evaluation completed, but the output did not render the last key
    /// sounds: [`HANDOFF_WAKES`] buffer events in a row made no progress.
    /// The run stopped there and the rest was cancelled.
    HandoffStalled,
}

/// Buffer events in a row without progress after which the runner stops
/// waiting for the last key sounds. A healthy wake takes queued sounds (up
/// to [`audio::MAX_PENDING_STARTS`]) and renders each in the block it
/// writes, and a start waiting one or two periods ahead is rendered within
/// three writes; an output that takes nothing for four wakes has stalled.
pub const HANDOFF_WAKES: u32 = 4;

/// Runs a scripted play on a [`VirtualClock`]. Each step moves the clock,
/// then feeds the script (which publishes), advances the logic, runs the
/// audio step and drains the render side. Every buffer is reserved at
/// construction, so the steps allocate nothing: the logic emits at most
/// [`Logic::max_events`] events over the whole play, which the stream and
/// effect collectors and the audio command queue reserve.
pub struct Runner<'a, O = RecordingOutput<SimDevice>> {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    logic: Logic<'a>,
    render: RenderSide,
    audio: RunnerAudio<O>,
    sounds: AudioQueue,
    stream: Vec<JudgeEvent>,
    effects: Vec<JudgeEvent>,
}

impl<O> std::fmt::Debug for Runner<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("now", &self.clock.now())
            .field("logic", &self.logic)
            .field("stream", &self.stream.len())
            .field("effects", &self.effects.len())
            .finish_non_exhaustive()
    }
}

impl<'a, O: AudioOutput + Wakes> Runner<'a, O> {
    /// Bootstraps the input side by feeding the script's items before
    /// `start` (device connections and the controls held before the play),
    /// opens recording at `start`, and prepares the logic for the
    /// participating sources. Items at `start` or later are fed by the
    /// steps and belong to the play, as events after the snapshot boundary
    /// do. `device` is indexed by the source ids. Key sounds go to `audio`
    /// through a [`crate::AudioSink`] after the render sink.
    ///
    /// # Errors
    ///
    /// [`LogicError::StartAtMinimum`] for a `start` of `i64::MIN`, or what
    /// [`Logic::new`] refuses.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        mut sources: SourceSet,
        mut script: ScriptedInput,
        config: LogicConfig,
        start: TimeUs,
        audio: RunnerAudio<O>,
    ) -> Result<Self, LogicError> {
        let Some(before) = start.0.checked_sub(1) else {
            return Err(LogicError::StartAtMinimum);
        };
        let clock = VirtualClock::new(start);
        script.poll(TimeUs(before), &mut sources);
        let input_start = sources.open_recording(start);
        let readers = sources.take_readers();
        let sinks: Vec<Box<dyn EventSink + Send + 'a>> = Vec::new();
        let (mut logic, render) =
            Logic::new(chart, rule, device, &input_start, readers, config, sinks)?;
        let capacity = logic.max_events();
        let (sink, sounds) = audio_link(capacity);
        logic.add_sink(Box::new(sink));
        Ok(Self {
            clock,
            sources,
            script,
            logic,
            render,
            audio,
            sounds,
            stream: Vec::with_capacity(capacity),
            effects: Vec::with_capacity(capacity),
        })
    }

    /// Steps the clock by `step` until evaluation completes or the play is
    /// aborted. Audio buffer events that fell between two steps run before
    /// the later step's logic, in time order, so none takes a key sound
    /// judged after it. An abort stops the audio before anything more is
    /// mixed; a focus loss known at the step stops it before the buffer
    /// events still due run, and so does a failed write. Once evaluation
    /// completes, the audio alone keeps running until every key sound has
    /// been rendered, unless [`HANDOFF_WAKES`] buffer events in a row make
    /// no progress.
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
                self.logic.abort(AbortCause::FocusLoss);
            } else if let Err(e) = self.audio.step(now, false, &mut self.sounds) {
                // Buffer events that fell between the previous step and this
                // one came before this step's logic, so they must not see the
                // commands it emits.
                return self.output_failed(e);
            }
            let advance = self.logic.advance_to(now);
            self.stream.extend_from_slice(self.logic.events());
            if let Some(cause) = advance.aborted {
                self.audio.cancel(&mut self.sounds);
                self.render_step();
                return RunOutcome::Aborted(cause);
            }
            if let Err(e) = self.audio.step(now, true, &mut self.sounds) {
                return self.output_failed(e);
            }
            self.render_step();
            if advance.complete {
                return self.finish_handoff();
            }
        }
    }

    fn output_failed(&mut self, e: OutputError) -> RunOutcome {
        self.audio.cancel(&mut self.sounds);
        RunOutcome::OutputFailed(e)
    }

    /// After evaluation completes, keeps the audio running, with input and
    /// evaluation frozen, until no key sound is queued or waiting to start,
    /// so the last one is rendered. (How long the tail plays on is session
    /// policy.)
    fn finish_handoff(&mut self) -> RunOutcome {
        let mut idle = 0;
        loop {
            let queued = self.sounds.len();
            let pending = self.audio.engine().mixer().pending();
            if queued == 0 && pending == 0 {
                return RunOutcome::Complete;
            }
            if idle >= HANDOFF_WAKES {
                self.audio.cancel(&mut self.sounds);
                return RunOutcome::HandoffStalled;
            }
            // One buffer event at a time, so completion is seen at the
            // first wake that reaches it.
            let at = self.audio.next_wake();
            if at > self.clock.now() {
                self.clock.set(at);
            }
            if let Err(e) = self.audio.wake(&mut self.sounds) {
                return self.output_failed(e);
            }
            let progressed =
                self.sounds.len() < queued || self.audio.engine().mixer().pending() < pending;
            idle = if progressed { 0 } else { idle + 1 };
        }
    }

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
    pub fn record(&self) -> Record {
        self.logic.record()
    }

    /// Why the record may be incomplete: an input queue, the input budget
    /// or the evaluator overflowed, or a device beyond the source limit did
    /// not take part.
    #[must_use]
    pub fn incomplete(&self) -> Incomplete {
        Incomplete {
            overflow: self.logic.overflowed() || self.sources.overflowed(),
            source_limit: self.sources.source_limited(),
            suspend: false,
        }
    }

    /// The logic step.
    #[must_use]
    pub const fn logic(&self) -> &Logic<'a> {
        &self.logic
    }

    /// The audio side.
    #[must_use]
    pub const fn audio(&self) -> &RunnerAudio<O> {
        &self.audio
    }

    /// Key sounds dropped because the audio command queue was full.
    #[must_use]
    pub fn dropped_sounds(&self) -> u64 {
        self.sounds.dropped()
    }

    /// Key sounds queued for audio and not yet taken.
    #[must_use]
    pub fn queued_sounds(&self) -> usize {
        self.sounds.len()
    }
}
