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
/// A cover setting's full amount, ‰.
pub const COVER_PERMILLE: i32 = 1_000;
/// The largest `duration.green`: three fifths of [`READOUT_MAX`], rounded
/// down.
pub const GREEN_MAX: i32 = READOUT_MAX / 5 * 3 + READOUT_MAX % 5 * 3 / 5;

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

const fn timer(name: &'static str, export: Export) -> TimerDef {
    TimerDef { name, export }
}

const E: Export = Export::Exportable;
const T: Export = Export::TapestryOnly;

/// The play screen's state. Fields come from the play view's snapshot,
/// the rates' whole-attempt denominator, the gauge kind and the BPM range
/// from the play context, the readouts from the frame and the clocks from
/// the prepared chart; timers from the play context, the scroll map, the
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
    /// The multiplicative hi-speed, in floored hundredths; 0 while
    /// `hispeed.valid` is false.
    pub const HISPEED_X100: u16 = 32;
    /// Whether the scroll policy has a multiplicative hi-speed.
    pub const HISPEED_VALID: u16 = 33;
    /// The travel duration, rounded half up to whole ms; 0 while
    /// `duration.valid` is false.
    pub const DURATION_MS: u16 = 34;
    /// `⌊3 · duration.ms / 5⌋`, the green number derived from the rounded
    /// duration.
    pub const DURATION_GREEN: u16 = 35;
    /// Whether the frame gives the travel duration.
    pub const DURATION_VALID: u16 = 36;
    /// The number of fields.
    pub const COUNT: usize = 37;
}

/// The built-in timer slots of [`PLAY`]; `when` timers follow from
/// [`BUILTIN`](timer::BUILTIN).
pub mod timer {
    /// The attempt's countdown start.
    pub const READY: u16 = 0;
    /// The attempt's start, after the countdown.
    pub const PLAY: u16 = 1;
    /// The shown gauge's fail time.
    pub const FAILED: u16 = 2;
    /// The first playing frame past the notes' end
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
    /// The chart's load, from its choice to the countdown
    /// ([`SkinFrame::loading`](crate::SkinFrame::loading)).
    pub const LOADING: u16 = 55;
    /// The number of built-in timers.
    pub const BUILTIN: u16 = 56;
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
    /// The lane's latest press: off until the play view carries lane input.
    Press,
    /// The lane's latest release: off, as `Press`.
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
        ] {
            assert!(
                matches!(ty(name), Some((FieldTy::Bool, e)) if e == export),
                "{name}"
            );
        }
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
