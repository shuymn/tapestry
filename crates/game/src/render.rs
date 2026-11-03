//! What the logic thread hands to rendering: effects through a queue, and
//! the latest play state through a triple buffer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use eval::{GaugeState, InputFacts, JudgeEvent, NoteFacts, Tally};

use crate::logic::EventSink;

/// The latest state of the play, for drawing. Only the newest one matters,
/// so it goes through a triple buffer, not a queue. Everything a frame
/// keeps on screen comes from here: the numbers, the gauges and each
/// note's state; effects only start animations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayView {
    /// The READY candidate this view is of: how many times the play's
    /// candidate was rearmed before it (design.md "Play Lifecycle", the
    /// start gate). Its effects are those queued after that rearm.
    pub generation: u64,
    /// Where the rearm put this candidate's chart time 0 on the rig's
    /// clock; `None` for the play's first candidate, whose origin the play
    /// hands the screen with it.
    pub origin: Option<TimeUs>,
    /// The watermark the evaluator last finalized, if it has advanced.
    pub finalized: Option<TimeUs>,
    /// The running totals.
    pub tally: Tally,
    /// Every gauge, in the preset's order.
    pub gauges: Vec<GaugeState>,
    /// Each note's state and each lane's visual long-note hold, by the
    /// chart's note identities ([`eval::Evaluator::notes`]), sized for the
    /// chart in every buffer before the play starts.
    pub notes: NoteFacts,
    /// Each lane's input as conversion left it
    /// ([`eval::Evaluator::input`]): held now, its latest press and
    /// release, and how far conversion went. Copied whole with every
    /// view, so no effect's loss can change it.
    pub input: InputFacts,
    /// Whether evaluation is complete.
    pub complete: bool,
    /// Whether the play is in its playing phase, as the lifecycle
    /// published it with the step that wrote this view
    /// ([`crate::live`]): false in READY and from a stop, the
    /// completion or a failure on; the step that processed one of those
    /// still published true. Never derived from `complete`.
    pub playing: bool,
    /// The number of events this candidate emitted so far: its effects
    /// with a lower sequence number are the ones this view includes.
    pub events: u64,
}

/// The logic thread's end of the play view.
pub type ViewWriter = triple_buffer::Input<PlayView>;

/// Sends every judgment event to rendering as an effect (judgment display,
/// bombs, lane flashes).
///
/// When the queue is full the effect is dropped and counted. Effects are
/// feedback only: the logic thread must never wait on rendering (design.md
/// "Overall Architecture"), and the play view carries the totals, the
/// gauges and every note's state, so a dropped effect loses an animation
/// (a flash, a bomb, a judgment's text), never a number or a note's state
/// on screen. The count shows in [`RenderSide::dropped`].
#[derive(Debug)]
pub struct RenderSink {
    effects: rtrb::Producer<JudgeEvent>,
    dropped: Arc<AtomicU64>,
    /// Effects queued so far.
    pushed: u64,
    rearmed: Arc<Rearmed>,
}

/// What a READY candidate's rearm tells the render side: how many rearms
/// there were and how many effects were queued before the latest, in one
/// word so the two are always read together. A rearm is published before
/// its candidate's first view, so no view names a rearm the render side
/// cannot see yet.
#[derive(Debug, Default)]
struct Rearmed {
    /// The rearms so far above [`QUEUED_BITS`], the effects queued before
    /// the latest rearm below: those are a cancelled candidate's.
    word: AtomicU64,
}

/// The bits of [`Rearmed::word`] counting queued effects: far beyond any
/// play's events.
const QUEUED_BITS: u32 = 40;

impl Rearmed {
    /// The rearms so far and the effects queued before the latest.
    fn read(&self) -> (u64, u64) {
        let w = self.word.load(Ordering::Acquire);
        (w >> QUEUED_BITS, w & ((1 << QUEUED_BITS) - 1))
    }
}

impl EventSink for RenderSink {
    fn receive(&mut self, ev: &JudgeEvent) {
        if self.effects.push(*ev).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.pushed += 1;
        }
    }

    /// Publishes the rearm: the view of the rearmed candidate, which
    /// carries its origin, follows.
    fn rearm(&mut self, _origin: TimeUs) {
        let (count, _) = self.rearmed.read();
        let queued = self.pushed & ((1 << QUEUED_BITS) - 1);
        self.rearmed
            .word
            .store(((count + 1) << QUEUED_BITS) | queued, Ordering::Release);
    }
}

/// The render thread's end: the effect queue and the play view.
#[derive(Debug)]
pub struct RenderSide {
    effects: rtrb::Consumer<JudgeEvent>,
    view: triple_buffer::Output<PlayView>,
    dropped: Arc<AtomicU64>,
    /// Effects popped so far, discarded ones included: with the rearm's
    /// count of effects queued before it, what tells a cancelled
    /// candidate's effects apart. Sequence numbers cannot, since a dropped
    /// effect leaves a hole in them.
    popped: u64,
    rearmed: Arc<Rearmed>,
}

impl RenderSide {
    /// Pops the next effect, discarding those a rearm left behind. With
    /// `seen`, a candidate's rearm count, only that candidate's effects
    /// are handed over, and with `below` only those with a lower sequence
    /// number: a later candidate's, and later ones of its own, stay
    /// queued.
    fn pop(&mut self, seen: Option<u64>, below: u64) -> Option<JudgeEvent> {
        loop {
            if self.effects.is_empty() {
                return None;
            }
            let (count, queued) = self.rearmed.read();
            if self.popped >= queued {
                if seen.is_some_and(|s| s != count) {
                    return None;
                }
                if self.effects.peek().ok()?.seq >= below {
                    return None;
                }
                let ev = self.effects.pop().ok()?;
                self.popped += 1;
                return Some(ev);
            }
            let _ = self.effects.pop();
            self.popped += 1;
        }
    }

    /// Hands every queued effect to `f`, in emission order. The logic step
    /// can keep pushing meanwhile, so one drain can hand over more than the
    /// queue holds; a frame takes its view and effects with
    /// [`acquire`](Self::acquire). A cancelled READY candidate's effects
    /// are never handed over.
    pub fn drain_effects(&mut self, mut f: impl FnMut(&JudgeEvent)) {
        while let Some(ev) = self.pop(None, u64::MAX) {
            f(&ev);
        }
    }

    /// Hands up to `n` queued effects to `f`, in emission order, and stops
    /// popping there: the rest stay queued, in order, for the next drain.
    pub fn drain_effects_up_to(&mut self, n: usize, mut f: impl FnMut(&JudgeEvent)) {
        for _ in 0..n {
            let Some(ev) = self.pop(None, u64::MAX) else {
                return;
            };
            f(&ev);
        }
    }

    /// One frame's acquisition (design.md "Rendering and Skins", the note
    /// snapshot): the newest view, and up to `n` of its candidate's
    /// effects handed to `f` in emission order, only those the view's
    /// events include ([`PlayView::events`]). A cancelled candidate's
    /// effects are discarded; later ones, the view's own past its events
    /// or a later candidate's, stay queued for the next frame.
    ///
    /// `None` when the view and the rearms disagree: a rearm is published
    /// whose candidate's view is not yet, or one came during the drain.
    /// The frame is then not built, and the next acquisition takes the
    /// newer candidate; what was handed to `f` before such a rearm is the
    /// cancelled candidate's. The caller starts its state over when the
    /// returned view's [`generation`](PlayView::generation) is not the
    /// one it drew last.
    pub fn acquire(&mut self, n: usize, mut f: impl FnMut(&JudgeEvent)) -> Option<&PlayView> {
        let (generation, events) = {
            let v = self.view.read();
            (v.generation, v.events)
        };
        if self.rearmed.read().0 != generation {
            return None;
        }
        for _ in 0..n {
            let Some(ev) = self.pop(Some(generation), events) else {
                break;
            };
            f(&ev);
        }
        if self.rearmed.read().0 != generation {
            return None;
        }
        Some(self.view.output_buffer())
    }

    /// The newest play view.
    pub fn view(&mut self) -> &PlayView {
        self.view.read()
    }

    /// Effects dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How many times the play's READY candidate was rearmed (design.md
    /// "Play Lifecycle", the start gate): a view of another
    /// [`generation`](PlayView::generation) is not the newest candidate's.
    #[must_use]
    pub fn rearms(&self) -> u64 {
        self.rearmed.read().0
    }
}

/// The link between logic and rendering: an effect queue holding
/// `effect_capacity` events, and a play view starting from `initial`, its
/// three buffers copies of it. Writing a view with as many gauges as
/// `initial` and note facts of its chart allocates nothing. [`Logic`](crate::Logic)
/// makes its own; a driver that publishes views itself makes one here.
pub fn render_link(
    effect_capacity: usize,
    initial: &PlayView,
) -> (RenderSink, ViewWriter, RenderSide) {
    let (producer, consumer) = rtrb::RingBuffer::new(effect_capacity);
    let (writer, reader) = triple_buffer::triple_buffer(initial);
    let dropped = Arc::new(AtomicU64::new(0));
    let rearmed = Arc::new(Rearmed::default());
    (
        RenderSink {
            effects: producer,
            dropped: Arc::clone(&dropped),
            pushed: 0,
            rearmed: Arc::clone(&rearmed),
        },
        writer,
        RenderSide {
            effects: consumer,
            view: reader,
            dropped,
            popped: 0,
            rearmed,
        },
    )
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use eval::{InputFacts, JudgeEvent, JudgeEventKind, NoteFacts, Tally};
    use rules::PerJudgment;

    use super::{PlayView, ViewWriter, render_link};
    use crate::logic::EventSink;

    fn effect(seq: u64) -> JudgeEvent {
        JudgeEvent {
            seq,
            time: TimeUs(0),
            origin: None,
            kind: JudgeEventKind::SongEnd,
        }
    }

    fn seqs(effects: &[JudgeEvent]) -> Vec<u64> {
        effects.iter().map(|e| e.seq).collect()
    }

    /// A play view before anything is judged.
    fn fresh() -> PlayView {
        PlayView {
            generation: 0,
            origin: None,
            finalized: None,
            tally: Tally {
                counts: PerJudgment {
                    perfect: 0,
                    great: 0,
                    good: 0,
                    bad: 0,
                    poor: 0,
                },
                ex_score: 0,
                bp: 0,
                combo: 0,
                max_combo: 0,
                empty_poor: 0,
                fast: 0,
                slow: 0,
            },
            gauges: Vec::new(),
            notes: NoteFacts::default(),
            input: InputFacts::NONE,
            complete: false,
            playing: false,
            events: 0,
        }
    }

    /// Publishes a view of candidate `generation` with `events` emitted,
    /// as the logic step does after a step or a rearm.
    fn publish(view: &mut ViewWriter, generation: u64, origin: Option<i64>, events: u64) {
        let v = view.input_buffer_mut();
        v.generation = generation;
        v.origin = origin.map(TimeUs);
        v.events = events;
        view.publish();
    }

    /// A frame's drain stops at its buffer's capacity even while the logic
    /// step refills the queue during it, so the buffer never grows; the
    /// rest stay queued, in order, for the next frame, and the full drain
    /// at the play's end still takes everything left.
    #[test]
    fn a_frame_drain_stops_at_its_buffer_while_the_queue_refills() {
        const QUEUE: usize = 4;
        let (mut sink, _view, mut side) = render_link(QUEUE, &fresh());
        for seq in 0..4 {
            sink.receive(&effect(seq));
        }
        let mut frame: Vec<JudgeEvent> = Vec::with_capacity(QUEUE);
        let capacity = frame.capacity();
        // Three more arrive while the first frame drains.
        let mut next = 4;
        side.drain_effects_up_to(capacity, |e| {
            frame.push(*e);
            if next < 7 {
                sink.receive(&effect(next));
                next += 1;
            }
        });
        assert_eq!(seqs(&frame), [0, 1, 2, 3]);
        assert_eq!(frame.capacity(), capacity);
        // The next frame draws the rest, in order.
        frame.clear();
        side.drain_effects_up_to(capacity, |e| frame.push(*e));
        assert_eq!(seqs(&frame), [4, 5, 6]);
        // What is still queued when the play ends is all handed over.
        for seq in 7..9 {
            sink.receive(&effect(seq));
        }
        let mut left = Vec::new();
        side.drain_effects(|e| left.push(*e));
        assert_eq!(seqs(&left), [7, 8]);
        assert_eq!(side.dropped(), 0);
    }

    /// A frame takes only the effects its view includes: those the logic
    /// step pushed for a later view stay queued until a frame acquires
    /// that view, and a frame's buffer bounds what it takes.
    #[test]
    fn a_frame_takes_the_effects_its_view_includes() {
        let (mut sink, mut view, mut side) = render_link(8, &fresh());
        for seq in 0..3 {
            sink.receive(&effect(seq));
        }
        publish(&mut view, 0, None, 2);
        // The step after pushes its effect before publishing its view.
        sink.receive(&effect(3));
        let mut frame = Vec::new();
        let v = side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!((v.generation, v.events), (0, 2));
        assert_eq!(seqs(&frame), [0, 1]);
        publish(&mut view, 0, None, 4);
        frame.clear();
        assert_eq!(side.acquire(1, |e| frame.push(*e)).unwrap().events, 4);
        assert_eq!(seqs(&frame), [2]);
        frame.clear();
        side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!(seqs(&frame), [3]);
    }

    /// A rearm before a frame's acquisition (design.md "Play Lifecycle",
    /// the start gate): the cancelled candidate's effects still queued,
    /// dropped ones leaving holes in their sequence, are discarded, never
    /// handed over, and the new candidate's view, with its origin, comes
    /// with its own effects.
    #[test]
    fn a_rearm_before_the_acquisition_discards_the_cancelled_candidates_effects() {
        let (mut sink, mut view, mut side) = render_link(2, &fresh());
        for seq in 0..4 {
            sink.receive(&effect(seq));
        }
        assert_eq!(side.dropped(), 2);
        publish(&mut view, 0, None, 4);
        sink.rearm(TimeUs(5_000));
        publish(&mut view, 1, Some(5_000), 0);
        let mut frame = Vec::new();
        let v = side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!((v.generation, v.origin), (1, Some(TimeUs(5_000))));
        assert!(frame.is_empty(), "{:?}", seqs(&frame));
        // The new candidate's effects, numbered from 0 again.
        sink.receive(&effect(0));
        sink.receive(&effect(1));
        publish(&mut view, 1, Some(5_000), 2);
        side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!(seqs(&frame), [0, 1]);
    }

    /// A rearm published while its candidate's view is not yet, the frame
    /// acquiring between the two (before or during its acquisition): no
    /// frame is built of the older view with the newer rearm, and none of
    /// the new candidate's effects are taken; once its view is published,
    /// the next acquisition takes it and every one of them.
    #[test]
    fn a_rearm_whose_view_is_not_yet_published_skips_the_frame() {
        let (mut sink, mut view, mut side) = render_link(8, &fresh());
        sink.receive(&effect(0));
        publish(&mut view, 0, None, 1);
        sink.rearm(TimeUs(5_000));
        sink.receive(&effect(0));
        let mut frame = Vec::new();
        assert!(side.acquire(8, |e| frame.push(*e)).is_none());
        assert!(frame.is_empty(), "{:?}", seqs(&frame));
        publish(&mut view, 1, Some(5_000), 1);
        let v = side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!((v.generation, v.origin), (1, Some(TimeUs(5_000))));
        assert_eq!(seqs(&frame), [0]);
    }

    /// A rearm during a frame's drain: the frame is not built, and what it
    /// took was the cancelled candidate's; the next frame takes the new
    /// candidate whole, its effects included, and a later rearm's
    /// candidate the same way.
    #[test]
    fn a_rearm_during_the_drain_skips_the_frame() {
        let (mut sink, mut view, mut side) = render_link(8, &fresh());
        for seq in 0..3 {
            sink.receive(&effect(seq));
        }
        publish(&mut view, 0, None, 3);
        let mut frame = Vec::new();
        let mut rearmed = false;
        let acquired = side
            .acquire(8, |e| {
                frame.push(*e);
                if !rearmed {
                    rearmed = true;
                    sink.rearm(TimeUs(7_000));
                    sink.receive(&effect(0));
                    publish(&mut view, 1, Some(7_000), 1);
                }
            })
            .map(|v| v.generation);
        assert_eq!(acquired, None);
        assert_eq!(seqs(&frame), [0]);
        frame.clear();
        let v = side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!((v.generation, v.origin), (1, Some(TimeUs(7_000))));
        assert_eq!(seqs(&frame), [0]);
        // A rearm between acquisitions with nothing queued.
        sink.rearm(TimeUs(9_000));
        publish(&mut view, 2, Some(9_000), 0);
        frame.clear();
        let v = side.acquire(8, |e| frame.push(*e)).unwrap();
        assert_eq!((v.generation, v.origin), (2, Some(TimeUs(9_000))));
        assert!(frame.is_empty());
        assert_eq!(side.rearms(), 2);
    }
}
