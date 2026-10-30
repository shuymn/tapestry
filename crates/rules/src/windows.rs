//! Window preparation: each lane class's table, and the window policy that
//! derives a chart's windows and LN-end edges from the rank the header
//! policy read (design.md "Evaluation Stages", preparation).

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
    /// The factor `rank` scales every edge by, as `(num, den)`: a rank's
    /// percentage over 100, or `#DEFEXRANK n` as `n × defexrank_percent /
    /// 100` %, applied to each edge as it is, never rounded to a whole
    /// percent first. A `#DEFEXRANK` not positive takes the absent scale.
    #[must_use]
    pub fn factor(&self, rank: Option<JudgeRank>) -> (i128, i128) {
        let percent = match rank {
            Some(JudgeRank::Rank(r)) => usize::try_from(r)
                .ok()
                .and_then(|r| self.rank_percent.get(r))
                .copied()
                .unwrap_or(self.absent_percent),
            // `v` is `n` in thousandths: v × p / (1000 × 100 × 100).
            Some(JudgeRank::DefExRankMilli(v)) if v > 0 => {
                return (
                    i128::from(v) * i128::from(self.defexrank_percent),
                    100 * 100 * 1_000,
                );
            }
            None | Some(JudgeRank::DefExRankMilli(_)) => self.absent_percent,
        };
        (i128::from(percent), 100)
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
    /// Every window and LN-end edge scaled by the factor of
    /// [`ScaledRank::factor`], each edge `floor(edge × num / den)` in µs;
    /// the empty POOR range is not scaled.
    Scaled(ScaledRank),
}

impl WindowPolicy {
    /// Each lane class's table for `rank`. Press resolution looks at a note
    /// inside the class's own BAD window or empty POOR range, each with its
    /// own bounds (design.md "Evaluation Stages"). Scaled edges may tie.
    /// `None` when the scale is not positive or a scaled edge passes
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE).
    #[must_use]
    pub fn prepare(
        &self,
        tables: &PerLaneClass<LaneTable>,
        rank: Option<JudgeRank>,
    ) -> Option<PerLaneClass<LaneTable>> {
        let (num, den) = match self {
            Self::Thousandths(scale) => (
                i128::from(scale.scale_milli(rank)?),
                i128::from(crate::SCALE_FULL_MILLI),
            ),
            Self::Scaled(scale) => scale.factor(rank),
        };
        // The empty POOR range is never scaled.
        let scaled = |t: &LaneTable| -> Option<LaneTable> {
            Some(LaneTable {
                windows: t.windows.scaled_by(num, den)?,
                ln_end: t.ln_end.scaled_by(num, den)?,
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
            panic!("beatoraja-style scales by percentages");
        };
        s
    }

    #[test]
    fn defexrank_scales_by_its_own_fraction() {
        let s = scaled();
        let preset = beatoraja_style();
        // The keys' PERFECT edge (20 ms) at `rank`.
        let perfect = |rank| {
            WindowPolicy::Scaled(s)
                .prepare(&preset.tables, rank)
                .map(|t| t.keys.windows.perfect.early)
        };
        assert_eq!(perfect(Some(JudgeRank::Rank(0))), Some(TimeUs(5 * MS)));
        assert_eq!(perfect(Some(JudgeRank::Rank(4))), Some(TimeUs(25 * MS)));
        assert_eq!(perfect(Some(JudgeRank::Rank(5))), Some(TimeUs(15 * MS)));
        assert_eq!(perfect(None), Some(TimeUs(15 * MS)));
        // n × 75 / 100 %, not rounded to a whole percent: 1 → 0.75 %,
        // 101 → 75.75 %, 120.5 → 90.375 %.
        let defex = |milli| perfect(Some(JudgeRank::DefExRankMilli(milli)));
        assert_eq!(defex(1_000), Some(TimeUs(150)));
        assert_eq!(defex(101_000), Some(TimeUs(15_150)));
        assert_eq!(defex(120_500), Some(TimeUs(18_075)));
        // A thousandth: 0.00075 % of 20 ms floors to 0 µs, a tie.
        assert_eq!(defex(1), Some(TimeUs(0)));
        assert_eq!(defex(0), Some(TimeUs(15 * MS)));
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
