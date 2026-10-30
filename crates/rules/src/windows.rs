//! Window preparation: each lane class's table, and the window policy that
//! derives a chart's windows and LN-end edges from the rank the header
//! policy read (design.md "Evaluation Stages", preparation).

use bms_model::{Lane, TimeUs};

use crate::{JudgeRank, JudgeWindows, LnEndWindows, RankScale, Window};

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
    /// `#DEFEXRANK n` scales by `n × defexrank_percent / 100` %, with `n`
    /// as the header policy read it, applied to each edge as it is (never
    /// rounded to a whole percent).
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
    /// The LR2 rule: at the percentage of [`ScaledRank::factor`], the
    /// PERFECT, GREAT and GOOD edges of the note windows and the LN-end
    /// edges are interpolated on the LR2 table below 100 % and scaled
    /// linearly from 100 %, each floored once in µs; then, on each side,
    /// GOOD is clamped to the BAD edge, GREAT to GOOD and PERFECT to GREAT.
    /// The LN-end edges clamp the same way, GOOD to the class's early BAD
    /// edge, since a release earlier than the GOOD edge is BAD at any
    /// distance. BAD and the empty POOR range are not scaled, so no derived
    /// edge is wider than the class's BAD edge, at any percentage.
    Interpolated(ScaledRank),
}

/// The LR2 table `WindowPolicy::Interpolated` reads, in µs: each row an
/// edge at 0, 25, 50, 75 and 100 %, ascending by row and by column. An
/// edge equal to a row's 100 % value follows that row; an edge between two
/// rows interpolates between them; an edge beyond the last row scales that
/// row by its own ratio to it.
const LR2_TABLE: [[i64; 5]; 4] = [
    [0, 0, 0, 0, 0],
    [0, 8_000, 15_000, 18_000, 21_000],
    [0, 24_000, 30_000, 40_000, 60_000],
    [0, 40_000, 60_000, 100_000, 120_000],
];

/// `edge` at the factor `num / den` (`num` and `den` positive, 1 at
/// 100 %) by the LR2 rule, before any clamp: `floor(edge × num / den)`
/// from 100 %; below it, a linear interpolation on `LR2_TABLE`, first
/// between the two rows the edge lies between (or along the last row
/// beyond it) at the two columns around the percentage, then between those
/// columns, computed exactly and floored once in µs. For a whole
/// percentage this equals the reference's two truncating divisions, since
/// flooring an integer plus a fraction below 1 over a positive integer
/// gives the integer's floor. `None` when the product does not fit
/// `i128`: wider than any window.
#[must_use]
fn interpolate_edge(edge: TimeUs, num: i128, den: i128) -> Option<i128> {
    let base = i128::from(edge.0);
    if num >= den {
        return Some(base.checked_mul(num)?.div_euclid(den));
    }
    let column = |row: &[i64; 5], c: i128| -> i128 {
        usize::try_from(c)
            .ok()
            .and_then(|c| row.get(c))
            .map_or(0, |&v| i128::from(v))
    };
    let last = |row: &[i64; 5]| column(row, 4);
    // The first row whose 100 % value is above the edge (row 0's is 0, so
    // never row 0 for an edge not negative); `None` beyond the last row.
    let above = LR2_TABLE.iter().position(|row| base < last(row));
    let lower = LR2_TABLE.get(above.unwrap_or(LR2_TABLE.len()).checked_sub(1)?)?;
    let upper = above.and_then(|s| LR2_TABLE.get(s));
    // `x(c) / d` is the edge at column `c`: between `lower` and the row
    // above, `n / d` of the way; beyond the last row, `n / d` times it.
    let (n, d) = match upper {
        Some(upper) => (base - last(lower), last(upper) - last(lower)),
        None => (base, last(lower)),
    };
    let x = |c: i128| -> Option<i128> {
        match upper {
            Some(upper) => d
                .checked_mul(column(lower, c))?
                .checked_add(n.checked_mul(column(upper, c) - column(lower, c))?),
            None => n.checked_mul(column(lower, c)),
        }
    };
    // The percentage, `100 num / den`, lies `r / den` of the way from
    // column `c` (25 c %) to the next.
    let c = num.checked_mul(4)?.div_euclid(den);
    let r = num * 4 - c * den;
    let (x1, x2) = (x(c)?, x(c + 1)?);
    let top = x1.checked_mul(den)?.checked_add(r.checked_mul(x2 - x1)?)?;
    top.checked_div_euclid(den.checked_mul(d)?)
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
            Self::Interpolated(scale) => {
                let (num, den) = scale.factor(rank);
                if num <= 0 || den <= 0 {
                    return None;
                }
                return Some(PerLaneClass {
                    keys: interpolated(&tables.keys, num, den)?,
                    scratch: interpolated(&tables.scratch, num, den)?,
                });
            }
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

/// `table` at the factor `num / den` under
/// [`WindowPolicy::Interpolated`]. `None` only for an edge that cannot be
/// one (negative), which validation refuses.
fn interpolated(table: &LaneTable, num: i128, den: i128) -> Option<LaneTable> {
    // Each edge interpolated, then no wider than `wider`.
    let clamp = |edge: TimeUs, wider: TimeUs| -> Option<TimeUs> {
        let v = interpolate_edge(edge, num, den)
            .map_or(i128::from(wider.0), |v| v.min(i128::from(wider.0)));
        i64::try_from(v).ok().filter(|v| *v >= 0).map(TimeUs)
    };
    let w = &table.windows;
    let side = |pick: fn(&Window) -> TimeUs| -> Option<[TimeUs; 3]> {
        let good = clamp(pick(&w.good), pick(&w.bad))?;
        let great = clamp(pick(&w.great), good)?;
        Some([clamp(pick(&w.perfect), great)?, great, good])
    };
    let [perfect, great, good] = side(|w| w.early)?;
    let [perfect_late, great_late, good_late] = side(|w| w.late)?;
    let window = |early, late| Window { early, late };
    let end = &table.ln_end;
    let end_good = clamp(end.good, w.bad.early)?;
    let end_great = clamp(end.great, end_good)?;
    Some(LaneTable {
        windows: JudgeWindows {
            perfect: window(perfect, perfect_late),
            great: window(great, great_late),
            good: window(good, good_late),
            bad: w.bad,
        },
        ln_end: LnEndWindows {
            perfect: clamp(end.perfect, end_great)?,
            great: end_great,
            good: end_good,
        },
        empty_poor: table.empty_poor,
    })
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

    /// LR2-style's PERFECT, GREAT, GOOD and LN-end GOOD edges (µs; early
    /// and late alike, keys and scratch alike) and BAD's, worked out by
    /// hand from the LR2 table: at a column of the table, its value; between
    /// columns, `t` of the way (37.5 % is half way from 25 % to 50 %,
    /// 99.75 % is 0.99 of the way from 75 % to 100 %); from 100 %, linear,
    /// then clamped to the next wider window.
    #[test]
    fn interpolated_windows_at_each_rank() {
        use crate::JudgeRank::{DefExRankMilli, Rank};
        let preset = crate::lr2_style();
        let at = |rank: Option<JudgeRank>| {
            let t = preset.window_policy.prepare(&preset.tables, rank).unwrap();
            assert_eq!(t.keys, t.scratch);
            let w = t.keys.windows;
            for window in [w.perfect, w.great, w.good, w.bad] {
                assert_eq!(window.early, window.late);
            }
            let l = t.keys.ln_end;
            assert_eq!((l.perfect, l.great), (l.good, l.good));
            assert_eq!(t.keys.empty_poor, preset.tables.keys.empty_poor);
            [
                w.perfect.early,
                w.great.early,
                w.good.early,
                l.good,
                w.bad.early,
            ]
            .map(|e| e.0)
        };
        let defex = |n: i64| Some(DefExRankMilli(n));
        for (rank, edges) in [
            (Some(Rank(0)), [8_000, 24_000, 40_000, 40_000, 200_000]),
            (Some(Rank(1)), [15_000, 30_000, 60_000, 60_000, 200_000]),
            (Some(Rank(2)), [18_000, 40_000, 100_000, 100_000, 200_000]),
            (Some(Rank(3)), [21_000, 60_000, 120_000, 120_000, 200_000]),
            (Some(Rank(4)), [18_000, 40_000, 100_000, 100_000, 200_000]),
            (None, [18_000, 40_000, 100_000, 100_000, 200_000]),
            (Some(Rank(5)), [18_000, 40_000, 100_000, 100_000, 200_000]),
            // 0.75 %: 0.03 of the way to 25 %.
            (defex(1_000), [240, 720, 1_200, 1_200, 200_000]),
            // 37.5 %, 60 %, 75 %, 99.75 %.
            (defex(50_000), [11_500, 27_000, 50_000, 50_000, 200_000]),
            (defex(80_000), [16_200, 34_000, 76_000, 76_000, 200_000]),
            (defex(100_000), [18_000, 40_000, 100_000, 100_000, 200_000]),
            (defex(133_000), [20_970, 59_800, 119_800, 119_800, 200_000]),
            // 150 %, 225 % and 300 %: linear, GOOD clamped from 270 and
            // 360 ms to BAD's 200 ms, the LN-end edges with it.
            (defex(200_000), [31_500, 90_000, 180_000, 180_000, 200_000]),
            (defex(300_000), [47_250, 135_000, 200_000, 200_000, 200_000]),
            (defex(400_000), [63_000, 180_000, 200_000, 200_000, 200_000]),
            // Every edge at BAD's, however large the #DEFEXRANK.
            (defex(i64::MAX), [200_000; 5]),
        ] {
            assert_eq!(at(rank), edges, "{rank:?}");
        }
    }

    /// Edges that are not on a row of the LR2 table, by hand: 40 ms lies
    /// 19/39 of the way from 21 to 60 ms, so at 50 % it is 15 + 15 × 19/39
    /// = 22.307… ms; 10 ms is 10/21 of 21 ms, 8.571… ms at 75 %; beyond
    /// the last row, 150 ms is 1.25 × 120 ms: 75 ms at 50 % and 62.5 ms
    /// at 37.5 %.
    #[test]
    fn interpolation_between_and_beyond_the_rows() {
        let at = |edge: i64, num: i128, den: i128| super::interpolate_edge(TimeUs(edge), num, den);
        assert_eq!(at(40_000, 1, 2), Some(22_307));
        assert_eq!(at(10_000, 3, 4), Some(8_571));
        assert_eq!(at(150_000, 1, 2), Some(75_000));
        assert_eq!(at(150_000, 3, 8), Some(62_500));
        assert_eq!(at(0, 1, 2), Some(0));
        // Continuous at 100 %: just below it on a row, and at it.
        assert_eq!(at(21_000, 99_999, 100_000), Some(20_999));
        assert_eq!(at(21_000, 1, 1), Some(21_000));
        // A product beyond i128 is wider than any window.
        assert_eq!(at(1 << 32, i128::MAX / 2, 1), None);
    }

    /// The clamp acts per side and per edge: an asymmetric table whose
    /// GOOD late edge scales past BAD's is clamped there, and GREAT and
    /// PERFECT with it only once they pass it too.
    #[test]
    fn interpolated_clamp_is_per_side() {
        let mut preset = crate::lr2_style();
        preset.tables.keys.windows.bad.late = TimeUs(150 * MS);
        let t = preset
            .window_policy
            .prepare(&preset.tables, Some(JudgeRank::DefExRankMilli(400_000)))
            .unwrap()
            .keys
            .windows;
        // 300 %: 63, 180 and 360 ms early, below BAD's 200; late, GOOD and
        // GREAT clamp to BAD's 150 ms.
        assert_eq!(
            [t.perfect, t.great, t.good].map(|w| (w.early.0, w.late.0)),
            [(63_000, 63_000), (180_000, 150_000), (200_000, 150_000)]
        );
        assert_eq!(t.bad.late, TimeUs(150 * MS));
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
