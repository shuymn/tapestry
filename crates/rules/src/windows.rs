//! Window preparation: each lane class's table, and the window policy that
//! derives a chart's windows, LN-end edges, empty POOR ranges and scan
//! envelopes from the rank the header policy read (design.md "Evaluation
//! Stages", preparation).

use bms_model::{Lane, TimeUs};

use crate::{JudgeRank, JudgeWindows, LnEndWindows, RankScale};

/// The lanes that share one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneClass {
    /// The seven keys.
    Keys,
    /// The scratch.
    Scratch,
}

impl LaneClass {
    /// Every lane class, in the order tables are written.
    pub const ALL: [Self; 2] = [Self::Keys, Self::Scratch];

    /// The class of `lane`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn of(lane: Lane) -> Self {
        if lane == Lane::SCRATCH {
            Self::Scratch
        } else {
            Self::Keys
        }
    }

    /// The class's name in field names and messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Keys => "keys",
            Self::Scratch => "scratch",
        }
    }
}

/// One value per lane class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerLaneClass<T> {
    /// For the keys.
    pub keys: T,
    /// For the scratch.
    pub scratch: T,
}

impl<T> PerLaneClass<T> {
    /// The value of `class`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn get(&self, class: LaneClass) -> &T {
        match class {
            LaneClass::Keys => &self.keys,
            LaneClass::Scratch => &self.scratch,
        }
    }

    /// The value of `lane`'s class.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn of_lane(&self, lane: Lane) -> &T {
        self.get(LaneClass::of(lane))
    }

    /// Both values, keys first.
    pub const fn each_mut(&mut self) -> [&mut T; 2] {
        [&mut self.keys, &mut self.scratch]
    }
}

impl<T: Copy> PerLaneClass<T> {
    /// `v` for every lane class.
    #[must_use]
    pub const fn both(v: T) -> Self {
        Self {
            keys: v,
            scratch: v,
        }
    }
}

/// How far a note may lie from a press to make it an empty POOR, both
/// distances inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptyPoorRange {
    /// How far ahead of the press (a magnitude, not negative).
    pub ahead: TimeUs,
    /// How far behind the press (a magnitude, not negative).
    pub behind: TimeUs,
}

/// One lane class's table: what its notes, LN ends and empty POORs are
/// judged by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneTable {
    /// The note windows.
    pub windows: JudgeWindows,
    /// The early edges of a long note's end.
    pub ln_end: LnEndWindows,
    /// The empty POOR range.
    pub empty_poor: EmptyPoorRange,
}

/// How `#RANK` and `#DEFEXRANK` become a scale, as whole percentages
/// ([`WindowPolicy::Scaled`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaledRank {
    /// The scale of `#RANK 0` to `#RANK 4`, in percent.
    pub rank_percent: [u32; 5],
    /// The scale with no rank, or a `#RANK` outside 0-4, in percent.
    pub absent_percent: u32,
    /// `#DEFEXRANK n` scales by `floor(n × defexrank_percent / 100)` %,
    /// with `n` as the header policy read it.
    pub defexrank_percent: u32,
}

impl ScaledRank {
    /// The scale of `rank` in percent; `None` when it does not fit `i64`.
    /// A `#DEFEXRANK` not positive takes the absent scale.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn percent(&self, rank: Option<JudgeRank>) -> Option<i64> {
        let rank_percent = |r: i32| {
            usize::try_from(r)
                .ok()
                .and_then(|r| self.rank_percent.get(r))
                .copied()
        };
        let percent = match rank {
            Some(JudgeRank::Rank(r)) => rank_percent(r).unwrap_or(self.absent_percent),
            // `v` is `n` in thousandths: floor(v × p / (100 × 1000)).
            Some(JudgeRank::DefExRankMilli(v)) if v > 0 => {
                let p = (i128::from(v) * i128::from(self.defexrank_percent)).div_euclid(100_000);
                return i64::try_from(p).ok();
            }
            None | Some(JudgeRank::DefExRankMilli(_)) => self.absent_percent,
        };
        Some(i64::from(percent))
    }
}

/// How each lane class's table derives from the rank the header policy
/// read (design.md "Evaluation Stages", preparation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPolicy {
    /// Every window and LN-end edge scaled by [`RankScale`], in thousandths
    /// of a percent, `#DEFEXRANK n` by `n` %; the empty POOR range is not
    /// scaled.
    Thousandths(RankScale),
    /// Every window and LN-end edge scaled by a whole percentage from
    /// [`ScaledRank`], each edge `floor(edge × percent / 100)` in µs; the
    /// empty POOR range is not scaled.
    Scaled(ScaledRank),
}

impl WindowPolicy {
    /// Each lane class's table for `rank`. Press resolution looks at a note
    /// inside the class's own BAD window or empty POOR range, each with its
    /// own bounds (design.md "Evaluation Stages", scan envelopes). A scale
    /// of 0 gives zero-width windows; scaled edges may tie. `None` when the
    /// scale is unusable (negative, or not fitting) or a scaled edge passes
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE).
    #[must_use]
    pub fn prepare(
        &self,
        tables: &PerLaneClass<LaneTable>,
        rank: Option<JudgeRank>,
    ) -> Option<PerLaneClass<LaneTable>> {
        let milli = match self {
            Self::Thousandths(scale) => scale.scale_milli(rank)?,
            Self::Scaled(scale) => scale.percent(rank)?.checked_mul(1_000)?,
        };
        // The empty POOR range is never scaled.
        let scaled = |t: &LaneTable| -> Option<LaneTable> {
            Some(LaneTable {
                windows: t.windows.scaled(milli)?,
                ln_end: t.ln_end.scaled(milli)?,
                empty_poor: t.empty_poor,
            })
        };
        Some(PerLaneClass {
            keys: scaled(&tables.keys)?,
            scratch: scaled(&tables.scratch)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{ScaledRank, WindowPolicy};
    use crate::{JudgeRank, beatoraja_style};

    const MS: i64 = 1_000;

    fn scaled() -> ScaledRank {
        let WindowPolicy::Scaled(s) = beatoraja_style().window_policy else {
            panic!("beatoraja-style scales by whole percentages");
        };
        s
    }

    #[test]
    fn scaled_ranks_are_whole_percentages() {
        let s = scaled();
        let percent = |rank| s.percent(Some(rank));
        assert_eq!(percent(JudgeRank::Rank(0)), Some(25));
        assert_eq!(percent(JudgeRank::Rank(4)), Some(125));
        assert_eq!(percent(JudgeRank::Rank(5)), Some(75));
        assert_eq!(percent(JudgeRank::Rank(-1)), Some(75));
        assert_eq!(s.percent(None), Some(75));
        // n × 75 / 100, floored: 100 → 75, 101 → 75.75 → 75, 1 → 0.
        let defex = |n: i64| percent(JudgeRank::DefExRankMilli(n * 1_000));
        assert_eq!(defex(100), Some(75));
        assert_eq!(defex(101), Some(75));
        assert_eq!(defex(102), Some(76));
        assert_eq!(defex(1), Some(0));
        // A fraction, as a decimal-reading header policy gives it:
        // 120.5 × 75 / 100 = 90.375 → 90.
        assert_eq!(percent(JudgeRank::DefExRankMilli(120_500)), Some(90));
        assert_eq!(percent(JudgeRank::DefExRankMilli(0)), Some(75));
        // A scale whose edges pass 2^32 µs is unusable.
        let tables = beatoraja_style().tables;
        let policy = WindowPolicy::Scaled(s);
        assert!(
            policy
                .prepare(&tables, Some(JudgeRank::DefExRankMilli(i64::MAX)))
                .is_none()
        );
    }

    #[test]
    fn a_scaled_policy_keeps_the_empty_poor_range() {
        let preset = beatoraja_style();
        let range = WindowPolicy::Scaled(scaled())
            .prepare(&preset.tables, Some(JudgeRank::Rank(0)))
            .unwrap()
            .scratch
            .empty_poor;
        assert_eq!(
            (range.ahead, range.behind),
            (TimeUs(500 * MS), TimeUs(160 * MS))
        );
    }
}
