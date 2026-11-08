//! The play state's clock (design.md "beatoraja Lua Skins", the state
//! clock): where on the monotonic clock the play state was entered, which
//! each play frame's `SkinFrame::state_us` runs from.

use bms_model::TimeUs;

/// The play state's origin and whether it belongs to a practice session.
/// Stamped when a play state is entered: a chart chosen to play, a retry,
/// the first of a practice session's page and loading state; kept across
/// a practice session's rounds and its page until selection is entered
/// again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StateClock {
    origin: Option<TimeUs>,
    practice: bool,
}

impl StateClock {
    /// A play state's scene entered at `now`: a chart's loading state, or
    /// with `practice` a practice session's page or loading state. Only a
    /// practice session already entered keeps its origin.
    pub(crate) fn enter(&mut self, now: TimeUs, practice: bool) {
        if !(practice && self.practice) {
            self.origin = Some(now);
        }
        self.practice = practice;
    }

    /// A retry received at `at`: a fresh origin, except within a practice
    /// session, whose rounds keep theirs.
    pub(crate) fn retry(&mut self, at: TimeUs) {
        if !self.practice {
            self.origin = Some(at);
        }
    }

    /// Selection entered: a practice session ends, and the next play state
    /// takes an origin of its own.
    pub(crate) fn leave(&mut self) {
        self.practice = false;
    }

    /// The origin, once a play state was entered.
    pub(crate) const fn origin(&self) -> Option<TimeUs> {
        self.origin
    }
}
