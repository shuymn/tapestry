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
    /// the scale is not positive or an edge leaves
    /// `0..=`[`MAX_WINDOW_EDGE`].
    #[must_use]
    pub fn scaled(&self, scale_milli: i64) -> Option<Self> {
        self.scaled_by(i128::from(scale_milli), i128::from(SCALE_FULL_MILLI))
    }

    /// Every edge scaled by `num / den` with [`scale_edge_by`].
    #[must_use]
    pub fn scaled_by(&self, num: i128, den: i128) -> Option<Self> {
        let window = |w: Window| -> Option<Window> {
            Some(Window {
                early: scale_edge_by(w.early, num, den)?,
                late: scale_edge_by(w.late, num, den)?,
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
/// [`RankScale::scale_milli`]): [`scale_edge_by`] with `scale_milli /
/// 100_000`.
#[must_use]
pub fn scale_edge(edge: TimeUs, scale_milli: i64) -> Option<TimeUs> {
    scale_edge_by(edge, i128::from(scale_milli), i128::from(SCALE_FULL_MILLI))
}

/// `edge` multiplied by `num / den`: `floor(edge × num / den)`, computed in
/// `i128`, so a fractional µs is dropped and a window never grows by
/// rounding. `None` when the factor is not positive, the product does not
/// fit `i128`, or the result lies outside `0..=`[`MAX_WINDOW_EDGE`].
#[must_use]
pub fn scale_edge_by(edge: TimeUs, num: i128, den: i128) -> Option<TimeUs> {
    if num <= 0 || den <= 0 {
        return None;
    }
    let v = i128::from(edge.0).checked_mul(num)?.div_euclid(den);
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
        self.scaled_by(i128::from(scale_milli), i128::from(SCALE_FULL_MILLI))
    }

    /// Every edge scaled by `num / den` with [`scale_edge_by`].
    #[must_use]
    pub fn scaled_by(&self, num: i128, den: i128) -> Option<Self> {
        Some(Self {
            perfect: scale_edge_by(self.perfect, num, den)?,
            great: scale_edge_by(self.great, num, den)?,
            good: scale_edge_by(self.good, num, den)?,
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
    /// press that takes a note first misses every earlier unjudged note of
    /// its lane, each a POOR at the press's time, in note order. Notes are
    /// never judged out of order.
    PoorAfterLateBadOrOvertaken,
}

/// Empty POOR: a press that judges no note but lies close to one.
///
/// A press that judges no note is an empty POOR when a note of its lane,
/// judged or not, lies inside its lane class's [`EmptyPoorRange`].
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
    /// (design.md "Gauges and Session Policy"): `increment × num / den`.
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
    /// Failed the moment a change, a mine's included, leaves it at 0 or
    /// strictly below `fails_below`; it is then 0 and never recovers. Its
    /// `min` is 0, and it starts above 0 and at or above `fails_below`.
    Survival {
        /// The value a change must not leave the gauge strictly below;
        /// `EMPTY` fails it only at 0.
        fails_below: GaugeValue,
    },
}

/// One band of reduced damage while the gauge is low.
///
/// When the value before a change is strictly below `below`, a negative
/// change is multiplied by `num / den`. A value exactly at `below` is
/// outside the band. Mines are not reduced (see [`MineRule`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowDamage {
    /// The value below which the band applies.
    pub below: GaugeValue,
    /// Numerator of the damage factor.
    pub num: u32,
    /// Denominator of the damage factor, not zero.
    pub den: u32,
}

impl LowDamage {
    /// Whether the band applies to a change from `before`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn applies_at(&self, before: GaugeValue) -> bool {
        before < self.below
    }
}

/// A factor `num / den` of a gauge change, `den` positive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Factor {
    /// Numerator, not negative.
    pub num: i128,
    /// Denominator, positive.
    pub den: i128,
}

impl Factor {
    /// The factor that changes nothing.
    pub const ONE: Self = Self { num: 1, den: 1 };

    /// `num / den` in lowest terms; `num` not negative and `den` positive.
    fn reduced(num: i128, den: i128) -> Self {
        let (mut a, mut b) = (num, den);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        let gcd = a.max(1);
        Self {
            num: num / gcd,
            den: den / gcd,
        }
    }
}

/// The factors a [`TotalCorrection`] gives one chart: one for increases and
/// one for damage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorrectionFactors {
    /// For a positive change.
    pub increase: Factor,
    /// For a negative change.
    pub damage: Factor,
}

/// How a gauge's changes are corrected by the chart's `#TOTAL` and note
/// count (design.md "Evaluation Stages", accounting step 2). It applies to
/// every judgment and empty POOR, never to a mine. `T` is the `#TOTAL` in
/// effect, read or the default, and `N` the gauge denominator's count of the
/// whole chart, also for a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TotalCorrection {
    /// No correction.
    None,
    /// An increase is multiplied by `max(0, min(0.15, (2T − 320) / N)) /
    /// 0.15`: nothing at `#TOTAL 160` and below, in full once `2T − 320 ≥
    /// 0.15 N`. With `T` in thousandths `t`, the factor is
    /// `clamp(2t − 320_000, 0, 150 N) / (150 N)`.
    LimitIncrease,
    /// Damage is multiplied by `max(fix1, fix2)`, where `fix1 = 10 /
    /// clamp(floor(T / 16) − 5, 1, 10)` (10 below `#TOTAL 112`, 1 from 240)
    /// and `fix2` falls with `N`: 10 up to 20 notes, then `8 + 0.2 (30 − N)`
    /// below 30, `5 + 0.2 (60 − N) / 3` below 60, `4 + (125 − N) / 65` below
    /// 125, `3 + 0.008 (250 − N)` below 250, `2 + 0.004 (500 − N)` below
    /// 500, `1 + 0.002 (1000 − N)` below 1000, and 1 from 1000.
    ScaleDamage,
}

impl TotalCorrection {
    /// The factors, in lowest terms, for a chart whose `#TOTAL` in effect is
    /// `total_milli` thousandths and whose gauge denominator counts `notes`
    /// (at least 1: a chart without notes is refused, and 0 counts as 1).
    /// Exact: nothing is rounded here.
    #[must_use]
    pub fn factors(&self, total_milli: i64, notes: usize) -> CorrectionFactors {
        let n = i128::try_from(notes.max(1)).unwrap_or(i128::MAX);
        let t = i128::from(total_milli);
        let (mut increase, mut damage) = (Factor::ONE, Factor::ONE);
        match self {
            Self::None => {}
            Self::LimitIncrease => {
                let den = n.saturating_mul(150);
                let num = t.saturating_mul(2).saturating_sub(320_000).clamp(0, den);
                increase = Factor::reduced(num, den);
            }
            Self::ScaleDamage => {
                let fix1 = (10, (t.div_euclid(16_000) - 5).clamp(1, 10));
                let fix2 = match n {
                    ..=20 => (10, 1),
                    21..=29 => (70 - n, 5),
                    30..=59 => (135 - n, 15),
                    60..=124 => (385 - n, 65),
                    125..=249 => (625 - n, 125),
                    250..=499 => (1_000 - n, 250),
                    500..=999 => (1_500 - n, 500),
                    _ => (1, 1),
                };
                // The larger, compared exactly; both are small fractions.
                let (num, den) = if fix1.0 * fix2.1 >= fix2.0 * fix1.1 {
                    fix1
                } else {
                    fix2
                };
                damage = Factor::reduced(num, den);
            }
        }
        CorrectionFactors { increase, damage }
    }
}

/// One gauge. Every gauge of a preset is computed side by side.
///
/// A change is computed from its base, corrected per `correction`, reduced
/// per `low_damage`, rounded once, added, clamped to `min..=max`, and a
/// survival gauge's failure checked, in that order (design.md "Evaluation
/// Stages", accounting).
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
    /// The `#TOTAL` correction of every change but a mine's. A gauge with a
    /// correction has fixed changes only.
    pub correction: TotalCorrection,
    /// Reduced damage while low: bands in strictly ascending order of
    /// `below`, of which the first that applies to the value before the
    /// change reduces it. Empty for none. Applies to every negative change
    /// except mines.
    pub low_damage: Vec<LowDamage>,
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

/// Which note a press takes, the primary of press resolution (design.md
/// "Evaluation Stages"). Only unjudged notes inside the lane class's BAD
/// window are eligible, and the notes before the one taken are passed
/// ([`MissRule`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assignment {
    /// The nearest unjudged note inside the BAD window; at equal distance,
    /// the earlier note. A late press therefore moves on to the next note
    /// once it is past the midpoint between the two.
    NearestTiesEarlier,
    /// Combo priority: the eligible notes are taken in time order, the first
    /// one first, and a later one replaces the note taken when the press is
    /// strictly past the taken note's late GOOD edge and not before the
    /// later note's early GOOD edge. With `d = press − note` and the lane
    /// class's windows, the note taken `c` gives way to a later `n` when
    /// `d(c) > good.late` and `d(n) ≥ −good.early`. A later note is taken
    /// only when the earlier one can no longer be hit GOOD or better.
    ComboPriority,
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
    /// It sets the per-note increment ([`GaugeDelta::TotalShare`]); each
    /// gauge's [`TotalCorrection`] reads it too.
    ScalesIncrease,
}

/// `#TOTAL` when the header policy reads none, from the gauge denominator's
/// count `N` of the whole chart, also for a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultTotal {
    /// A fixed value.
    Fixed {
        /// The value, in thousandths, positive.
        milli: i64,
    },
    /// `max(260, 7.605 N / (0.01 N + 6.5))`, in thousandths rounded down:
    /// 260 up to 337 notes, then rising toward 760.5.
    NotesCurve,
    /// `160 + (N + clamp(N − 400, 0, 200)) × 0.16`: 0.16 per note, twice
    /// that for the 401st to the 600th.
    NotesLinear,
}

impl DefaultTotal {
    /// The value for a chart of `notes`, in thousandths, computed in
    /// `i128` and saturated to `i64`.
    #[must_use]
    pub fn milli(&self, notes: usize) -> i64 {
        let n = i128::try_from(notes).unwrap_or(i128::MAX);
        let v = match *self {
            Self::Fixed { milli } => i128::from(milli),
            // 1000 × 7.605 N / (0.01 N + 6.5) = 7_605_000 N / (10 N + 6_500).
            Self::NotesCurve => n
                .saturating_mul(7_605_000)
                .div_euclid(n.saturating_mul(10).saturating_add(6_500))
                .max(260_000),
            Self::NotesLinear => n
                .saturating_add((n - 400).clamp(0, 200))
                .saturating_mul(160)
                .saturating_add(160_000),
        };
        i64::try_from(v).unwrap_or(i64::MAX)
    }
}

/// When and how much a mine damages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MineRule {
    /// A mine damages every gauge when its lane is held as it passes, by the
    /// chart's damage value × `per_unit`, as is: never corrected by a
    /// [`TotalCorrection`] or reduced by [`LowDamage`], then clamped like any
    /// change, and a survival gauge it leaves too low fails. It has no
    /// judgment, combo or BP effect.
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
    /// `#TOTAL` when the header policy reads none.
    pub default_total: DefaultTotal,
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

    use super::{
        CorrectionFactors, DefaultTotal, Factor, GaugeValue, LowDamage, RankScale, TotalCorrection,
        Window,
    };
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
        assert_eq!(ln_end.scaled(0), None);
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
        assert_eq!(windows.scaled(0), None);
        // 0.75 % of 20 ms, as a fraction: 150 µs.
        assert_eq!(
            super::scale_edge_by(TimeUs(20_000), 75, 10_000),
            Some(TimeUs(150))
        );
        assert_eq!(super::scale_edge_by(TimeUs(1 << 40), i128::MAX, 1), None);
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
    }

    /// `num / den` in lowest terms, as the factors are given.
    fn f(num: i128, den: i128) -> Factor {
        Factor::reduced(num, den)
    }

    #[test]
    fn factors_are_in_lowest_terms() {
        assert_eq!(f(80_000, 150_000), Factor { num: 8, den: 15 });
        assert_eq!(f(0, 150_000), Factor { num: 0, den: 1 });
        assert_eq!(f(10, 10), Factor::ONE);
    }

    /// Both formulas at the edges of their pieces, against values worked out
    /// by hand: `NotesCurve` reaches 260 between 337 and 338 notes
    /// (`7605 × 338 / 9880 = 260.171…`), `NotesLinear` doubles its step
    /// from the 401st note to the 600th. 0 notes is never evaluated (such a
    /// chart is refused) but stays defined.
    #[test]
    fn default_totals_at_their_edges() {
        let curve = |n| DefaultTotal::NotesCurve.milli(n);
        let linear = |n| DefaultTotal::NotesLinear.milli(n);
        for (n, c, l) in [
            (0, 260_000, 160_000),
            (1, 260_000, 160_160),
            (337, 260_000, 213_920),
            (338, 260_171, 214_080),
            (399, 289_265, 223_840),
            (400, 289_714, 224_000),
            (401, 290_162, 224_320),
            (599, 364_723, 287_680),
            (600, 365_040, 288_000),
            (601, 365_356, 288_160),
            // 460.909 09…, rounded down to thousandths.
            (1_000, 460_909, 352_000),
        ] {
            assert_eq!((curve(n), linear(n)), (c, l), "{n} notes");
        }
        assert_eq!(DefaultTotal::Fixed { milli: 160_000 }.milli(5), 160_000);
        // Toward 760.5, without overflow.
        assert_eq!(curve(usize::MAX), 760_499);
        assert_eq!(linear(usize::MAX), i64::MAX);
    }

    /// `LimitIncrease` at `#TOTAL` below, at and above 160, at 320 around
    /// `0.15 N`; `ScaleDamage`'s `fix1` at each `#TOTAL` step it changes on
    /// and `fix2` on both sides of each piece, worked out by hand.
    #[test]
    fn total_corrections_at_their_edges() {
        let limit = |t, n| TotalCorrection::LimitIncrease.factors(t, n);
        let one = CorrectionFactors {
            increase: Factor::ONE,
            damage: Factor::ONE,
        };
        assert_eq!(TotalCorrection::None.factors(1, 1), one);
        for (t, n, increase) in [
            (159_999, 1_000, f(0, 150_000)),
            (160_000, 1_000, f(0, 150_000)),
            (160_001, 1_000, f(2, 150_000)),
            // (400 − 320) / 1000 = 0.08 of 0.15.
            (200_000, 1_000, f(80_000, 150_000)),
            // 320 / 2133 ≥ 0.15, 320 / 2134 < 0.15.
            (320_000, 2_133, f(319_950, 319_950)),
            (320_000, 2_134, f(320_000, 320_100)),
            // 0 notes counts as 1.
            (320_000, 0, f(150, 150)),
        ] {
            assert_eq!(limit(t, n).increase, increase, "#TOTAL {t} over {n}");
            assert_eq!(limit(t, n).damage, Factor::ONE);
        }
        let damage = |t, n| TotalCorrection::ScaleDamage.factors(t, n).damage;
        assert_eq!(
            TotalCorrection::ScaleDamage.factors(1, 1).increase,
            Factor::ONE
        );
        // fix1 alone, from 1000 notes (fix2 = 1).
        for (t, fix1) in [
            (1, f(10, 1)),
            (111_999, f(10, 1)),
            (112_000, f(10, 2)),
            (223_999, f(10, 8)),
            (224_000, f(10, 9)),
            (239_999, f(10, 9)),
            (240_000, f(10, 10)),
            (i64::MAX, f(10, 10)),
        ] {
            assert_eq!(damage(t, 1_000), fix1, "#TOTAL {t}");
        }
        // fix2 alone, at #TOTAL 240 (fix1 = 1).
        for (n, fix2) in [
            (0, f(10, 1)),
            (1, f(10, 1)),
            (20, f(10, 1)),
            (21, f(49, 5)),
            (29, f(41, 5)),
            (30, f(105, 15)),
            (59, f(76, 15)),
            (60, f(325, 65)),
            (124, f(261, 65)),
            (125, f(500, 125)),
            (249, f(376, 125)),
            (250, f(750, 250)),
            (399, f(601, 250)),
            (400, f(600, 250)),
            (401, f(599, 250)),
            (499, f(501, 250)),
            (500, f(1_000, 500)),
            (599, f(901, 500)),
            (600, f(900, 500)),
            (601, f(899, 500)),
            (999, f(501, 500)),
            (1_000, Factor::ONE),
            (usize::MAX, Factor::ONE),
        ] {
            assert_eq!(damage(240_000, n), fix2, "{n} notes");
        }
        // The larger of the two: 10/9 over 1.002, 2.4 over 10/9; a tie (5
        // and 5 at 60 notes and #TOTAL 112) is the same factor either way.
        assert_eq!(damage(224_000, 999), f(10, 9));
        assert_eq!(damage(224_000, 400), f(600, 250));
        assert_eq!(damage(112_000, 60), f(10, 2));
    }
}
