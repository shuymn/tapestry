//! Checks that a preset's values are usable.

use std::collections::HashSet;

use bms_model::TimeUs;

use crate::{
    GaugeDelta, GaugeKind, GaugeRule, GaugeValue, JudgeWindows, Judgment, LnEndWindows, MineRule,
    RuleError, RulePreset, SCALE_FULL_MILLI, Side,
};

impl RulePreset {
    /// Checks that this build runs the preset's implementation, the windows
    /// and LN-end edges ascend and stay within
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE) at every rank scale of
    /// the preset, gauge values stay within their bounds, and every factor
    /// and scale is usable.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), RuleError> {
        if !self.implementation.is_supported() {
            return Err(RuleError::UnsupportedImplementation {
                name: self.implementation.name(),
                version: self.implementation.version(),
            });
        }
        if self.id.is_empty() {
            return Err(RuleError::EmptyName { field: "id" });
        }
        check_windows(&self.windows)?;
        if self.windows.scaled(SCALE_FULL_MILLI).is_none() {
            return Err(RuleError::OutOfRange { field: "windows" });
        }
        check_ln_end(&self.ln_end, &self.windows)?;
        if self.ln_end.scaled(SCALE_FULL_MILLI).is_none() {
            return Err(RuleError::OutOfRange { field: "ln_end" });
        }
        non_negative(self.empty_poor.ahead, "empty_poor.ahead")?;
        non_negative(self.empty_poor.behind, "empty_poor.behind")?;
        let r = &self.rank_scale;
        // Every scale is positive and keeps every edge within the cap.
        if r.rank_milli
            .iter()
            .chain([&r.absent_milli])
            .any(|&s| self.windows.scaled(s).is_none() || self.ln_end.scaled(s).is_none())
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

/// The LN-end edges are not negative, strictly widen from PERFECT to GOOD,
/// and are each at least the note window's early edge of the same judgment:
/// a press held at least as long as its long note releases at `d = h + hold
/// − length ≥ h` from its end, which then never grades below the head.
fn check_ln_end(ln_end: &LnEndWindows, windows: &JudgeWindows) -> Result<(), RuleError> {
    let mut previous = -1;
    for (judgment, edge) in ln_end.table() {
        if edge.0 <= previous {
            return Err(RuleError::LnEndNotAscending { judgment });
        }
        previous = edge.0;
    }
    for ((judgment, edge), (_, window)) in ln_end.table().into_iter().zip(windows.table()) {
        if edge < window.early {
            return Err(RuleError::LnEndNarrowerThanNote { judgment });
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
        .chain([(gauge.empty_poor, "empty_poor")]);
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
    fn only_the_supported_standard_version_validates() {
        let with = |version| {
            let mut preset = custom();
            preset.implementation = crate::Implementation::Standard { version };
            preset.validate()
        };
        assert_eq!(with(2), Ok(()));
        for version in [0, 1, 3, u32::MAX] {
            assert_eq!(
                with(version),
                Err(RuleError::UnsupportedImplementation {
                    name: "standard",
                    version
                })
            );
        }
    }

    #[test]
    fn ln_end_edges_must_ascend_from_zero() {
        let with = |p: i64, g: i64, d: i64| {
            let mut preset = custom();
            preset.ln_end.perfect = TimeUs(p);
            preset.ln_end.great = TimeUs(g);
            preset.ln_end.good = TimeUs(d);
            preset.validate()
        };
        let not = |judgment| Err(RuleError::LnEndNotAscending { judgment });
        // The note windows' early edges are 20, 45 and 100 ms.
        assert_eq!(with(100_000, 150_000, 200_000), Ok(()));
        assert_eq!(with(100_000, 100_001, 100_002), Ok(()));
        assert_eq!(with(100_000, 100_000, 200_000), not(Judgment::Great));
        assert_eq!(with(100_000, 150_000, 150_000), not(Judgment::Good));
        assert_eq!(with(100_000, 150_000, 149_999), not(Judgment::Good));
        assert_eq!(with(-1, 150_000, 200_000), not(Judgment::Perfect));
    }

    #[test]
    fn ln_end_edges_are_at_least_the_note_windows_early_edges() {
        let mut preset = custom();
        preset.windows.perfect.early = TimeUs(0);
        let with = |p: i64, g: i64, d: i64| {
            let mut preset = preset.clone();
            preset.ln_end.perfect = TimeUs(p);
            preset.ln_end.great = TimeUs(g);
            preset.ln_end.good = TimeUs(d);
            preset.validate()
        };
        let narrower = |judgment| Err(RuleError::LnEndNarrowerThanNote { judgment });
        // Note early edges: PERFECT 0, GREAT 45 ms, GOOD 100 ms. Equal is
        // accepted, 1 µs narrower refused; the late edges do not matter.
        assert_eq!(with(0, 45_000, 100_000), Ok(()));
        assert_eq!(with(0, 44_999, 100_000), narrower(Judgment::Great));
        assert_eq!(with(0, 45_000, 99_999), narrower(Judgment::Good));
        let mut wide = preset.clone();
        wide.windows.perfect.early = TimeUs(30_000);
        wide.ln_end.perfect = TimeUs(29_999);
        assert_eq!(wide.validate(), narrower(Judgment::Perfect));
        wide.ln_end.perfect = TimeUs(30_000);
        assert_eq!(wide.validate(), Ok(()));
    }

    #[test]
    fn edges_are_capped_at_2_pow_32_before_and_after_the_rank_scale() {
        let cap: i64 = 1 << 32;
        // The BAD late edge or the GOOD LN-end edge at `top`, with no rank
        // widening it.
        let unscaled = || {
            let mut preset = custom();
            preset.rank_scale.rank_milli = [100_000; 4];
            preset
        };
        let with_bad = |top: i64| {
            let mut preset = unscaled();
            preset.windows.bad.late = TimeUs(top);
            preset.validate()
        };
        assert_eq!(with_bad(cap), Ok(()));
        assert_eq!(
            with_bad(cap + 1),
            Err(RuleError::OutOfRange { field: "windows" })
        );
        let with_good_end = |top: i64| {
            let mut preset = unscaled();
            preset.ln_end.good = TimeUs(top);
            preset.validate()
        };
        assert_eq!(with_good_end(cap), Ok(()));
        assert_eq!(
            with_good_end(cap + 1),
            Err(RuleError::OutOfRange { field: "ln_end" })
        );
        // #RANK 3 scales by 125 %: floor(3_435_973_837 × 1.25) =
        // 4_294_967_296, the cap; floor(3_435_973_838 × 1.25) =
        // 4_294_967_297, past it.
        let scaled = |edge: i64, ln_end: bool| {
            let mut preset = custom();
            if ln_end {
                preset.ln_end.good = TimeUs(edge);
            } else {
                preset.windows.bad.early = TimeUs(edge);
            }
            preset.validate()
        };
        let rank = Err(RuleError::OutOfRange {
            field: "rank_scale",
        });
        assert_eq!(cap * 4 / 5, 3_435_973_836);
        for ln_end in [false, true] {
            assert_eq!(scaled(3_435_973_836, ln_end), Ok(()));
            assert_eq!(scaled(3_435_973_837, ln_end), Ok(()));
            assert_eq!(scaled(3_435_973_838, ln_end), rank);
        }
    }

    #[test]
    fn negative_empty_poor_distances_are_rejected() {
        let with = |ahead: i64, behind: i64| {
            let mut preset = custom();
            preset.empty_poor.ahead = TimeUs(ahead);
            preset.empty_poor.behind = TimeUs(behind);
            preset.validate()
        };
        assert_eq!(with(0, 0), Ok(()));
        assert_eq!(
            with(0, -1),
            Err(RuleError::OutOfRange {
                field: "empty_poor.behind"
            })
        );
        assert_eq!(
            with(-1, 0),
            Err(RuleError::OutOfRange {
                field: "empty_poor.ahead"
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
