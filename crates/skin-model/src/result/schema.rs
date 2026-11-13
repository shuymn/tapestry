//! The result screen's schema: the fields and built-in timers a result
//! skin reads, each at a fixed slot, filled from the result view
//! (docs/stage3.md §3, the Result profile).
//!
//! A value is shown, hidden or unavailable. A hidden value is known and
//! has nothing to draw: its field holds `i32::MIN`, which every expression
//! reads as that integer and a number does not draw. An unavailable value
//! has its group's validity field false, which a front end joins into the
//! visibility of each element that reads it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use crate::JUDGE_CLASS_MAX;
use crate::play::schema::{FPS_MAX, YEAR_MAX};
use crate::schema::{Export, Field, FieldTy, Schema, TimerDef};

/// The largest magnitude a shown value reads: one inside `i32::MIN` and
/// `i32::MAX`, which a number reserves.
pub const VALUE_MAX: i32 = i32::MAX - 1;
/// What a hidden value reads.
pub const HIDDEN: i32 = i32::MIN;
/// The largest clear type: the source's index of a MAX clear.
pub const CLEAR_MAX: i32 = 10;
/// The largest grade: AAA.
pub const GRADE_MAX: i32 = 7;

const T: Export = Export::TapestryOnly;

const fn flag(name: &'static str) -> Field {
    Field {
        name,
        ty: FieldTy::Bool,
        export: T,
    }
}

const fn int(name: &'static str, lo: i32, hi: i32) -> Field {
    Field {
        name,
        ty: FieldTy::Int { lo, hi },
        export: T,
    }
}

const fn value(name: &'static str) -> Field {
    int(name, 0, VALUE_MAX)
}

const fn signed(name: &'static str) -> Field {
    int(name, -VALUE_MAX, VALUE_MAX)
}

/// A value that may be hidden: its range includes [`HIDDEN`].
const fn hideable(name: &'static str) -> Field {
    int(name, HIDDEN, VALUE_MAX)
}

const fn timer(name: &'static str) -> TimerDef {
    TimerDef { name, export: T }
}

/// The result screen's state: the viewed result, its timing statistics and
/// its best before this play, from the result view; the chart's facts; and
/// timers from the screen's lifetime.
pub static RESULT: Schema = Schema {
    fields: &[
        flag("result.valid"),
        value("ex_score"),
        value("max_combo"),
        value("result.bp"),
        value("chart.notes"),
        value("count.perfect"),
        value("count.great"),
        value("count.good"),
        value("count.bad"),
        value("count.poor"),
        value("count.empty_poor"),
        value("count.fast"),
        value("count.slow"),
        value("count.early.perfect"),
        value("count.late.perfect"),
        value("count.early.great"),
        value("count.late.great"),
        value("count.early.good"),
        value("count.late.good"),
        value("count.early.bad"),
        value("count.late.bad"),
        value("count.early.poor"),
        value("count.late.poor"),
        value("count.early.empty_poor"),
        value("count.late.empty_poor"),
        int("gauge", 0, 1_000),
        flag("result.rate_valid"),
        value("rate.current_bp"),
        int("result.grade", 0, GRADE_MAX),
        value("result.next_rank"),
        int("result.clear", 0, CLEAR_MAX),
        flag("result.cleared"),
        flag("timing.valid"),
        signed("timing.mean_x100"),
        value("timing.sd_x100"),
        flag("best.known"),
        value("best.ex"),
        signed("best.ex_diff"),
        hideable("best.bp"),
        hideable("best.bp_diff"),
        int("best.clear", 0, CLEAR_MAX),
        flag("best.ex_updated"),
        flag("best.bp_updated"),
        flag("best.rate_valid"),
        value("best.rate_bp"),
        int("best.grade", 0, GRADE_MAX),
        flag("best.rate_updated"),
        signed("chart.level"),
        flag("chart.level_valid"),
        value("chart.total"),
        flag("chart.total_valid"),
        value("chart.bpm_min_x100"),
        value("chart.bpm_max_x100"),
        flag("chart.bpm_changes"),
        flag("chart.bpm_valid"),
        int("chart.difficulty", 0, 5),
        value("chart.length_ms"),
        flag("chart.length_valid"),
        flag("chart.notes_valid"),
        flag("duration.valid"),
        value("duration_10us"),
        int("chart.judge_class", 0, JUDGE_CLASS_MAX),
        flag("chart.judge_class_valid"),
        flag("chart.stagefile"),
        flag("chart.backbmp"),
        int("app.fps", 0, FPS_MAX),
        int("clock.year", 0, YEAR_MAX),
        int("clock.month", 0, 12),
        int("clock.day", 0, 31),
        int("clock.hour", 0, 23),
        int("clock.minute", 0, 59),
        int("clock.second", 0, 59),
        int("app.uptime_h", 0, i32::MAX),
        int("app.uptime_m", 0, 59),
        int("app.uptime_s", 0, 59),
    ],
    timers: &[timer("screen"), timer("fadeout")],
    enums: &[],
};

/// The field slots of [`RESULT`].
pub mod field {
    /// Whether the view's result is known: false while the view is
    /// unavailable.
    pub const RESULT_VALID: u16 = 0;
    /// The EX score.
    pub const EX_SCORE: u16 = 1;
    pub const MAX_COMBO: u16 = 2;
    /// The BP.
    pub const RESULT_BP: u16 = 3;
    /// The chart's judged notes of the draw played, under
    /// [`CHART_NOTES_VALID`].
    pub const CHART_NOTES: u16 = 4;
    /// Each grade's judgments, PERFECT to POOR, then the empty POORs.
    pub const COUNT_PERFECT: u16 = 5;
    pub const COUNT_GREAT: u16 = 6;
    pub const COUNT_GOOD: u16 = 7;
    pub const COUNT_BAD: u16 = 8;
    pub const COUNT_POOR: u16 = 9;
    pub const COUNT_EMPTY_POOR: u16 = 10;
    /// FAST and SLOW, as play's.
    pub const COUNT_FAST: u16 = 11;
    pub const COUNT_SLOW: u16 = 12;
    /// Each kind's early and late judgments in pairs, PERFECT to POOR, then
    /// the empty POORs', from 13 to 24, as play's.
    pub const COUNT_EARLY_PERFECT: u16 = 13;
    pub const COUNT_LATE_EMPTY_POOR: u16 = 24;
    /// The emphasized gauge's end value, in ‰.
    pub const GAUGE: u16 = 25;
    /// Whether the rate and grade are known: a largest EX score above 0.
    pub const RESULT_RATE_VALID: u16 = 26;
    /// `⌊10 000 · ex / max_ex⌋`.
    pub const RATE_CURRENT_BP: u16 = 27;
    /// The grade, 0 (F) to 7 (AAA).
    pub const RESULT_GRADE: u16 = 28;
    /// The EX score to the next grade step, or to the largest.
    pub const RESULT_NEXT_RANK: u16 = 29;
    /// The clear type, the source's index ([`LampKey::clear_type`](crate::LampKey::clear_type)).
    pub const RESULT_CLEAR: u16 = 30;
    /// Whether the view cleared: its lamp is not failed (options 90 and 91).
    pub const RESULT_CLEARED: u16 = 31;
    /// Whether the timing statistics are known: one sample or more.
    pub const TIMING_VALID: u16 = 32;
    /// The mean deviation in hundredths of a ms, truncated toward 0, early
    /// positive.
    pub const TIMING_MEAN_X100: u16 = 33;
    /// The population standard deviation in hundredths of a ms, rounded
    /// down.
    pub const TIMING_SD_X100: u16 = 34;
    /// Whether the best before this play is known, an empty one included.
    pub const BEST_KNOWN: u16 = 35;
    /// Its EX score.
    pub const BEST_EX: u16 = 36;
    /// The EX score minus it.
    pub const BEST_EX_DIFF: u16 = 37;
    /// Its BP; [`HIDDEN`](super::HIDDEN) when it has none.
    pub const BEST_BP: u16 = 38;
    /// The BP minus it; hidden as `best.bp`.
    pub const BEST_BP_DIFF: u16 = 39;
    /// Its clear type.
    pub const BEST_CLEAR: u16 = 40;
    /// Whether the EX score beat it, and the BP.
    pub const BEST_EX_UPDATED: u16 = 41;
    pub const BEST_BP_UPDATED: u16 = 42;
    /// Whether its rate is known: not for a chart that draws.
    pub const BEST_RATE_VALID: u16 = 43;
    /// Its rate, grade, and whether the rate beat it.
    pub const BEST_RATE_BP: u16 = 44;
    pub const BEST_GRADE: u16 = 45;
    pub const BEST_RATE_UPDATED: u16 = 46;
    /// The chart's play level and gauge total, each with its validity.
    pub const CHART_LEVEL: u16 = 47;
    pub const CHART_LEVEL_VALID: u16 = 48;
    pub const CHART_TOTAL: u16 = 49;
    pub const CHART_TOTAL_VALID: u16 = 50;
    /// Its lowest and highest tempos in floored hundredths of a BPM,
    /// whether they differ, and whether they are known.
    pub const CHART_BPM_MIN_X100: u16 = 51;
    pub const CHART_BPM_MAX_X100: u16 = 52;
    pub const CHART_BPM_CHANGES: u16 = 53;
    pub const CHART_BPM_VALID: u16 = 54;
    /// Its `#DIFFICULTY` from 1 to 5, 0 for any other or none.
    pub const CHART_DIFFICULTY: u16 = 55;
    /// Its length in whole ms, and whether it is known.
    pub const CHART_LENGTH_MS: u16 = 56;
    pub const CHART_LENGTH_VALID: u16 = 57;
    /// Whether [`CHART_NOTES`] is known.
    pub const CHART_NOTES_VALID: u16 = 58;
    /// Whether the mean absolute duration is known: the result has notes.
    pub const DURATION_VALID: u16 = 59;
    /// The mean absolute duration in units of 10 µs, rounded down, the
    /// source's penalized mean: its whole ms and their hundredths are
    /// exact within the field's bound for every duration a `u32` of µs
    /// holds, where µs themselves would pass it past about 35 minutes.
    pub const DURATION_10US: u16 = 60;
    /// The chart's judgment class, 0 the hardest to 4 the easiest, as
    /// play's `chart.judge_class`; 0 while unknown.
    pub const CHART_JUDGE_CLASS: u16 = 61;
    /// Whether [`CHART_JUDGE_CLASS`] is known: the chart's rank is in a
    /// class.
    pub const CHART_JUDGE_CLASS_VALID: u16 = 62;
    /// Whether the chart's stage file and back image are images, as the
    /// attempt resolved them before any result import (options 190, 191,
    /// 194 and 195).
    pub const CHART_STAGEFILE: u16 = 63;
    pub const CHART_BACKBMP: u16 = 64;
    /// The application's readouts of the frame
    /// ([`ResultFrame::app`](crate::ResultFrame::app)), as play's and
    /// selection's fields of the same names: the frames presented in the
    /// last second, the local year, month (1 to 12), day, 24-hour hour,
    /// minute and second, then the uptime's whole hours (unwrapped), its
    /// minutes within the hour and its seconds within the minute. Filled
    /// per frame, never from the view: a view's fields leave them 0.
    pub const APP_FPS: u16 = 65;
    pub const CLOCK_YEAR: u16 = APP_FPS + 1;
    pub const CLOCK_SECOND: u16 = APP_FPS + 6;
    pub const APP_UPTIME_H: u16 = APP_FPS + 7;
    pub const APP_UPTIME_S: u16 = APP_FPS + 9;
    /// The number of fields.
    pub const COUNT: usize = APP_UPTIME_S as usize + 1;
}

/// The built-in timer slots of [`RESULT`]; `when` timers follow from
/// [`BUILTIN`](timer::BUILTIN).
pub mod timer {
    /// The screen's entry.
    pub const SCREEN: u16 = 0;
    /// The fade-out's start; off until it starts.
    pub const FADEOUT: u16 = 1;
    /// The number of built-in timers.
    pub const BUILTIN: u16 = 2;
}

#[cfg(test)]
mod tests {
    use super::{RESULT, field, timer};

    #[test]
    fn the_result_schema_is_sound_and_its_slots_match() {
        RESULT.check().unwrap();
        assert_eq!(RESULT.fields.len(), field::COUNT);
        assert_eq!(RESULT.timers.len(), usize::from(timer::BUILTIN));
        let names: Vec<&str> = RESULT.fields.iter().map(|f| f.name).collect();
        for (slot, name) in [
            (field::RESULT_VALID, "result.valid"),
            (field::EX_SCORE, "ex_score"),
            (field::CHART_NOTES, "chart.notes"),
            (field::CHART_NOTES_VALID, "chart.notes_valid"),
            (field::COUNT_EMPTY_POOR, "count.empty_poor"),
            (field::COUNT_SLOW, "count.slow"),
            (field::COUNT_EARLY_PERFECT, "count.early.perfect"),
            (field::COUNT_LATE_EMPTY_POOR, "count.late.empty_poor"),
            (field::GAUGE, "gauge"),
            (field::RATE_CURRENT_BP, "rate.current_bp"),
            (field::RESULT_CLEARED, "result.cleared"),
            (field::TIMING_SD_X100, "timing.sd_x100"),
            (field::BEST_KNOWN, "best.known"),
            (field::BEST_BP_DIFF, "best.bp_diff"),
            (field::BEST_RATE_UPDATED, "best.rate_updated"),
            (field::CHART_LEVEL, "chart.level"),
            (field::CHART_BPM_VALID, "chart.bpm_valid"),
            (field::CHART_LENGTH_VALID, "chart.length_valid"),
            (field::DURATION_VALID, "duration.valid"),
            (field::DURATION_10US, "duration_10us"),
            (field::CHART_JUDGE_CLASS, "chart.judge_class"),
            (field::CHART_JUDGE_CLASS_VALID, "chart.judge_class_valid"),
            (field::CHART_STAGEFILE, "chart.stagefile"),
            (field::CHART_BACKBMP, "chart.backbmp"),
            (field::APP_FPS, "app.fps"),
            (field::CLOCK_YEAR, "clock.year"),
            (field::CLOCK_SECOND, "clock.second"),
            (field::APP_UPTIME_H, "app.uptime_h"),
            (field::APP_UPTIME_S, "app.uptime_s"),
        ] {
            assert_eq!(names[usize::from(slot)], name);
        }
        assert_eq!(RESULT.timers[usize::from(timer::FADEOUT)].name, "fadeout");
        // The application's readouts read as play's fields of the same
        // names, with the same ranges.
        for f in RESULT
            .fields
            .get(usize::from(field::APP_FPS)..=usize::from(field::APP_UPTIME_S))
            .unwrap()
        {
            let (_, play) = crate::PLAY.field(f.name).unwrap();
            assert_eq!(f.ty, play.ty, "{}", f.name);
        }
    }
}
