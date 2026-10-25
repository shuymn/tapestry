//! The canonical binary encoding of a preset, which identifies it in records.
//!
//! Fields are written in a fixed order. Unsigned integers, enum tags,
//! lengths and booleans are LEB128 varints; signed integers are zigzag
//! varints; strings are a length then UTF-8 bytes. Every struct is
//! destructured in full, so adding a field fails to compile until it is
//! encoded. A change of layout bumps [`FORMAT_VERSION`].

use bms_model::TimeUs;

use crate::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, LateHeadRule, LnBreakRule, LnMode, LnModeRule, LowDamage,
    MineRule, MissRule, PerJudgment, RankScale, RulePreset, SameTimeOrder, ScoreWeights, TotalMode,
    Window,
};

/// Version of the encoding layout, written first.
const FORMAT_VERSION: u64 = 1;

impl RulePreset {
    /// The preset's canonical bytes: equal presets give equal bytes, and any
    /// change of value gives different bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.uint(FORMAT_VERSION);
        let Self {
            id,
            version,
            implementation,
            windows,
            miss,
            empty_poor,
            ln_break,
            rank_scale,
            default_total_milli,
            denominator,
            gauges,
            score,
            same_time_order,
            assignment,
            ln_mode,
            total_mode,
            mine,
        } = self;
        w.str(id);
        w.uint(u64::from(*version));
        w.implementation(*implementation);
        w.windows(windows);
        w.uint(match miss {
            MissRule::PoorAfterLateBad => 0,
        });
        w.empty_poor(empty_poor);
        w.ln_break(ln_break);
        w.rank_scale(rank_scale);
        w.int(*default_total_milli);
        w.uint(match denominator {
            Denominator::TapsAndLnHeads => 0,
        });
        w.len(gauges.len());
        for gauge in gauges {
            w.gauge(gauge);
        }
        w.score(score);
        w.uint(match same_time_order {
            SameTimeOrder::InputsFirst => 0,
        });
        w.uint(match assignment {
            Assignment::NearestTiesEarlier => 0,
        });
        w.ln_mode(ln_mode);
        w.uint(match total_mode {
            TotalMode::ScalesIncrease => 0,
        });
        w.mine(mine);
        w.0
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn uint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                return;
            }
            self.0.push(byte | 0x80);
        }
    }

    fn int(&mut self, v: i64) {
        // Zigzag: 0, -1, 1, -2, … map to 0, 1, 2, 3, …
        self.uint(((v << 1) ^ (v >> 63)) as u64);
    }

    fn len(&mut self, n: usize) {
        self.uint(u64::try_from(n).unwrap_or(u64::MAX));
    }

    fn bool(&mut self, b: bool) {
        self.uint(u64::from(b));
    }

    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.extend_from_slice(s.as_bytes());
    }

    fn time(&mut self, t: TimeUs) {
        self.int(t.0);
    }

    fn gauge_value(&mut self, v: GaugeValue) {
        self.int(v.0);
    }

    fn implementation(&mut self, implementation: Implementation) {
        self.str(implementation.name());
        self.uint(u64::from(implementation.version()));
    }

    fn windows(&mut self, windows: &JudgeWindows) {
        let JudgeWindows {
            perfect,
            great,
            good,
            bad,
        } = windows;
        for &Window { early, late } in [perfect, great, good, bad] {
            self.time(early);
            self.time(late);
        }
    }

    fn empty_poor(&mut self, rule: &EmptyPoorRule) {
        let EmptyPoorRule {
            ahead,
            breaks_combo,
            counts_bp,
            counted,
        } = *rule;
        self.time(ahead);
        self.bool(breaks_combo);
        self.bool(counts_bp);
        self.bool(counted);
    }

    fn ln_break(&mut self, rule: &LnBreakRule) {
        let LnBreakRule {
            release_margin,
            breaks_combo,
            counts_bp,
        } = *rule;
        self.time(release_margin);
        self.bool(breaks_combo);
        self.bool(counts_bp);
    }

    fn rank_scale(&mut self, scale: &RankScale) {
        let RankScale {
            rank_milli,
            absent_milli,
        } = scale;
        self.len(rank_milli.len());
        for &s in rank_milli {
            self.int(s);
        }
        self.int(*absent_milli);
    }

    fn per_judgment<T: Copy>(
        &mut self,
        values: &PerJudgment<T>,
        mut each: impl FnMut(&mut Self, T),
    ) {
        let PerJudgment {
            perfect,
            great,
            good,
            bad,
            poor,
        } = *values;
        for v in [perfect, great, good, bad, poor] {
            each(self, v);
        }
    }

    fn delta(&mut self, delta: GaugeDelta) {
        match delta {
            GaugeDelta::Fixed(v) => {
                self.uint(0);
                self.gauge_value(v);
            }
            GaugeDelta::TotalShare { num, den } => {
                self.uint(1);
                self.uint(u64::from(num));
                self.uint(u64::from(den));
            }
        }
    }

    fn gauge(&mut self, gauge: &GaugeRule) {
        let GaugeRule {
            id,
            kind,
            start,
            min,
            max,
            change,
            empty_poor,
            ln_break,
            low_damage,
        } = gauge;
        self.str(id);
        match kind {
            GaugeKind::EndThreshold { clear } => {
                self.uint(0);
                self.gauge_value(*clear);
            }
            GaugeKind::Survival => self.uint(1),
        }
        self.gauge_value(*start);
        self.gauge_value(*min);
        self.gauge_value(*max);
        self.per_judgment(change, Self::delta);
        self.delta(*empty_poor);
        self.delta(*ln_break);
        match low_damage {
            None => self.uint(0),
            Some(LowDamage { below, num, den }) => {
                self.uint(1);
                self.gauge_value(*below);
                self.uint(u64::from(*num));
                self.uint(u64::from(*den));
            }
        }
    }

    fn score(&mut self, score: &ScoreWeights) {
        let ScoreWeights {
            ex,
            breaks_combo,
            counts_bp,
        } = score;
        self.per_judgment(ex, |w, v| w.uint(u64::from(v)));
        self.per_judgment(breaks_combo, Self::bool);
        self.per_judgment(counts_bp, Self::bool);
    }

    fn ln_mode(&mut self, rule: &LnModeRule) {
        let LnModeRule { default, late_head } = *rule;
        self.uint(match default {
            LnMode::Ln => 0,
        });
        self.uint(match late_head {
            LateHeadRule::CompletesAtPress => 0,
        });
    }

    fn mine(&mut self, rule: &MineRule) {
        match *rule {
            MineRule::WhileHeld { per_unit } => {
                self.uint(0);
                self.gauge_value(per_unit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use bms_model::TimeUs;

    use crate::custom;

    use super::Writer;

    #[test]
    fn canonical_bytes_change_with_any_window() {
        let base = custom();
        let base_bytes = base.canonical_bytes();
        assert_eq!(base_bytes, custom().canonical_bytes());

        let edges: [fn(&mut crate::JudgeWindows) -> &mut TimeUs; 8] = [
            |w| &mut w.perfect.early,
            |w| &mut w.perfect.late,
            |w| &mut w.great.early,
            |w| &mut w.great.late,
            |w| &mut w.good.early,
            |w| &mut w.good.late,
            |w| &mut w.bad.early,
            |w| &mut w.bad.late,
        ];
        let mut seen = HashSet::from([base_bytes.clone()]);
        for edge in edges {
            for step in [-1, 1] {
                let mut preset = base.clone();
                edge(&mut preset.windows).0 += step;
                assert!(
                    seen.insert(preset.canonical_bytes()),
                    "a 1 µs change of a window edge left the bytes unchanged or equal to another edge's"
                );
            }
        }
        assert_eq!(seen.len(), 1 + 8 * 2);
    }

    #[test]
    fn canonical_bytes_cover_identity_and_gauges() {
        let base = custom().canonical_bytes();
        let changed = |edit: fn(&mut crate::RulePreset)| {
            let mut preset = custom();
            edit(&mut preset);
            preset.canonical_bytes()
        };
        assert_ne!(changed(|p| p.id.push('x')), base);
        assert_ne!(changed(|p| p.version += 1), base);
        assert_ne!(
            changed(|p| p.implementation = crate::Implementation::Standard { version: 2 }),
            base
        );
        assert_ne!(changed(|p| p.gauges[1].low_damage = None), base);
        assert_ne!(changed(|p| p.gauges.swap(0, 1)), base);
        assert_ne!(changed(|p| p.empty_poor.counted = true), base);
    }

    #[test]
    fn varints_are_leb128_with_zigzag() {
        let mut w = Writer(Vec::new());
        w.uint(0);
        w.uint(127);
        w.uint(128);
        w.uint(300);
        w.int(-1);
        w.int(1);
        w.int(-64);
        w.int(i64::MIN);
        assert_eq!(
            w.0,
            [
                0x00, 0x7f, 0x80, 0x01, 0xac, 0x02, 0x01, 0x02, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0x01
            ]
        );
    }
}
