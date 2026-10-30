//! The rule preset types.
//!
//! Each choice enum has only the variants a preset in use needs; a later
//! preset adds the variant it needs.

use bms_model::{DOMAIN, TimeUs};

use crate::{HeaderPolicy, JudgeRank, LaneTable, PerLaneClass, WindowPolicy};

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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
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
    /// The version of the standard evaluator this build runs. A preset or
    /// record naming another version is refused, never evaluated.
    pub const STANDARD_VERSION: u32 = 2;

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

    /// Whether this build runs it: the standard evaluator at
    /// [`STANDARD_VERSION`](Self::STANDARD_VERSION).
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        match self {
            Self::Standard { version } => *version == Self::STANDARD_VERSION,
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

/// The judgment windows of one lane class ([`LaneTable`]).
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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn table(&self) -> [(Judgment, Window); 4] {
        [
            (Judgment::Perfect, self.perfect),
            (Judgment::Great, self.great),
            (Judgment::Good, self.good),
            (Judgment::Bad, self.bad),
        ]
    }

    /// Every edge scaled by `scale_milli` with [`scale_edge`]. `None` when
    /// the scale is negative or an edge leaves `0..=`[`MAX_WINDOW_EDGE`].
    #[must_use]
    pub fn scaled(&self, scale_milli: i64) -> Option<Self> {
        let window = |w: Window| -> Option<Window> {
            Some(Window {
                early: scale_edge(w.early, scale_milli)?,
                late: scale_edge(w.late, scale_milli)?,
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

/// The widest a window or LN-end edge may be, before and after the rank
/// scale: the chart time domain, `2^32` µs. With note times and LN ends
/// inside `±2^32` µs, a press deviation and a release's distance from its
/// long note's end fit in `i64`.
pub const MAX_WINDOW_EDGE: TimeUs = TimeUs(DOMAIN);

/// `edge` multiplied by `scale_milli` (thousandths of a percent, see
/// [`RankScale::scale_milli`]): `floor(edge × scale_milli / 100_000)`,
/// computed in `i128`, so a fractional µs is dropped and a window never
/// grows by rounding. A scale of 0 gives a zero-width edge. `None` when the
/// scale is negative or the result lies outside `0..=`[`MAX_WINDOW_EDGE`].
#[must_use]
pub fn scale_edge(edge: TimeUs, scale_milli: i64) -> Option<TimeUs> {
    if scale_milli < 0 {
        return None;
    }
    let v = (i128::from(edge.0) * i128::from(scale_milli)).div_euclid(i128::from(SCALE_FULL_MILLI));
    let v = i64::try_from(v).ok()?;
    (0..=MAX_WINDOW_EDGE.0).contains(&v).then_some(TimeUs(v))
}

/// The early edges of a long note's end, as distances before the end.
///
/// A release at `d = release − end`, before the end, gets the best judgment
/// whose edge holds `−edge ≤ d` (inclusive, like a note window), else BAD.
/// The long note's judgment is the worse of its head's and its release's,
/// so a release at or after the end leaves the head's judgment. LN mode
/// reads no late side, and a BAD edge would decide nothing, so neither is
/// held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LnEndWindows {
    /// PERFECT.
    pub perfect: TimeUs,
    /// GREAT.
    pub great: TimeUs,
    /// GOOD.
    pub good: TimeUs,
}

impl LnEndWindows {
    /// The edges best first, each with its judgment.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn table(&self) -> [(Judgment, TimeUs); 3] {
        [
            (Judgment::Perfect, self.perfect),
            (Judgment::Great, self.great),
            (Judgment::Good, self.good),
        ]
    }

    /// Every edge scaled by `scale_milli` with [`scale_edge`], the factor
    /// the note windows take. `None` as for [`JudgeWindows::scaled`].
    #[must_use]
    pub fn scaled(&self, scale_milli: i64) -> Option<Self> {
        Some(Self {
            perfect: scale_edge(self.perfect, scale_milli)?,
            great: scale_edge(self.great, scale_milli)?,
            good: scale_edge(self.good, scale_milli)?,
        })
    }

    /// The judgment of a release at `d = release − end`: the best whose
    /// edge holds `−edge ≤ d`, else BAD. Compared in `i128`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn judgment_of(&self, d: TimeUs) -> Judgment {
        self.table()
            .into_iter()
            .find(|&(_, edge)| -i128::from(edge.0) <= i128::from(d.0))
            .map_or(Judgment::Bad, |(j, _)| j)
    }
}

/// When an unpressed note is missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissRule {
    /// A note not pressed by its late BAD edge is a POOR at that edge, and a
    /// press that judges a note first misses every earlier unjudged note of
    /// its lane, each a POOR at the press's time, in note order. Notes are
    /// never judged out of order.
    PoorAfterLateBadOrOvertaken,
}

/// Empty POOR: a press that judges no note but lies close to one.
///
/// A press that judges no note is an empty POOR when a note of its lane,
/// judged or not, lies inside its lane class's [`EmptyPoorRange`] and the
/// scan envelope the [`WindowPolicy`] prepares.
///
/// Every such press counts, with no cap. Its gauge change is
/// [`GaugeRule::empty_poor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmptyPoorRule {
    /// Whether it breaks the combo.
    pub breaks_combo: bool,
    /// Whether it adds to BP.
    pub counts_bp: bool,
    /// Whether it is counted in the judgment counts.
    pub counted: bool,
}

/// How `#RANK` and `#DEFEXRANK` scale every window and LN-end edge under
/// [`WindowPolicy::Thousandths`], in thousandths of a percent (`100_000` =
/// unchanged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RankScale {
    /// The scale for `#RANK 0` to `#RANK 3`.
    pub rank_milli: [i64; 4],
    /// The scale when the chart declares no rank, or a `#RANK` outside 0-3
    /// (an extension this preset does not support).
    pub absent_milli: i64,
}

impl RankScale {
    /// The window scale for the rank the header policy read.
    ///
    /// `#DEFEXRANK n` scales by `n` %, which [`JudgeRank`] already holds in
    /// thousandths of a percent. No rank and a `#RANK` outside 0-3 use
    /// `absent_milli`. `None` for a `#DEFEXRANK` that is not positive.
    #[must_use]
    pub fn scale_milli(&self, rank: Option<JudgeRank>) -> Option<i64> {
        match rank {
            #[expect(clippy::cast_sign_loss, reason = "the pattern bounds `r` to 0-3")]
            Some(JudgeRank::Rank(r @ 0..=3)) => Some(self.rank_milli[r as usize]),
            Some(JudgeRank::DefExRankMilli(v)) => (v > 0).then_some(v),
            None | Some(JudgeRank::Rank(_)) => Some(self.absent_milli),
        }
    }
}

/// Which notes make up the gauge denominator (the `notes` of `TOTAL/notes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denominator {
    /// Taps and long notes, one judgment each; mines are excluded.
    TapsAndLnHeads,
}

/// A gauge's change for one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeDelta {
    /// A fixed amount (negative for damage).
    Fixed(GaugeValue),
    /// A share of the per-note increment `floor(total_milli × 100 / notes)`
    /// (design.md "Gauges and Session Policy"): `floor(increment × num / den)`.
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
    /// Failed the moment it reaches 0, and never recovers. Its `min` is 0,
    /// and it starts above 0.
    Survival,
}

/// Reduced damage while the gauge is low.
///
/// When the value before a change is strictly below `below`, a negative
/// change `d` becomes `floor(d × num / den)` (a signed floor, so a remainder
/// adds damage). A value exactly at `below` takes full damage. Mines are not
/// reduced (see [`MineRule`]).
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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn applies_at(&self, before: GaugeValue) -> bool {
        before < self.below
    }

    /// The reduced form of `damage` (a negative change); other changes are
    /// returned as they are.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn reduce(&self, damage: GaugeValue) -> GaugeValue {
        if damage.0 >= 0 {
            return damage;
        }
        let reduced =
            (i128::from(damage.0) * i128::from(self.num)).div_euclid(i128::from(self.den));
        // A validated preset has num ≤ den, so this stays within `damage`;
        // saturate otherwise.
        GaugeValue(i64::try_from(reduced).unwrap_or(i64::MIN))
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
    /// Lower clamp bound; must be `EMPTY` for survival gauges, which fail at
    /// zero.
    pub min: GaugeValue,
    /// Upper bound.
    pub max: GaugeValue,
    /// Change per judgment.
    pub change: PerJudgment<GaugeDelta>,
    /// Change for an empty POOR.
    pub empty_poor: GaugeDelta,
    /// Reduced damage while low, if any. Applies to every negative change
    /// except mines.
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
    /// The nearest unjudged note inside the BAD window; at equal distance,
    /// the earlier note. A late press therefore moves on to the next note
    /// once it is past the midpoint between the two.
    NearestTiesEarlier,
}

/// How long notes are judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LnMode {
    /// One judgment per long note: the worse of its head's (by the note
    /// windows) and its release's (by [`LnEndWindows`]). A head graded BAD
    /// decides the long note at the press, and a missed head is one POOR.
    Ln,
}

/// What a head pressed after its LN's end does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LateHeadRule {
    /// A head pressed inside its late window at or after the LN's end
    /// decides the long note at the press with the head's judgment, and the
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
    /// chart's damage value × `per_unit`, as is: never reduced by
    /// [`LowDamage`], then clamped like any change. It has no judgment, combo
    /// or BP effect.
    WhileHeld {
        /// Gauge damage per unit of the chart's value (not negative).
        per_unit: GaugeValue,
    },
}

impl MineRule {
    /// The gauge change of a mine whose chart damage value is
    /// `chart_damage`, computed in `i128` and saturated to `i64`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn change(&self, chart_damage: i32) -> GaugeValue {
        let Self::WhileHeld { per_unit } = *self;
        let v = -(i128::from(chart_damage) * i128::from(per_unit.0));
        GaugeValue(i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX }))
    }
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
    /// Each lane class's note windows, LN-end edges and empty POOR range at
    /// scale 100 %. Every lane class states its own table.
    pub tables: PerLaneClass<LaneTable>,
    /// When an unpressed note is missed.
    pub miss: MissRule,
    /// What an empty POOR does.
    pub empty_poor: EmptyPoorRule,
    /// How the chart's judgment headers are read.
    pub headers: HeaderPolicy,
    /// How the tables derive from the rank the header policy read.
    pub window_policy: WindowPolicy,
    /// `#TOTAL` when the header policy reads none, in thousandths.
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
    use bms_model::TimeUs;

    use super::{GaugeValue, LowDamage, RankScale, Window};
    use crate::JudgeRank;
    use crate::custom;

    #[test]
    fn rank_scale_floors_each_edge() {
        let preset = custom();
        let scale = |rank| {
            preset
                .window_policy
                .prepare(&preset.tables, Some(JudgeRank::Rank(rank)))
                .unwrap()
                .keys
                .table
                .windows
        };
        let windows = preset.tables.keys.windows;
        // #RANK 0 (50 %) and 3 (125 %) divide evenly.
        assert_eq!(scale(0).great, Window::symmetric(22_500));
        assert_eq!(scale(3).bad, Window::symmetric(225_000));
        assert_eq!(scale(2), windows);
        // 33.333 %: 20 ms × 0.33333 = 6666.6 µs, floored to 6666.
        let third = windows.scaled(33_333).unwrap();
        assert_eq!(third.perfect, Window::symmetric(6_666));
        assert_eq!(third.bad, Window::symmetric(59_999));
    }

    #[test]
    fn ln_end_edges_scale_like_note_windows_and_grade_inclusively() {
        let ln_end = custom().tables.keys.ln_end;
        // 100, 150, 200 ms at 33.333 %: 33_333, 49_999.5 and 66_666 µs,
        // floored.
        let third = ln_end.scaled(33_333).unwrap();
        assert_eq!(
            (third.perfect, third.great, third.good),
            (TimeUs(33_333), TimeUs(49_999), TimeUs(66_666))
        );
        assert_eq!(ln_end.scaled(100_000), Some(ln_end));
        // A scale of 0 gives zero-width edges; a negative one is unusable.
        let zero = ln_end.scaled(0).unwrap();
        assert_eq!(
            (zero.perfect, zero.great, zero.good),
            (TimeUs(0), TimeUs(0), TimeUs(0))
        );
        assert_eq!(ln_end.scaled(-1), None);
        // A 1 µs edge at 50 % floors to 0.
        let tiny = super::LnEndWindows {
            perfect: TimeUs(1),
            great: TimeUs(2),
            good: TimeUs(3),
        };
        let half = tiny.scaled(50_000).unwrap();
        assert_eq!(
            (half.perfect, half.great, half.good),
            (TimeUs(0), TimeUs(1), TimeUs(1))
        );
        use crate::Judgment::{Bad, Good, Great, Perfect};
        for (d, j) in [
            (0, Perfect),
            (-100_000, Perfect),
            (-100_001, Great),
            (-150_000, Great),
            (-150_001, Good),
            (-200_000, Good),
            (-200_001, Bad),
            (i64::MIN, Bad),
        ] {
            assert_eq!(ln_end.judgment_of(TimeUs(d)), j, "release at {d}");
        }
    }

    #[test]
    fn defexrank_is_a_percentage_and_unknown_ranks_use_absent() {
        let rank = RankScale {
            rank_milli: [50_000, 75_000, 100_000, 125_000],
            absent_milli: 100_000,
        };
        assert_eq!(
            rank.scale_milli(Some(JudgeRank::DefExRankMilli(80_000))),
            Some(80_000)
        );
        assert_eq!(rank.scale_milli(None), Some(100_000));
        assert_eq!(rank.scale_milli(Some(JudgeRank::Rank(4))), Some(100_000));
        assert_eq!(rank.scale_milli(Some(JudgeRank::Rank(-1))), Some(100_000));
        assert_eq!(rank.scale_milli(Some(JudgeRank::DefExRankMilli(0))), None);
        let w = Window::symmetric(i64::MAX);
        let windows = super::JudgeWindows {
            perfect: w,
            great: w,
            good: w,
            bad: w,
        };
        assert_eq!(windows.scaled(200_000), None);
        assert_eq!(windows.scaled(-1), None);
        assert_eq!(windows.scaled(0).map(|w| w.bad), Some(Window::symmetric(0)));
    }

    #[test]
    fn mine_damage_is_half_a_percent_per_unit() {
        let mine = custom().mine;
        // `1E` in base 36 is 1 × 36 + 14 = 50 units; 50 × 0.5 % = 25 %, so a
        // survival gauge at 40 % keeps 15 %.
        assert_eq!(mine.change(50), GaugeValue(-2_500_000));
        assert_eq!(mine.change(1), GaugeValue(-50_000));
        assert_eq!(mine.change(0), GaugeValue(0));
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
        // Signed floor: -1.5 becomes -2.
        assert_eq!(low.reduce(GaugeValue(-3)), GaugeValue(-2));
        assert_eq!(low.reduce(GaugeValue(16_000)), GaugeValue(16_000));
    }
}
