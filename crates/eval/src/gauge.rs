//! Gauges in fixed point: every gauge of the preset is computed side by
//! side, each change computed in `i128`, reduced, clamped, then narrowed.

use bms_model::TimeUs;
use rules::{GaugeDelta, GaugeKind, GaugeRule, GaugeValue, Judgment, MineRule};

/// What changes the gauges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    /// A note was judged.
    Judged(Judgment),
    /// An empty POOR.
    EmptyPoor,
    /// An LN break.
    LnBreak,
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
    /// When a survival gauge reached 0, if it did.
    pub failed_at: Option<TimeUs>,
}

/// The per-note increment of D5: `floor(total_milli × 100 / notes)`, in
/// `i128` so an extreme `#TOTAL` cannot overflow. `notes` is not zero.
pub(crate) fn increment(total_milli: i64, notes: usize) -> i128 {
    let notes = i128::try_from(notes).unwrap_or(i128::MAX);
    (i128::from(total_milli) * 100).div_euclid(notes.max(1))
}

/// The change a gauge rule gives for `cause`, before any reduction.
fn delta(rule: &GaugeRule, mine: &MineRule, cause: Cause, increment: i128) -> i128 {
    let d = match cause {
        Cause::Judged(j) => *rule.change.get(j),
        Cause::EmptyPoor => rule.empty_poor,
        Cause::LnBreak => rule.ln_break,
        Cause::Mine(damage) => return i128::from(mine.change(damage).0),
    };
    match d {
        GaugeDelta::Fixed(v) => i128::from(v.0),
        GaugeDelta::TotalShare { num, den } => {
            (increment * i128::from(num)).div_euclid(i128::from(den).max(1))
        }
    }
}

/// `before` changed by `delta`: reduced while low (except for mines), then
/// clamped to the rule's bounds, then narrowed.
fn step(rule: &GaugeRule, before: GaugeValue, delta: i128, cause: Cause) -> GaugeValue {
    let mut delta = delta;
    if let Some(low) = rule.low_damage
        && delta < 0
        && !matches!(cause, Cause::Mine(_))
        && low.applies_at(before)
    {
        // A negative change other than a mine's is a fixed amount, so it
        // fits in i64.
        let d = i64::try_from(delta).unwrap_or(i64::MIN);
        delta = i128::from(low.reduce(GaugeValue(d)).0);
    }
    let next = (i128::from(before.0) + delta).clamp(i128::from(rule.min.0), i128::from(rule.max.0));
    // Clamped to bounds that are i64 values.
    GaugeValue(i64::try_from(next).unwrap_or(rule.max.0))
}

/// Every gauge of a play.
#[derive(Clone, Debug)]
pub(crate) struct Gauges {
    states: Vec<GaugeState>,
    increment: i128,
}

impl Gauges {
    pub(crate) fn new(rules: &[GaugeRule], increment: i128) -> Self {
        Self {
            states: rules
                .iter()
                .map(|r| GaugeState {
                    value: r.start,
                    reference: r.start,
                    failed_at: None,
                })
                .collect(),
            increment,
        }
    }

    pub(crate) fn states(&self) -> &[GaugeState] {
        &self.states
    }

    /// Applies `cause` at `time` to every gauge.
    pub(crate) fn apply(
        &mut self,
        rules: &[GaugeRule],
        mine: &MineRule,
        cause: Cause,
        time: TimeUs,
    ) {
        for (rule, state) in rules.iter().zip(&mut self.states) {
            let d = delta(rule, mine, cause, self.increment);
            state.reference = step(rule, state.reference, d, cause);
            match rule.kind {
                GaugeKind::EndThreshold { .. } => state.value = state.reference,
                GaugeKind::Survival => {
                    if state.failed_at.is_some() {
                        continue;
                    }
                    let next = step(rule, state.value, d, cause);
                    if next <= rule.min {
                        state.value = GaugeValue::EMPTY;
                        state.failed_at = Some(time);
                    } else {
                        state.value = next;
                    }
                }
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
