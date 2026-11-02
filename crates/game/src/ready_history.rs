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
use input::{Arrival as InputArrival, SourceReader};
use replay::{
    Arrival, CancelReason, CancelledOpening, MAX_CANCELLED_EVENTS, MAX_CANCELLED_OPENINGS,
    MAX_HELD, MAX_OPENING_ARRIVALS, ReadyHistory,
};

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
    pub fn keep(
        &mut self,
        start: &InputStart,
        origin: TimeUs,
        closed: TimeUs,
        readers: &mut [SourceReader],
        arrivals: &[InputArrival],
        reason: CancelReason,
    ) -> Result<(), HistoryFull> {
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
        let events_from = self.events.len();
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
        let arrivals_from = self.arrivals.len();
        let fit = arrivals.len().min(MAX_OPENING_ARRIVALS);
        full |= fit < arrivals.len();
        self.arrivals.extend(arrivals.iter().take(fit).map(arrival));
        self.kept.push(CancelledOpening {
            start: InputStart {
                opened_at: start.opened_at,
                held,
            },
            origin,
            closed,
            events: range(events_from, self.events.len()),
            arrivals: range(arrivals_from, self.arrivals.len()),
            reason,
        });
        if full {
            self.exhausted = true;
            return Err(HistoryFull::Events);
        }
        Ok(())
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
    }
}

/// `from..to` as the record's arena range; the arenas stay far below
/// `u32::MAX`.
fn range(from: usize, to: usize) -> Range<u32> {
    let at = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    at(from)..at(to)
}
