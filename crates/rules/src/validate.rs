//! Checks that a preset's values are usable.

use std::collections::HashSet;

use bms_model::TimeUs;

use crate::{
    GaugeDelta, GaugeKind, GaugeRule, GaugeValue, JudgeRank, JudgeWindows, Judgment, LaneClass,
    LaneTable, LnEndWindows, MineRule, RuleError, RulePreset, SCALE_FULL_MILLI, Side, WindowPolicy,
};

impl RulePreset {
    /// Checks that this build runs the preset's implementation, each lane
    /// class's own windows and LN-end edges ascend and stay within
    /// [`MAX_WINDOW_EDGE`](crate::MAX_WINDOW_EDGE) at every fixed scale of
    /// the window policy (`#RANK 0` to `#RANK 4` and no rank), gauge values
    /// stay within their bounds, and every factor and scale is usable.
    /// Only the preset's own tables must ascend: edges the scale derives
    /// may tie.
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
        for class in LaneClass::ALL {
            check_table(class, self.tables.get(class))?;
        }
        // Every fixed scale of the policy keeps every edge within the cap;
        // derived edges may tie. A #DEFEXRANK is checked when a chart
        // declares it.
        let field = match self.window_policy {
            WindowPolicy::Thousandths(_) => "rank_scale",
            WindowPolicy::Scaled(_) => "rank_percent",
        };
        let ranks = (0..5).map(|r| Some(JudgeRank::Rank(r))).chain([None]);
        for rank in ranks {
            if self.window_policy.prepare(&self.tables, rank).is_none() {
                return Err(RuleError::OutOfRange { field });
            }
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

/// A lane class's own table: its windows and LN-end edges ascend and stay
/// within the cap, and its empty POOR range is not negative.
fn check_table(class: LaneClass, table: &LaneTable) -> Result<(), RuleError> {
    let field = |keys, scratch| match class {
        LaneClass::Keys => keys,
        LaneClass::Scratch => scratch,
    };
    check_windows(class, &table.windows)?;
    if table.windows.scaled(SCALE_FULL_MILLI).is_none() {
        return Err(RuleError::OutOfRange {
            field: field("keys.windows", "scratch.windows"),
        });
    }
    check_ln_end(class, &table.ln_end, &table.windows)?;
    if table.ln_end.scaled(SCALE_FULL_MILLI).is_none() {
        return Err(RuleError::OutOfRange {
            field: field("keys.ln_end", "scratch.ln_end"),
        });
    }
    let range = table.empty_poor;
    non_negative(
        range.ahead,
        field("keys.empty_poor.ahead", "scratch.empty_poor.ahead"),
    )?;
    non_negative(
        range.behind,
        field("keys.empty_poor.behind", "scratch.empty_poor.behind"),
    )
}

/// Each side's edges are not negative and strictly widen from PERFECT to BAD.
fn check_windows(class: LaneClass, windows: &JudgeWindows) -> Result<(), RuleError> {
    for side in [Side::Early, Side::Late] {
        let mut previous = -1;
        for (judgment, window) in windows.table() {
            let edge = match side {
                Side::Early => window.early,
                Side::Late => window.late,
            };
            if edge.0 <= previous {
                return Err(RuleError::WindowsNotAscending {
                    class,
                    side,
                    judgment,
                });
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
fn check_ln_end(
    class: LaneClass,
    ln_end: &LnEndWindows,
    windows: &JudgeWindows,
) -> Result<(), RuleError> {
    let mut previous = -1;
    for (judgment, edge) in ln_end.table() {
        if edge.0 <= previous {
            return Err(RuleError::LnEndNotAscending { class, judgment });
        }
        previous = edge.0;
    }
    for ((judgment, edge), (_, window)) in ln_end.table().into_iter().zip(windows.table()) {
        if edge < window.early {
            return Err(RuleError::LnEndNarrowerThanNote { class, judgment });
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

    use crate::{GaugeValue, Judgment, LaneClass, RuleError, Side, WindowPolicy, custom};

    #[test]
    fn custom_preset_validates() {
        assert_eq!(custom().validate(), Ok(()));
    }

    #[test]
    fn descending_windows_are_rejected_in_either_table() {
        let mut preset = custom();
        preset.tables.keys.windows.good.late = TimeUs(44_000);
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                class: LaneClass::Keys,
                side: Side::Late,
                judgment: Judgment::Good,
            })
        );

        let mut preset = custom();
        preset.tables.scratch.windows.bad.early = preset.tables.scratch.windows.good.early;
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                class: LaneClass::Scratch,
                side: Side::Early,
                judgment: Judgment::Bad,
            })
        );

        let mut preset = custom();
        preset.tables.scratch.windows.perfect.early = TimeUs(-1);
        assert_eq!(
            preset.validate(),
            Err(RuleError::WindowsNotAscending {
                class: LaneClass::Scratch,
                side: Side::Early,
                judgment: Judgment::Perfect,
            })
        );
    }

    #[test]
    fn a_scale_not_positive_is_refused() {
        let with_rank_0 = |milli| {
            let mut preset = custom();
            let WindowPolicy::Thousandths(scale) = &mut preset.window_policy else {
                panic!("custom scales in thousandths");
            };
            scale.rank_milli[0] = milli;
            preset.validate()
        };
        assert_eq!(with_rank_0(1), Ok(()));
        for milli in [0, -1] {
            assert_eq!(
                with_rank_0(milli),
                Err(RuleError::OutOfRange {
                    field: "rank_scale"
                })
            );
        }
        let mut preset = crate::beatoraja_style();
        let WindowPolicy::Scaled(scale) = &mut preset.window_policy else {
            panic!("beatoraja-style scales by percentages");
        };
        scale.absent_percent = 0;
        assert_eq!(
            preset.validate(),
            Err(RuleError::OutOfRange {
                field: "rank_percent"
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
            let ln_end = &mut preset.tables.scratch.ln_end;
            ln_end.perfect = TimeUs(p);
            ln_end.great = TimeUs(g);
            ln_end.good = TimeUs(d);
            preset.validate()
        };
        let not = |judgment| {
            Err(RuleError::LnEndNotAscending {
                class: LaneClass::Scratch,
                judgment,
            })
        };
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
        preset.tables.keys.windows.perfect.early = TimeUs(0);
        let with = |p: i64, g: i64, d: i64| {
            let mut preset = preset.clone();
            let ln_end = &mut preset.tables.keys.ln_end;
            ln_end.perfect = TimeUs(p);
            ln_end.great = TimeUs(g);
            ln_end.good = TimeUs(d);
            preset.validate()
        };
        let narrower = |class, judgment| Err(RuleError::LnEndNarrowerThanNote { class, judgment });
        let keys = LaneClass::Keys;
        // Note early edges: PERFECT 0, GREAT 45 ms, GOOD 100 ms. Equal is
        // accepted, 1 µs narrower refused; the late edges do not matter.
        assert_eq!(with(0, 45_000, 100_000), Ok(()));
        assert_eq!(with(0, 44_999, 100_000), narrower(keys, Judgment::Great));
        assert_eq!(with(0, 45_000, 99_999), narrower(keys, Judgment::Good));
        // Each class against its own windows.
        let mut wide = preset.clone();
        wide.tables.scratch.windows.perfect.early = TimeUs(30_000);
        wide.tables.scratch.ln_end.perfect = TimeUs(29_999);
        assert_eq!(
            wide.validate(),
            narrower(LaneClass::Scratch, Judgment::Perfect)
        );
        wide.tables.scratch.ln_end.perfect = TimeUs(30_000);
        assert_eq!(wide.validate(), Ok(()));
    }

    #[test]
    fn edges_are_capped_at_2_pow_32_before_and_after_the_rank_scale() {
        let cap: i64 = 1 << 32;
        // The BAD late edge or the GOOD LN-end edge at `top`, with no rank
        // widening it.
        let unscaled = || {
            let mut preset = custom();
            preset.window_policy = WindowPolicy::Thousandths(crate::RankScale {
                rank_milli: [100_000; 4],
                absent_milli: 100_000,
            });
            preset
        };
        let with_bad = |top: i64| {
            let mut preset = unscaled();
            preset.tables.keys.windows.bad.late = TimeUs(top);
            preset.validate()
        };
        assert_eq!(with_bad(cap), Ok(()));
        assert_eq!(
            with_bad(cap + 1),
            Err(RuleError::OutOfRange {
                field: "keys.windows"
            })
        );
        let with_good_end = |top: i64| {
            let mut preset = unscaled();
            preset.tables.scratch.ln_end.good = TimeUs(top);
            preset.validate()
        };
        assert_eq!(with_good_end(cap), Ok(()));
        assert_eq!(
            with_good_end(cap + 1),
            Err(RuleError::OutOfRange {
                field: "scratch.ln_end"
            })
        );
        // #RANK 3 scales by 125 %: floor(3_435_973_837 × 1.25) =
        // 4_294_967_296, the cap; floor(3_435_973_838 × 1.25) =
        // 4_294_967_297, past it.
        let scaled = |edge: i64, ln_end: bool| {
            let mut preset = custom();
            if ln_end {
                preset.tables.scratch.ln_end.good = TimeUs(edge);
            } else {
                preset.tables.keys.windows.bad.early = TimeUs(edge);
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
        // beatoraja-style's #RANK 4 scales by 125 % too, in whole percent.
        let mut preset = crate::beatoraja_style();
        preset.tables.scratch.windows.bad.late = TimeUs(3_435_973_837);
        assert_eq!(preset.validate(), Ok(()));
        preset.tables.scratch.windows.bad.late = TimeUs(3_435_973_838);
        assert_eq!(
            preset.validate(),
            Err(RuleError::OutOfRange {
                field: "rank_percent"
            })
        );
    }

    #[test]
    fn negative_empty_poor_distances_are_rejected() {
        let with = |ahead: i64, behind: i64| {
            let mut preset = custom();
            preset.tables.scratch.empty_poor.ahead = TimeUs(ahead);
            preset.tables.scratch.empty_poor.behind = TimeUs(behind);
            preset.validate()
        };
        assert_eq!(with(0, 0), Ok(()));
        assert_eq!(
            with(0, -1),
            Err(RuleError::OutOfRange {
                field: "scratch.empty_poor.behind"
            })
        );
        assert_eq!(
            with(-1, 0),
            Err(RuleError::OutOfRange {
                field: "scratch.empty_poor.ahead"
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
