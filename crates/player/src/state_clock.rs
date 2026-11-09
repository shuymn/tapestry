//! The play state's clock (design.md "beatoraja Lua Skins", the state
//! clock): where on the monotonic clock the play state was entered, which
//! each play frame's `SkinFrame::state_us` runs from.

use bms_model::TimeUs;

/// The play state's origin, stamped when the game begins a play state
/// (`Rig::play_state_began`): a chart chosen to play, P, a direct launch,
/// a retry the flow takes up outside practice. A practice session keeps
/// its first origin through its retries, Starts, rounds and page, since
/// the flow begins no play state within it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StateClock {
    origin: Option<TimeUs>,
}

impl StateClock {
    /// A play state begun at `now`.
    pub(crate) fn began(&mut self, now: TimeUs) {
        self.origin = Some(now);
    }

    /// The origin, once a play state began.
    pub(crate) const fn origin(&self) -> Option<TimeUs> {
        self.origin
    }
}
