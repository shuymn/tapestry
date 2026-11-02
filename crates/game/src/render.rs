//! What the logic thread hands to rendering: effects through a queue, and
//! the latest play state through a triple buffer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering, fence};

use bms_model::TimeUs;
use eval::{GaugeState, JudgeEvent, Tally};

use crate::logic::EventSink;

/// The latest state of the play, for drawing. Only the newest one matters,
/// so it goes through a triple buffer, not a queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayView {
    /// The watermark the evaluator last finalized, if it has advanced.
    pub finalized: Option<TimeUs>,
    /// The running totals.
    pub tally: Tally,
    /// Every gauge, in the preset's order.
    pub gauges: Vec<GaugeState>,
    /// Whether evaluation is complete.
    pub complete: bool,
    /// Whether the play is in its playing phase, as the lifecycle
    /// published it with the step that wrote this view
    /// ([`crate::live`]): false in READY and from a stop, the
    /// completion or a failure on; the step that processed one of those
    /// still published true. Never derived from `complete`.
    pub playing: bool,
    /// The number of events emitted so far.
    pub events: u64,
}

/// The logic thread's end of the play view.
pub type ViewWriter = triple_buffer::Input<PlayView>;

/// Sends every judgment event to rendering as an effect (judgment display,
/// bombs, lane flashes).
///
/// When the queue is full the effect is dropped and counted. Effects are
/// display-only: the logic thread must never wait on rendering (design.md
/// "Overall Architecture"), and the play view still carries the totals and
/// gauges, so a dropped effect loses a flash, never a number on screen. The
/// count shows in [`RenderSide::dropped`].
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
/// word so the two are always read together, and the latest rearmed
/// candidate's origin, read with the count as one snapshot through a
/// sequence the one writer, the logic step, makes odd while it writes.
#[derive(Debug, Default)]
struct Rearmed {
    /// Odd while a rearm is written, advanced by two for each.
    seq: AtomicU64,
    /// The rearms so far above [`QUEUED_BITS`], the effects queued before
    /// the latest rearm below: those are a cancelled candidate's.
    word: AtomicU64,
    /// The latest rearmed candidate's chart time 0 on the rig's clock.
    origin: AtomicI64,
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

    /// A rearm's writing begins: its candidate's `origin`, which no
    /// snapshot takes until [`publish`](Self::publish).
    fn begin(&self, origin: TimeUs) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.origin.store(origin.0, Ordering::Relaxed);
    }

    /// The rearm begun is published: `word` with its origin.
    fn publish(&self, word: u64) {
        self.word.store(word, Ordering::Release);
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Release);
    }

    /// The rearms so far and the latest one's origin, of one rearm;
    /// `None` while one is written.
    fn try_snapshot(&self) -> Option<(u64, TimeUs)> {
        let before = self.seq.load(Ordering::Acquire);
        if before & 1 == 1 {
            return None;
        }
        let word = self.word.load(Ordering::Acquire);
        let origin = self.origin.load(Ordering::Acquire);
        let after = self.seq.load(Ordering::Relaxed);
        (before == after).then_some((word >> QUEUED_BITS, TimeUs(origin)))
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

    fn rearm(&mut self, origin: TimeUs) {
        self.rearmed.begin(origin);
        let (count, _) = self.rearmed.read();
        let queued = self.pushed & ((1 << QUEUED_BITS) - 1);
        self.rearmed.publish(((count + 1) << QUEUED_BITS) | queued);
    }
}

/// The render thread's end: the effect queue and the play view.
#[derive(Debug)]
pub struct RenderSide {
    effects: rtrb::Consumer<JudgeEvent>,
    view: triple_buffer::Output<PlayView>,
    dropped: Arc<AtomicU64>,
    /// Effects popped so far, discarded ones included.
    popped: u64,
    rearmed: Arc<Rearmed>,
}

impl RenderSide {
    /// Pops the next effect, discarding those a rearm left behind. With
    /// `seen`, the rearms the caller's state is of, an effect of a later
    /// candidate stays queued: a drain never hands a newer candidate's
    /// effects to state that has not started over for it.
    fn pop(&mut self, seen: Option<u64>) -> Option<JudgeEvent> {
        loop {
            if self.effects.is_empty() {
                return None;
            }
            let (count, queued) = self.rearmed.read();
            if self.popped >= queued {
                if seen.is_some_and(|s| s != count) {
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
    /// queue holds; a frame bounded by its buffer uses
    /// [`drain_effects_up_to`](Self::drain_effects_up_to). A cancelled
    /// READY candidate's effects are never handed over.
    pub fn drain_effects(&mut self, mut f: impl FnMut(&JudgeEvent)) {
        while let Some(ev) = self.pop(None) {
            f(&ev);
        }
    }

    /// Hands up to `n` queued effects to `f`, in emission order, and stops
    /// popping there: the rest stay queued, in order, for the next drain.
    pub fn drain_effects_up_to(&mut self, n: usize, f: impl FnMut(&JudgeEvent)) {
        self.drain(n, None, f);
    }

    /// [`drain_effects_up_to`](Self::drain_effects_up_to) for state of the
    /// candidate after `seen` rearms ([`rearms`](Self::rearms), read
    /// before the state started over): a later candidate's effects stay
    /// queued until the caller's state has started over for it, so a
    /// rearm between the caller's look and its drain loses none of them.
    pub fn drain_effects_of(&mut self, seen: u64, n: usize, f: impl FnMut(&JudgeEvent)) {
        self.drain(n, Some(seen), f);
    }

    fn drain(&mut self, n: usize, seen: Option<u64>, mut f: impl FnMut(&JudgeEvent)) {
        for _ in 0..n {
            let Some(ev) = self.pop(seen) else {
                return;
            };
            f(&ev);
        }
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

    /// How many times the play's READY candidate was rearmed: when it
    /// changes, the screen starts the candidate's clocks and READY timers
    /// over, keeping its chart and text resources (design.md "Play
    /// Lifecycle", the start gate).
    #[must_use]
    pub fn rearms(&self) -> u64 {
        self.rearmed.read().0
    }

    /// [`rearms`](Self::rearms) with where the latest rearmed READY
    /// candidate puts its chart time 0 on the rig's clock, both of the
    /// same rearm: the screen draws from there once the count changed.
    /// A rearm being written is waited out, which takes the logic step a
    /// few stores.
    #[must_use]
    pub fn rearm_state(&self) -> (u64, TimeUs) {
        loop {
            if let Some(state) = self.rearmed.try_snapshot() {
                return state;
            }
            std::hint::spin_loop();
        }
    }
}

/// The link between logic and rendering: an effect queue holding
/// `effect_capacity` events, and a play view starting from `initial`.
/// Writing a view with as many gauges as `initial` allocates nothing.
pub(crate) fn render_link(
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
    use eval::{JudgeEvent, JudgeEventKind, Tally};
    use rules::PerJudgment;

    use super::{PlayView, QUEUED_BITS, render_link};
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
            },
            gauges: Vec::new(),
            complete: false,
            playing: false,
            events: 0,
        }
    }

    /// A frame's drain stops at its buffer's capacity even while the logic
    /// step refills the queue during it, so the buffer never grows; the
    /// rest stay queued, in order, for the next frame, and the full drain
    /// at the play's end still takes everything left.
    #[test]
    fn a_frame_drain_stops_at_its_buffer_while_the_queue_refills() {
        const QUEUE: usize = 4;
        let initial = PlayView {
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
            },
            gauges: Vec::new(),
            complete: false,
            playing: false,
            events: 0,
        };
        let (mut sink, _view, mut side) = render_link(QUEUE, &initial);
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

    /// A rearm landing between a frame's look at the rearms and its drain
    /// (design.md "Play Lifecycle", the start gate): the drain discards the
    /// cancelled candidate's effects and leaves the new candidate's queued,
    /// so the next frame, which starts its state over first, gets every one
    /// of them; the rearm's word is read whole, never a count with another
    /// rearm's queued effects.
    #[test]
    fn a_rearm_between_the_look_and_the_drain_loses_no_effect() {
        let initial = PlayView {
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
            },
            gauges: Vec::new(),
            complete: false,
            playing: false,
            events: 0,
        };
        let (mut sink, _view, mut side) = render_link(8, &initial);
        sink.receive(&effect(0));
        sink.receive(&effect(1));
        // The frame looks: candidate 0, whose effects it takes.
        let seen = side.rearms();
        assert_eq!(seen, 0);
        let mut frame = Vec::new();
        side.drain_effects_of(seen, 1, |e| frame.push(*e));
        assert_eq!(seqs(&frame), [0]);
        // Next frame: it looks, then the logic step rearms and candidate 1
        // judges, all before the frame drains.
        let seen = side.rearms();
        sink.rearm(TimeUs(5_000));
        sink.receive(&effect(10));
        sink.receive(&effect(11));
        frame.clear();
        side.drain_effects_of(seen, 8, |e| frame.push(*e));
        assert!(frame.is_empty(), "{:?}", seqs(&frame));
        // The frame after sees the rearm, starts over, and gets them all.
        let (seen, origin) = side.rearm_state();
        assert_eq!((seen, origin), (1, TimeUs(5_000)));
        side.drain_effects_of(seen, 8, |e| frame.push(*e));
        assert_eq!(seqs(&frame), [10, 11]);
    }

    /// A second rearm overlapping a frame's look: while its origin is
    /// stored and its count not yet published, no snapshot pairs the first
    /// rearm's count with the second's origin; once published, both are
    /// the second's.
    #[test]
    fn a_rearm_state_is_never_of_two_rearms() {
        let (mut sink, _view, side) = render_link(8, &fresh());
        sink.rearm(TimeUs(5_000));
        assert_eq!(side.rearm_state(), (1, TimeUs(5_000)));
        // The logic step's second rearm, halfway.
        sink.rearmed.begin(TimeUs(9_000));
        assert_eq!(side.rearmed.try_snapshot(), None, "a mixed snapshot");
        sink.rearmed.publish(2 << QUEUED_BITS);
        assert_eq!(side.rearm_state(), (2, TimeUs(9_000)));
    }
}
