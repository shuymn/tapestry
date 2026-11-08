//! The play state's clock (design.md "beatoraja Lua Skins", the state
//! clock): where on the monotonic clock the play state was entered, which
//! each play frame's `SkinFrame::state_us` runs from.

use bms_model::TimeUs;

/// The play state's origin and whether it belongs to a practice session.
/// Stamped when the game begins a play state (`Rig::play_state_began`): a
/// chart chosen to play, a direct launch, a retry the flow takes up. A
/// practice session keeps its first origin through its retries, Starts,
/// rounds and page, until selection is entered again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StateClock {
    origin: Option<TimeUs>,
    practice: bool,
}

impl StateClock {
    /// A play state begun at `now`, with `practice` within a practice
    /// session: only a practice session already begun keeps its origin.
    pub(crate) fn began(&mut self, now: TimeUs, practice: bool) {
        if !(practice && self.practice) {
            self.origin = Some(now);
        }
        self.practice = practice;
    }

    /// Selection entered: a practice session ends, and the next play state
    /// takes an origin of its own.
    pub(crate) fn leave(&mut self) {
        self.practice = false;
    }

    /// The origin, once a play state began.
    pub(crate) const fn origin(&self) -> Option<TimeUs> {
        self.origin
    }
}
