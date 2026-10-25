//! The custom preset: this player's own judgment. Every value is a starting
//! point tuned by playing.

use bms_model::TimeUs;

use crate::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, LateHeadRule, LnBreakRule, LnMode, LnModeRule, LowDamage,
    MineRule, MissRule, PerJudgment, RankScale, RulePreset, SameTimeOrder, ScoreWeights, TotalMode,
    Window,
};

const MS: i64 = 1_000;

const fn pct(n: i64) -> GaugeValue {
    GaugeValue(n * GaugeValue::PERCENT)
}

const fn fixed(v: GaugeValue) -> GaugeDelta {
    GaugeDelta::Fixed(v)
}

const fn damage(v: GaugeValue) -> GaugeDelta {
    GaugeDelta::Fixed(GaugeValue(-v.0))
}

/// The custom preset, the default.
#[must_use]
pub fn custom() -> RulePreset {
    let full_share = GaugeDelta::TotalShare { num: 1, den: 1 };
    let normal = GaugeRule {
        id: "normal".to_owned(),
        kind: GaugeKind::EndThreshold { clear: pct(80) },
        start: pct(20),
        min: pct(2),
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: full_share,
            great: full_share,
            good: GaugeDelta::TotalShare { num: 1, den: 2 },
            bad: damage(pct(2)),
            poor: damage(pct(6)),
        },
        empty_poor: damage(pct(3)),
        ln_break: damage(pct(6)),
        low_damage: None,
    };
    let survival = GaugeRule {
        id: "survival".to_owned(),
        kind: GaugeKind::Survival,
        start: GaugeValue::FULL,
        min: GaugeValue::EMPTY,
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: fixed(GaugeValue::centi_percent(16)),
            great: fixed(GaugeValue::centi_percent(16)),
            good: fixed(GaugeValue::EMPTY),
            bad: damage(pct(5)),
            poor: damage(pct(9)),
        },
        empty_poor: damage(GaugeValue::centi_percent(450)),
        ln_break: damage(pct(9)),
        low_damage: Some(LowDamage {
            below: pct(30),
            num: 1,
            den: 2,
        }),
    };
    RulePreset {
        id: "custom".to_owned(),
        version: 1,
        implementation: Implementation::Standard { version: 1 },
        windows: JudgeWindows {
            perfect: Window::symmetric(20 * MS),
            great: Window::symmetric(45 * MS),
            good: Window::symmetric(100 * MS),
            bad: Window::symmetric(180 * MS),
        },
        miss: MissRule::PoorAfterLateBad,
        empty_poor: EmptyPoorRule {
            ahead: TimeUs(300 * MS),
            breaks_combo: false,
            counts_bp: false,
            counted: false,
        },
        ln_break: LnBreakRule {
            release_margin: TimeUs(100 * MS),
            breaks_combo: true,
            counts_bp: true,
        },
        rank_scale: RankScale {
            rank_milli: [50_000, 75_000, 100_000, 125_000],
            absent_milli: 100_000,
        },
        default_total_milli: 160_000,
        denominator: Denominator::TapsAndLnHeads,
        gauges: vec![normal, survival],
        score: ScoreWeights {
            ex: PerJudgment {
                perfect: 2,
                great: 1,
                good: 0,
                bad: 0,
                poor: 0,
            },
            breaks_combo: PerJudgment {
                perfect: false,
                great: false,
                good: false,
                bad: true,
                poor: true,
            },
            counts_bp: PerJudgment {
                perfect: false,
                great: false,
                good: false,
                bad: true,
                poor: true,
            },
        },
        same_time_order: SameTimeOrder::InputsFirst,
        assignment: Assignment::NearestTiesEarlier,
        ln_mode: LnModeRule {
            default: LnMode::Ln,
            late_head: LateHeadRule::CompletesAtPress,
        },
        total_mode: TotalMode::ScalesIncrease,
        mine: MineRule::WhileHeld {
            per_unit: GaugeValue(GaugeValue::PERCENT),
        },
    }
}
