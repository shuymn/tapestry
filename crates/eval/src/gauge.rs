//! Gauges in fixed point: every gauge of the preset is computed side by
//! side, each change computed in `i128` in the order of design.md
//! "Evaluation Stages" (accounting), then narrowed.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use rules::{
    CorrectionFactors, Factor, GaugeDelta, GaugeKind, GaugeRule, GaugeValue, Judgment, MineRule,
};

/// Whether `rule`'s gauge can start a section at `v` (design.md "Section
/// Evaluation"): within its bounds, and for a survival gauge above 0 and at
/// or above its failure threshold, as a preset's own start must be.
/// [`Evaluator::new`](crate::Evaluator::new) refuses a section with a
/// value its gauge does not accept.
#[must_use]
pub fn accepts_initial(rule: &GaugeRule, v: GaugeValue) -> bool {
    (rule.min..=rule.max).contains(&v)
        && match rule.kind {
            GaugeKind::EndThreshold { .. } => true,
            GaugeKind::Survival { fails_below } => v > GaugeValue::EMPTY && v >= fails_below,
        }
}

/// What changes the gauges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A note was judged.
    Judged(Judgment),
    /// An empty POOR.
    EmptyPoor,
    /// A mine passed a held lane; the chart's damage value.
    Mine(i32),
}

/// One gauge's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeState {
    /// The real value. A survival gauge stays at 0 once it fails.
    pub value: GaugeValue,
    /// The value had the gauge never failed; equal to `value` for a gauge
    /// that cannot fail. It is a reference only, never used for a clear.
    pub reference: GaugeValue,
    /// When a survival gauge failed, if it did.
    pub failed_at: Option<TimeUs>,
}

/// The per-note increment of design.md "Gauges and Session Policy":
/// `floor(total_milli × 100 / notes)`, in `i128` so an extreme `#TOTAL` cannot
/// overflow. `notes` is not zero.
pub(crate) fn increment(total_milli: i64, notes: usize) -> i128 {
    let notes = i128::try_from(notes).unwrap_or(i128::MAX);
    (i128::from(total_milli) * 100).div_euclid(notes.max(1))
}

/// `(num, den)` multiplied by `f`, saturating: a validated preset's factors
/// keep both far inside `i128`.
fn times((num, den): (i128, i128), f: Factor) -> (i128, i128) {
    (num.saturating_mul(f.num), den.saturating_mul(f.den))
}

/// The change `cause` makes to a gauge at `before`, steps 1 to 5 of
/// design.md "Evaluation Stages" accounting: the base change, the `#TOTAL`
/// correction, the low-gauge reduction chosen from `before`, then one
/// signed floor. A mine's change is its damage as it is.
fn change(
    rule: &GaugeRule,
    mine: &MineRule,
    cause: Cause,
    increment: i128,
    correction: CorrectionFactors,
    before: GaugeValue,
) -> i128 {
    // 1. The base change, as an exact fraction.
    let base = match cause {
        Cause::Judged(j) => *rule.change.get(j),
        Cause::EmptyPoor => rule.empty_poor,
        Cause::Mine(damage) => return i128::from(mine.change(damage).0),
    };
    let mut d = match base {
        GaugeDelta::Fixed(v) => (i128::from(v.0), 1),
        GaugeDelta::TotalShare { num, den } => {
            (increment.saturating_mul(i128::from(num)), i128::from(den))
        }
    };
    // 2. The #TOTAL correction, by the sign of the change.
    if d.0 > 0 {
        d = times(d, correction.increase);
    } else if d.0 < 0 {
        d = times(d, correction.damage);
    }
    // 4. The low-gauge reduction, the first band that applies to `before`.
    if d.0 < 0
        && let Some(band) = rule.low_damage.iter().find(|b| b.applies_at(before))
    {
        d = times(
            d,
            Factor {
                num: i128::from(band.num),
                den: i128::from(band.den),
            },
        );
    }
    // 5. One signed floor; a validated preset's denominators are positive.
    d.0.div_euclid(d.1.max(1))
}

/// `before` changed by `delta`, then clamped to the rule's bounds, then
/// narrowed.
fn step(rule: &GaugeRule, before: GaugeValue, delta: i128) -> GaugeValue {
    let next = i128::from(before.0)
        .saturating_add(delta)
        .clamp(i128::from(rule.min.0), i128::from(rule.max.0));
    // Clamped to bounds that are i64 values.
    GaugeValue(i64::try_from(next).unwrap_or(rule.max.0))
}

/// Every gauge of a play.
#[derive(Clone, Debug)]
pub(crate) struct Gauges {
    states: Vec<GaugeState>,
    /// Each gauge's `#TOTAL` correction for this chart.
    corrections: Vec<CorrectionFactors>,
    increment: i128,
}

impl Gauges {
    /// Each gauge at `initial`'s value, else at its rule's start;
    /// `initial`, when given, holds one value per rule. `total_milli` is the
    /// `#TOTAL` in effect and `notes` the gauge denominator's count, which
    /// set the increment and each gauge's correction.
    pub(crate) fn new(
        rules: &[GaugeRule],
        total_milli: i64,
        notes: usize,
        initial: Option<&[GaugeValue]>,
    ) -> Self {
        Self {
            states: rules
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let start = initial.and_then(|v| v.get(i)).copied().unwrap_or(r.start);
                    GaugeState {
                        value: start,
                        reference: start,
                        failed_at: None,
                    }
                })
                .collect(),
            corrections: rules
                .iter()
                .map(|r| r.correction.factors(total_milli, notes))
                .collect(),
            increment: increment(total_milli, notes),
        }
    }

    /// Whether `initial` can start `rules`' gauges: one value per rule,
    /// each accepted by its rule ([`accepts_initial`]).
    pub(crate) fn accepts(rules: &[GaugeRule], initial: &[GaugeValue]) -> bool {
        rules.len() == initial.len()
            && rules
                .iter()
                .zip(initial)
                .all(|(r, &v)| accepts_initial(r, v))
    }

    pub(crate) fn states(&self) -> &[GaugeState] {
        &self.states
    }

    /// Applies `cause` at `time` to every gauge: each change is computed
    /// from the value before it, added and clamped (steps 6 and 7), and a
    /// survival gauge it leaves at 0 or strictly below its threshold fails
    /// at 0 (step 8).
    pub(crate) fn apply(
        &mut self,
        rules: &[GaugeRule],
        mine: &MineRule,
        cause: Cause,
        time: TimeUs,
    ) {
        let gauges = rules.iter().zip(&mut self.states).zip(&self.corrections);
        for ((rule, state), &correction) in gauges {
            let d = change(
                rule,
                mine,
                cause,
                self.increment,
                correction,
                state.reference,
            );
            state.reference = step(rule, state.reference, d);
            // Until a failure the real value equals the reference; after
            // one it stays at 0.
            if state.failed_at.is_some() {
                continue;
            }
            state.value = state.reference;
            if let GaugeKind::Survival { fails_below } = rule.kind
                && (state.value <= GaugeValue::EMPTY || state.value < fails_below)
            {
                state.value = GaugeValue::EMPTY;
                state.failed_at = Some(time);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::increment;

    #[test]
    fn increment_floors_and_survives_extreme_totals() {
        // #TOTAL 160 with one note, and with as many notes as fit in usize.
        assert_eq!(increment(160_000, 1), 16_000_000);
        assert_eq!(increment(160_000, 10_000), 1_600);
        assert_eq!(increment(160_000, usize::MAX), 0);
        // 100.001 over 3 notes: 10_000_100 / 3 = 3_333_366 rem 2.
        assert_eq!(increment(100_001, 3), 3_333_366);
        // Beyond i64 before clamping.
        assert_eq!(increment(i64::MAX, 1), i128::from(i64::MAX) * 100);
    }
}
