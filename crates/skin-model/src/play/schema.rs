//! The play screen's schema: the fields and built-in timers a play skin
//! reads, each at a fixed slot.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::Lane;

use crate::schema::{Export, Field, FieldTy, Schema, TimerDef};

/// The largest count a field reads: a 7-digit number, and `x * 100` stays
/// inside `i32`.
pub const MAX_COUNT: i32 = 9_999_999;
/// A full gauge in the gauge field's unit, ‰.
pub const GAUGE_PERMILLE: i32 = 1_000;

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

/// The play screen's state. Fields come only from the play view's
/// snapshot; timers from the play context, the scroll map, the snapshot's
/// fail time and the drained effects (stage3.md R3).
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
    enums: &[],
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
    /// The number of fields.
    pub const COUNT: usize = 13;
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
