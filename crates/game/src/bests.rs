//! What the saving extension point hears after a play's result, and what it
//! may answer for the end screen: the play's standing against the personal
//! best of its chart and preset (design.md "Personal Bests"). The bests
//! themselves live outside `game`, in whatever the sink keeps them in.

use bms_model::Chart;
use eval::Tally;
use replay::PlayRecord;
use rules::RulePreset;
use serde::{Deserialize, Serialize};

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
    /// The best max combo, of any counted play, not only the best EX
    /// score's; `None` until a play counts for the best.
    pub max_combo: Option<u32>,
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
    /// The best's max combo, 0 included; `None` (nothing drawn) while
    /// no play counts for the best.
    pub max_combo: Option<u32>,
    /// The view's max combo minus it; `None` as `max_combo`.
    pub max_combo_diff: Option<i64>,
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
        let (best_ex, best_bp, best_combo, clear) = match before {
            BestLookup::Unavailable => return Self::Unavailable,
            BestLookup::KnownEmpty => (0, None, None, None),
            BestLookup::Known(b) => (b.ex.unwrap_or(0), b.bp, b.max_combo, b.clear),
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
            max_combo: best_combo,
            max_combo_diff: best_combo.map(|c| i64::from(tally.max_combo) - i64::from(c)),
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

/// The target score a play is paced against (`target` in `player.toml`,
/// design.md "Settings Files"): a rank's 27th of the largest EX score, the
/// largest itself, or the next rank's above the personal best.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScoreTarget {
    /// 17/27.
    #[serde(rename = "a-")]
    AMinus,
    /// 18/27.
    #[serde(rename = "a")]
    A,
    /// 19/27.
    #[serde(rename = "a+")]
    APlus,
    /// 20/27.
    #[serde(rename = "aa-")]
    AaMinus,
    /// 21/27.
    #[serde(rename = "aa")]
    Aa,
    /// 22/27.
    #[serde(rename = "aa+")]
    AaPlus,
    /// 23/27.
    #[serde(rename = "aaa-")]
    AaaMinus,
    /// 24/27.
    #[serde(rename = "aaa")]
    Aaa,
    /// 25/27.
    #[serde(rename = "aaa+")]
    AaaPlus,
    /// 26/27.
    #[serde(rename = "max-")]
    MaxMinus,
    /// The largest EX score, the default.
    #[default]
    #[serde(rename = "max")]
    Max,
    /// The first rank's 27th from 15/27 (A-'s band floor, the source's
    /// first) to 26/27 above the personal best, else the largest.
    #[serde(rename = "next")]
    Next,
}

impl ScoreTarget {
    /// Every target, in the order of their values.
    pub const ALL: [Self; 12] = [
        Self::AMinus,
        Self::A,
        Self::APlus,
        Self::AaMinus,
        Self::Aa,
        Self::AaPlus,
        Self::AaaMinus,
        Self::Aaa,
        Self::AaaPlus,
        Self::MaxMinus,
        Self::Max,
        Self::Next,
    ];

    /// A rank target's 27ths.
    const fn rank(self) -> Option<u64> {
        Some(match self {
            Self::AMinus => 17,
            Self::A => 18,
            Self::APlus => 19,
            Self::AaMinus => 20,
            Self::Aa => 21,
            Self::AaPlus => 22,
            Self::AaaMinus => 23,
            Self::Aaa => 24,
            Self::AaaPlus => 25,
            Self::MaxMinus => 26,
            Self::Max | Self::Next => return None,
        })
    }
}

/// What kind of play a [`score_target`] is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetPlay {
    /// A full play of a chart with no `#RANDOM`.
    Fixed,
    /// A full play of a `#RANDOM` chart: its best may be another draw's,
    /// so [`ScoreTarget::Next`] gives the largest EX score.
    Random,
    /// A practice round: no target, 0.
    Practice,
}

/// The target EX score of `target` for a chart of `notes` notes, whose
/// largest EX score is `2 · notes`, against the personal best EX score
/// `best` (design.md "Personal Bests"): `⌈2n·k/27⌉` for a rank of k 27ths,
/// `2n` for [`ScoreTarget::Max`], and for [`ScoreTarget::Next`] the first
/// `⌈2n·i/27⌉` above `best` for `i` from 15 to 26, `2n` when none is or for
/// a `#RANDOM` chart; 0 in practice. Exact integers, no float.
#[must_use]
pub fn score_target(target: ScoreTarget, notes: u32, best: u32, play: TargetPlay) -> u32 {
    let max = 2 * u64::from(notes);
    let at = |k: u64| (max * k).div_ceil(27);
    let ex = match (play, target.rank()) {
        (TargetPlay::Practice, _) => 0,
        (_, Some(k)) => at(k),
        (TargetPlay::Fixed, None) if target == ScoreTarget::Next => (15..=26)
            .map(at)
            .find(|&t| t > u64::from(best))
            .unwrap_or(max),
        (_, None) => max,
    };
    u32::try_from(ex).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{ScoreTarget, TargetPlay, score_target};

    const RANKS: [ScoreTarget; 10] = [
        ScoreTarget::AMinus,
        ScoreTarget::A,
        ScoreTarget::APlus,
        ScoreTarget::AaMinus,
        ScoreTarget::Aa,
        ScoreTarget::AaPlus,
        ScoreTarget::AaaMinus,
        ScoreTarget::Aaa,
        ScoreTarget::AaaPlus,
        ScoreTarget::MaxMinus,
    ];

    fn fixed(target: ScoreTarget, notes: u32, best: u32) -> u32 {
        score_target(target, notes, best, TargetPlay::Fixed)
    }

    /// Each rank is `⌈2n·k/27⌉` exactly, k = 17 (A-) to 26 (MAX-); MAX is
    /// `2n`. The values are worked by hand: at n = 81 every rank is a
    /// whole `6k`, so AAA- is 138 (a float `ceil` of `162 · 23/27` gives
    /// 139); at n = 7 each is `14k/27` rounded up; at n = 1 every rank is
    /// the whole 2.
    #[test]
    fn a_rank_target_is_its_27ths_rounded_up_exactly() {
        assert_eq!(fixed(ScoreTarget::AaaMinus, 81, 0), 138);
        for (notes, expected) in [
            (1, [2, 2, 2, 2, 2, 2, 2, 2, 2, 2]),
            (7, [9, 10, 10, 11, 11, 12, 12, 13, 13, 14]),
            (81, [102, 108, 114, 120, 126, 132, 138, 144, 150, 156]),
        ] {
            let got = RANKS.map(|r| fixed(r, notes, 0));
            assert_eq!(got, expected, "n = {notes}");
            assert_eq!(fixed(ScoreTarget::Max, notes, 0), 2 * notes);
        }
        // A chart without notes has no target, whatever the choice.
        for target in ScoreTarget::ALL {
            assert_eq!(fixed(target, 0, 0), 0, "{target:?}");
        }
    }

    /// `"next"` at n = 81, whose 27ths are `6i`: the first of `6 · 15` =
    /// 90 to `6 · 26` = 156 above the best. A best on a threshold gives
    /// the next one, a best between two the upper; one at 156 or above has
    /// none and gives 162.
    #[test]
    fn next_is_the_first_27th_from_15_above_the_best() {
        assert_eq!(fixed(ScoreTarget::Next, 81, 0), 90);
        assert_eq!(fixed(ScoreTarget::Next, 81, 89), 90);
        for i in 15..26 {
            assert_eq!(fixed(ScoreTarget::Next, 81, 6 * i), 6 * (i + 1), "on {i}");
            assert_eq!(
                fixed(ScoreTarget::Next, 81, 6 * i + 3),
                6 * (i + 1),
                "past {i}"
            );
        }
        assert_eq!(fixed(ScoreTarget::Next, 81, 155), 156);
        assert_eq!(fixed(ScoreTarget::Next, 81, 156), 162);
        assert_eq!(fixed(ScoreTarget::Next, 81, 162), 162);
        // n = 7: 15/27 of 14 is 7.78, rounded up to 8, both from no best
        // and above a best of 7.
        assert_eq!(fixed(ScoreTarget::Next, 7, 0), 8);
        assert_eq!(fixed(ScoreTarget::Next, 7, 7), 8);
    }

    /// Practice has no target, whatever the choice; a `#RANDOM` chart's
    /// `"next"` is the largest EX score whatever the best, and its ranks
    /// stay its own notes' 27ths.
    #[test]
    fn practice_gives_0_and_random_next_gives_the_largest() {
        for target in ScoreTarget::ALL {
            assert_eq!(
                score_target(target, 81, 100, TargetPlay::Practice),
                0,
                "{target:?}"
            );
        }
        for best in [0, 90, 200] {
            assert_eq!(
                score_target(ScoreTarget::Next, 81, best, TargetPlay::Random),
                162
            );
        }
        assert_eq!(
            score_target(ScoreTarget::AaaMinus, 81, 0, TargetPlay::Random),
            138
        );
        assert_eq!(
            score_target(ScoreTarget::Max, 81, 0, TargetPlay::Random),
            162
        );
    }

    /// A view of max combo `combo` compared with `before`.
    fn compare_combo(before: &super::BestLookup, combo: u32) -> super::BestCompare {
        let tally = eval::Tally {
            max_combo: combo,
            ..eval::Tally::default()
        };
        let figures = crate::result_figures::ResultFigures {
            notes: 200,
            max_ex: 400,
            rate: None,
            next_rank: 0,
            clear: super::BestLamp::Failed,
            cleared: false,
            gauge_permille: 0,
            timing: None,
            duration_us: None,
            areas: [[0; 2]; 5],
        };
        super::BestCompare::of(before, &tally, &figures, false)
    }

    /// A best of max combo `combo` (another play's than the best EX's).
    fn known(combo: Option<u32>) -> super::BestLookup {
        super::BestLookup::Known(super::BestScore {
            plays: 2,
            ex: Some(90),
            bp: Some(3),
            max_combo: combo,
            clear: Some(super::BestLamp::Failed),
        })
    }

    /// The best max combo before the play is carried with the view's max
    /// combo less it: 100 before and 120 now give 100 and 20.
    #[test]
    fn compare_carries_the_best_max_combo() {
        let super::BestCompare::Known(b) = compare_combo(&known(Some(100)), 120) else {
            panic!("unavailable");
        };
        assert_eq!((b.max_combo, b.max_combo_diff), (Some(100), Some(20)));
        let super::BestCompare::Known(b) = compare_combo(&known(Some(100)), 80) else {
            panic!("unavailable");
        };
        assert_eq!((b.max_combo, b.max_combo_diff), (Some(100), Some(-20)));
    }

    /// A known best max combo of 0 is drawn: 0, and 50 − 0 for a view of
    /// 50.
    #[test]
    fn a_zero_previous_combo_shows_173_and_175() {
        let super::BestCompare::Known(b) = compare_combo(&known(Some(0)), 50) else {
            panic!("unavailable");
        };
        assert_eq!((b.max_combo, b.max_combo_diff), (Some(0), Some(50)));
    }

    /// An empty best and one with no max combo draw nothing.
    #[test]
    fn no_previous_combo_hides_173_and_175() {
        for before in [known(None), super::BestLookup::KnownEmpty] {
            let super::BestCompare::Known(b) = compare_combo(&before, 50) else {
                panic!("unavailable");
            };
            assert_eq!((b.max_combo, b.max_combo_diff), (None, None), "{before:?}");
        }
    }
}
