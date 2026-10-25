//! The logic step: read every source's watermark, pop what it guarantees,
//! merge through the global watermark, and advance the evaluator there.

use std::fmt;

use bms_model::{Chart, TimeUs};
use eval::{
    DeviceProfile, EvalError, EvalInput, Evaluator, InputStart, JudgeEvent, PhysicalInputEvent,
    Published, SourceId, SourceState, Staged, merge, watermark,
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
    /// The input budget of the whole play: the capacity of the recording
    /// window (D8). Every popped input spends one unit, whatever its
    /// source; it sizes the record, the staging, the merge, the evaluator's
    /// input ring, and through [`Evaluator::max_events`] the output.
    pub inputs: usize,
    /// The effects the render queue holds.
    pub effects: usize,
}

/// Why [`Logic::new`] refused to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicError {
    /// The evaluator refused the chart, preset or device profile.
    Eval(EvalError),
    /// More readers than [`SourceId`]s.
    TooManySources(usize),
}

impl fmt::Display for LogicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eval(e) => write!(f, "evaluation cannot start: {e}"),
            Self::TooManySources(n) => write!(f, "{n} sources exceed the source id range"),
        }
    }
}

impl std::error::Error for LogicError {}

/// Why a play was aborted. It ends at the watermark already finalized
/// (D13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortCause {
    /// The window lost focus.
    FocusLoss,
    /// No participating source is left open.
    LastSourceRemoved,
    /// An input did not fit the play's input budget, or the evaluator
    /// refused it: the record is incomplete (overflow).
    Overflow,
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
        })
    }
}

/// One source as the logic step sees it.
#[derive(Debug)]
struct Source<R> {
    reader: R,
    /// The last publication read; `Retired` sticks once reached.
    published: Published,
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
    /// Per step: each source's publication.
    bounds: Vec<Published>,
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
        let evaluator =
            Evaluator::new(chart, rule, device, start, config.inputs).map_err(LogicError::Eval)?;
        // Each buffer can hold the whole budget, since one source may spend
        // all of it.
        let staged = (0..n)
            .map(|i| {
                let id = SourceId(u16::try_from(i).unwrap_or(u16::MAX));
                Staged::with_capacity(id, config.inputs)
            })
            .collect();
        let sources = readers
            .into_iter()
            .map(|reader| Source {
                reader,
                published: Published::open(TimeUs(i64::MIN)),
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
        let max_events = evaluator.max_events(config.inputs);
        let logic = Self {
            evaluator,
            device,
            start: start.clone(),
            sources,
            staged,
            inputs: config.inputs,
            max_events,
            emitted: 0,
            bounds: vec![Published::open(TimeUs(i64::MIN)); n],
            merged: Vec::with_capacity(config.inputs),
            // Sized for the whole play's inputs, not one step's (it is
            // cleared every step and never shrinks).
            out: Vec::with_capacity(max_events),
            record: Vec::with_capacity(config.inputs),
            render,
            sinks,
            view,
            finalized: None,
            overflowed: false,
            aborted: None,
        };
        Ok((logic, side))
    }

    /// One step at logic time `now`, in the order of D8: (1) read every
    /// source's watermark; (2) pop each source's events up to it into its
    /// staging, stopping after the first event beyond it; (3) take the
    /// global watermark `W`; (4) merge the staged events at or before `W`
    /// into the evaluator, leaving later ones staged; (5) advance to `W`
    /// and hand the events to the sinks. Reading the watermarks after
    /// popping would let an event pushed in between be skipped while its
    /// miss is finalized.
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
        for (s, bound) in self.sources.iter_mut().zip(&mut self.bounds) {
            if s.published.state != SourceState::Retired {
                s.published = s.reader.published();
            }
            open |= s.published.state == SourceState::Open;
            *bound = s.published;
        }
        // (3), taken here since it depends only on the publications: the
        // global watermark. With no source left open the play is over: it
        // ends at what was already finalized, before a closed source's
        // final watermark could finalize more.
        let Some(w) = watermark(&self.bounds, self.device).filter(|_| open) else {
            self.abort(AbortCause::LastSourceRemoved);
            return self.idle();
        };
        // (2) Then each source's guaranteed prefix, within the budget.
        for (i, s) in self.sources.iter_mut().enumerate() {
            let up_to = s.published.received_up_to;
            if s.published.state == SourceState::Retired {
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
                if self.staged[i].push(ev).is_err() {
                    // Another source's event: it cannot be ordered here.
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
            self.overflowed = true;
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
        for (s, staged) in self.sources.iter_mut().zip(&self.staged) {
            s.published.retire_if_drained(s.drained, staged);
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
    /// advance (D13). Every event still queued is popped into the record,
    /// while the budget lasts; staged ones are there already. None of them
    /// is evaluated. Only the first cause is kept.
    pub fn abort(&mut self, cause: AbortCause) {
        if self.aborted.is_some() {
            return;
        }
        self.aborted = Some(cause);
        for s in &mut self.sources {
            if s.published.state == SourceState::Retired {
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
