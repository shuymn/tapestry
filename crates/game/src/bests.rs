//! What the saving extension point hears after a play's result, and what it
//! may answer for the end screen: the play's standing against the personal
//! best of its chart and preset (design.md "Personal Bests"). The bests
//! themselves live outside `game`, in whatever the sink keeps them in.

use bms_model::Chart;
use eval::Tally;
use replay::PlayRecord;
use rules::RulePreset;

use crate::result_figures::{Rate, ResultFigures};
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

/// A best's lamp, or a view's: FULL COMBO, else the cleared gauge with the
/// highest lamp rank, else failed (design.md "Personal Bests").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestLamp {
    FullCombo,
    /// The gauge's index in the preset.
    Cleared {
        gauge: u16,
    },
    Failed,
}

/// One key's personal best as the end screen and a result skin show it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BestScore {
    /// Saved, verified plays counted under the key.
    pub plays: u64,
    /// The best EX score; `None` until a play counts for the best.
    pub ex: Option<u32>,
    /// The best BP; `None` until a play counts for the best.
    pub bp: Option<u32>,
    /// The lamp; `None` until a play counts for the best.
    pub clear: Option<BestLamp>,
}

/// The key a best is stored under, as `game` names it: the chart file's
/// and the preset's SHA-256 (the LN mode is the store's one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BestKey {
    /// SHA-256 of the chart file.
    pub chart_sha256: [u8; 32],
    /// SHA-256 of the preset's canonical encoding.
    pub preset_sha256: [u8; 32],
}

/// A key's best before this play, as a result view compares with it
/// ([`RecordSink::best_before`](crate::RecordSink::best_before)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BestLookup {
    /// Known to be empty: no row, or a row no play set a best in.
    KnownEmpty,
    /// A best, its values set.
    Known(BestScore),
    /// Not known: no store, or the read failed.
    Unavailable,
}

impl BestLookup {
    /// The best a counted play's transaction read before it: `None` for
    /// the key's first play.
    #[must_use]
    pub fn previous(previous: Option<&BestScore>) -> Self {
        match previous {
            Some(s) if s.ex.is_some() => Self::Known(s.clone()),
            _ => Self::KnownEmpty,
        }
    }
}

/// A view's result against the best before this play (design.md "Personal
/// Bests", skin projections).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestCompare {
    /// The best is not known: every best value is unavailable.
    Unavailable,
    /// Known, an empty best included.
    Known(BestFigures),
}

/// The best values of a known best against a view's result. An empty best
/// is the source's empty score: EX 0, no BP, no lamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BestFigures {
    /// The best's EX score; 0 when empty.
    pub ex: u32,
    /// The view's EX score minus it.
    pub ex_diff: i64,
    /// The best's BP; `None` (nothing drawn) when empty.
    pub bp: Option<u32>,
    /// The view's BP minus it; `None` when empty.
    pub bp_diff: Option<i64>,
    /// The best's lamp; `None` when empty.
    pub clear: Option<BestLamp>,
    /// Whether the view's EX score beats it.
    pub ex_updated: bool,
    /// Whether the view's BP beats it; true when empty.
    pub bp_updated: bool,
    /// The best's rate over the view's largest EX score; `None` for a
    /// chart that declares `#RANDOM` or `#SETRANDOM`, or a largest EX
    /// score of 0.
    pub rate: Option<BestRate>,
}

/// A best's rate over the view's largest EX score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BestRate {
    /// The rate and grade.
    pub rate: Rate,
    /// Whether the view's rate beats it: compared exactly, over the same
    /// largest EX score.
    pub updated: bool,
}

impl BestCompare {
    /// The view of `figures` and `tally` against `before`; `random` for a
    /// chart whose best rate is not known.
    #[must_use]
    pub fn of(before: &BestLookup, tally: &Tally, figures: &ResultFigures, random: bool) -> Self {
        let (ex, bp) = (tally.ex_score, tally.bp);
        let (best_ex, best_bp, clear) = match before {
            BestLookup::Unavailable => return Self::Unavailable,
            BestLookup::KnownEmpty => (0, None, None),
            BestLookup::Known(b) => (b.ex.unwrap_or(0), b.bp, b.clear),
        };
        let rate = (!random)
            .then(|| Rate::of(best_ex, figures.max_ex))
            .flatten()
            .map(|rate| BestRate {
                rate,
                updated: ex > best_ex,
            });
        Self::Known(BestFigures {
            ex: best_ex,
            ex_diff: i64::from(ex) - i64::from(best_ex),
            bp: best_bp,
            bp_diff: best_bp.map(|b| i64::from(bp) - i64::from(b)),
            clear,
            ex_updated: ex > best_ex,
            bp_updated: best_bp.is_none_or(|b| bp < b),
            rate,
        })
    }
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
