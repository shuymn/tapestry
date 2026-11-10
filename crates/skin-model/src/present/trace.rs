//! The program's invocation trace (dev-tools; design.md "beatoraja Lua
//! Skins", the reference executor): what [`super::Present::run`] did with
//! each callback this thread built a frame for, and the age each element
//! drawn with a timer was sampled at ([`crate::widget`]'s `sample`).
//!
//! In-play code: it records only while [`arm`]ed, and checks that with
//! one `Cell` read otherwise, so an unarmed build allocates nothing. Only
//! the offline harness arms it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::{Cell, RefCell};

use bms_model::TimeUs;

use super::{Fail, Val};

/// Why a callback did not run this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Its slots do not run on this attempt (the carried-state limits).
    SlotsOff,
    /// The callback it follows gave no value, or a false draw, or an off
    /// timer.
    After,
    /// Its gate was false or unavailable.
    Gate,
}

/// What production converted an invocation's result to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Converted {
    /// A draw's truthiness.
    Bool(bool),
    /// A value's `toint()` and whether it gave one (not unavailable and
    /// no sentinel).
    Int(i32, bool),
    /// A timer invocation's `tolong()`, an error the off value; `None`
    /// for no value.
    Long(Option<i64>),
}

/// One thing a frame's build did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// Callback `k` invoked: its `call` (2 for a timer's second), its raw
    /// result and its conversion.
    Ran {
        k: u16,
        call: u8,
        result: Result<Val, Fail>,
        conv: Converted,
    },
    /// Timer callback `k` after its invocations: whether it was on, the
    /// second invocation's start, the elapsed ms before the cap, and the
    /// start installed in the play's timers (`None` while off).
    Timer {
        k: u16,
        on: bool,
        start: Option<i64>,
        elapsed_ms: Option<i64>,
        installed: Option<TimeUs>,
    },
    /// Callback `k` not invoked.
    Skipped { k: u16, why: Skip },
    /// An element's track sampled on timer `slot` at `age_us`: a sampling
    /// attempt, recorded just before the track is asked.
    Sampled { slot: usize, age_us: i64 },
}

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) };
}

/// Starts recording this thread's builds.
pub fn arm() {
    ARMED.with(|a| a.set(true));
}

/// Stops recording and drops what was recorded.
pub fn disarm() {
    ARMED.with(|a| a.set(false));
    let _ = take();
}

/// What was recorded since the last take.
#[must_use]
pub fn take() -> Vec<Event> {
    EVENTS.with(|e| {
        e.try_borrow_mut()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    })
}

/// Records `event` when armed; nothing otherwise.
pub(crate) fn record(event: impl FnOnce() -> Event) {
    if ARMED.with(Cell::get) {
        EVENTS.with(|e| {
            if let Ok(mut v) = e.try_borrow_mut() {
                v.push(event());
            }
        });
    }
}
