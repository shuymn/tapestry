//! The play screen's schema: the fields and built-in timers a play skin
//! reads, each at a fixed slot.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::Lane;

use crate::schema::{EnumDef, EnumId, Export, Field, FieldTy, Schema, TimerDef};

/// The largest count a field reads: a 7-digit number, and `x * 100` stays
/// inside `i32`.
pub const MAX_COUNT: i32 = 9_999_999;
/// A full gauge in the gauge field's unit, ‰.
pub const GAUGE_PERMILLE: i32 = 1_000;
/// 100 % in the score rates' unit, basis points.
pub const RATE_BASIS_POINTS: i32 = 10_000;
/// The enum `gauge.kind` reads: the shown gauge's look
/// ([`GaugeLook`](crate::GaugeLook)), its variants in look order.
pub const GAUGE_KIND_ENUM: EnumId = EnumId(0);
/// The largest BPM, hi-speed or duration readout: one below `i32::MAX`,
/// which a source number widget reserves.
pub const READOUT_MAX: i32 = i32::MAX - 1;
/// [`READOUT_MAX`] as a `u32`.
pub(crate) const READOUT_MAX_U32: u32 = READOUT_MAX.unsigned_abs();
/// The largest lane visual state, `judge.lane.N`: an empty POOR at or
/// after its note, `5 · 2 + 1` (`play/JudgeManager.java:671-672`).
pub const LANE_JUDGE_MAX: i32 = 11;
/// The whole attempt in `time.progress_ppm`'s unit, parts per million.
pub const PROGRESS_FULL: i32 = 1_000_000;
/// The largest `chart.difficulty`: `#DIFFICULTY` 5.
pub const DIFFICULTY_MAX: i32 = 5;
/// The largest `chart.judge_class`: the easiest of the five classes.
pub const JUDGE_CLASS_MAX: i32 = 4;
/// A cover setting's full amount, ‰.
pub const COVER_PERMILLE: i32 = 1_000;
/// The largest `duration.green`: three fifths of [`READOUT_MAX`], rounded
/// down.
pub const GREEN_MAX: i32 = READOUT_MAX / 5 * 3 + READOUT_MAX % 5 * 3 / 5;
/// The ranks' lower bounds in 27ths of the rate, from the top: the bands
/// `rank.current.K` and the thresholds `rank.whole.K` read
/// (`skin/property/BooleanPropertyFactory.java:280-288, 619-626`).
pub const RANK_TWENTY_SEVENTHS: [u32; 8] = [24, 21, 18, 15, 12, 9, 6, 0];
/// The largest `settings.fix_hispeed`: 4, Floating from the minimum
/// tempo.
pub const FIX_HISPEED_MAX: i32 = 4;
/// The largest `native.hispeed.x100`: the multiplier's maximum, 20.
const NATIVE_HISPEED_MAX: i32 = 2_000;
/// The largest `native.target.green`: the target green number's maximum.
pub(crate) const NATIVE_TARGET_MAX: i32 = 6_000;

const fn int(name: &'static str, hi: i32, export: Export) -> Field {
    Field {
        name,
        ty: FieldTy::Int { lo: 0, hi },
        export,
    }
}

const fn flag(name: &'static str, export: Export) -> Field {
    Field {
        name,
        ty: FieldTy::Bool,
        export,
    }
}

const fn rate(name: &'static str) -> Field {
    Field {
        name,
        ty: FieldTy::Int {
            lo: 0,
            hi: i32::MAX,
        },
        export: E,
    }
}

const fn count(name: &'static str, export: Export) -> Field {
    Field {
        name,
        ty: FieldTy::Int {
            lo: 0,
            hi: MAX_COUNT,
        },
        export,
    }
}

/// A difference of counts, either sign.
const fn diff(name: &'static str) -> Field {
    Field {
        name,
        ty: FieldTy::Int {
            lo: -MAX_COUNT,
            hi: MAX_COUNT,
        },
        export: E,
    }
}

const fn timer(name: &'static str, export: Export) -> TimerDef {
    TimerDef { name, export }
}

const E: Export = Export::Exportable;
const T: Export = Export::TapestryOnly;

/// The play screen's state. Fields come from the play view's snapshot,
/// the rates' whole-attempt denominator, the gauge kind, the BPM range,
/// the BPM-change fact and the play level from the play context, the
/// readouts from the frame and the clocks from the prepared chart; timers from the play context, the scroll map, the
/// snapshot's fail time and the drained effects (stage3.md R3).
pub static PLAY: Schema = Schema {
    fields: &[
        count("combo", E),
        count("max_combo", E),
        count("ex_score", E),
        count("bp", E),
        count("count.perfect", E),
        count("count.great", E),
        count("count.good", E),
        count("count.bad", E),
        count("count.poor", E),
        count("count.empty_poor", T),
        count("notes", E),
        Field {
            name: "gauge",
            ty: FieldTy::Int {
                lo: 0,
                hi: GAUGE_PERMILLE,
            },
            export: E,
        },
        Field {
            name: "load.percent",
            ty: FieldTy::Int { lo: 0, hi: 100 },
            export: E,
        },
        rate("rate.current_bp"),
        rate("rate.whole_bp"),
        Field {
            name: "gauge.kind",
            ty: FieldTy::Enum(GAUGE_KIND_ENUM),
            export: E,
        },
        Field {
            name: "gauge.valid",
            ty: FieldTy::Bool,
            export: E,
        },
        int("bpm.current_x100", READOUT_MAX, T),
        int("bpm.min_x100", READOUT_MAX, T),
        int("bpm.max_x100", READOUT_MAX, T),
        flag("bpm.current_valid", T),
        flag("bpm.range_valid", T),
        int("time.remaining_s", i32::MAX, T),
        int("time.elapsed_s", i32::MAX, E),
        int("cover.permille", COVER_PERMILLE, E),
        int("lift.permille", COVER_PERMILLE, E),
        int("hidden.permille", COVER_PERMILLE, E),
        int("cover.absolute_permille", COVER_PERMILLE, E),
        flag("cover.enabled", E),
        flag("lift.enabled", E),
        flag("hidden.enabled", E),
        flag("cover.adjusting", E),
        int("hispeed.x100", READOUT_MAX, E),
        flag("hispeed.valid", E),
        int("duration.ms", READOUT_MAX, E),
        int("duration.green", GREEN_MAX, E),
        flag("duration.valid", E),
        count("count.fast", E),
        count("count.slow", E),
        count("count.early.perfect", E),
        count("count.late.perfect", E),
        count("count.early.great", E),
        count("count.late.great", E),
        count("count.early.good", E),
        count("count.late.good", E),
        count("count.early.bad", E),
        count("count.late.bad", E),
        count("count.early.poor", E),
        count("count.late.poor", E),
        count("count.early.empty_poor", E),
        count("count.late.empty_poor", E),
        int("duration.current.covered.ms", READOUT_MAX, E),
        int("duration.current.covered.green", READOUT_MAX, E),
        flag("duration.current.covered.valid", E),
        int("duration.current.uncovered.ms", READOUT_MAX, E),
        int("duration.current.uncovered.green", READOUT_MAX, E),
        flag("duration.current.uncovered.valid", E),
        int("duration.main.covered.ms", READOUT_MAX, E),
        int("duration.main.covered.green", READOUT_MAX, E),
        flag("duration.main.covered.valid", E),
        int("duration.main.uncovered.ms", READOUT_MAX, E),
        int("duration.main.uncovered.green", READOUT_MAX, E),
        flag("duration.main.uncovered.valid", E),
        int("duration.min.covered.ms", READOUT_MAX, E),
        int("duration.min.covered.green", READOUT_MAX, E),
        flag("duration.min.covered.valid", E),
        int("duration.min.uncovered.ms", READOUT_MAX, E),
        int("duration.min.uncovered.green", READOUT_MAX, E),
        flag("duration.min.uncovered.valid", E),
        int("duration.max.covered.ms", READOUT_MAX, E),
        int("duration.max.covered.green", READOUT_MAX, E),
        flag("duration.max.covered.valid", E),
        int("duration.max.uncovered.ms", READOUT_MAX, E),
        int("duration.max.uncovered.green", READOUT_MAX, E),
        flag("duration.max.uncovered.valid", E),
        int("native.hispeed.x100", NATIVE_HISPEED_MAX, T),
        flag("native.hispeed.valid", T),
        int("native.duration.ms", READOUT_MAX, T),
        int("native.duration.green", READOUT_MAX, T),
        flag("native.duration.valid", T),
        int("native.target.green", NATIVE_TARGET_MAX, T),
        flag("native.target.valid", T),
        int("native.sudden.white", COVER_PERMILLE, T),
        int("native.lift.white", COVER_PERMILLE, T),
        int("native.hidden.white", COVER_PERMILLE, T),
        flag("native.floating", T),
        flag("bpm.change", T),
        flag("bpm.change_valid", T),
        Field {
            name: "chart.level",
            ty: FieldTy::Int {
                lo: -i32::MAX,
                hi: READOUT_MAX,
            },
            export: T,
        },
        flag("chart.level_valid", T),
        flag("autoplay", T),
        Field {
            name: "judge.recent_ms",
            ty: FieldTy::Int {
                lo: -READOUT_MAX,
                hi: READOUT_MAX,
            },
            export: T,
        },
        flag("judge.recent_imperfect", T),
        int("judge.lane.0", LANE_JUDGE_MAX, T),
        int("judge.lane.1", LANE_JUDGE_MAX, T),
        int("judge.lane.2", LANE_JUDGE_MAX, T),
        int("judge.lane.3", LANE_JUDGE_MAX, T),
        int("judge.lane.4", LANE_JUDGE_MAX, T),
        int("judge.lane.5", LANE_JUDGE_MAX, T),
        int("judge.lane.6", LANE_JUDGE_MAX, T),
        int("judge.lane.7", LANE_JUDGE_MAX, T),
        int("time.progress_ppm", PROGRESS_FULL, T),
        int("chart.difficulty", DIFFICULTY_MAX, T),
        flag("chart.difficulty_valid", T),
        int("chart.total", READOUT_MAX, T),
        flag("chart.total_valid", T),
        flag("chart.bga", T),
        flag("chart.bga_valid", T),
        int("chart.judge_class", JUDGE_CLASS_MAX, T),
        flag("chart.judge_class_valid", T),
        Field {
            name: "point",
            ty: FieldTy::Int {
                lo: i32::MIN,
                hi: i32::MAX,
            },
            export: E,
        },
        flag("chart.stagefile", T),
        flag("chart.backbmp", T),
        count("best.ex", E),
        count("target.ex", E),
        diff("best.diff"),
        diff("target.diff"),
        rate("best.rate_bp"),
        rate("target.rate_bp"),
        flag("best.valid", T),
        int("best.rate_ppm", i32::MAX, T),
        flag("best.reached", T),
        flag("target.reached", T),
        flag("rank.current.0", E),
        flag("rank.current.1", E),
        flag("rank.current.2", E),
        flag("rank.current.3", E),
        flag("rank.current.4", E),
        flag("rank.current.5", E),
        flag("rank.current.6", E),
        flag("rank.current.7", E),
        flag("rank.whole.0", E),
        flag("rank.whole.1", E),
        flag("rank.whole.2", E),
        flag("rank.whole.3", E),
        flag("rank.whole.4", E),
        flag("rank.whole.5", E),
        flag("rank.whole.6", E),
        flag("rank.whole.7", E),
        Field {
            name: "display_offset.ms",
            ty: FieldTy::Int {
                lo: -READOUT_MAX,
                hi: READOUT_MAX,
            },
            export: T,
        },
        int("settings.fix_hispeed", FIX_HISPEED_MAX, T),
    ],
    timers: &[
        timer("ready", E),
        timer("play", E),
        timer("failed", E),
        timer("last_note", E),
        timer("full_combo", E),
        timer("gauge_max", E),
        timer("beat", E),
        timer("judge", E),
        timer("judge.perfect", E),
        timer("judge.great", E),
        timer("judge.good", E),
        timer("judge.bad", E),
        timer("judge.poor", E),
        timer("judge.empty_poor", T),
        timer("combo.up", E),
        timer("press.0", E),
        timer("press.1", E),
        timer("press.2", E),
        timer("press.3", E),
        timer("press.4", E),
        timer("press.5", E),
        timer("press.6", E),
        timer("press.7", E),
        timer("release.0", E),
        timer("release.1", E),
        timer("release.2", E),
        timer("release.3", E),
        timer("release.4", E),
        timer("release.5", E),
        timer("release.6", E),
        timer("release.7", E),
        timer("bomb.0", E),
        timer("bomb.1", E),
        timer("bomb.2", E),
        timer("bomb.3", E),
        timer("bomb.4", E),
        timer("bomb.5", E),
        timer("bomb.6", E),
        timer("bomb.7", E),
        timer("hold.0", E),
        timer("hold.1", E),
        timer("hold.2", E),
        timer("hold.3", E),
        timer("hold.4", E),
        timer("hold.5", E),
        timer("hold.6", E),
        timer("hold.7", E),
        timer("mine.0", T),
        timer("mine.1", T),
        timer("mine.2", T),
        timer("mine.3", T),
        timer("mine.4", T),
        timer("mine.5", T),
        timer("mine.6", T),
        timer("mine.7", T),
        timer("loading", E),
        timer("screen", E),
        timer("rhythm", E),
    ],
    enums: &[EnumDef {
        name: "gauge_kind",
        variants: &["assist_easy", "easy", "normal", "hard", "ex_hard", "hazard"],
    }],
};

/// The field slots of [`PLAY`].
pub mod field {
    /// The current combo.
    pub const COMBO: u16 = 0;
    pub const MAX_COMBO: u16 = 1;
    pub const EX_SCORE: u16 = 2;
    pub const BP: u16 = 3;
    pub const COUNT_PERFECT: u16 = 4;
    pub const COUNT_GREAT: u16 = 5;
    pub const COUNT_GOOD: u16 = 6;
    pub const COUNT_BAD: u16 = 7;
    pub const COUNT_POOR: u16 = 8;
    pub const COUNT_EMPTY_POOR: u16 = 9;
    /// The attempt's target count.
    pub const NOTES: u16 = 10;
    /// The shown gauge, in ‰.
    pub const GAUGE: u16 = 11;
    /// The chart's load, in %: 100 once it is loaded.
    pub const LOAD_PERCENT: u16 = 12;
    /// The score rate over the targets judged so far, in basis points.
    pub const RATE_CURRENT_BP: u16 = 13;
    /// The score rate over the attempt's targets, in basis points.
    pub const RATE_WHOLE_BP: u16 = 14;
    /// The shown gauge's look, a [`GAUGE_KIND_ENUM`](super::GAUGE_KIND_ENUM)
    /// value; `normal` while `gauge.valid` is false.
    pub const GAUGE_KIND: u16 = 15;
    /// Whether the shown gauge has both a descriptor and a snapshot.
    pub const GAUGE_VALID: u16 = 16;
    /// The tempo at the display head, in floored hundredths of a BPM; 0
    /// while `bpm.current_valid` is false.
    pub const BPM_CURRENT_X100: u16 = 17;
    /// The chart's lowest BPM, in floored hundredths; 0 while
    /// `bpm.range_valid` is false.
    pub const BPM_MIN_X100: u16 = 18;
    /// The chart's highest BPM, in floored hundredths; 0 while
    /// `bpm.range_valid` is false.
    pub const BPM_MAX_X100: u16 = 19;
    /// Whether the frame gives the current tempo.
    pub const BPM_CURRENT_VALID: u16 = 20;
    /// Whether the play context gives the chart's BPM range.
    pub const BPM_RANGE_VALID: u16 = 21;
    /// The whole seconds, rounded up, from the later of now and the
    /// attempt's start to the last object or the section's end.
    pub const TIME_REMAINING_S: u16 = 22;
    /// The whole seconds, rounded down, since the attempt's start.
    pub const TIME_ELAPSED_S: u16 = 23;
    /// The configured lane cover, in ‰, kept while it is off.
    pub const COVER_PERMILLE: u16 = 24;
    /// The configured lift, in ‰, kept while it is off.
    pub const LIFT_PERMILLE: u16 = 25;
    /// The configured hidden cover, in ‰, kept while it is off.
    pub const HIDDEN_PERMILLE: u16 = 26;
    /// The configured lane cover over the lane's whole length, in ‰:
    /// `⌊(1 000 − lift) · cover / 1 000⌋` from the configured amounts.
    pub const COVER_ABSOLUTE_PERMILLE: u16 = 27;
    /// Whether the lane cover is on.
    pub const COVER_ENABLED: u16 = 28;
    /// Whether the lift is on.
    pub const LIFT_ENABLED: u16 = 29;
    /// Whether the hidden cover is on.
    pub const HIDDEN_ENABLED: u16 = 30;
    /// Whether the cover-adjustment mode is active.
    pub const COVER_ADJUSTING: u16 = 31;
    /// The hi-speed projected into the source's coordinates,
    /// `⌊100 · H / (1 − l)⌋` for the effective lift `l`, in floored
    /// hundredths and not clamped to 20; 0 while `hispeed.valid` is false.
    pub const HISPEED_X100: u16 = 32;
    /// Whether `H` is known and a field is left above the lift.
    pub const HISPEED_VALID: u16 = 33;
    /// The projected travel duration at the current tempo with the
    /// effective lane cover, rounded half up to whole ms; 0 while
    /// `duration.valid` is false.
    pub const DURATION_MS: u16 = 34;
    /// `⌊3 · duration.ms / 5⌋`, the green number derived from the rounded
    /// duration.
    pub const DURATION_GREEN: u16 = 35;
    /// Whether the frame gives the travel duration.
    pub const DURATION_VALID: u16 = 36;
    /// FAST: the early judgments other than PERFECT and the early empty
    /// POORs ([`Tally::fast`](eval::Tally::fast)).
    pub const COUNT_FAST: u16 = 37;
    /// SLOW, as FAST ([`Tally::slow`](eval::Tally::slow)).
    pub const COUNT_SLOW: u16 = 38;
    /// Each kind's early and late judgments, PERFECT to POOR, then the
    /// empty POORs' ([`Tally::early`](eval::Tally::early),
    /// [`Tally::late`](eval::Tally::late)), in pairs from 39 to 50.
    pub const COUNT_EARLY_PERFECT: u16 = 39;
    pub const COUNT_LATE_PERFECT: u16 = 40;
    pub const COUNT_EARLY_GREAT: u16 = 41;
    pub const COUNT_LATE_GREAT: u16 = 42;
    pub const COUNT_EARLY_GOOD: u16 = 43;
    pub const COUNT_LATE_GOOD: u16 = 44;
    pub const COUNT_EARLY_BAD: u16 = 45;
    pub const COUNT_LATE_BAD: u16 = 46;
    pub const COUNT_EARLY_POOR: u16 = 47;
    pub const COUNT_LATE_POOR: u16 = 48;
    pub const COUNT_EARLY_EMPTY_POOR: u16 = 49;
    pub const COUNT_LATE_EMPTY_POOR: u16 = 50;
    /// The first of the eight duration variants' fields, 51 to 74
    /// ([`FrameReadouts::duration_variants`](crate::FrameReadouts::duration_variants)):
    /// variant `i` (the tempo `t` = current, main, min, max, then the
    /// cover `k` = covered, uncovered, `i = 2t + k`) is
    /// `duration.<t>.<k>.ms` at `51 + 3i`, rounded half up, `.green` at
    /// `52 + 3i`, rounded half up from the exact ratio × 3/5, and `.valid`
    /// at `53 + 3i`; see [`duration_variant`].
    pub const DURATION_VARIANTS: u16 = 51;
    /// The active multiplier from the lanes' gain, rounded half up to
    /// hundredths; 0 while `native.hispeed.valid` is false.
    pub const NATIVE_HISPEED_X100: u16 = 75;
    /// Whether the lanes have a gain.
    pub const NATIVE_HISPEED_VALID: u16 = 76;
    /// The exact current travel duration, rounded half up to whole ms;
    /// 0 while `native.duration.valid` is false.
    pub const NATIVE_DURATION_MS: u16 = 77;
    /// The green number rounded half up from exactly three fifths of the
    /// duration.
    pub const NATIVE_DURATION_GREEN: u16 = 78;
    /// Whether the frame gives the native duration.
    pub const NATIVE_DURATION_VALID: u16 = 79;
    /// The target green number; 0 while `native.target.valid` is false.
    pub const NATIVE_TARGET_GREEN: u16 = 80;
    /// Whether there is a target: Floating only.
    pub const NATIVE_TARGET_VALID: u16 = 81;
    /// SUDDEN+'s white number: `⌊(1 000 − l) · cover / 1 000⌋` of the
    /// configured lane cover and the effective lift `l`.
    pub const NATIVE_SUDDEN_WHITE: u16 = 82;
    /// LIFT's white number: the configured lift.
    pub const NATIVE_LIFT_WHITE: u16 = 83;
    /// HIDDEN+'s white number: `⌊(1 000 − l) · hidden / 1 000⌋` of the
    /// configured hidden cover and the effective lift `l`.
    pub const NATIVE_HIDDEN_WHITE: u16 = 84;
    /// Whether the speed mode is Floating.
    pub const NATIVE_FLOATING: u16 = 85;
    /// Whether the chart's whole BPM extremes differ, uncapped; false
    /// while `bpm.change_valid` is false.
    pub const BPM_CHANGE: u16 = 86;
    /// Whether the play context gives the chart's BPM-change fact.
    pub const BPM_CHANGE_VALID: u16 = 87;
    /// The chart's play level; 0 while `chart.level_valid` is false.
    pub const CHART_LEVEL: u16 = 88;
    /// Whether the chart's play level is known and a number can show it:
    /// a source number widget reserves `i32::MIN` and `i32::MAX`.
    pub const CHART_LEVEL_VALID: u16 = 89;
    /// Whether the attempt is autoplay, known while loading too.
    pub const AUTOPLAY: u16 = 90;
    /// The latest judgment's timing as the source shows it: note time
    /// minus press, in ms truncated toward 0 (early positive), over the
    /// judgments and empty POORs the source takes; 0 before any.
    pub const JUDGE_RECENT_MS: u16 = 91;
    /// Whether that judgment is worse than PERFECT; false before any.
    pub const JUDGE_RECENT_IMPERFECT: u16 = 92;
    /// The first of the eight lanes' visual states, `judge.lane.N` at
    /// `93 + N` for [`Lane::index`](bms_model::Lane::index) `N`; see
    /// [`lane_judge`].
    pub const JUDGE_LANES: u16 = 93;
    /// The attempt's progress in parts per million, rounded down: 0 before
    /// the attempt's start (READY), [`PROGRESS_FULL`](super::PROGRESS_FULL)
    /// at and after the end `time.remaining_s` counts to, and between them
    /// the share of the way from the start to that end.
    pub const TIME_PROGRESS_PPM: u16 = 101;
    /// The chart's difficulty: its `#DIFFICULTY` from 1 to 5, and 0 for
    /// any other or none; 0 while `chart.difficulty_valid` is false.
    pub const CHART_DIFFICULTY: u16 = 102;
    /// Whether the chart's difficulty is known.
    pub const CHART_DIFFICULTY_VALID: u16 = 103;
    /// The chart's gauge total, whole; 0 while `chart.total_valid` is
    /// false.
    pub const CHART_TOTAL: u16 = 104;
    /// Whether the chart's gauge total is known and a number can show it.
    pub const CHART_TOTAL_VALID: u16 = 105;
    /// Whether the chart defines a BGA image; false while
    /// `chart.bga_valid` is false.
    pub const CHART_BGA: u16 = 106;
    /// Whether that is known.
    pub const CHART_BGA_VALID: u16 = 107;
    /// The number of fields.
    /// The chart's judgment class from its own rank as the reference
    /// computes it, 0 the hardest to 4 the easiest; 0 while
    /// `chart.judge_class_valid` is false.
    pub const CHART_JUDGE_CLASS: u16 = 108;
    /// Whether the chart has a judgment class.
    pub const CHART_JUDGE_CLASS_VALID: u16 = 109;
    /// The source's 7-key point, with its intermediate widths: any `i32`.
    pub const POINT: u16 = 110;
    /// Whether the chart's stage file is drawable in the installation:
    /// read from its folder, decoded and published into its chart layer
    /// (S10-12). While loading it is available only once the images are
    /// published ([`Loading::set_chart_images`](crate::Loading::set_chart_images)).
    pub const CHART_STAGEFILE: u16 = 111;
    /// Whether the chart's back image is, as `chart.stagefile`.
    pub const CHART_BACKBMP: u16 = 112;
    /// The play's best EX score B
    /// ([`ScoreBasis::best`](crate::ScoreBasis::best)): 0 when known
    /// empty, unavailable (no library, a failed read) or in a practice
    /// round.
    pub const BEST_EX: u16 = 113;
    /// The play's target EX score T
    /// ([`ScoreBasis::target`](crate::ScoreBasis::target)).
    pub const TARGET_EX: u16 = 114;
    /// The EX score less the best paced linearly to the judged targets,
    /// `ex − ⌊B · p / n⌋` (`p` the judged targets, `n` the attempt's
    /// targets; 0 at `n = 0`), clamped to ±[`MAX_COUNT`](super::MAX_COUNT).
    /// Shown only while `best.valid`.
    pub const BEST_DIFF: u16 = 115;
    /// The EX score less the target paced alike, `ex − ⌊T · p / n⌋`.
    pub const TARGET_DIFF: u16 = 116;
    /// B's rate over the attempt's targets, `⌊10 000 · B / 2n⌋` basis
    /// points, 0 at `n = 0`. Shown only while `best.valid`.
    pub const BEST_RATE_BP: u16 = 117;
    /// T's rate alike.
    pub const TARGET_RATE_BP: u16 = 118;
    /// Whether the values measured against the best are shown: false for a
    /// chart that declares `#RANDOM` or `#SETRANDOM`, whose best may come
    /// from another draw. Not whether the best was read: an unavailable
    /// best is the empty score, shown.
    pub const BEST_VALID: u16 = 119;
    /// B's rate in parts per million, `⌊10⁶ · B / 2n⌋`: 0 at `n = 0` and
    /// while `best.valid` is false. Nonzero for any best above 0 while
    /// `2n ≤ 10⁶`, which the parser's object cap keeps.
    pub const BEST_RATE_PPM: u16 = 120;
    /// Whether the attempt has a judgment and its EX score is at least B.
    pub const BEST_REACHED: u16 = 121;
    /// Whether the attempt has a judgment and its EX score is at least T.
    pub const TARGET_REACHED: u16 = 122;
    /// The first of the eight current-rank flags, `rank.current.K` at
    /// `123 + K`: the band of the rate over the judged targets, from
    /// [`RANK_TWENTY_SEVENTHS`](super::RANK_TWENTY_SEVENTHS)`[K]`/27 up to
    /// the band above; see [`rank_current`].
    pub const RANK_CURRENT: u16 = 123;
    /// The first of the eight whole-rank flags, `rank.whole.K` at
    /// `131 + K`: the rate over the attempt's targets at least
    /// [`RANK_TWENTY_SEVENTHS`](super::RANK_TWENTY_SEVENTHS)`[K]`/27; see
    /// [`rank_whole`].
    pub const RANK_WHOLE: u16 = 131;
    /// The display offset in whole ms, truncated toward zero as the
    /// selection screen shows it, from the frame's live presentation.
    pub const DISPLAY_OFFSET_MS: u16 = 139;
    /// The speed policy's hi-speed fix, as the selection screen's: 0
    /// Classic; Floating 1 from the start tempo, 2 the maximum, 3 the main
    /// and 4 the minimum.
    pub const SETTINGS_FIX_HISPEED: u16 = 140;
    /// The number of fields.
    pub const COUNT: usize = 141;

    /// The `rank.current.K` slot, `k` below 8.
    #[must_use]
    pub const fn rank_current(k: u16) -> u16 {
        RANK_CURRENT + k
    }

    /// The `rank.whole.K` slot, `k` below 8.
    #[must_use]
    pub const fn rank_whole(k: u16) -> u16 {
        RANK_WHOLE + k
    }

    /// The `judge.lane.N` slot of `lane`: the lane's visual state as the
    /// source codes it, 0 to [`LANE_JUDGE_MAX`](super::LANE_JUDGE_MAX).
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`Lane::index` is below LANE_COUNT (8)"
    )]
    pub const fn lane_judge(lane: bms_model::Lane) -> u16 {
        JUDGE_LANES + lane.index() as u16
    }

    /// The `duration.<t>.<k>.ms` slot of the variant at `tempo` (0 to 3:
    /// current, main, min, max), `covered` or not; `.green` and `.valid`
    /// follow it.
    #[must_use]
    pub const fn duration_variant(tempo: u16, covered: bool) -> u16 {
        DURATION_VARIANTS + 3 * (2 * tempo + if covered { 0 } else { 1 })
    }
}

/// The built-in timer slots of [`PLAY`]; `when` timers follow from
/// [`BUILTIN`](timer::BUILTIN).
pub mod timer {
    /// The attempt's READY state start.
    pub const READY: u16 = 0;
    /// The attempt's start, after READY.
    pub const PLAY: u16 = 1;
    /// The shown gauge's fail time.
    pub const FAILED: u16 = 2;
    /// The notes' end, from the first playing frame at or past it
    /// ([`PlayContext::last_note_end`](crate::PlayContext::last_note_end)).
    pub const LAST_NOTE: u16 = 3;
    /// The judgment that made the combo equal the target count.
    pub const FULL_COMBO: u16 = 4;
    /// The shown gauge at its maximum.
    pub const GAUGE_MAX: u16 = 5;
    /// The latest integer-beat crossing on the display clock.
    pub const BEAT: u16 = 6;
    /// The latest judgment of any grade.
    pub const JUDGE: u16 = 7;
    /// The first `judge.<grade>` slot; see [`Grade::timer`](super::Grade::timer).
    pub const JUDGE_PERFECT: u16 = 8;
    /// The `judge.empty_poor` slot.
    pub const JUDGE_EMPTY_POOR: u16 = 13;
    /// A judgment that left a combo above 0.
    pub const COMBO_UP: u16 = 14;
    /// The first per-lane slot; see [`lane_timer`](super::lane_timer).
    pub const LANES: u16 = 15;
    /// The loading lifetime's origin while the play screen loads
    /// ([`LoadState::since`](crate::LoadState::since)); off in a play.
    pub const LOADING: u16 = 55;
    /// The screen's start, which an untimed element runs on: the loading
    /// lifetime's origin while loading, `ready`'s start in a play.
    pub const SCREEN: u16 = 56;
    /// The rhythm clock: its start is `now` less the time since the
    /// latest bar line at 1 000 ms per chart beat, from `play`'s start
    /// through the completed tail to the progress end + 5 s, then held.
    pub const RHYTHM: u16 = 57;
    /// The number of built-in timers.
    pub const BUILTIN: u16 = 58;
}

/// A judgment as the skin shows it: the five grades and the empty POOR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grade {
    Perfect,
    Great,
    Good,
    Bad,
    Poor,
    EmptyPoor,
}

impl Grade {
    /// Every grade, in slot order.
    pub const ALL: [Self; 6] = [
        Self::Perfect,
        Self::Great,
        Self::Good,
        Self::Bad,
        Self::Poor,
        Self::EmptyPoor,
    ];

    /// The grade's `judge.<grade>` timer slot. Only the latest judgment's
    /// grade is on.
    #[must_use]
    pub const fn timer(self) -> u16 {
        timer::JUDGE_PERFECT + self as u16
    }

    pub(crate) const fn of(j: rules::Judgment) -> Self {
        match j {
            rules::Judgment::Perfect => Self::Perfect,
            rules::Judgment::Great => Self::Great,
            rules::Judgment::Good => Self::Good,
            rules::Judgment::Bad => Self::Bad,
            rules::Judgment::Poor => Self::Poor,
        }
    }
}

/// A per-lane timer kind, in slot order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneTimer {
    /// Key-on: while the lane is held, from its latest press (from the
    /// READY start for a lane held since the opening); off while it is
    /// released.
    Press,
    /// Key-off: while the lane is released, from its latest release; off
    /// while it is held and before its first release.
    Release,
    /// The lane's latest judgment within the skin's bomb threshold, from a
    /// press.
    Bomb,
    /// The lane's visual long-note hold.
    Hold,
    /// The lane's latest mine hit.
    Mine,
}

/// The slot of `kind`'s timer on `lane`: `press.N`, `release.N`,
/// `bomb.N`, `hold.N` or `mine.N`, where `N` is [`Lane::index`].
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "`Lane::index` is below LANE_COUNT (8)"
)]
pub const fn lane_timer(kind: LaneTimer, lane: Lane) -> u16 {
    timer::LANES + 8 * kind as u16 + lane.index() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_schema_is_valid() {
        assert_eq!(PLAY.check(), Ok(()));
        assert_eq!(PLAY.fields.len(), field::COUNT);
        assert_eq!(PLAY.timers.len(), usize::from(timer::BUILTIN));
    }

    #[test]
    fn gauge_kind_values_follow_the_looks() {
        let variants = PLAY
            .enums
            .get(usize::from(GAUGE_KIND_ENUM.0))
            .map(|e| e.variants);
        let names = ["assist_easy", "easy", "normal", "hard", "ex_hard", "hazard"];
        assert_eq!(variants, Some(&names[..]));
        for (i, look) in crate::GaugeLook::ALL.into_iter().enumerate() {
            assert_eq!(usize::try_from(look.schema_value()).ok(), Some(i));
            assert_eq!(look.index(), i);
        }
        assert!(matches!(
            PLAY.field("gauge.kind").map(|(_, f)| f.ty),
            Some(FieldTy::Enum(GAUGE_KIND_ENUM))
        ));
    }

    /// The readouts' ranges, and which have no counterpart in the export
    /// format.
    #[test]
    fn readout_fields_declare_their_ranges_and_exports() {
        assert_eq!(READOUT_MAX, 2_147_483_646);
        assert_eq!(i64::from(GREEN_MAX), i64::from(READOUT_MAX) * 3 / 5);
        let ty = |name: &str| PLAY.field(name).map(|(_, f)| (f.ty, f.export));
        let int = |hi| FieldTy::Int { lo: 0, hi };
        for (name, hi, export) in [
            ("bpm.current_x100", READOUT_MAX, T),
            ("bpm.min_x100", READOUT_MAX, T),
            ("bpm.max_x100", READOUT_MAX, T),
            ("time.remaining_s", i32::MAX, T),
            ("time.elapsed_s", i32::MAX, E),
            ("cover.permille", 1_000, E),
            ("lift.permille", 1_000, E),
            ("hidden.permille", 1_000, E),
            ("cover.absolute_permille", 1_000, E),
            ("hispeed.x100", READOUT_MAX, E),
            ("duration.ms", READOUT_MAX, E),
            ("duration.green", GREEN_MAX, E),
        ] {
            assert!(
                matches!(ty(name), Some((t, e)) if t == int(hi) && e == export),
                "{name}"
            );
        }
        for (name, export) in [
            ("bpm.current_valid", T),
            ("bpm.range_valid", T),
            ("cover.enabled", E),
            ("lift.enabled", E),
            ("hidden.enabled", E),
            ("cover.adjusting", E),
            ("hispeed.valid", E),
            ("duration.valid", E),
            ("native.hispeed.valid", T),
            ("native.duration.valid", T),
            ("native.target.valid", T),
            ("native.floating", T),
            ("bpm.change", T),
            ("bpm.change_valid", T),
        ] {
            assert!(
                matches!(ty(name), Some((FieldTy::Bool, e)) if e == export),
                "{name}"
            );
        }
        for (name, hi) in [
            ("native.hispeed.x100", 2_000),
            ("native.duration.ms", READOUT_MAX),
            ("native.duration.green", READOUT_MAX),
            ("native.target.green", 6_000),
            ("native.sudden.white", 1_000),
            ("native.lift.white", 1_000),
            ("native.hidden.white", 1_000),
        ] {
            assert!(matches!(ty(name), Some((t, T)) if t == int(hi)), "{name}");
        }
        for t in ["current", "main", "min", "max"] {
            for k in ["covered", "uncovered"] {
                for v in ["ms", "green"] {
                    let name = format!("duration.{t}.{k}.{v}");
                    assert!(
                        matches!(ty(&name), Some((t, E)) if t == int(READOUT_MAX)),
                        "{name}"
                    );
                }
                let name = format!("duration.{t}.{k}.valid");
                assert!(matches!(ty(&name), Some((FieldTy::Bool, E))), "{name}");
            }
        }
    }

    /// Slots 51 to 74 are `51 + 3 · (2t + k) + {0, 1, 2}` for the tempo
    /// `t` and the cover `k`, the native block follows to 85, and the
    /// BPM-change fact takes 86 and 87.
    #[test]
    fn duration_variants_and_the_native_block_take_their_slots() {
        let slot = |name: &str| PLAY.field(name).map(|(s, _)| s);
        for (t, tempo) in ["current", "main", "min", "max"].into_iter().enumerate() {
            for (k, cover) in ["covered", "uncovered"].into_iter().enumerate() {
                let t16 = u16::try_from(t).unwrap();
                let k16 = u16::try_from(k).unwrap();
                let first = 51 + 3 * (2 * t16 + k16);
                assert_eq!(field::duration_variant(t16, k == 0), first);
                for (i, v) in ["ms", "green", "valid"].into_iter().enumerate() {
                    let name = format!("duration.{tempo}.{cover}.{v}");
                    assert_eq!(
                        slot(&name),
                        Some(first + u16::try_from(i).unwrap()),
                        "{name}"
                    );
                }
            }
        }
        let native = [
            ("native.hispeed.x100", field::NATIVE_HISPEED_X100),
            ("native.hispeed.valid", field::NATIVE_HISPEED_VALID),
            ("native.duration.ms", field::NATIVE_DURATION_MS),
            ("native.duration.green", field::NATIVE_DURATION_GREEN),
            ("native.duration.valid", field::NATIVE_DURATION_VALID),
            ("native.target.green", field::NATIVE_TARGET_GREEN),
            ("native.target.valid", field::NATIVE_TARGET_VALID),
            ("native.sudden.white", field::NATIVE_SUDDEN_WHITE),
            ("native.lift.white", field::NATIVE_LIFT_WHITE),
            ("native.hidden.white", field::NATIVE_HIDDEN_WHITE),
            ("native.floating", field::NATIVE_FLOATING),
        ];
        for (i, (name, s)) in native.into_iter().enumerate() {
            assert_eq!(usize::from(s), 75 + i, "{name}");
            assert_eq!(slot(name), Some(s), "{name}");
        }
        assert_eq!(field::BPM_CHANGE, 86);
        assert_eq!(slot("bpm.change"), Some(86));
        assert_eq!(field::BPM_CHANGE_VALID, 87);
        assert_eq!(slot("bpm.change_valid"), Some(87));
        assert_eq!(slot("chart.level"), Some(field::CHART_LEVEL));
        assert_eq!(slot("chart.level_valid"), Some(field::CHART_LEVEL_VALID));
        assert_eq!(slot("autoplay"), Some(field::AUTOPLAY));
        assert_eq!(slot("judge.recent_ms"), Some(field::JUDGE_RECENT_MS));
        assert_eq!(
            slot("judge.recent_imperfect"),
            Some(field::JUDGE_RECENT_IMPERFECT)
        );
        for lane in Lane::all() {
            let name = format!("judge.lane.{}", lane.index());
            assert_eq!(slot(&name), Some(field::lane_judge(lane)), "{name}");
            assert_eq!(
                PLAY.field(&name).map(|(_, f)| (f.ty, f.export)),
                Some((FieldTy::Int { lo: 0, hi: 11 }, T)),
                "{name}"
            );
        }
        assert_eq!(field::lane_judge(Lane::SCRATCH), 93);
        assert_eq!(slot("time.progress_ppm"), Some(field::TIME_PROGRESS_PPM));
        assert_eq!(
            PLAY.field("time.progress_ppm")
                .map(|(_, f)| (f.ty, f.export)),
            Some((
                FieldTy::Int {
                    lo: 0,
                    hi: 1_000_000
                },
                T
            ))
        );
        let chart = [
            ("chart.difficulty", field::CHART_DIFFICULTY, Some(5)),
            (
                "chart.difficulty_valid",
                field::CHART_DIFFICULTY_VALID,
                None,
            ),
            ("chart.total", field::CHART_TOTAL, Some(i32::MAX - 1)),
            ("chart.total_valid", field::CHART_TOTAL_VALID, None),
            ("chart.bga", field::CHART_BGA, None),
            ("chart.bga_valid", field::CHART_BGA_VALID, None),
            ("chart.judge_class", field::CHART_JUDGE_CLASS, Some(4)),
            (
                "chart.judge_class_valid",
                field::CHART_JUDGE_CLASS_VALID,
                None,
            ),
        ];
        for (i, (name, s, hi)) in chart.into_iter().enumerate() {
            assert_eq!(usize::from(s), 102 + i, "{name}");
            assert_eq!(slot(name), Some(s), "{name}");
            let ty = hi.map_or(FieldTy::Bool, |hi| FieldTy::Int { lo: 0, hi });
            assert_eq!(
                PLAY.field(name).map(|(_, f)| (f.ty, f.export)),
                Some((ty, T)),
                "{name}"
            );
        }
        assert_eq!(slot("point"), Some(field::POINT));
        assert_eq!(
            PLAY.field("point").map(|(_, f)| (f.ty, f.export)),
            Some((
                FieldTy::Int {
                    lo: i32::MIN,
                    hi: i32::MAX
                },
                E
            ))
        );
        // The chart's images, after the point.
        for (name, s) in [
            ("chart.stagefile", field::CHART_STAGEFILE),
            ("chart.backbmp", field::CHART_BACKBMP),
        ] {
            assert_eq!(slot(name), Some(s), "{name}");
            assert_eq!(
                PLAY.field(name).map(|(_, f)| (f.ty, f.export)),
                Some((FieldTy::Bool, T)),
                "{name}"
            );
        }
        assert_eq!(field::CHART_STAGEFILE, field::POINT + 1);
        let int = |lo, hi| FieldTy::Int { lo, hi };
        let score = [
            ("best.ex", field::BEST_EX, int(0, MAX_COUNT), E),
            ("target.ex", field::TARGET_EX, int(0, MAX_COUNT), E),
            ("best.diff", field::BEST_DIFF, int(-MAX_COUNT, MAX_COUNT), E),
            (
                "target.diff",
                field::TARGET_DIFF,
                int(-MAX_COUNT, MAX_COUNT),
                E,
            ),
            ("best.rate_bp", field::BEST_RATE_BP, int(0, i32::MAX), E),
            ("target.rate_bp", field::TARGET_RATE_BP, int(0, i32::MAX), E),
            ("best.valid", field::BEST_VALID, FieldTy::Bool, T),
            ("best.rate_ppm", field::BEST_RATE_PPM, int(0, i32::MAX), T),
            ("best.reached", field::BEST_REACHED, FieldTy::Bool, T),
            ("target.reached", field::TARGET_REACHED, FieldTy::Bool, T),
        ];
        for (i, (name, s, ty, export)) in score.into_iter().enumerate() {
            assert_eq!(usize::from(s), 113 + i, "{name}");
            assert_eq!(slot(name), Some(s), "{name}");
            assert_eq!(
                PLAY.field(name).map(|(_, f)| (f.ty, f.export)),
                Some((ty, export)),
                "{name}"
            );
        }
        for k in 0..8_u16 {
            for (kind, s, first) in [
                ("current", field::rank_current(k), 123),
                ("whole", field::rank_whole(k), 131),
            ] {
                let name = format!("rank.{kind}.{k}");
                assert_eq!(s, first + k, "{name}");
                assert_eq!(slot(&name), Some(s), "{name}");
                assert_eq!(
                    PLAY.field(&name).map(|(_, f)| (f.ty, f.export)),
                    Some((FieldTy::Bool, E)),
                    "{name}"
                );
            }
        }
        assert_eq!(slot("display_offset.ms"), Some(field::DISPLAY_OFFSET_MS));
        assert_eq!(
            PLAY.field("display_offset.ms")
                .map(|(_, f)| (f.ty, f.export)),
            Some((
                FieldTy::Int {
                    lo: -READOUT_MAX,
                    hi: READOUT_MAX
                },
                T
            ))
        );
        assert_eq!(
            slot("settings.fix_hispeed"),
            Some(field::SETTINGS_FIX_HISPEED)
        );
        assert_eq!(
            PLAY.field("settings.fix_hispeed")
                .map(|(_, f)| (f.ty, f.export)),
            Some((FieldTy::Int { lo: 0, hi: 4 }, T))
        );
        assert_eq!(field::COUNT, 141);
    }

    #[test]
    fn slot_constants_match_names() {
        let fields = [
            ("combo", field::COMBO),
            ("max_combo", field::MAX_COMBO),
            ("ex_score", field::EX_SCORE),
            ("bp", field::BP),
            ("count.perfect", field::COUNT_PERFECT),
            ("count.great", field::COUNT_GREAT),
            ("count.good", field::COUNT_GOOD),
            ("count.bad", field::COUNT_BAD),
            ("count.poor", field::COUNT_POOR),
            ("count.empty_poor", field::COUNT_EMPTY_POOR),
            ("notes", field::NOTES),
            ("gauge", field::GAUGE),
            ("load.percent", field::LOAD_PERCENT),
            ("rate.current_bp", field::RATE_CURRENT_BP),
            ("rate.whole_bp", field::RATE_WHOLE_BP),
            ("gauge.kind", field::GAUGE_KIND),
            ("gauge.valid", field::GAUGE_VALID),
            ("bpm.current_x100", field::BPM_CURRENT_X100),
            ("bpm.min_x100", field::BPM_MIN_X100),
            ("bpm.max_x100", field::BPM_MAX_X100),
            ("bpm.current_valid", field::BPM_CURRENT_VALID),
            ("bpm.range_valid", field::BPM_RANGE_VALID),
            ("time.remaining_s", field::TIME_REMAINING_S),
            ("time.elapsed_s", field::TIME_ELAPSED_S),
            ("cover.permille", field::COVER_PERMILLE),
            ("lift.permille", field::LIFT_PERMILLE),
            ("hidden.permille", field::HIDDEN_PERMILLE),
            ("cover.absolute_permille", field::COVER_ABSOLUTE_PERMILLE),
            ("cover.enabled", field::COVER_ENABLED),
            ("lift.enabled", field::LIFT_ENABLED),
            ("hidden.enabled", field::HIDDEN_ENABLED),
            ("cover.adjusting", field::COVER_ADJUSTING),
            ("hispeed.x100", field::HISPEED_X100),
            ("hispeed.valid", field::HISPEED_VALID),
            ("duration.ms", field::DURATION_MS),
            ("duration.green", field::DURATION_GREEN),
            ("duration.valid", field::DURATION_VALID),
            ("count.fast", field::COUNT_FAST),
            ("count.slow", field::COUNT_SLOW),
            ("count.early.perfect", field::COUNT_EARLY_PERFECT),
            ("count.late.perfect", field::COUNT_LATE_PERFECT),
            ("count.early.great", field::COUNT_EARLY_GREAT),
            ("count.late.great", field::COUNT_LATE_GREAT),
            ("count.early.good", field::COUNT_EARLY_GOOD),
            ("count.late.good", field::COUNT_LATE_GOOD),
            ("count.early.bad", field::COUNT_EARLY_BAD),
            ("count.late.bad", field::COUNT_LATE_BAD),
            ("count.early.poor", field::COUNT_EARLY_POOR),
            ("count.late.poor", field::COUNT_LATE_POOR),
            ("count.early.empty_poor", field::COUNT_EARLY_EMPTY_POOR),
            ("count.late.empty_poor", field::COUNT_LATE_EMPTY_POOR),
        ];
        for (name, slot) in fields {
            assert_eq!(PLAY.field(name).map(|(s, _)| s), Some(slot), "{name}");
        }
        let timers = [
            ("ready", timer::READY),
            ("play", timer::PLAY),
            ("failed", timer::FAILED),
            ("last_note", timer::LAST_NOTE),
            ("full_combo", timer::FULL_COMBO),
            ("gauge_max", timer::GAUGE_MAX),
            ("beat", timer::BEAT),
            ("judge", timer::JUDGE),
            ("judge.perfect", Grade::Perfect.timer()),
            ("judge.great", Grade::Great.timer()),
            ("judge.good", Grade::Good.timer()),
            ("judge.bad", Grade::Bad.timer()),
            ("judge.poor", Grade::Poor.timer()),
            ("judge.empty_poor", Grade::EmptyPoor.timer()),
            ("judge.empty_poor", timer::JUDGE_EMPTY_POOR),
            ("combo.up", timer::COMBO_UP),
            ("loading", timer::LOADING),
            ("screen", timer::SCREEN),
            ("rhythm", timer::RHYTHM),
        ];
        for (name, slot) in timers {
            assert_eq!(PLAY.timer(name).map(|(s, _)| s), Some(slot), "{name}");
        }
        let kinds = [
            (LaneTimer::Press, "press"),
            (LaneTimer::Release, "release"),
            (LaneTimer::Bomb, "bomb"),
            (LaneTimer::Hold, "hold"),
            (LaneTimer::Mine, "mine"),
        ];
        for (kind, prefix) in kinds {
            for lane in Lane::all() {
                let name = format!("{prefix}.{}", lane.index());
                assert_eq!(
                    PLAY.timer(&name).map(|(s, _)| s),
                    Some(lane_timer(kind, lane)),
                    "{name}"
                );
            }
        }
    }
}
