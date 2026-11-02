//! What the logic thread hands to rendering: effects through a queue, and
//! the latest play state through a triple buffer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
}

impl EventSink for RenderSink {
    fn receive(&mut self, ev: &JudgeEvent) {
        if self.effects.push(*ev).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The render thread's end: the effect queue and the play view.
#[derive(Debug)]
pub struct RenderSide {
    effects: rtrb::Consumer<JudgeEvent>,
    view: triple_buffer::Output<PlayView>,
    dropped: Arc<AtomicU64>,
}

impl RenderSide {
    /// Hands every queued effect to `f`, in emission order. The logic step
    /// can keep pushing meanwhile, so one drain can hand over more than the
    /// queue holds; a frame bounded by its buffer uses
    /// [`drain_effects_up_to`](Self::drain_effects_up_to).
    pub fn drain_effects(&mut self, mut f: impl FnMut(&JudgeEvent)) {
        while let Ok(ev) = self.effects.pop() {
            f(&ev);
        }
    }

    /// Hands up to `n` queued effects to `f`, in emission order, and stops
    /// popping there: the rest stay queued, in order, for the next drain.
    pub fn drain_effects_up_to(&mut self, n: usize, mut f: impl FnMut(&JudgeEvent)) {
        for _ in 0..n {
            let Ok(ev) = self.effects.pop() else {
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
    (
        RenderSink {
            effects: producer,
            dropped: Arc::clone(&dropped),
        },
        writer,
        RenderSide {
            effects: consumer,
            view: reader,
            dropped,
        },
    )
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use eval::{JudgeEvent, JudgeEventKind, Tally};
    use rules::PerJudgment;

    use super::{PlayView, render_link};
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
}
