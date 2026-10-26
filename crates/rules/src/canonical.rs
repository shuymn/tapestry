//! The canonical binary encoding of a preset, which identifies it in records.
//!
//! Fields are written in a fixed order. Unsigned integers, enum tags,
//! lengths and booleans are LEB128 varints; signed integers are zigzag
//! varints; strings are a length then UTF-8 bytes. Every struct is
//! destructured in full, so adding a field fails to compile until it is
//! encoded. A change of layout adds a format version.
//!
//! Format 2 adds the empty POOR's `behind` distance after `ahead`. A preset
//! without it is still written in format 1, so the bytes, and the hash a
//! record holds, of a preset made before format 2 do not change.

use bms_model::TimeUs;

use crate::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, LateHeadRule, LnBreakRule, LnMode, LnModeRule, LowDamage,
    MineRule, MissRule, PerJudgment, RankScale, RulePreset, SameTimeOrder, ScoreWeights, TotalMode,
    Window,
};

/// The first encoding layout, for presets without an empty POOR `behind`.
const FORMAT_V1: u64 = 1;
/// The layout with the empty POOR's `behind` distance.
const FORMAT_V2: u64 = 2;

impl RulePreset {
    /// The preset's canonical bytes: equal presets give equal bytes, and any
    /// change of value gives different bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.uint(if self.empty_poor.behind.is_some() {
            FORMAT_V2
        } else {
            FORMAT_V1
        });
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
            MissRule::PoorAfterLateBadOrOvertaken => 1,
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
            behind,
            breaks_combo,
            counts_bp,
            counted,
        } = *rule;
        self.time(ahead);
        // Format 2 only; format 1 is chosen by its absence.
        if let Some(behind) = behind {
            self.time(behind);
        }
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
        assert_ne!(
            changed(|p| p.empty_poor.behind = Some(TimeUs(150_001))),
            base
        );
        assert_ne!(
            changed(|p| p.miss = crate::MissRule::PoorAfterLateBad),
            base
        );
    }

    /// A preset with every field distinct from its neighbours, in format 1:
    /// no empty POOR `behind`.
    fn representative() -> crate::RulePreset {
        let mut p = custom();
        p.id = "rep".to_owned();
        p.version = 7;
        p.implementation = crate::Implementation::Standard { version: 3 };
        let w = |early, late| crate::Window {
            early: TimeUs(early),
            late: TimeUs(late),
        };
        p.windows = crate::JudgeWindows {
            perfect: w(18_000, 22_000),
            great: w(40_000, 50_000),
            good: w(90_000, 110_000),
            bad: w(170_000, 200_000),
        };
        p.empty_poor.ahead = TimeUs(250_000);
        p.empty_poor.behind = None;
        p.empty_poor.counts_bp = false;
        p.empty_poor.counted = true;
        p.miss = crate::MissRule::PoorAfterLateBad;
        p.ln_break.release_margin = TimeUs(80_000);
        p.ln_break.counts_bp = false;
        p.rank_scale = crate::RankScale {
            rank_milli: [40_000, 70_000, 100_000, 130_000],
            absent_milli: 90_000,
        };
        p.default_total_milli = 123_456;
        p
    }

    /// The layout of format version 1, pinned. The expected bytes were
    /// computed by a separate encoder written from the module docs, so a
    /// reordered or dropped field fails here; an intended layout change adds
    /// a format version and a fixture of its own.
    #[test]
    fn canonical_bytes_match_pinned_layout() {
        let p = representative();
        let expected: [u8; 198] = [
            0x01, 0x03, 0x72, 0x65, 0x70, 0x07, 0x08, 0x73, 0x74, 0x61, 0x6e, 0x64, //
            0x61, 0x72, 0x64, 0x03, 0xa0, 0x99, 0x02, 0xe0, 0xd7, 0x02, 0x80, 0xf1, //
            0x04, 0xa0, 0x8d, 0x06, 0xa0, 0xfe, 0x0a, 0xe0, 0xb6, 0x0d, 0xa0, 0xe0, //
            0x14, 0x80, 0xb5, 0x18, 0x00, 0xa0, 0xc2, 0x1e, 0x00, 0x00, 0x01, 0x80, //
            0xe2, 0x09, 0x01, 0x00, 0x04, 0x80, 0xf1, 0x04, 0xe0, 0xc5, 0x08, 0xc0, //
            0x9a, 0x0c, 0xa0, 0xef, 0x0f, 0xa0, 0xfe, 0x0a, 0x80, 0x89, 0x0f, 0x00, //
            0x02, 0x06, 0x6e, 0x6f, 0x72, 0x6d, 0x61, 0x6c, 0x00, 0x80, 0xc8, 0xd0, //
            0x07, 0x80, 0x92, 0xf4, 0x01, 0x80, 0xb5, 0x18, 0x80, 0xda, 0xc4, 0x09, //
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x00, 0xff, 0xb4, //
            0x18, 0x00, 0xff, 0x9e, 0x49, 0x00, 0xbf, 0xcf, 0x24, 0x00, 0xff, 0x9e, //
            0x49, 0x00, 0x08, 0x73, 0x75, 0x72, 0x76, 0x69, 0x76, 0x61, 0x6c, 0x01, //
            0x80, 0xda, 0xc4, 0x09, 0x00, 0x80, 0xda, 0xc4, 0x09, 0x00, 0x80, 0xfa, //
            0x01, 0x00, 0x80, 0xfa, 0x01, 0x00, 0x00, 0x00, 0xbf, 0x84, 0x3d, 0x00, //
            0xbf, 0xee, 0x6d, 0x00, 0x9f, 0xf7, 0x36, 0x00, 0xbf, 0xee, 0x6d, 0x01, //
            0x80, 0x9b, 0xee, 0x02, 0x01, 0x02, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0xa0, 0x8d, 0x06,
        ];
        assert_eq!(p.canonical_bytes(), expected);
    }

    /// Format 2, pinned: format 1's bytes by hand with the version byte 2,
    /// `behind` 120 ms (zigzag 240000 = `80 d3 0e`) inserted after `ahead`
    /// (`a0 c2 1e`).
    #[test]
    fn canonical_bytes_with_behind_use_format_2() {
        let mut p = representative();
        p.empty_poor.behind = Some(TimeUs(120_000));
        let expected: [u8; 201] = [
            0x02, 0x03, 0x72, 0x65, 0x70, 0x07, 0x08, 0x73, 0x74, 0x61, 0x6e, 0x64, //
            0x61, 0x72, 0x64, 0x03, 0xa0, 0x99, 0x02, 0xe0, 0xd7, 0x02, 0x80, 0xf1, //
            0x04, 0xa0, 0x8d, 0x06, 0xa0, 0xfe, 0x0a, 0xe0, 0xb6, 0x0d, 0xa0, 0xe0, //
            0x14, 0x80, 0xb5, 0x18, 0x00, 0xa0, 0xc2, 0x1e, 0x80, 0xd3, 0x0e, 0x00, //
            0x00, 0x01, 0x80, 0xe2, 0x09, 0x01, 0x00, 0x04, 0x80, 0xf1, 0x04, 0xe0, //
            0xc5, 0x08, 0xc0, 0x9a, 0x0c, 0xa0, 0xef, 0x0f, 0xa0, 0xfe, 0x0a, 0x80, //
            0x89, 0x0f, 0x00, 0x02, 0x06, 0x6e, 0x6f, 0x72, 0x6d, 0x61, 0x6c, 0x00, //
            0x80, 0xc8, 0xd0, 0x07, 0x80, 0x92, 0xf4, 0x01, 0x80, 0xb5, 0x18, 0x80, //
            0xda, 0xc4, 0x09, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, //
            0x00, 0xff, 0xb4, 0x18, 0x00, 0xff, 0x9e, 0x49, 0x00, 0xbf, 0xcf, 0x24, //
            0x00, 0xff, 0x9e, 0x49, 0x00, 0x08, 0x73, 0x75, 0x72, 0x76, 0x69, 0x76, //
            0x61, 0x6c, 0x01, 0x80, 0xda, 0xc4, 0x09, 0x00, 0x80, 0xda, 0xc4, 0x09, //
            0x00, 0x80, 0xfa, 0x01, 0x00, 0x80, 0xfa, 0x01, 0x00, 0x00, 0x00, 0xbf, //
            0x84, 0x3d, 0x00, 0xbf, 0xee, 0x6d, 0x00, 0x9f, 0xf7, 0x36, 0x00, 0xbf, //
            0xee, 0x6d, 0x01, 0x80, 0x9b, 0xee, 0x02, 0x01, 0x02, 0x02, 0x01, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa0, 0x8d, 0x06,
        ];
        assert_eq!(p.canonical_bytes(), expected);
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
