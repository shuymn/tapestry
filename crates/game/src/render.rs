//! What the logic thread hands to rendering: effects through a queue, and
//! the latest play state through a triple buffer.

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
    /// Hands every queued effect to `f`, in emission order.
    pub fn drain_effects(&mut self, mut f: impl FnMut(&JudgeEvent)) {
        while let Ok(ev) = self.effects.pop() {
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
