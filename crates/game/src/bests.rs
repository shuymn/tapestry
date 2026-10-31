//! What the saving extension point hears after a play's result, and what it
//! may answer for the end screen: the play's standing against the personal
//! best of its chart and preset (design.md "Personal Bests"). The bests
//! themselves live outside `game`, in whatever the sink keeps them in.

use bms_model::Chart;
use replay::PlayRecord;
use rules::RulePreset;

use crate::results::ResultSummary;
use crate::save::{RecordRef, SaveError};

/// A play once its record is saved, or failed to be, and its result is
/// summarized ([`RecordSink::after_result`](crate::RecordSink::after_result)).
#[derive(Clone, Copy, Debug)]
pub struct SavedPlay<'a> {
    /// The play's record.
    pub record: &'a PlayRecord,
    /// Where it was saved, or why it was not.
    pub saved: Result<&'a RecordRef, &'a SaveError>,
    /// The result extension's summary: the as-played re-evaluation.
    pub summary: &'a ResultSummary,
    /// The chart just played, after the transform.
    pub chart: &'a Chart,
    /// The preset the play ran under.
    pub preset: &'a RulePreset,
}

/// One key's personal best as the end screen shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BestScore {
    /// Saved, verified plays counted under the key.
    pub plays: u64,
    /// The best EX score; `None` until a play counts for the best.
    pub ex: Option<u32>,
    /// The best BP; `None` until a play counts for the best.
    pub bp: Option<u32>,
    /// The lamp: FULL COMBO, else the hardest gauge cleared, else failed;
    /// `None` until a play counts for the best.
    pub lamp: Option<String>,
}

/// What became of a play against its personal best.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BestView {
    /// The play was counted: the key's best before and after it, and,
    /// when it could not set a best, why.
    Counted {
        /// The play's own EX score and BP.
        play: (u32, u32),
        /// The best before the play; `None` for its first.
        previous: Option<BestScore>,
        /// The best after it.
        current: BestScore,
        /// Why the play could not set a best, if it could not.
        ineligible: Option<String>,
    },
    /// The play was not counted, and why.
    NotCounted(String),
}
