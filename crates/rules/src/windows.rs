//! Window preparation: each lane class's table, and the window policy that
//! derives a chart's windows, LN-end edges, empty POOR ranges and scan
//! envelopes from the rank the header policy read (design.md "Evaluation
//! Stages", preparation).

use bms_model::{Lane, TimeUs};

use crate::{JudgeRank, JudgeWindows, LnEndWindows, RankScale, scale_edge};

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
/// distances inclusive. Press resolution reaches a note only inside the
/// scan envelope too, which can cut the range's far end
/// ([`WindowPolicy::Scaled`]).
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

impl LaneTable {
    /// The farthest the table reaches ahead of and behind a press: the
    /// wider of the BAD window's and the empty POOR range's.
    fn reach(&self) -> (TimeUs, TimeUs) {
        let bad = self.windows.bad;
        let ep = self.empty_poor;
        (bad.early.max(ep.ahead), bad.late.max(ep.behind))
    }
}

/// One bound of a [`ScanEnvelope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reach {
    /// The distance from the press (a magnitude, not negative).
    pub distance: TimeUs,
    /// Whether a note exactly `distance` away is inside.
    pub inclusive: bool,
}

/// The farthest a note may lie ahead of or behind a press for press
/// resolution to look at it: no note outside is a candidate or makes an
/// empty POOR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanEnvelope {
    /// Toward notes after the press.
    pub ahead: Reach,
    /// Toward notes before the press.
    pub behind: Reach,
}

impl ScanEnvelope {
    /// Whether a note at deviation `d = press − note` lies inside.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn contains(&self, d: i128) -> bool {
        let (reach, distance) = if d < 0 {
            (self.ahead, -d)
        } else {
            (self.behind, d)
        };
        let r = i128::from(reach.distance.0);
        distance < r || (reach.inclusive && distance == r)
    }
}

/// One lane class's table after the window policy applied the chart's
/// rank, with its scan envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedTable {
    /// The scaled table. Its edges may tie; they never cross.
    pub table: LaneTable,
    /// Where press resolution looks for notes.
    pub envelope: ScanEnvelope,
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
    /// Whether the empty POOR range scales with the note windows. The note
    /// windows and LN-end edges always scale.
    pub scales_empty_poor: bool,
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
    /// scaled. Each lane class's scan envelope is its own BAD window and
    /// empty POOR range, so it never cuts either.
    Thousandths(RankScale),
    /// Every window and LN-end edge scaled by a whole percentage from
    /// [`ScaledRank`], each edge `floor(edge × percent / 100)` in µs; the
    /// empty POOR range only if [`ScaledRank::scales_empty_poor`]. One scan
    /// envelope spans both lane classes: ahead to the farthest early BAD
    /// edge or empty POOR reach of either table, that distance excluded;
    /// behind to the farthest late one, included. Once a scaled early BAD
    /// edge passes the empty POOR range's far end, a note exactly at that
    /// end comes inside.
    Scaled(ScaledRank),
}

impl WindowPolicy {
    /// Each lane class's table and scan envelope for `rank`. A scale of 0
    /// gives zero-width windows. `None` when the scale is unusable
    /// (negative, or not fitting) or a scaled edge passes
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE).
    #[must_use]
    pub fn prepare(
        &self,
        tables: &PerLaneClass<LaneTable>,
        rank: Option<JudgeRank>,
    ) -> Option<PerLaneClass<PreparedTable>> {
        let scaled = |t: &LaneTable, milli: i64, empty_poor: bool| -> Option<LaneTable> {
            let range = t.empty_poor;
            Some(LaneTable {
                windows: t.windows.scaled(milli)?,
                ln_end: t.ln_end.scaled(milli)?,
                empty_poor: if empty_poor {
                    EmptyPoorRange {
                        ahead: scale_edge(range.ahead, milli)?,
                        behind: scale_edge(range.behind, milli)?,
                    }
                } else {
                    range
                },
            })
        };
        match self {
            Self::Thousandths(scale) => {
                let milli = scale.scale_milli(rank)?;
                let own = |t: &LaneTable| -> Option<PreparedTable> {
                    let table = scaled(t, milli, false)?;
                    let (ahead, behind) = table.reach();
                    Some(PreparedTable {
                        table,
                        envelope: ScanEnvelope {
                            ahead: Reach {
                                distance: ahead,
                                inclusive: true,
                            },
                            behind: Reach {
                                distance: behind,
                                inclusive: true,
                            },
                        },
                    })
                };
                Some(PerLaneClass {
                    keys: own(&tables.keys)?,
                    scratch: own(&tables.scratch)?,
                })
            }
            Self::Scaled(rank_scale) => {
                let milli = rank_scale.percent(rank)?.checked_mul(1_000)?;
                let empty_poor = rank_scale.scales_empty_poor;
                let keys = scaled(&tables.keys, milli, empty_poor)?;
                let scratch = scaled(&tables.scratch, milli, empty_poor)?;
                let (keys_ahead, keys_behind) = keys.reach();
                let (scratch_ahead, scratch_behind) = scratch.reach();
                let envelope = ScanEnvelope {
                    ahead: Reach {
                        distance: keys_ahead.max(scratch_ahead),
                        inclusive: false,
                    },
                    behind: Reach {
                        distance: keys_behind.max(scratch_behind),
                        inclusive: true,
                    },
                };
                Some(PerLaneClass {
                    keys: PreparedTable {
                        table: keys,
                        envelope,
                    },
                    scratch: PreparedTable {
                        table: scratch,
                        envelope,
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{Reach, ScaledRank, ScanEnvelope, WindowPolicy};
    use crate::{JudgeRank, beatoraja_style, custom};

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
    fn a_scaled_empty_poor_range_follows_the_scale() {
        let mut s = scaled();
        let preset = beatoraja_style();
        let at = |s: ScaledRank| {
            WindowPolicy::Scaled(s)
                .prepare(&preset.tables, Some(JudgeRank::Rank(0)))
                .unwrap()
                .scratch
                .table
                .empty_poor
        };
        assert_eq!(at(s).behind, TimeUs(160 * MS));
        s.scales_empty_poor = true;
        // 25 % of 500 and 160 ms.
        assert_eq!(at(s).ahead, TimeUs(125 * MS));
        assert_eq!(at(s).behind, TimeUs(40 * MS));
    }

    #[test]
    fn thousandths_envelopes_never_cut_a_range() {
        let preset = custom();
        let prepared = preset.window_policy.prepare(&preset.tables, None).unwrap();
        let inclusive = |ms: i64| Reach {
            distance: TimeUs(ms * MS),
            inclusive: true,
        };
        assert_eq!(
            prepared.keys.envelope,
            ScanEnvelope {
                ahead: inclusive(500),
                behind: inclusive(180),
            }
        );
        assert!(prepared.keys.envelope.contains(-500 * i128::from(MS)));
        assert!(!prepared.keys.envelope.contains(-500 * i128::from(MS) - 1));
        assert!(prepared.keys.envelope.contains(180 * i128::from(MS)));
        assert!(!prepared.keys.envelope.contains(180 * i128::from(MS) + 1));
    }
}
