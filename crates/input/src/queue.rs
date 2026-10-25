//! One source's queue and watermark: the input thread pushes a batch, then
//! publishes how far the source has been received; the logic thread reads
//! the publication first and pops after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use bms_model::TimeUs;
use eval::{PhysicalInputEvent, Published};

/// The queue was full: the event was not queued, and the source's log is
/// incomplete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow;

/// State shared by the two ends of a queue.
#[derive(Debug)]
struct Shared {
    /// What the source last published, in µs; `i64::MIN` before the first
    /// publication.
    received_up_to: AtomicI64,
    /// Set after the final watermark is stored.
    closed: AtomicBool,
    /// Set when a push found the queue full.
    overflowed: AtomicBool,
}

/// The input thread's end of a source queue.
#[derive(Debug)]
pub struct SourceWriter {
    queue: rtrb::Producer<PhysicalInputEvent>,
    shared: Arc<Shared>,
}

/// The logic thread's end of a source queue.
#[derive(Debug)]
pub struct SourceReader {
    queue: rtrb::Consumer<PhysicalInputEvent>,
    shared: Arc<Shared>,
}

/// A queue for one source holding at most `capacity` events. Nothing is
/// allocated after this call.
#[must_use]
pub fn source_queue(capacity: usize) -> (SourceWriter, SourceReader) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    let shared = Arc::new(Shared {
        received_up_to: AtomicI64::new(i64::MIN),
        closed: AtomicBool::new(false),
        overflowed: AtomicBool::new(false),
    });
    (
        SourceWriter {
            queue: producer,
            shared: Arc::clone(&shared),
        },
        SourceReader {
            queue: consumer,
            shared,
        },
    )
}

impl SourceWriter {
    /// Queues an event.
    ///
    /// # Errors
    ///
    /// [`Overflow`] when the queue is full. The event is dropped and the
    /// reader sees [`SourceReader::overflowed`], so the loss is never
    /// silent.
    pub fn push(&mut self, ev: PhysicalInputEvent) -> Result<(), Overflow> {
        self.queue.push(ev).map_err(|_| {
            self.shared.overflowed.store(true, Ordering::Release);
            Overflow
        })
    }

    /// Publishes that every event received at or before `received_up_to`
    /// is queued. Call it after pushing them. A publication never moves the
    /// watermark back.
    pub fn publish(&self, received_up_to: TimeUs) {
        self.shared
            .received_up_to
            .fetch_max(received_up_to.0, Ordering::Release);
    }

    /// Publishes the source's final watermark and closes it. Events already
    /// queued stay poppable.
    pub fn close(&self, final_watermark: TimeUs) {
        self.publish(final_watermark);
        self.shared.closed.store(true, Ordering::Release);
    }
}

impl SourceReader {
    /// What the source last published. Read it before popping: every event
    /// received at or before it is then poppable.
    #[must_use]
    pub fn published(&self) -> Published {
        // The closed flag is stored after the final watermark, so reading
        // it first means a closed source reports its final watermark.
        let closed = self.shared.closed.load(Ordering::Acquire);
        let at = TimeUs(self.shared.received_up_to.load(Ordering::Acquire));
        if closed {
            Published::closed(at)
        } else {
            Published::open(at)
        }
    }

    /// The next queued event in receive order.
    pub fn pop(&mut self) -> Option<PhysicalInputEvent> {
        self.queue.pop().ok()
    }

    /// Whether a push found the queue full.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.shared.overflowed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;
    use eval::{ControlId, InputKind, PhysicalInputEvent, Published, SourceId};

    use super::source_queue;

    fn ev(at: i64) -> PhysicalInputEvent {
        PhysicalInputEvent {
            received: TimeUs(at),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(1),
                value: 1,
            },
        }
    }

    #[test]
    fn publication_never_moves_back() {
        let (w, r) = source_queue(1);
        assert_eq!(r.published(), Published::open(TimeUs(i64::MIN)));
        w.publish(TimeUs(-1));
        assert_eq!(r.published(), Published::open(TimeUs(-1)));
        w.publish(TimeUs(-5));
        assert_eq!(r.published(), Published::open(TimeUs(-1)));
    }

    #[test]
    fn closed_source_keeps_its_queued_events() {
        let (mut w, mut r) = source_queue(4);
        w.push(ev(10)).unwrap();
        w.push(ev(12)).unwrap();
        w.close(TimeUs(12));
        assert_eq!(r.published(), Published::closed(TimeUs(12)));
        assert_eq!(r.pop(), Some(ev(10)));
        assert_eq!(r.pop(), Some(ev(12)));
        assert_eq!(r.pop(), None);
        assert!(!r.overflowed());
    }
}
