//! Chart model shared by the parser, the evaluator, and the player.
//!
//! Times are integer microseconds and display positions integer counts of
//! 2^-80 beat; this crate does not use floating point.
//! The float lints below are partial checks: they catch float arithmetic and
//! comparisons, not every use of a float type.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod builder;
mod chart;
mod clock;
mod error;
mod scroll;

pub use builder::ChartBuilder;
pub use chart::{
    BgmEvent, Chart, ChartIdentity, ChartMeta, HeaderValue, JudgmentHeader, JudgmentHeaderName,
    LANE_COUNT, Lane, LnMode, Note, NoteKind, SoundId, SoundRef,
};
pub use clock::{TimeSource, VirtualClock};
pub use error::ModelError;
pub use scroll::{BEAT, DOMAIN, FRAC_BITS, Position, ScrollChange, ScrollMap, ScrollSegment};

/// A point in chart time, in integer microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeUs(pub i64);

impl TimeUs {
    /// Adds `rhs`, returning `None` on overflow.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Subtracts `rhs`, returning `None` on overflow.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TimeUs;

    #[test]
    fn checked_arithmetic_reports_overflow() {
        assert_eq!(TimeUs(1).checked_add(TimeUs(2)), Some(TimeUs(3)));
        assert_eq!(TimeUs(1).checked_sub(TimeUs(2)), Some(TimeUs(-1)));
        assert_eq!(TimeUs(i64::MAX).checked_add(TimeUs(1)), None);
        assert_eq!(TimeUs(i64::MIN).checked_sub(TimeUs(1)), None);
    }
}
