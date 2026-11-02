//! The attempt's cancelled READY openings, kept as they are cancelled
//! (design.md "Input Logs and Replays", cancelled openings): the input
//! side's snapshot, events and arrivals of each go into arenas reserved
//! before the first READY opens, and become the record's
//! [`ReadyHistory`] when it is saved.
//!
//! Keeping an opening allocates nothing. When an arena runs out the
//! opening is kept with what fit and the history is exhausted: the capture
//! is then sealed and incomplete, never continued as if complete.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::ops::Range;

use bms_model::TimeUs;
use eval::{ControlId, InputStart, PhysicalInputEvent, SourceId};
use input::Arrival as InputArrival;
use replay::{
    Arrival, CancelReason, CancelledOpening, MAX_CANCELLED_EVENTS, MAX_CANCELLED_OPENINGS,
    MAX_HELD, MAX_OPENING_ARRIVALS, ReadyHistory,
};

use crate::logic::SourceRead;
use crate::session::arrival;

type Held = (SourceId, ControlId, i32);

/// Why an opening was not kept whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryFull {
    /// [`MAX_CANCELLED_OPENINGS`] are kept already: this one is not.
    #[error("the history holds {MAX_CANCELLED_OPENINGS} cancelled openings already")]
    Openings,
    /// Its held snapshot is larger than the storage reserved for one: it
    /// is not kept.
    #[error("a cancelled opening's held snapshot exceeds its reserved storage")]
    Held,
    /// It is kept with the events and arrivals that fit; the rest are
    /// left in the readers.
    #[error("the history's events or an opening's arrivals ran out")]
    Events,
}

/// The arenas of an attempt's cancelled openings, reserved once and
/// reused by each attempt ([`clear`](Self::clear)).
#[derive(Debug)]
pub struct HistoryRecorder {
    /// The kept openings, each origin still on the input thread's clock.
    kept: Vec<CancelledOpening>,
    /// Held lists for openings still to be kept, each with the reserved
    /// storage of one snapshot.
    spare: Vec<Vec<Held>>,
    held: usize,
    events: Vec<PhysicalInputEvent>,
    arrivals: Vec<Arrival>,
    exhausted: bool,
    /// Whether the last kept opening is begun and not sealed.
    open: bool,
}

impl HistoryRecorder {
    /// Reserves the arenas for openings whose held snapshot has at most
    /// `held` entries (the source table's snapshot storage), at most
    /// [`MAX_HELD`].
    #[must_use]
    pub fn new(held: usize) -> Self {
        let held = held.min(MAX_HELD);
        Self {
            kept: Vec::with_capacity(MAX_CANCELLED_OPENINGS),
            spare: (0..MAX_CANCELLED_OPENINGS)
                .map(|_| Vec::with_capacity(held))
                .collect(),
            held,
            events: Vec::with_capacity(MAX_CANCELLED_EVENTS),
            arrivals: Vec::with_capacity(MAX_CANCELLED_OPENINGS * MAX_OPENING_ARRIVALS),
            exhausted: false,
            open: false,
        }
    }

    /// The bytes [`new`](Self::new) reserves for `held` entries per
    /// snapshot: the reservation's charge for the history.
    #[must_use]
    pub const fn bytes(held: usize) -> usize {
        let held = if held < MAX_HELD { held } else { MAX_HELD };
        MAX_CANCELLED_OPENINGS
            * (size_of::<CancelledOpening>() + size_of::<Vec<Held>>() + held * size_of::<Held>())
            + MAX_CANCELLED_EVENTS * size_of::<PhysicalInputEvent>()
            + MAX_CANCELLED_OPENINGS * MAX_OPENING_ARRIVALS * size_of::<Arrival>()
    }

    /// The bytes its arenas hold now, by their capacities.
    #[must_use]
    pub fn reserved(&self) -> usize {
        let held: usize = self
            .spare
            .iter()
            .map(Vec::capacity)
            .chain(self.kept.iter().map(|o| o.start.held.capacity()))
            .sum();
        self.kept.capacity() * size_of::<CancelledOpening>()
            + self.spare.capacity() * size_of::<Vec<Held>>()
            + held * size_of::<Held>()
            + self.events.capacity() * size_of::<PhysicalInputEvent>()
            + self.arrivals.capacity() * size_of::<Arrival>()
    }

    /// Keeps the opening that `start` opened, whose time 0 lay at
    /// `origin` on the input thread's clock, closed at `closed` on its own
    /// timeline for `reason`: its events are drained from `readers` in
    /// source order, each source's in receive order, and its `arrivals`
    /// copied. Events that do not fit stay in `readers`, so the input side
    /// refuses to reopen until they are drained: a capture whose history
    /// ran out is sealed, not continued.
    ///
    /// # Errors
    ///
    /// [`HistoryFull`]; the history is then exhausted.
    pub fn keep<R: SourceRead>(
        &mut self,
        start: &InputStart,
        origin: TimeUs,
        closed: TimeUs,
        readers: &mut [R],
        arrivals: &[InputArrival],
        reason: CancelReason,
    ) -> Result<(), HistoryFull> {
        self.begin(start, origin)?;
        let drained = self.drain(readers);
        let sealed = self.seal(closed, arrivals, reason);
        drained.and(sealed)
    }

    /// Begins keeping the cancelled opening that `start` opened, whose
    /// time 0 lay at `origin` on the input thread's clock: its events
    /// follow ([`take`](Self::take), [`drain`](Self::drain)) until it is
    /// sealed ([`seal`](Self::seal)), while the attempt waits for START
    /// and SELECT to be released.
    ///
    /// # Errors
    ///
    /// [`HistoryFull::Held`] or [`HistoryFull::Openings`]: it is not kept,
    /// and the history is exhausted.
    pub fn begin(&mut self, start: &InputStart, origin: TimeUs) -> Result<(), HistoryFull> {
        if start.held.len() > self.held {
            self.exhausted = true;
            return Err(HistoryFull::Held);
        }
        let Some(mut held) = self.spare.pop() else {
            self.exhausted = true;
            return Err(HistoryFull::Openings);
        };
        held.clear();
        held.extend_from_slice(&start.held);
        let at = range(self.events.len(), self.events.len());
        self.kept.push(CancelledOpening {
            start: InputStart {
                opened_at: start.opened_at,
                held,
            },
            origin,
            closed: start.opened_at,
            events: at,
            arrivals: range(self.arrivals.len(), self.arrivals.len()),
            reason: CancelReason::Held,
        });
        self.open = true;
        Ok(())
    }

    /// Adds `events`, already taken from the readers in their order, to
    /// the opening begun.
    ///
    /// # Errors
    ///
    /// [`HistoryFull::Events`] for those that do not fit; the history is
    /// exhausted.
    pub fn take(&mut self, events: &[PhysicalInputEvent]) -> Result<(), HistoryFull> {
        if !self.open {
            return Ok(());
        }
        let fit = events
            .len()
            .min(MAX_CANCELLED_EVENTS.saturating_sub(self.events.len()));
        self.events.extend(events.iter().take(fit));
        self.extend_last();
        if fit < events.len() {
            self.exhausted = true;
            return Err(HistoryFull::Events);
        }
        Ok(())
    }

    /// Drains `readers` into the opening begun, in source order, each
    /// source's in receive order. Events that do not fit stay in
    /// `readers`, so the input side refuses to reopen until they are
    /// drained: a capture whose history ran out is sealed, not continued.
    ///
    /// # Errors
    ///
    /// [`HistoryFull::Events`]; the history is exhausted.
    pub fn drain<R: SourceRead>(&mut self, readers: &mut [R]) -> Result<(), HistoryFull> {
        if !self.open {
            return Ok(());
        }
        let mut full = false;
        for reader in readers {
            while self.events.len() < MAX_CANCELLED_EVENTS
                && let Some(ev) = reader.pop()
            {
                self.events.push(ev);
            }
            if !reader.is_empty() {
                full = true;
                break;
            }
        }
        self.extend_last();
        if full {
            self.exhausted = true;
            return Err(HistoryFull::Events);
        }
        Ok(())
    }

    /// Seals the opening begun: closed at `closed` on its own timeline for
    /// `reason`, with its `arrivals`.
    ///
    /// # Errors
    ///
    /// [`HistoryFull::Events`] when its arrivals do not fit; those that do
    /// are kept, and the history is exhausted.
    pub fn seal(
        &mut self,
        closed: TimeUs,
        arrivals: &[InputArrival],
        reason: CancelReason,
    ) -> Result<(), HistoryFull> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        let arrivals_from = self.arrivals.len();
        let fit = arrivals.len().min(MAX_OPENING_ARRIVALS);
        self.arrivals.extend(arrivals.iter().take(fit).map(arrival));
        let to = self.arrivals.len();
        if let Some(o) = self.kept.last_mut() {
            o.closed = closed;
            o.arrivals = range(arrivals_from, to);
            o.reason = reason;
        }
        if fit < arrivals.len() {
            self.exhausted = true;
            return Err(HistoryFull::Events);
        }
        Ok(())
    }

    /// Whether an opening is begun and not sealed.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// How many more events the arena takes.
    #[must_use]
    pub const fn room(&self) -> usize {
        MAX_CANCELLED_EVENTS.saturating_sub(self.events.len())
    }

    /// The capture ran out where the history could not see it (the input
    /// side's buffers): it is sealed and incomplete.
    pub const fn exhaust(&mut self) {
        self.exhausted = true;
    }

    /// The opening begun reaches the end of the events arena.
    fn extend_last(&mut self) {
        let to = self.events.len();
        if let Some(o) = self.kept.last_mut() {
            o.events = range(usize::try_from(o.events.start).unwrap_or(to), to);
        }
    }

    /// Whether an opening could not be kept whole: the capture is sealed
    /// and incomplete.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// How many openings are kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.kept.len()
    }

    /// Whether none is kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// The record's history, each origin taken against `last`, where the
    /// attempt's last opening put its time 0 on the input thread's clock:
    /// the winning opening's, or the last kept one's for a capture
    /// cancelled before playback. It allocates the record's copy.
    #[must_use]
    pub fn history(&self, last: TimeUs) -> ReadyHistory {
        ReadyHistory {
            openings: self
                .kept
                .iter()
                .map(|o| CancelledOpening {
                    origin: TimeUs(o.origin.0.saturating_sub(last.0)),
                    ..o.clone()
                })
                .collect(),
            events: self.events.clone(),
            arrivals: self.arrivals.clone(),
        }
    }

    /// Where the last kept opening put its time 0 on the input thread's
    /// clock.
    #[must_use]
    pub fn last_origin(&self) -> Option<TimeUs> {
        self.kept.last().map(|o| o.origin)
    }

    /// Empties the arenas for the next attempt, keeping their storage.
    pub fn clear(&mut self) {
        for o in self.kept.drain(..) {
            self.spare.push(o.start.held);
        }
        self.events.clear();
        self.arrivals.clear();
        self.exhausted = false;
        self.open = false;
    }
}

/// `from..to` as the record's arena range; the arenas stay far below
/// `u32::MAX`.
fn range(from: usize, to: usize) -> Range<u32> {
    let at = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    at(from)..at(to)
}
