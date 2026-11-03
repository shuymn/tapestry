//! The logic step: read every source's watermark, pop what it guarantees,
//! merge through the global watermark, and advance the evaluator there.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use bms_model::{Chart, Lane, SoundId, TimeUs};
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
    /// A rearmed READY candidate's chart time 0 lies at `origin` on the
    /// rig's clock (design.md "Play Lifecycle", the start gate); the
    /// cancelled candidate's events are discarded. Nothing by default.
    fn rearm(&mut self, origin: TimeUs) {
        let _ = origin;
    }
    /// A press of the audio tail's feedback on `lane` plays `sound`
    /// (design.md "Play Lifecycle", the audio tail): a key sound with no
    /// event behind it, its press received at `origin`. Nothing by
    /// default.
    fn key_sound(&mut self, lane: Lane, sound: SoundId, origin: TimeUs) {
        let _ = (lane, sound, origin);
    }
}

/// The logic thread's end of a source queue: [`input::SourceReader`] in
/// play, a double in tests.
pub trait SourceRead {
    /// What the source last published, read with `Acquire`.
    fn published(&self) -> Published;
    /// The next queued event in receive order.
    fn pop(&mut self) -> Option<PhysicalInputEvent>;
    /// Whether no event is queued.
    fn is_empty(&self) -> bool;
}

impl SourceRead for SourceReader {
    fn published(&self) -> Published {
        Self::published(self)
    }

    fn pop(&mut self) -> Option<PhysicalInputEvent> {
        Self::pop(self)
    }

    fn is_empty(&self) -> bool {
        Self::is_empty(self)
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
    /// reserves (design.md "Recording Capacity"). A full play's refusal
    /// suggests practicing a section, whose window is shorter.
    #[error(
        "the play is too long to record with these controls: it needs room for {needed} inputs, and {allowed} fit{hint}",
        hint = if *section { "" } else { "; practice a section" }
    )]
    RecordingTooLarge {
        /// The inputs the window needs.
        needed: usize,
        /// The most the policy allows for this chart and these sources.
        allowed: usize,
        /// Whether the play was a practice round of a section.
        section: bool,
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
    /// A rearmed practice round's audio restore could not be planned
    /// (design.md "Play Lifecycle", the start gate).
    RestoreFailed,
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

/// One source as the logic step sees it, beside its reader.
#[derive(Clone, Copy, Debug, Default)]
struct Source {
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
    /// The source readers, by source id; lent out between a cancelled
    /// READY candidate and its rearm ([`lend_readers`](Self::lend_readers)).
    readers: Vec<R>,
    sources: Vec<Source>,
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
    /// The READY candidate's rearms so far ([`PlayView::generation`]).
    generation: u64,
    /// The latest rearm's origin ([`PlayView::origin`]).
    origin: Option<TimeUs>,
    /// The play phase the next publication carries ([`PlayView::playing`]).
    playing: bool,
    finalized: Option<TimeUs>,
    overflowed: bool,
    aborted: Option<AbortCause>,
    /// Inputs the audio tail's feedback popped and could not convert
    /// ([`feedback_to`](Self::feedback_to)).
    feedback_dropped: u64,
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
        Self::with_section(chart, rule, device, start, None, readers, config, sinks)
    }

    /// [`new`](Self::new) for a play of `section` of the chart, if given: a
    /// practice round (design.md "Section Evaluation"), evaluated in it.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), and an unusable section.
    #[expect(
        clippy::too_many_arguments,
        reason = "the play's parts, as `new` takes them, and its section"
    )]
    pub fn with_section(
        chart: &'a Chart,
        rule: &'a RulePreset,
        device: &'a DeviceProfile,
        start: &InputStart,
        section: Option<&eval::Section>,
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
        let evaluator = Evaluator::new(chart, rule, device, start, section, budget)
            .map_err(LogicError::Eval)?;
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
        let sources = vec![Source::default(); n];
        // Each of the view's three buffers takes a copy of this one, the
        // note facts sized for the chart (`Reservation`).
        let initial = PlayView {
            generation: 0,
            origin: None,
            finalized: None,
            tally: *evaluator.tally(),
            gauges: evaluator.gauges().to_vec(),
            notes: evaluator.notes().clone(),
            input: *evaluator.input(),
            complete: false,
            playing: false,
            events: 0,
        };
        let (render, view, side) = render_link(config.effects, &initial);
        let max_events = evaluator.max_events(budget);
        let logic = Self {
            evaluator,
            device,
            start: start.clone(),
            readers,
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
            generation: 0,
            origin: None,
            playing: false,
            finalized: None,
            overflowed: false,
            aborted: None,
            feedback_dropped: 0,
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
        let open = self.read_publications();
        // (3), taken here since it depends only on the publications: the
        // global watermark. With no source left open the play is over: it
        // ends at what was already finalized, before a closed source's
        // final watermark could finalize more.
        let Some(w) = watermark(&self.published, self.device).filter(|_| open) else {
            self.abort(AbortCause::LastSourceRemoved);
            return self.idle();
        };
        // (2) Then each source's guaranteed prefix, within the budget.
        for (((s, r), p), staged) in self
            .sources
            .iter_mut()
            .zip(&mut self.readers)
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
                let Some(ev) = r.pop() else {
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
        self.retire_drained();
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

    /// One step of the audio tail's feedback, once evaluation is complete
    /// (design.md "Play Lifecycle", the audio tail): the same reads, pops,
    /// staging and merge through the global watermark as
    /// [`advance_to`](Self::advance_to), from the staging the completion
    /// left on, then conversion without judging
    /// ([`Evaluator::feedback_to`]): each press sends its unjudged sound to
    /// every sink ([`EventSink::key_sound`]), and the view is published
    /// with the lanes' input and the conversion watermark moved on, the
    /// scored stream, the tally, the gauges, the note facts and the
    /// finalized watermark as the completion froze them. Nothing is
    /// recorded or spends the budget. The staging holds at most what
    /// [`advance_to`](Self::advance_to)'s merge holds: an input beyond it
    /// is dropped and counted ([`feedback_dropped`](Self::feedback_dropped)),
    /// never allocated for. Returns the watermark converted through, or
    /// `None` with no source left open. Allocates nothing.
    pub fn feedback_to(&mut self) -> Option<TimeUs> {
        // A feedback step emits no event.
        self.out.clear();
        let open = self.read_publications();
        let w = watermark(&self.published, self.device).filter(|_| open)?;
        let room = self.merged.capacity();
        let mut staged: usize = self.staged.iter().map(Staged::len).sum();
        for (((s, r), p), buffer) in self
            .sources
            .iter_mut()
            .zip(&mut self.readers)
            .zip(&self.published)
            .zip(&mut self.staged)
        {
            let up_to = p.received_up_to;
            if p.state == SourceState::Retired {
                continue;
            }
            s.drained = false;
            while s.last_popped.is_none_or(|t| t <= up_to) {
                let Some(ev) = r.pop() else {
                    s.drained = true;
                    break;
                };
                s.last_popped = Some(ev.received);
                if staged >= room || buffer.push(ev).is_err() {
                    self.feedback_dropped = self.feedback_dropped.saturating_add(1);
                } else {
                    staged += 1;
                }
            }
        }
        merge(&mut self.staged, self.device, w, &mut self.merged);
        let sinks = &mut self.sinks;
        self.evaluator
            .feedback_to(&self.merged, w, |lane, sound, origin| {
                for sink in sinks.iter_mut() {
                    sink.key_sound(lane, sound, origin);
                }
            });
        self.merged.clear();
        self.retire_drained();
        self.publish_view();
        Some(w)
    }

    /// Reads every source's publication, a retired one's sticking; returns
    /// whether a source is still open.
    fn read_publications(&mut self) -> bool {
        let mut open = false;
        for (r, p) in self.readers.iter().zip(&mut self.published) {
            if p.state != SourceState::Retired {
                *p = r.published();
            }
            open |= p.state == SourceState::Open;
        }
        open
    }

    /// Retires each closed source whose queue and staging are drained.
    fn retire_drained(&mut self) {
        for ((s, p), staged) in self
            .sources
            .iter()
            .zip(&mut self.published)
            .zip(&self.staged)
        {
            p.retire_if_drained(s.drained, staged);
        }
    }

    /// Inputs the audio tail's feedback dropped, its staging full
    /// ([`feedback_to`](Self::feedback_to)): none of them is in the
    /// record, which closed at completion.
    #[must_use]
    pub const fn feedback_dropped(&self) -> u64 {
        self.feedback_dropped
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
        for (r, p) in self.readers.iter_mut().zip(&self.published) {
            if p.state == SourceState::Retired {
                continue;
            }
            while let Some(ev) = r.pop() {
                if self.record.len() < self.inputs {
                    self.record.push(ev);
                } else {
                    self.overflowed = true;
                }
            }
        }
    }

    /// Sets the play phase the next step publishes with its view
    /// ([`PlayView::playing`]): the lifecycle's, as the step enters it.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
    }

    /// The play left its playing phase (stopped, completed or failed):
    /// publishes the view again with [`PlayView::playing`] false, so no
    /// later frame is drawn as playing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn stop_playing(&mut self) {
        self.playing = false;
        self.publish_view();
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
            generation: self.generation,
            origin: self.origin,
            finalized: self.finalized,
            tally: *self.evaluator.tally(),
            gauges: self.evaluator.gauges().to_vec(),
            notes: self.evaluator.notes().clone(),
            input: *self.evaluator.input(),
            complete: self.evaluator.is_complete(),
            playing: self.playing,
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

    /// The held-input snapshot the play started from.
    #[must_use]
    pub const fn start(&self) -> &InputStart {
        &self.start
    }

    /// Every event popped so far, in pop order: a cancelled READY
    /// candidate's input up to its cancellation ([`crate::HistoryRecorder`]).
    #[must_use]
    pub fn popped(&self) -> &[PhysicalInputEvent] {
        &self.record
    }

    /// Reserves room for a held-input snapshot of `held` entries, so a
    /// [`rearm`](Self::rearm) with one that size allocates nothing.
    pub fn reserve_start(&mut self, held: usize) {
        self.start
            .held
            .reserve(held.saturating_sub(self.start.held.len()));
    }

    /// Lends the readers out, in source order, between a cancelled READY
    /// candidate and its rearm: the input side checks and reopens their
    /// queues ([`input::SourceSet::reopen_candidate`]). No step advances
    /// until they are back ([`restore_readers`](Self::restore_readers)).
    /// Moves the vector; nothing is allocated.
    pub fn lend_readers(&mut self) -> Vec<R> {
        std::mem::take(&mut self.readers)
    }

    /// Takes back the readers [`lend_readers`](Self::lend_readers) lent.
    pub fn restore_readers(&mut self, readers: Vec<R>) {
        self.readers = readers;
    }

    /// The readers, to drain a cancelled candidate's events into its
    /// history.
    pub fn readers_mut(&mut self) -> &mut [R] {
        &mut self.readers
    }

    /// Starts the next READY candidate of the attempt from `start`, its
    /// chart time 0 at `origin` on the rig's clock (design.md "Play
    /// Lifecycle", the start gate): the cancelled candidate's evaluator is
    /// discarded whole ([`Evaluator::reset`]), never rewound or merged;
    /// its staging, record, publications, effects and sinks start over,
    /// and the view is published afresh, after the render sink's rearm,
    /// as the next generation with `origin` and the cleared note facts.
    /// The readers must be back. Nothing is allocated while `start` fits
    /// the reserved snapshot ([`reserve_start`](Self::reserve_start)).
    pub fn rearm(&mut self, start: &InputStart, origin: TimeUs) {
        self.evaluator.reset(start);
        self.start.opened_at = start.opened_at;
        self.start.held.clear();
        self.start.held.extend_from_slice(&start.held);
        for s in &mut self.sources {
            *s = Source::default();
        }
        for s in &mut self.staged {
            s.clear();
        }
        self.published.fill(Published::open(TimeUs(i64::MIN)));
        self.merged.clear();
        self.out.clear();
        self.record.clear();
        self.emitted = 0;
        self.playing = false;
        self.finalized = None;
        self.overflowed = false;
        self.aborted = None;
        self.feedback_dropped = 0;
        self.generation += 1;
        self.origin = Some(origin);
        self.render.rearm(origin);
        for sink in &mut self.sinks {
            sink.rearm(origin);
        }
        self.publish_view();
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
        v.generation = self.generation;
        v.origin = self.origin;
        v.finalized = self.finalized;
        v.tally = *self.evaluator.tally();
        // Within the capacity every buffer got from the initial view.
        v.gauges.clear();
        v.gauges.extend_from_slice(self.evaluator.gauges());
        // Copied only when this buffer holds older facts: the revision
        // advances with every change, a reset's included, and never goes
        // back. Of the same chart, so within the buffer's storage.
        let notes = self.evaluator.notes();
        if v.notes.revision() != notes.revision() {
            v.notes.copy_from(notes);
        }
        // Whole, every time: the held lanes and the latest edges.
        v.input = *self.evaluator.input();
        v.complete = self.evaluator.is_complete();
        v.playing = self.playing;
        v.events = self.emitted;
        self.view.publish();
    }
}
