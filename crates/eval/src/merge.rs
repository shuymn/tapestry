//! The watermark over all sources, and the merge of staged events into one
//! stream in evaluation-time order.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::VecDeque;

use bms_model::TimeUs;

use crate::input::{DeviceProfile, PhysicalInputEvent, SourceId, evaluation_time};

/// Where a source is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceState {
    /// Taking part and publishing.
    Open,
    /// Removed: its last publication is its final watermark. It stays in the
    /// watermark minimum until everything it queued has been merged.
    Closed,
    /// Removed and drained; it no longer bounds the watermark.
    Retired,
}

/// What a source last published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// Every event of the source received at or before this time is queued.
    pub received_up_to: TimeUs,
    /// The source's state.
    pub state: SourceState,
}

impl Published {
    /// An open source that published `received_up_to`.
    #[must_use]
    pub const fn open(received_up_to: TimeUs) -> Self {
        Self {
            received_up_to,
            state: SourceState::Open,
        }
    }

    /// A closed source whose final watermark is `final_watermark`.
    #[must_use]
    pub const fn closed(final_watermark: TimeUs) -> Self {
        Self {
            received_up_to: final_watermark,
            state: SourceState::Closed,
        }
    }

    /// Retires a closed source once nothing of it is left to merge: its
    /// queue is empty (`nothing_queued`) and so is its staging buffer.
    pub fn retire_if_drained(&mut self, nothing_queued: bool, staged: &Staged) {
        if self.state == SourceState::Closed && nothing_queued && staged.is_empty() {
            self.state = SourceState::Retired;
        }
    }
}

/// The global watermark: the minimum, over the sources not yet retired, of
/// `received_up_to − input offset`. `published` is indexed by
/// [`SourceId`]. `None` once every source is retired, which happens only
/// when the play is aborted.
#[must_use]
pub fn watermark(published: &[Published], device: &DeviceProfile) -> Option<TimeUs> {
    published
        .iter()
        .enumerate()
        .filter(|(_, p)| p.state != SourceState::Retired)
        .map(|(i, p)| {
            let offset = u16::try_from(i).map_or(TimeUs(0), |id| device.offset(SourceId(id)));
            TimeUs(p.received_up_to.0.saturating_sub(offset.0))
        })
        .min()
}

/// Why [`Staged::push`] refused an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageError {
    /// The buffer is at its capacity.
    Full,
    /// The event belongs to another source.
    OtherSource,
}

/// One source's events popped from its queue and not yet merged, in receive
/// order. Its capacity is fixed at construction.
#[derive(Clone, Debug)]
pub struct Staged {
    source: SourceId,
    events: VecDeque<PhysicalInputEvent>,
    capacity: usize,
}

impl Staged {
    /// An empty buffer for `source` holding at most `capacity` events.
    #[must_use]
    pub fn with_capacity(source: SourceId, capacity: usize) -> Self {
        Self {
            source,
            events: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// The source this buffer stages.
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    /// Appends an event of this source, in receive order.
    ///
    /// # Errors
    ///
    /// [`StageError::Full`] at capacity, so nothing is ever allocated or
    /// dropped silently; [`StageError::OtherSource`] for another source's
    /// event.
    pub fn push(&mut self, ev: PhysicalInputEvent) -> Result<(), StageError> {
        if ev.source != self.source {
            return Err(StageError::OtherSource);
        }
        if self.events.len() >= self.capacity {
            return Err(StageError::Full);
        }
        self.events.push_back(ev);
        Ok(())
    }

    /// The number of staged events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing is staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Moves every staged event with evaluation time ≤ `up_to` into `out`,
/// ordered by (evaluation time, source id, order within the source).
/// Later events stay staged.
///
/// One source's events are already in evaluation-time order, because one
/// offset applies to the whole source. This appends to `out` and allocates
/// only if `out` lacks the capacity.
pub fn merge(
    staged: &mut [Staged],
    device: &DeviceProfile,
    up_to: TimeUs,
    out: &mut Vec<PhysicalInputEvent>,
) {
    loop {
        let mut best: Option<(TimeUs, SourceId, usize)> = None;
        for (i, s) in staged.iter().enumerate() {
            let Some(front) = s.events.front() else {
                continue;
            };
            let t = evaluation_time(front, device);
            if t > up_to {
                continue;
            }
            if best.is_none_or(|(bt, bs, _)| (t, s.source) < (bt, bs)) {
                best = Some((t, s.source, i));
            }
        }
        let Some((_, _, i)) = best else {
            return;
        };
        if let Some(ev) = staged.get_mut(i).and_then(|s| s.events.pop_front()) {
            out.push(ev);
        }
    }
}
