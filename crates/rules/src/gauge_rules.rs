//! Gauge constructors the standard presets share.

use crate::{
    GaugeDelta, GaugeKind, GaugeRule, GaugeValue, LowDamage, PerJudgment, TotalCorrection,
};

/// `n` hundredths of a percent as a fixed change.
pub(crate) const fn centi(n: i64) -> GaugeDelta {
    GaugeDelta::Fixed(GaugeValue::centi_percent(n))
}

/// `n` percent.
pub(crate) const fn pct(n: i64) -> GaugeValue {
    GaugeValue(n * GaugeValue::PERCENT)
}

/// A gauge decided at the end: start 20 %, min 2 %, cleared at `clear` %,
/// with the `TOTAL` modifier on its increases, each `(num, den)` of
/// `TOTAL / notes` as given (PERFECT and GREAT, then GOOD), and `damage`
/// in hundredths of a percent (BAD, POOR, empty POOR).
pub(crate) fn end_gauge(id: &str, clear: i64, up: [(u32, u32); 2], damage: [i64; 3]) -> GaugeRule {
    let [best, good] = up.map(|(num, den)| GaugeDelta::TotalShare { num, den });
    let [bad, poor, empty] = damage.map(|d| centi(-d));
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::EndThreshold { clear: pct(clear) },
        start: pct(20),
        min: pct(2),
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: best,
            great: best,
            good,
            bad,
            poor,
        },
        empty_poor: empty,
        correction: TotalCorrection::None,
        low_damage: Vec::new(),
    }
}

/// A survival gauge from 100 %, failing strictly below `fails_below` (and
/// at 0), with fixed changes in hundredths of a percent: PERFECT, GREAT,
/// GOOD, BAD, POOR, empty POOR.
pub(crate) fn survival_gauge(
    id: &str,
    fails_below: GaugeValue,
    change: [i64; 6],
    correction: TotalCorrection,
    low_damage: Vec<LowDamage>,
) -> GaugeRule {
    let [perfect, great, good, bad, poor, empty] = change.map(centi);
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::Survival { fails_below },
        start: GaugeValue::FULL,
        min: GaugeValue::EMPTY,
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect,
            great,
            good,
            bad,
            poor,
        },
        empty_poor: empty,
        correction,
        low_damage,
    }
}
