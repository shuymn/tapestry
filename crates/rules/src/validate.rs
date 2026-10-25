//! Checks that a preset's values are usable.

use std::collections::HashSet;

use bms_model::TimeUs;

use crate::{
    GaugeDelta, GaugeKind, GaugeRule, GaugeValue, JudgeWindows, Judgment, MineRule, RuleError,
    RulePreset, Side,
};

impl RulePreset {
    /// Checks that the windows ascend, gauge values stay within their bounds,
    /// and every factor and scale is usable.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), RuleError> {
        if self.id.is_empty() {
            return Err(RuleError::EmptyName { field: "id" });
        }
        check_windows(&self.windows)?;
        non_negative(self.empty_poor.ahead, "empty_poor.ahead")?;
        non_negative(self.ln_break.release_margin, "ln_break.release_margin")?;
        let r = &self.rank_scale;
        if r.rank_milli
            .iter()
            .chain([&r.absent_milli])
            .any(|&s| s <= 0)
        {
            return Err(RuleError::OutOfRange {
                field: "rank_scale",
            });
        }
        if self.default_total_milli <= 0 {
            return Err(RuleError::OutOfRange {
                field: "default_total_milli",
            });
        }
        let MineRule::WhileHeld { per_unit } = self.mine;
        if !(GaugeValue::EMPTY..=GaugeValue::FULL).contains(&per_unit) {
            return Err(RuleError::OutOfRange {
                field: "mine.per_unit",
            });
        }
        if self.gauges.is_empty() {
            return Err(RuleError::NoGauges);
        }
        let mut ids = HashSet::new();
        for gauge in &self.gauges {
            if !ids.insert(gauge.id.as_str()) {
                return Err(RuleError::DuplicateGauge {
                    id: gauge.id.clone(),
                });
            }
            check_gauge(gauge)?;
        }
        Ok(())
    }
}

/// Each side's edges are not negative and strictly widen from PERFECT to BAD.
fn check_windows(windows: &JudgeWindows) -> Result<(), RuleError> {
    for side in [Side::Early, Side::Late] {
        let mut previous = -1;
        for (judgment, window) in windows.table() {
            let edge = match side {
                Side::Early => window.early,
                Side::Late => window.late,
            };
            if edge.0 <= previous {
                return Err(RuleError::WindowsNotAscending { side, judgment });
            }
            previous = edge.0;
        }
    }
    Ok(())
}

fn non_negative(t: TimeUs, field: &'static str) -> Result<(), RuleError> {
    if t.0 < 0 {
        return Err(RuleError::OutOfRange { field });
    }
    Ok(())
}

fn check_gauge(gauge: &GaugeRule) -> Result<(), RuleError> {
    let fail = |field| RuleError::GaugeOutOfBounds {
        id: gauge.id.clone(),
        field,
    };
    if gauge.id.is_empty() {
        return Err(RuleError::EmptyName { field: "gauge id" });
    }
    let ordered = GaugeValue::EMPTY <= gauge.min
        && gauge.min <= gauge.start
        && gauge.start <= gauge.max
        && gauge.max <= GaugeValue::FULL;
    if !ordered {
        return Err(fail("min, start, max"));
    }
    if gauge.kind == GaugeKind::Survival {
        if gauge.min != GaugeValue::EMPTY {
            return Err(fail("min"));
        }
        // Starting at 0 would start the gauge failed.
        if gauge.start == GaugeValue::EMPTY {
            return Err(fail("start"));
        }
    }
    let bounded = gauge.min..=gauge.max;
    if let GaugeKind::EndThreshold { clear } = gauge.kind
        && !bounded.contains(&clear)
    {
        return Err(fail("clear"));
    }
    let deltas = Judgment::ALL
        .iter()
        .map(|&j| (*gauge.change.get(j), "change"))
        .chain([
            (gauge.empty_poor, "empty_poor"),
            (gauge.ln_break, "ln_break"),
        ]);
    for (delta, field) in deltas {
        let usable = match delta {
            GaugeDelta::Fixed(v) => (-GaugeValue::FULL.0..=GaugeValue::FULL.0).contains(&v.0),
            GaugeDelta::TotalShare { den, .. } => den != 0,
        };
        if !usable {
            return Err(fail(field));
        }
    }
    if let Some(low) = gauge.low_damage
        && (low.den == 0 || low.num > low.den || !bounded.contains(&low.below))
    {
        return Err(fail("low_damage"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use crate::{GaugeValue, Judgment, RuleError, Side, custom};

    #[test]
    fn custom_preset_validates() {
        assert_eq!(custom().validate(), Ok(()));
    }

    #[test]
    fn descending_windows_are_rejected() {
        let mut preset = custom();
        preset.windows.good.late = TimeUs(44_000);
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                side: Side::Late,
                judgment: Judgment::Good,
            })
        );

        let mut preset = custom();
        preset.windows.bad.early = preset.windows.good.early;
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                side: Side::Early,
                judgment: Judgment::Bad,
            })
        );

        let mut preset = custom();
        preset.windows.perfect.early = TimeUs(-1);
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                side: Side::Early,
                judgment: Judgment::Perfect,
            })
        );
    }

    #[test]
    fn gauge_values_outside_bounds_are_rejected() {
        let out = |edit: fn(&mut crate::RulePreset)| {
            let mut preset = custom();
            edit(&mut preset);
            preset.validate()
        };
        let bounds = |field| {
            Err(RuleError::GaugeOutOfBounds {
                id: "normal".to_owned(),
                field,
            })
        };
        assert_eq!(
            out(|p| p.gauges[0].max = GaugeValue(GaugeValue::FULL.0 + 1)),
            bounds("min, start, max")
        );
        assert_eq!(
            out(|p| p.gauges[0].start = GaugeValue(GaugeValue::PERCENT)),
            bounds("min, start, max")
        );
        assert_eq!(
            out(|p| p.gauges[0].kind = crate::GaugeKind::EndThreshold {
                clear: GaugeValue(GaugeValue::FULL.0 + 1)
            }),
            bounds("clear")
        );
        assert_eq!(
            out(|p| p.gauges[0].change.good = crate::GaugeDelta::TotalShare { num: 1, den: 0 }),
            bounds("change")
        );
        assert_eq!(
            out(|p| p.gauges[1].id = "normal".to_owned()),
            Err(RuleError::DuplicateGauge {
                id: "normal".to_owned()
            })
        );
        assert_eq!(
            out(|p| p.gauges[1].min = GaugeValue(2 * GaugeValue::PERCENT)),
            Err(RuleError::GaugeOutOfBounds {
                id: "survival".to_owned(),
                field: "min",
            })
        );
        assert_eq!(
            out(|p| p.gauges[1].start = GaugeValue::EMPTY),
            Err(RuleError::GaugeOutOfBounds {
                id: "survival".to_owned(),
                field: "start",
            })
        );
        assert_eq!(out(|p| p.gauges.clear()), Err(RuleError::NoGauges));
    }
}
