//! The logic step: read every source's watermark, pop what it guarantees,
//! merge through the global watermark, and advance the evaluator there.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use bms_model::{Chart, TimeUs};
use eval::{
    DeviceProfile, EvalError, EvalInput, Evaluator, GaugeState, InputStart, JudgeEvent,
    PhysicalInputEvent, Published, SourceId, SourceState, Staged, merge, watermark,
};
use input::SourceReader;
use rules::RulePreset;

use crate::render::{PlayView, RenderSide, RenderSink, ViewWriter, render_link};

/// Receives every judgment event the logic step emits, in order.
pub trait EventSink {
    /// One event.
    fn receive(&mut self, ev: &JudgeEvent);
}

/// The logic thread's end of a source queue: [`input::SourceReader`] in
/// play, a double in tests.
pub trait SourceRead {
    /// What the source last published, read with `Acquire`.
    fn published(&self) -> Published;
    /// The next queued event in receive order.
    fn pop(&mut self) -> Option<PhysicalInputEvent>;
}

impl SourceRead for SourceReader {
    fn published(&self) -> Published {
        Self::published(self)
    }

    fn pop(&mut self) -> Option<PhysicalInputEvent> {
        Self::pop(self)
    }
}

/// Fixed sizes of a [`Logic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicConfig {
    /// The control events of the whole play: the capacity of the recording
    /// window (design.md "Recording Capacity"). The input budget is this plus
    /// one disconnect per source. Every popped input spends one unit, whatever
    /// its source; the budget sizes the record, the staging, the merge, the
    /// evaluator's input ring, and through [`Evaluator::max_events`] the
    /// output.
    pub inputs: usize,
    /// The effects the render queue holds.
    pub effects: usize,
}

/// Why [`Logic::new`] refused to start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LogicError {
    /// The evaluator refused the chart, preset or device profile.
    #[error("evaluation cannot start")]
    Eval(#[source] EvalError),
    /// More readers than [`SourceId`]s.
    #[error("{0} sources exceed the source id range")]
    TooManySources(usize),
    /// A runner start of `i64::MIN` leaves no time before it for the
    /// bootstrap, so the opening could not separate it from the play.
    #[error("the play cannot start at the earliest time")]
    StartAtMinimum,
    /// The play's recording window needs more inputs than the recording policy
    /// reserves (design.md "Recording Capacity").
    #[error(
        "the play is too long to record with these controls: it needs room for {needed} inputs, and {allowed} fit"
    )]
    RecordingTooLarge {
        /// The inputs the window needs.
        needed: usize,
        /// The most the policy allows for this chart and these sources.
        allowed: usize,
    },
}

/// Why a play was aborted. It ends at the watermark already finalized
/// (design.md "Play Lifecycle").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortCause {
    /// The window lost focus.
    FocusLoss,
    /// No participating source is left open.
    LastSourceRemoved,
    /// An input did not fit the play's input budget, or the evaluator
    /// refused it: the record is incomplete (overflow).
    Overflow,
    /// The player pressed retry.
    Retry,
    /// The player pressed quit.
    Quit,
    /// The system suspended: the clock jumped, so the record is incomplete
    /// (suspend).
    Suspend,
    /// The audio output failed.
    OutputFailed,
    /// A survival gauge failed and the session policy ends the play.
    GaugeFailed,
    /// The screen could not be drawn.
    ScreenFailed,
    /// The input thread failed: the record is incomplete (input failed).
    InputFailed,
}

/// What one logic step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    /// The global watermark of this step: everything at or before it is
    /// final. `None` when the step did not advance.
    pub watermark: Option<TimeUs>,
    /// Events emitted in this step.
    pub emitted: usize,
    /// Whether evaluation is complete.
    pub complete: bool,
    /// Why the play was aborted, in this step or before.
    pub aborted: Option<AbortCause>,
    /// `now − watermark`: how long finalization trails the logic time.
    pub finalization_delay: Option<TimeUs>,
    /// `now − origin` of the oldest input judged in this step: input to
    /// judgment.
    pub judgment_latency: Option<TimeUs>,
}

/// The recorded play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Every event popped, in pop order (each source's in its receive
    /// order), within the input budget.
    pub events: Vec<PhysicalInputEvent>,
    /// How each source is evaluated.
    pub device: DeviceProfile,
    /// The controls held when recording opened.
    pub start: InputStart,
    /// The watermark the evaluator last finalized; `None` if it never
    /// advanced.
    pub finalized: Option<TimeUs>,
}

impl Record {
    /// The record as batch evaluation takes it, stopping at the finalized
    /// watermark; `None` if nothing was ever finalized, so there is nothing
    /// to re-evaluate.
    #[must_use]
    pub fn eval_input(&self) -> Option<EvalInput> {
        self.finalized.map(|finalized| EvalInput {
            events: self.events.clone(),
            device: self.device.clone(),
            start: self.start.clone(),
            finalized: Some(finalized),
            section: None,
        })
    }
}

/// One source as the logic step sees it.
#[derive(Debug)]
struct Source<R> {
    reader: R,
    /// The receive time of the last event popped.
    last_popped: Option<TimeUs>,
    /// Whether the last pop found the queue empty.
    drained: bool,
}

/// The logic step. It owns the evaluator, the source readers, their
/// staging buffers, the collected record, the event sinks and the play
/// view's writer. After construction, [`advance_to`](Self::advance_to)
/// allocates nothing: every popped input spends the play's input budget,
/// so staging, merge, record and output stay within what was reserved.
pub struct Logic<'a, R = SourceReader> {
    evaluator: Evaluator<'a>,
    device: &'a DeviceProfile,
    start: InputStart,
    sources: Vec<Source<R>>,
    staged: Vec<Staged>,
    /// The input budget; the record's length is what has been spent.
    inputs: usize,
    /// The output bound for the whole play.
    max_events: usize,
    /// Events emitted so far.
    emitted: u64,
    /// Each source's last publication, by source id; `Retired` sticks once
    /// reached.
    published: Vec<Published>,
    merged: Vec<PhysicalInputEvent>,
    out: Vec<JudgeEvent>,
    record: Vec<PhysicalInputEvent>,
    render: RenderSink,
    sinks: Vec<Box<dyn EventSink + Send + 'a>>,
    view: ViewWriter,
    finalized: Option<TimeUs>,
    overflowed: bool,
    aborted: Option<AbortCause>,
}

impl<R> fmt::Debug for Logic<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logic")
            .field("sources", &self.sources.len())
            .field("finalized", &self.finalized)
            .field("recorded", &self.record.len())
            .field("overflowed", &self.overflowed)
            .field("aborted", &self.aborted)
            .finish_non_exhaustive()
    }
}

impl<'a, R: SourceRead> Logic<'a, R> {
    /// Prepares the step for one play. `readers` are indexed by
    /// [`SourceId`]; `sinks` receive every event after the render sink.
    /// Returns the render thread's end of the effect queue and play view.
    ///
    /// # Errors
    ///
    /// What [`Evaluator::new`] refuses, or more readers than source ids.
    pub fn new(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        start: &InputStart,
        readers: Vec<R>,
        config: LogicConfig,
        sinks: Vec<Box<dyn EventSink + Send + 'a>>,
    ) -> Result<(Self, RenderSide), LogicError> {
        let n = readers.len();
        if n > usize::from(u16::MAX) + 1 {
            return Err(LogicError::TooManySources(n));
        }
        // The control events of the window, plus one `Disconnected` per
        // participant, which the rates of design.md "Recording Capacity" do not
        // count.
        let budget = config.inputs.saturating_add(n);
        let evaluator =
            Evaluator::new(chart, rule, device, start, None, budget).map_err(LogicError::Eval)?;
        // Each buffer holds every control event of the window and its own
        // source's disconnect; a source beyond that exceeds the window, and
        // the push refused overflows like the budget.
        let per_source = config.inputs.saturating_add(1);
        let staged = (0..n)
            .map(|i| {
                let id = SourceId(u16::try_from(i).unwrap_or(u16::MAX));
                Staged::with_capacity(id, per_source)
            })
            .collect();
        let sources = readers
            .into_iter()
            .map(|reader| Source {
                reader,
                last_popped: None,
                drained: false,
            })
            .collect();
        let initial = PlayView {
            finalized: None,
            tally: *evaluator.tally(),
            gauges: evaluator.gauges().to_vec(),
            complete: false,
            events: 0,
        };
        let (render, view, side) = render_link(config.effects, &initial);
        let max_events = evaluator.max_events(budget);
        let logic = Self {
            evaluator,
            device,
            start: start.clone(),
            sources,
            staged,
            inputs: budget,
            max_events,
            emitted: 0,
            published: vec![Published::open(TimeUs(i64::MIN)); n],
            merged: Vec::with_capacity(budget),
            // Sized for the whole play's inputs, not one step's (it is
            // cleared every step and never shrinks).
            out: Vec::with_capacity(max_events),
            record: Vec::with_capacity(budget),
            render,
            sinks,
            view,
            finalized: None,
            overflowed: false,
            aborted: None,
        };
        Ok((logic, side))
    }

    /// One step at logic time `now`, in the order of design.md "Watermark": (1)
    /// read every source's watermark; (2) pop each source's events up to it
    /// into its staging, stopping after the first event beyond it; (3) take the
    /// global watermark `W`; (4) merge the staged events at or before `W` into
    /// the evaluator, leaving later ones staged; (5) advance to `W` and hand
    /// the events to the sinks. Reading the watermarks after popping would let
    /// an event pushed in between be skipped while its miss is finalized.
    ///
    /// `W` depends only on the publications, so it is taken right after
    /// (1). The play is aborted instead of advancing when no source is
    /// left open, or when an input exceeds the budget or is refused.
    /// After an abort nothing advances.
    pub fn advance_to(&mut self, now: TimeUs) -> Advance {
        self.out.clear();
        if self.aborted.is_some() {
            return self.idle();
        }
        // (1) Every watermark first.
        let mut open = false;
        for (s, p) in self.sources.iter().zip(&mut self.published) {
            if p.state != SourceState::Retired {
                *p = s.reader.published();
            }
            open |= p.state == SourceState::Open;
        }
        // (3), taken here since it depends only on the publications: the
        // global watermark. With no source left open the play is over: it
        // ends at what was already finalized, before a closed source's
        // final watermark could finalize more.
        let Some(w) = watermark(&self.published, self.device).filter(|_| open) else {
            self.abort(AbortCause::LastSourceRemoved);
            return self.idle();
        };
        // (2) Then each source's guaranteed prefix, within the budget.
        for ((s, p), staged) in self
            .sources
            .iter_mut()
            .zip(&self.published)
            .zip(&mut self.staged)
        {
            let up_to = p.received_up_to;
            if p.state == SourceState::Retired {
                continue;
            }
            s.drained = false;
            // An event beyond `up_to` ends the pops: the rest of the queue
            // is beyond it too.
            while s.last_popped.is_none_or(|t| t <= up_to) {
                let Some(ev) = s.reader.pop() else {
                    s.drained = true;
                    break;
                };
                s.last_popped = Some(ev.received);
                if self.record.len() >= self.inputs {
                    // Over budget: the event is lost and the play ends.
                    self.overflowed = true;
                    break;
                }
                self.record.push(ev);
                if staged.push(ev).is_err() {
                    // Another source's event, which cannot be ordered here, or
                    // more than this source's share of the window.
                    self.overflowed = true;
                    break;
                }
            }
            if self.overflowed {
                break;
            }
        }
        if self.overflowed {
            self.abort(AbortCause::Overflow);
            return self.idle();
        }
        // (4) Merge through it; later events stay staged. Everything staged
        // was popped within the budget, so `merged` never grows.
        merge(&mut self.staged, self.device, w, &mut self.merged);
        let refused = self
            .merged
            .iter()
            .any(|&ev| self.evaluator.push_input(ev).is_err());
        self.merged.clear();
        if refused {
            self.abort(AbortCause::Overflow);
            return self.idle();
        }
        // (5) Advance. The watermark never moves back: each publication
        // only grows, and a source leaves the minimum only by retiring.
        if self.evaluator.advance_to(w, &mut self.out).is_ok() {
            self.finalized = Some(w);
        }
        self.emitted += self.out.len() as u64;
        let mut oldest: Option<TimeUs> = None;
        for ev in &self.out {
            self.render.receive(ev);
            for sink in &mut self.sinks {
                sink.receive(ev);
            }
            if let Some(o) = ev.origin {
                oldest = Some(oldest.map_or(o, |t| t.min(o)));
            }
        }
        for ((s, p), staged) in self
            .sources
            .iter()
            .zip(&mut self.published)
            .zip(&self.staged)
        {
            p.retire_if_drained(s.drained, staged);
        }
        self.publish_view();
        Advance {
            watermark: Some(w),
            emitted: self.out.len(),
            complete: self.evaluator.is_complete(),
            aborted: None,
            finalization_delay: Some(TimeUs(now.0.saturating_sub(w.0))),
            judgment_latency: oldest.map(|o| TimeUs(now.0.saturating_sub(o.0))),
        }
    }

    /// Ends the play at the watermark already finalized, with no further
    /// advance (design.md "Play Lifecycle"). Every event still queued is popped
    /// into the record, while the budget lasts; staged ones are there already.
    /// None of them is evaluated. Only the first cause is kept.
    pub fn abort(&mut self, cause: AbortCause) {
        if self.aborted.is_some() {
            return;
        }
        self.aborted = Some(cause);
        self.overflowed |= cause == AbortCause::Overflow;
        self.drain_queues();
    }

    /// After recording has closed: an aborted play's record takes every event
    /// still queued, unevaluated, so input that arrived between the abort and
    /// the closing is kept (design.md "Play Lifecycle": the abort's suffix
    /// stays in the record). Nothing happens for a play that was not aborted,
    /// whose record ends at its completion.
    pub fn drain_after_close(&mut self) {
        if self.aborted.is_some() {
            self.drain_queues();
        }
    }

    /// Pops every queued event into the record, while the budget lasts.
    fn drain_queues(&mut self) {
        for (s, p) in self.sources.iter_mut().zip(&self.published) {
            if p.state == SourceState::Retired {
                continue;
            }
            while let Some(ev) = s.reader.pop() {
                if self.record.len() < self.inputs {
                    self.record.push(ev);
                } else {
                    self.overflowed = true;
                }
            }
        }
    }

    /// The events emitted by the last step.
    #[must_use]
    pub fn events(&self) -> &[JudgeEvent] {
        &self.out
    }

    /// The most events the whole play can emit: what a collector of the
    /// stream reserves so it never grows.
    #[must_use]
    pub const fn max_events(&self) -> usize {
        self.max_events
    }

    /// Adds a sink after the others, before play: a sink sized from
    /// [`Self::max_events`] can only be made once the step exists.
    pub fn add_sink(&mut self, sink: Box<dyn EventSink + Send + 'a>) {
        self.sinks.push(sink);
    }

    /// The watermark the evaluator last finalized.
    #[must_use]
    pub const fn finalized(&self) -> Option<TimeUs> {
        self.finalized
    }

    /// Whether evaluation is complete.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.evaluator.is_complete()
    }

    /// Why the play was aborted, if it was.
    #[must_use]
    pub const fn aborted(&self) -> Option<AbortCause> {
        self.aborted
    }

    /// Whether an input did not fit the budget or was refused: the record
    /// is incomplete (overflow).
    #[must_use]
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Every gauge, in the preset's order, as of the last step.
    #[must_use]
    pub fn gauges(&self) -> &[GaugeState] {
        self.evaluator.gauges()
    }

    /// The play view as of the last step: what the render side's newest
    /// view holds, read on the logic side (it allocates).
    #[must_use]
    pub fn view(&self) -> PlayView {
        PlayView {
            finalized: self.finalized,
            tally: *self.evaluator.tally(),
            gauges: self.evaluator.gauges().to_vec(),
            complete: self.evaluator.is_complete(),
            events: self.emitted,
        }
    }

    /// The record, taking the step apart: its events move rather than
    /// being copied.
    #[must_use]
    pub fn into_record(self) -> Record {
        Record {
            events: self.record,
            device: self.device.clone(),
            start: self.start,
            finalized: self.finalized,
        }
    }

    /// The play as recorded so far.
    #[must_use]
    pub fn record(&self) -> Record {
        Record {
            events: self.record.clone(),
            device: self.device.clone(),
            start: self.start.clone(),
            finalized: self.finalized,
        }
    }

    fn idle(&self) -> Advance {
        Advance {
            watermark: None,
            emitted: 0,
            complete: self.evaluator.is_complete(),
            aborted: self.aborted,
            finalization_delay: None,
            judgment_latency: None,
        }
    }

    fn publish_view(&mut self) {
        let v = self.view.input_buffer_mut();
        v.finalized = self.finalized;
        v.tally = *self.evaluator.tally();
        // Within the capacity every buffer got from the initial view.
        v.gauges.clear();
        v.gauges.extend_from_slice(self.evaluator.gauges());
        v.complete = self.evaluator.is_complete();
        v.events = self.emitted;
        self.view.publish();
    }
}
