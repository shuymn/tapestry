//! The canonical binary encoding of a preset, which identifies it in records.
//!
//! Fields are written in a fixed order. Unsigned integers, enum tags,
//! lengths and booleans are LEB128 varints; signed integers are zigzag
//! varints; strings are a length then UTF-8 bytes. Every struct is
//! destructured in full, so adding a field fails to compile until it is
//! encoded. The first byte is the format version. Before the first release
//! a change of layout bumps it and only the current layout is written, so a
//! record made under an older layout no longer matches any preset and is
//! refused when re-evaluated.
//!
//! [`RulePreset::from_canonical_bytes`] is the exact inverse for the current
//! version: it accepts only the bytes the encoder writes for a valid preset,
//! so every accepted input re-encodes to itself. Every count is checked
//! against the bytes left before anything is allocated for it.

use bms_model::TimeUs;

use crate::{
    Assignment, CanonicalError, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule,
    GaugeValue, Implementation, JudgeWindows, LateHeadRule, LnEndWindows, LnMode, LnModeRule,
    LowDamage, MineRule, MissRule, PerJudgment, RankScale, RulePreset, SameTimeOrder, ScoreWeights,
    TotalMode, Window,
};

/// The encoding layout. Version 4 has the LN-end edges after the windows
/// and no LN break.
const FORMAT_VERSION: u64 = 4;

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
            ln_end,
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
        w.ln_end(ln_end);
        w.uint(match miss {
            MissRule::PoorAfterLateBadOrOvertaken => 0,
        });
        w.empty_poor(empty_poor);
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

impl RulePreset {
    /// Decodes [`canonical_bytes`](Self::canonical_bytes) of the current
    /// version back into the preset.
    ///
    /// # Errors
    ///
    /// The first problem found in reading order: another version, bytes
    /// that end inside a value, a varint that overflows or is not minimal, a
    /// count beyond the bytes left, an unknown tag or implementation, a
    /// value too large for its type, a string that is not UTF-8, a rank
    /// scale without four entries; then bytes after the preset; and last a
    /// preset that fails [`validate`](Self::validate).
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, CanonicalError> {
        let mut r = Reader(bytes);
        let format = r.uint()?;
        if format != FORMAT_VERSION {
            return Err(CanonicalError::UnsupportedVersion(format));
        }
        let id = r.str("id")?;
        let version = r.u32("version")?;
        let implementation = r.implementation()?;
        let windows = r.windows()?;
        let ln_end = r.ln_end()?;
        let miss = r.tag("miss", |t| {
            (t == 0).then_some(MissRule::PoorAfterLateBadOrOvertaken)
        })?;
        let empty_poor = r.empty_poor()?;
        let rank_scale = r.rank_scale()?;
        let default_total_milli = r.int()?;
        let denominator = r.tag("denominator", |t| {
            (t == 0).then_some(Denominator::TapsAndLnHeads)
        })?;
        let count = r.count("gauges")?;
        let mut gauges = Vec::with_capacity(count);
        for _ in 0..count {
            gauges.push(r.gauge()?);
        }
        let score = r.score()?;
        let same_time_order = r.tag("same_time_order", |t| {
            (t == 0).then_some(SameTimeOrder::InputsFirst)
        })?;
        let assignment = r.tag("assignment", |t| {
            (t == 0).then_some(Assignment::NearestTiesEarlier)
        })?;
        let ln_mode = r.ln_mode()?;
        let total_mode = r.tag("total_mode", |t| {
            (t == 0).then_some(TotalMode::ScalesIncrease)
        })?;
        let mine = r.mine()?;
        if !r.0.is_empty() {
            return Err(CanonicalError::TrailingBytes(r.0.len()));
        }
        let preset = Self {
            id,
            version,
            implementation,
            windows,
            miss,
            empty_poor,
            ln_end,
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
        };
        preset.validate().map_err(CanonicalError::Invalid)?;
        Ok(preset)
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
        #[expect(
            clippy::cast_sign_loss,
            reason = "zigzag reinterprets the bits on purpose"
        )]
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
        self.time(behind);
        self.bool(breaks_combo);
        self.bool(counts_bp);
        self.bool(counted);
    }

    fn ln_end(&mut self, ln_end: &LnEndWindows) {
        let LnEndWindows {
            perfect,
            great,
            good,
        } = *ln_end;
        for edge in [perfect, great, good] {
            self.time(edge);
        }
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

/// The bytes not read yet. Each method reads what the [`Writer`] method of
/// the same name writes.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    /// A LEB128 varint in its shortest form: no zero byte after the first,
    /// and at most ten bytes, the tenth holding only bit 63.
    fn uint(&mut self) -> Result<u64, CanonicalError> {
        let mut v = 0_u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.0.split_first().ok_or(CanonicalError::Truncated)?;
            self.0 = rest;
            if shift == 63 && byte > 1 {
                return Err(CanonicalError::VarintOverflow);
            }
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(CanonicalError::NonMinimalVarint);
                }
                return Ok(v);
            }
        }
        Err(CanonicalError::VarintOverflow)
    }

    fn int(&mut self) -> Result<i64, CanonicalError> {
        let v = self.uint()?;
        Ok((v >> 1).cast_signed() ^ -(v & 1).cast_signed())
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, CanonicalError> {
        u32::try_from(self.uint()?).map_err(|_| CanonicalError::ValueTooLarge { field })
    }

    /// A count no larger than the bytes left, since every entry takes at
    /// least one byte.
    fn count(&mut self, field: &'static str) -> Result<usize, CanonicalError> {
        let count = self.uint()?;
        usize::try_from(count)
            .ok()
            .filter(|&n| n <= self.0.len())
            .ok_or(CanonicalError::CountBeyondData { field, count })
    }

    fn tag<T>(
        &mut self,
        field: &'static str,
        known: impl FnOnce(u64) -> Option<T>,
    ) -> Result<T, CanonicalError> {
        let tag = self.uint()?;
        known(tag).ok_or(CanonicalError::UnknownTag { field, tag })
    }

    fn bool(&mut self, field: &'static str) -> Result<bool, CanonicalError> {
        self.tag(field, |t| match t {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        })
    }

    fn str(&mut self, field: &'static str) -> Result<String, CanonicalError> {
        let n = self.count(field)?;
        let (s, rest) = self
            .0
            .split_at_checked(n)
            .ok_or(CanonicalError::Truncated)?;
        self.0 = rest;
        let s = std::str::from_utf8(s).map_err(|_| CanonicalError::InvalidUtf8 { field })?;
        Ok(s.to_owned())
    }

    fn time(&mut self) -> Result<TimeUs, CanonicalError> {
        self.int().map(TimeUs)
    }

    fn gauge_value(&mut self) -> Result<GaugeValue, CanonicalError> {
        self.int().map(GaugeValue)
    }

    fn implementation(&mut self) -> Result<Implementation, CanonicalError> {
        let name = self.str("implementation")?;
        let version = self.u32("implementation version")?;
        match name.as_str() {
            "standard" => Ok(Implementation::Standard { version }),
            _ => Err(CanonicalError::UnknownImplementation { name }),
        }
    }

    fn window(&mut self) -> Result<Window, CanonicalError> {
        Ok(Window {
            early: self.time()?,
            late: self.time()?,
        })
    }

    fn windows(&mut self) -> Result<JudgeWindows, CanonicalError> {
        Ok(JudgeWindows {
            perfect: self.window()?,
            great: self.window()?,
            good: self.window()?,
            bad: self.window()?,
        })
    }

    fn empty_poor(&mut self) -> Result<EmptyPoorRule, CanonicalError> {
        Ok(EmptyPoorRule {
            ahead: self.time()?,
            behind: self.time()?,
            breaks_combo: self.bool("empty_poor.breaks_combo")?,
            counts_bp: self.bool("empty_poor.counts_bp")?,
            counted: self.bool("empty_poor.counted")?,
        })
    }

    fn ln_end(&mut self) -> Result<LnEndWindows, CanonicalError> {
        Ok(LnEndWindows {
            perfect: self.time()?,
            great: self.time()?,
            good: self.time()?,
        })
    }

    fn rank_scale(&mut self) -> Result<RankScale, CanonicalError> {
        let len = self.count("rank_scale")?;
        if len != 4 {
            return Err(CanonicalError::WrongLength {
                field: "rank_scale",
                len,
            });
        }
        Ok(RankScale {
            rank_milli: [self.int()?, self.int()?, self.int()?, self.int()?],
            absent_milli: self.int()?,
        })
    }

    fn per_judgment<T>(
        &mut self,
        mut each: impl FnMut(&mut Self) -> Result<T, CanonicalError>,
    ) -> Result<PerJudgment<T>, CanonicalError> {
        Ok(PerJudgment {
            perfect: each(self)?,
            great: each(self)?,
            good: each(self)?,
            bad: each(self)?,
            poor: each(self)?,
        })
    }

    fn delta(&mut self) -> Result<GaugeDelta, CanonicalError> {
        match self.uint()? {
            0 => Ok(GaugeDelta::Fixed(self.gauge_value()?)),
            1 => Ok(GaugeDelta::TotalShare {
                num: self.u32("gauge delta num")?,
                den: self.u32("gauge delta den")?,
            }),
            tag => Err(CanonicalError::UnknownTag {
                field: "gauge delta",
                tag,
            }),
        }
    }

    fn gauge(&mut self) -> Result<GaugeRule, CanonicalError> {
        let id = self.str("gauge id")?;
        let kind = match self.uint()? {
            0 => GaugeKind::EndThreshold {
                clear: self.gauge_value()?,
            },
            1 => GaugeKind::Survival,
            tag => {
                return Err(CanonicalError::UnknownTag {
                    field: "gauge kind",
                    tag,
                });
            }
        };
        let start = self.gauge_value()?;
        let min = self.gauge_value()?;
        let max = self.gauge_value()?;
        let change = self.per_judgment(Self::delta)?;
        let empty_poor = self.delta()?;
        let low_damage = match self.uint()? {
            0 => None,
            1 => Some(LowDamage {
                below: self.gauge_value()?,
                num: self.u32("low_damage num")?,
                den: self.u32("low_damage den")?,
            }),
            tag => {
                return Err(CanonicalError::UnknownTag {
                    field: "low_damage",
                    tag,
                });
            }
        };
        Ok(GaugeRule {
            id,
            kind,
            start,
            min,
            max,
            change,
            empty_poor,
            low_damage,
        })
    }

    fn score(&mut self) -> Result<ScoreWeights, CanonicalError> {
        Ok(ScoreWeights {
            ex: self.per_judgment(|r| r.u32("score.ex"))?,
            breaks_combo: self.per_judgment(|r| r.bool("score.breaks_combo"))?,
            counts_bp: self.per_judgment(|r| r.bool("score.counts_bp"))?,
        })
    }

    fn ln_mode(&mut self) -> Result<LnModeRule, CanonicalError> {
        Ok(LnModeRule {
            default: self.tag("ln_mode.default", |t| (t == 0).then_some(LnMode::Ln))?,
            late_head: self.tag("ln_mode.late_head", |t| {
                (t == 0).then_some(LateHeadRule::CompletesAtPress)
            })?,
        })
    }

    fn mine(&mut self) -> Result<MineRule, CanonicalError> {
        let tag = self.uint()?;
        match tag {
            0 => Ok(MineRule::WhileHeld {
                per_unit: self.gauge_value()?,
            }),
            _ => Err(CanonicalError::UnknownTag { field: "mine", tag }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use bms_model::TimeUs;

    use crate::{CanonicalError, GaugeKind, Judgment, RuleError, RulePreset, Side, custom};

    use super::{FORMAT_VERSION, Reader, Writer};

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
    fn canonical_bytes_start_with_format_4_and_change_with_any_ln_end_edge() {
        let base = custom();
        let base_bytes = base.canonical_bytes();
        assert_eq!(base_bytes.first(), Some(&4));
        let edges: [fn(&mut crate::LnEndWindows) -> &mut TimeUs; 3] =
            [|w| &mut w.perfect, |w| &mut w.great, |w| &mut w.good];
        let mut seen = HashSet::from([base_bytes]);
        for edge in edges {
            for step in [-1, 1] {
                let mut preset = base.clone();
                edge(&mut preset.ln_end).0 += step;
                assert!(
                    seen.insert(preset.canonical_bytes()),
                    "a 1 µs change of an LN-end edge left the bytes unchanged or equal to another edge's"
                );
            }
        }
        assert_eq!(seen.len(), 1 + 3 * 2);
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
            changed(|p| p.implementation = crate::Implementation::Standard { version: 3 }),
            base
        );
        assert_ne!(changed(|p| p.gauges[1].low_damage = None), base);
        assert_ne!(changed(|p| p.gauges.swap(0, 1)), base);
        assert_ne!(changed(|p| p.empty_poor.counted = true), base);
        assert_ne!(changed(|p| p.empty_poor.behind = TimeUs(150_001)), base);
    }

    /// A preset with every field distinct from its neighbours.
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
        p.empty_poor.behind = TimeUs(120_000);
        p.empty_poor.counts_bp = false;
        p.empty_poor.counted = true;
        p.ln_end = crate::LnEndWindows {
            perfect: TimeUs(90_000),
            great: TimeUs(140_000),
            good: TimeUs(210_000),
        };
        p.rank_scale = crate::RankScale {
            rank_milli: [40_000, 70_000, 100_000, 130_000],
            absent_milli: 90_000,
        };
        p.default_total_milli = 123_456;
        p
    }

    /// The bytes of [`representative`], written by hand from the module docs
    /// field by field: the format version 4, then `rep`, version 7, the
    /// implementation, the windows, the LN-end edges (90 ms is zigzag 180000
    /// = `a0 fe 0a`, 140 ms is `c0 8b 11`, 210 ms is `a0 d1 19`), the miss
    /// rule (0), the empty POOR (`ahead` 250 ms is zigzag 500000 = `a0 c2
    /// 1e`, `behind` 120 ms is zigzag 240000 = `80 d3 0e`), and the rest in
    /// declaration order.
    const PINNED: [u8; 197] = [
        0x04, 0x03, 0x72, 0x65, 0x70, 0x07, 0x08, 0x73, 0x74, 0x61, 0x6e, 0x64, //
        0x61, 0x72, 0x64, 0x03, 0xa0, 0x99, 0x02, 0xe0, 0xd7, 0x02, 0x80, 0xf1, //
        0x04, 0xa0, 0x8d, 0x06, 0xa0, 0xfe, 0x0a, 0xe0, 0xb6, 0x0d, 0xa0, 0xe0, //
        0x14, 0x80, 0xb5, 0x18, 0xa0, 0xfe, 0x0a, 0xc0, 0x8b, 0x11, 0xa0, 0xd1, //
        0x19, 0x00, 0xa0, 0xc2, 0x1e, 0x80, 0xd3, 0x0e, 0x00, 0x00, 0x01, 0x04, //
        0x80, 0xf1, 0x04, 0xe0, 0xc5, 0x08, 0xc0, 0x9a, 0x0c, 0xa0, 0xef, 0x0f, //
        0xa0, 0xfe, 0x0a, 0x80, 0x89, 0x0f, 0x00, 0x02, 0x06, 0x6e, 0x6f, 0x72, //
        0x6d, 0x61, 0x6c, 0x00, 0x80, 0xc8, 0xd0, 0x07, 0x80, 0x92, 0xf4, 0x01, //
        0x80, 0xb5, 0x18, 0x80, 0xda, 0xc4, 0x09, 0x01, 0x01, 0x01, 0x01, 0x01, //
        0x01, 0x01, 0x01, 0x02, 0x00, 0xff, 0xb4, 0x18, 0x00, 0xff, 0x9e, 0x49, //
        0x00, 0xbf, 0xcf, 0x24, 0x00, 0x08, 0x73, 0x75, 0x72, 0x76, 0x69, 0x76, //
        0x61, 0x6c, 0x01, 0x80, 0xda, 0xc4, 0x09, 0x00, 0x80, 0xda, 0xc4, 0x09, //
        0x00, 0x80, 0xfa, 0x01, 0x00, 0x80, 0xfa, 0x01, 0x00, 0x00, 0x00, 0xbf, //
        0x84, 0x3d, 0x00, 0xbf, 0xee, 0x6d, 0x00, 0x9f, 0xf7, 0x36, 0x01, 0x80, //
        0x9b, 0xee, 0x02, 0x01, 0x02, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0xa0, 0x8d, 0x06,
    ];

    /// The layout, pinned, so a reordered or dropped field fails here; an
    /// intended layout change bumps the format version and this fixture.
    #[test]
    fn canonical_bytes_match_pinned_layout() {
        assert_eq!(representative().canonical_bytes(), PINNED);
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

    fn decode(bytes: &[u8]) -> Result<RulePreset, CanonicalError> {
        RulePreset::from_canonical_bytes(bytes)
    }

    /// Where a field of [`custom`]'s bytes starts: the length of the
    /// version and the fields `before` writes.
    fn offset(before: impl FnOnce(&mut Writer, &RulePreset)) -> usize {
        let preset = custom();
        let mut w = Writer(Vec::new());
        w.uint(FORMAT_VERSION);
        before(&mut w, &preset);
        w.0.len()
    }

    /// Up to the miss rule's tag.
    fn through_ln_end(w: &mut Writer, p: &RulePreset) {
        w.str(&p.id);
        w.uint(u64::from(p.version));
        w.implementation(p.implementation);
        w.windows(&p.windows);
        w.ln_end(&p.ln_end);
    }

    /// Up to the gauge count.
    fn through_denominator(w: &mut Writer, p: &RulePreset) {
        through_ln_end(w, p);
        w.uint(0);
        w.empty_poor(&p.empty_poor);
        w.rank_scale(&p.rank_scale);
        w.int(p.default_total_milli);
        w.uint(0);
    }

    /// [`custom`]'s bytes with the varint at `at` replaced by `v`.
    fn with_uint_at(at: usize, v: u64) -> Vec<u8> {
        let bytes = custom().canonical_bytes();
        let mut old = Reader(&bytes[at..]);
        old.uint().unwrap();
        let end = bytes.len() - old.0.len();
        let mut w = Writer(bytes[..at].to_vec());
        w.uint(v);
        w.0.extend_from_slice(&bytes[end..]);
        w.0
    }

    #[test]
    fn custom_preset_decodes() {
        assert_eq!(decode(&custom().canonical_bytes()), Ok(custom()));
    }

    #[test]
    fn other_canonical_version_is_refused() {
        for v in [0, FORMAT_VERSION - 1, FORMAT_VERSION + 1, u64::MAX] {
            assert_eq!(
                decode(&with_uint_at(0, v)),
                Err(CanonicalError::UnsupportedVersion(v))
            );
        }
        assert_eq!(decode(&[]), Err(CanonicalError::Truncated));
    }

    #[test]
    fn count_beyond_the_bytes_is_refused() {
        // The id's length: one more than the bytes left is refused; exactly
        // the bytes left is read, and the next field finds nothing.
        assert_eq!(
            decode(&[4, 4, b'a', b'b', b'c']),
            Err(CanonicalError::CountBeyondData {
                field: "id",
                count: 4
            })
        );
        assert_eq!(
            decode(&[4, 3, b'a', b'b', b'c']),
            Err(CanonicalError::Truncated)
        );
        assert_eq!(
            decode(&[
                4, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01
            ]),
            Err(CanonicalError::CountBeyondData {
                field: "id",
                count: 1 << 63
            })
        );
        // The gauge count, checked before the gauges are allocated.
        let at = offset(through_denominator);
        let left = custom().canonical_bytes().len() - at - 1;
        for count in [u64::try_from(left).unwrap() + 1, u64::MAX] {
            assert_eq!(
                decode(&with_uint_at(at, count)),
                Err(CanonicalError::CountBeyondData {
                    field: "gauges",
                    count
                })
            );
        }
        // The rank scale's length.
        let at = offset(|w, p| {
            through_ln_end(w, p);
            w.uint(0);
            w.empty_poor(&p.empty_poor);
        });
        assert_eq!(
            decode(&with_uint_at(at, u64::MAX)),
            Err(CanonicalError::CountBeyondData {
                field: "rank_scale",
                count: u64::MAX
            })
        );
        assert_eq!(
            decode(&with_uint_at(at, 5)),
            Err(CanonicalError::WrongLength {
                field: "rank_scale",
                len: 5
            })
        );
    }

    #[test]
    fn unknown_tag_is_refused() {
        let unknown = |field, tag| Err(CanonicalError::UnknownTag { field, tag });
        let miss = offset(through_ln_end);
        assert_eq!(decode(&with_uint_at(miss, 1)), unknown("miss", 1));
        let breaks_combo = offset(|w, p| {
            through_ln_end(w, p);
            w.uint(0);
            w.time(p.empty_poor.ahead);
            w.time(p.empty_poor.behind);
        });
        assert_eq!(
            decode(&with_uint_at(breaks_combo, 2)),
            unknown("empty_poor.breaks_combo", 2)
        );
        let kind = offset(|w, p| {
            through_denominator(w, p);
            w.len(p.gauges.len());
            w.str(&p.gauges[0].id);
        });
        assert_eq!(decode(&with_uint_at(kind, 2)), unknown("gauge kind", 2));
        let delta = offset(|w, p| {
            through_denominator(w, p);
            w.len(p.gauges.len());
            let g = &p.gauges[0];
            w.str(&g.id);
            let GaugeKind::EndThreshold { clear } = g.kind else {
                panic!("the first custom gauge has a clear threshold");
            };
            w.uint(0);
            w.gauge_value(clear);
            w.gauge_value(g.start);
            w.gauge_value(g.min);
            w.gauge_value(g.max);
        });
        assert_eq!(
            decode(&with_uint_at(delta, u64::MAX)),
            unknown("gauge delta", u64::MAX)
        );
        let bytes = custom().canonical_bytes();
        let mut mine = Writer(Vec::new());
        mine.mine(&custom().mine);
        let mine = bytes.len() - mine.0.len();
        assert_eq!(decode(&with_uint_at(mine, 1)), unknown("mine", 1));

        // The implementation's tag is its name.
        let mut p = custom();
        p.id = "x".to_owned();
        let mut bytes = p.canonical_bytes();
        // The version, the id `x` and the preset version 3, then the name.
        assert_eq!(&bytes[4..13], b"\x08standard");
        bytes[5] = b'S';
        assert_eq!(
            decode(&bytes),
            Err(CanonicalError::UnknownImplementation {
                name: "Standard".to_owned()
            })
        );
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = custom().canonical_bytes();
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(CanonicalError::TrailingBytes(1)));
        bytes.extend_from_slice(&[0; 9]);
        assert_eq!(decode(&bytes), Err(CanonicalError::TrailingBytes(10)));
        bytes.truncate(bytes.len() - 11);
        assert_eq!(decode(&bytes), Err(CanonicalError::Truncated));
    }

    #[test]
    fn invalid_decoded_preset_is_refused() {
        let mut p = custom();
        p.windows.bad.early = p.windows.good.early;
        assert_eq!(
            decode(&p.canonical_bytes()),
            Err(CanonicalError::Invalid(RuleError::WindowsNotAscending {
                side: Side::Early,
                judgment: Judgment::Bad,
            }))
        );
        assert_eq!(
            decode(&PINNED),
            Err(CanonicalError::Invalid(
                RuleError::UnsupportedImplementation {
                    name: "standard",
                    version: 3
                }
            ))
        );
        let mut p = custom();
        p.gauges.clear();
        assert_eq!(
            decode(&p.canonical_bytes()),
            Err(CanonicalError::Invalid(RuleError::NoGauges))
        );
    }

    #[test]
    fn values_outside_their_type_are_refused() {
        // `version` is a u32.
        let at = offset(|w, p| w.str(&p.id));
        assert_eq!(
            decode(&with_uint_at(at, u64::from(u32::MAX))).map(|p| p.version),
            Ok(u32::MAX)
        );
        assert_eq!(
            decode(&with_uint_at(at, u64::from(u32::MAX) + 1)),
            Err(CanonicalError::ValueTooLarge { field: "version" })
        );
        // The id is not UTF-8.
        let mut bytes = custom().canonical_bytes();
        bytes[2] = 0xff;
        assert_eq!(
            decode(&bytes),
            Err(CanonicalError::InvalidUtf8 { field: "id" })
        );
    }

    #[test]
    fn varints_in_another_form_are_refused() {
        let read = |bytes: &[u8]| {
            let mut r = Reader(bytes);
            r.uint().map(|v| (v, r.0.len()))
        };
        let mut max = vec![0xff; 9];
        max.push(0x01);
        assert_eq!(read(&max), Ok((u64::MAX, 0)));
        assert_eq!(read(&[0x80, 0x01, 0x00]), Ok((128, 1)));
        assert_eq!(read(&[0x80, 0x00]), Err(CanonicalError::NonMinimalVarint));
        assert_eq!(
            read(&[0xff, 0x80, 0x00]),
            Err(CanonicalError::NonMinimalVarint)
        );
        assert_eq!(read(&[0x80]), Err(CanonicalError::Truncated));
        max[9] = 0x02;
        assert_eq!(read(&max), Err(CanonicalError::VarintOverflow));
        max[9] = 0x81;
        assert_eq!(read(&max), Err(CanonicalError::VarintOverflow));
        let mut w = Writer(Vec::new());
        let ints = [0, -1, 1, -64, 64, i64::MIN, i64::MAX];
        for v in ints {
            w.int(v);
        }
        let mut r = Reader(&w.0);
        for v in ints {
            assert_eq!(r.int(), Ok(v));
        }
        assert!(r.0.is_empty());
    }
}
