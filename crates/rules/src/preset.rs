//! The rule preset types.
//!
//! Each choice enum has only the variants a preset in use needs; a later
//! preset adds the variant it needs.

use bms_model::{JudgeRank, TimeUs};

/// A gauge amount in units of 1/100000 % (1 % = `100_000`, 100 % =
/// `10_000_000`).
///
/// Products of gauge values are computed in `i128` and remainders are
/// floored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GaugeValue(pub i64);

impl GaugeValue {
    /// One percent.
    pub const PERCENT: i64 = 100_000;
    /// An empty gauge.
    pub const EMPTY: Self = Self(0);
    /// A full gauge (100 %).
    pub const FULL: Self = Self(100 * Self::PERCENT);

    /// `n` hundredths of a percent.
    #[must_use]
    pub const fn centi_percent(n: i64) -> Self {
        Self(n * (Self::PERCENT / 100))
    }
}

/// A judgment a scored note can receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Judgment {
    /// The best judgment.
    Perfect,
    /// The second judgment.
    Great,
    /// The third judgment.
    Good,
    /// The worst judgment given to a press.
    Bad,
    /// A missed note.
    Poor,
}

impl Judgment {
    /// Every judgment, best first.
    pub const ALL: [Self; 5] = [
        Self::Perfect,
        Self::Great,
        Self::Good,
        Self::Bad,
        Self::Poor,
    ];
}

/// One value per judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerJudgment<T> {
    /// For PERFECT.
    pub perfect: T,
    /// For GREAT.
    pub great: T,
    /// For GOOD.
    pub good: T,
    /// For BAD.
    pub bad: T,
    /// For POOR.
    pub poor: T,
}

impl<T> PerJudgment<T> {
    /// The value for `j`.
    pub const fn get(&self, j: Judgment) -> &T {
        match j {
            Judgment::Perfect => &self.perfect,
            Judgment::Great => &self.great,
            Judgment::Good => &self.good,
            Judgment::Bad => &self.bad,
            Judgment::Poor => &self.poor,
        }
    }
}

/// The evaluation implementation a preset runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Implementation {
    /// The standard evaluator.
    Standard {
        /// Its version; a change of meaning bumps it.
        version: u32,
    },
}

impl Implementation {
    /// The implementation's name as recorded.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Standard { .. } => "standard",
        }
    }

    /// The implementation's version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        match self {
            Self::Standard { version } => *version,
        }
    }
}

/// One judgment's window, as distances from the note time.
///
/// A press at deviation `d = press − note` falls inside when
/// `−early ≤ d ≤ late`; both edges are inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// How far before the note the window opens (a magnitude, not negative).
    pub early: TimeUs,
    /// How far after the note the window closes (a magnitude, not negative).
    pub late: TimeUs,
}

impl Window {
    /// The same early and late distance.
    #[must_use]
    pub const fn symmetric(us: i64) -> Self {
        Self {
            early: TimeUs(us),
            late: TimeUs(us),
        }
    }
}

/// The scale factor of an unchanged window, in thousandths of a percent.
pub const SCALE_FULL_MILLI: i64 = 100_000;

/// The judgment windows, one table for every lane.
///
/// A press is given the best judgment whose window contains it. An unpressed
/// note is missed per [`MissRule`] at the late BAD edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JudgeWindows {
    /// PERFECT.
    pub perfect: Window,
    /// GREAT.
    pub great: Window,
    /// GOOD.
    pub good: Window,
    /// BAD.
    pub bad: Window,
}

impl JudgeWindows {
    /// The windows best first, each with its judgment.
    #[must_use]
    pub const fn table(&self) -> [(Judgment, Window); 4] {
        [
            (Judgment::Perfect, self.perfect),
            (Judgment::Great, self.great),
            (Judgment::Good, self.good),
            (Judgment::Bad, self.bad),
        ]
    }

    /// Every edge multiplied by `scale_milli` (thousandths of a percent, see
    /// [`RankScale::scale_milli`]).
    ///
    /// Each edge is `floor(edge × scale_milli / 100_000)`, computed in
    /// `i128`, so a fractional µs is dropped and the window never grows by
    /// rounding. `None` when the scale is not positive or an edge leaves the
    /// `i64` range.
    #[must_use]
    pub fn scaled(&self, scale_milli: i64) -> Option<Self> {
        if scale_milli <= 0 {
            return None;
        }
        let edge = |t: TimeUs| -> Option<TimeUs> {
            let v = (i128::from(t.0) * i128::from(scale_milli))
                .div_euclid(i128::from(SCALE_FULL_MILLI));
            i64::try_from(v).ok().map(TimeUs)
        };
        let window = |w: Window| -> Option<Window> {
            Some(Window {
                early: edge(w.early)?,
                late: edge(w.late)?,
            })
        };
        Some(Self {
            perfect: window(self.perfect)?,
            great: window(self.great)?,
            good: window(self.good)?,
            bad: window(self.bad)?,
        })
    }
}

/// When an unpressed note is missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissRule {
    /// A note not pressed by its late BAD edge is a POOR at that edge.
    PoorAfterLateBad,
}

/// Empty POOR: a press that judges no note but has one close ahead.
///
/// A press is an empty POOR when no unjudged note of its lane lies inside the
/// BAD window but one lies at most `ahead` after the press. Its gauge change
/// is [`GaugeRule::empty_poor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptyPoorRule {
    /// How far ahead of the press a note makes it an empty POOR (inclusive).
    /// Not scaled by the rank.
    pub ahead: TimeUs,
    /// Whether it breaks the combo.
    pub breaks_combo: bool,
    /// Whether it adds to BP.
    pub counts_bp: bool,
    /// Whether it is counted in the judgment counts.
    pub counted: bool,
}

/// LN break: releasing a long note too early after its head was scored.
///
/// A release strictly earlier than `end − release_margin` breaks the LN: the
/// head's judgment, counts and EX score stay, `ln_breaks` grows by one, and
/// the gauge takes [`GaugeRule::ln_break`]. The LN is finished, and a later
/// press on the lane is an ordinary press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LnBreakRule {
    /// How long before the LN's end a release is still not a break.
    pub release_margin: TimeUs,
    /// Whether a break breaks the combo.
    pub breaks_combo: bool,
    /// Whether a break adds to BP.
    pub counts_bp: bool,
}

/// How `#RANK` and `#DEFEXRANK` scale every window, in thousandths of a
/// percent (`100_000` = unchanged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RankScale {
    /// The scale for `#RANK 0` to `#RANK 3`.
    pub rank_milli: [i64; 4],
    /// The scale when the chart declares no rank.
    pub absent_milli: i64,
}

impl RankScale {
    /// The window scale for a chart's declared rank.
    ///
    /// `#DEFEXRANK n` scales by `n` %, which the chart model already holds in
    /// thousandths of a percent. `None` for a `#RANK` outside 0-3 or a
    /// `#DEFEXRANK` that is not positive.
    #[must_use]
    pub fn scale_milli(&self, rank: Option<JudgeRank>) -> Option<i64> {
        match rank {
            None => Some(self.absent_milli),
            Some(JudgeRank::Rank(r)) => usize::try_from(r)
                .ok()
                .and_then(|i| self.rank_milli.get(i).copied()),
            Some(JudgeRank::DefExRankMilli(v)) => (v > 0).then_some(v),
        }
    }
}

/// Which notes make up the gauge denominator (the `notes` of `TOTAL/notes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denominator {
    /// Taps and LN heads; mines are excluded.
    TapsAndLnHeads,
}

/// A gauge's change for one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeDelta {
    /// A fixed amount (negative for damage).
    Fixed(GaugeValue),
    /// A share of the per-note increment `floor(total_milli × 100 / notes)`
    /// (D5): `floor(increment × num / den)`.
    TotalShare {
        /// Numerator.
        num: u32,
        /// Denominator, not zero.
        den: u32,
    },
}

/// How a gauge decides a clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeKind {
    /// Cleared when the value at the song's end is at or above `clear`; never
    /// fails midway.
    EndThreshold {
        /// The clear threshold.
        clear: GaugeValue,
    },
    /// Failed the moment it reaches its minimum, and never recovers.
    Survival,
}

/// Reduced damage while the gauge is low.
///
/// When the value before a change is strictly below `below`, a negative
/// change `d` becomes `−floor(|d| × num / den)`. A value exactly at `below`
/// takes full damage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowDamage {
    /// The value below which damage is reduced.
    pub below: GaugeValue,
    /// Numerator of the damage factor.
    pub num: u32,
    /// Denominator of the damage factor, not zero.
    pub den: u32,
}

impl LowDamage {
    /// Whether the reduction applies to a change from `before`.
    #[must_use]
    pub fn applies_at(&self, before: GaugeValue) -> bool {
        before < self.below
    }

    /// The reduced form of `damage` (a negative change); other changes are
    /// returned as they are.
    #[must_use]
    pub fn reduce(&self, damage: GaugeValue) -> GaugeValue {
        if damage.0 >= 0 {
            return damage;
        }
        let magnitude = (-i128::from(damage.0)) * i128::from(self.num) / i128::from(self.den);
        // The reduced magnitude never exceeds the original when num ≤ den;
        // saturate otherwise, so a validated preset cannot overflow here.
        GaugeValue(i64::try_from(-magnitude).unwrap_or(i64::MIN))
    }
}

/// One gauge. Every gauge of a preset is computed side by side.
///
/// A change is computed, reduced per `low_damage`, and then clamped to
/// `min..=max`, in that order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GaugeRule {
    /// Name of the gauge, unique within the preset.
    pub id: String,
    /// How it decides a clear.
    pub kind: GaugeKind,
    /// Value at the start.
    pub start: GaugeValue,
    /// Lower bound; a survival gauge fails on reaching it.
    pub min: GaugeValue,
    /// Upper bound.
    pub max: GaugeValue,
    /// Change per judgment.
    pub change: PerJudgment<GaugeDelta>,
    /// Change for an empty POOR.
    pub empty_poor: GaugeDelta,
    /// Change for an LN break.
    pub ln_break: GaugeDelta,
    /// Reduced damage while low, if any. Applies to every negative change,
    /// mines included.
    pub low_damage: Option<LowDamage>,
}

/// EX score weights and what each judgment does to combo and BP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoreWeights {
    /// EX score per judgment.
    pub ex: PerJudgment<u32>,
    /// Whether the judgment breaks the combo.
    pub breaks_combo: PerJudgment<bool>,
    /// Whether the judgment adds to BP.
    pub counts_bp: PerJudgment<bool>,
}

/// The processing order of an input and a deadline at the same time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameTimeOrder {
    /// Inputs before deadlines, so a press exactly on a late edge counts.
    InputsFirst,
}

/// How a press is assigned to a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assignment {
    /// The nearest unjudged note inside the window; at equal distance, the
    /// earlier note.
    NearestTiesEarlier,
}

/// How long notes are judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LnMode {
    /// Only the head is judged, as a tap; holding to the end (see
    /// [`LnBreakRule`]) completes it.
    Ln,
}

/// What a head pressed after its LN's end does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LateHeadRule {
    /// A head pressed inside its late window but after the LN's end is judged
    /// by its deviation, the LN counts as completed at the press, and the
    /// following release is ignored.
    CompletesAtPress,
}

/// The long-note judgment rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LnModeRule {
    /// The mode used when the chart declares none.
    pub default: LnMode,
    /// What a head pressed after its LN's end does.
    pub late_head: LateHeadRule,
}

/// How `#TOTAL` applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalMode {
    /// It sets the per-note increment only ([`GaugeDelta::TotalShare`]).
    ScalesIncrease,
}

/// When and how much a mine damages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MineRule {
    /// A mine damages every gauge when its lane is held as it passes, by the
    /// chart's damage value × `per_unit`. It has no judgment, combo or BP
    /// effect.
    WhileHeld {
        /// Gauge damage per unit of the chart's value (not negative).
        per_unit: GaugeValue,
    },
}

/// A named set of judgment and gauge rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulePreset {
    /// Name of the preset.
    pub id: String,
    /// Version of the preset's values.
    pub version: u32,
    /// The evaluation implementation it runs on.
    pub implementation: Implementation,
    /// The judgment windows at scale 100 %.
    pub windows: JudgeWindows,
    /// When an unpressed note is missed.
    pub miss: MissRule,
    /// Empty POOR.
    pub empty_poor: EmptyPoorRule,
    /// LN break.
    pub ln_break: LnBreakRule,
    /// Window scale per rank.
    pub rank_scale: RankScale,
    /// `#TOTAL` when the chart declares none, in thousandths.
    pub default_total_milli: i64,
    /// Which notes make up the gauge denominator.
    pub denominator: Denominator,
    /// The gauges, computed side by side.
    pub gauges: Vec<GaugeRule>,
    /// Score weights, combo and BP.
    pub score: ScoreWeights,
    /// Order at equal times.
    pub same_time_order: SameTimeOrder,
    /// How a press is assigned to a note.
    pub assignment: Assignment,
    /// Long-note judgment.
    pub ln_mode: LnModeRule,
    /// How `#TOTAL` applies.
    pub total_mode: TotalMode,
    /// Mines.
    pub mine: MineRule,
}

#[cfg(test)]
mod tests {
    use bms_model::JudgeRank;

    use super::{GaugeValue, LowDamage, RankScale, Window};
    use crate::custom;

    #[test]
    fn rank_scale_floors_each_edge() {
        let preset = custom();
        let scale = |rank| {
            preset
                .rank_scale
                .scale_milli(Some(JudgeRank::Rank(rank)))
                .and_then(|s| preset.windows.scaled(s))
                .unwrap()
        };
        // #RANK 0 (50 %) and 3 (125 %) divide evenly.
        assert_eq!(scale(0).great, Window::symmetric(22_500));
        assert_eq!(scale(3).bad, Window::symmetric(225_000));
        assert_eq!(scale(2), preset.windows);
        // 33.333 %: 20 ms × 0.33333 = 6666.6 µs, floored to 6666.
        let third = preset.windows.scaled(33_333).unwrap();
        assert_eq!(third.perfect, Window::symmetric(6_666));
        assert_eq!(third.bad, Window::symmetric(59_999));
    }

    #[test]
    fn defexrank_is_a_percentage_and_bad_ranks_are_none() {
        let rank = RankScale {
            rank_milli: [50_000, 75_000, 100_000, 125_000],
            absent_milli: 100_000,
        };
        assert_eq!(
            rank.scale_milli(Some(JudgeRank::DefExRankMilli(80_000))),
            Some(80_000)
        );
        assert_eq!(rank.scale_milli(None), Some(100_000));
        assert_eq!(rank.scale_milli(Some(JudgeRank::Rank(4))), None);
        assert_eq!(rank.scale_milli(Some(JudgeRank::Rank(-1))), None);
        assert_eq!(rank.scale_milli(Some(JudgeRank::DefExRankMilli(0))), None);
        let w = Window::symmetric(i64::MAX);
        let windows = super::JudgeWindows {
            perfect: w,
            great: w,
            good: w,
            bad: w,
        };
        assert_eq!(windows.scaled(200_000), None);
        assert_eq!(windows.scaled(0), None);
    }

    #[test]
    fn low_damage_starts_strictly_below_threshold() {
        let low = LowDamage {
            below: GaugeValue(30 * GaugeValue::PERCENT),
            num: 1,
            den: 2,
        };
        assert!(!low.applies_at(GaugeValue(30 * GaugeValue::PERCENT)));
        assert!(low.applies_at(GaugeValue(30 * GaugeValue::PERCENT - 1)));
        assert_eq!(low.reduce(GaugeValue(-900_000)), GaugeValue(-450_000));
        assert_eq!(low.reduce(GaugeValue(-3)), GaugeValue(-1));
        assert_eq!(low.reduce(GaugeValue(16_000)), GaugeValue(16_000));
    }
}
