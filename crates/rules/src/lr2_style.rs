//! The LR2-style preset: similar behavior to the LR2 rule set of LR2oraja
//! Endless Dream, from its public source code. It is "-style", not
//! "compatible" (design.md "Standard Presets").
//!
//! # Sources
//!
//! Every value cites a file and line of LR2oraja Endless Dream,
//! <https://github.com/seraxis/lr2oraja-endlessdream> at `d4995153`, under
//! `core/src/bms/player/beatoraja/`. The rule set is `BMSPlayerRule.LR2`
//! (`play/BMSPlayerRule.java:19`): `GaugeProperty.LR2` and
//! `JudgeProperty.LR2`. Pairs in `play/JudgeProperty.java` are {late edge,
//! early edge} with note − press as the deviation
//! (`play/JudgeManager.java:415`). Where Endless Dream keeps beatoraja's
//! code unchanged, beatoraja-style's module cites it.
//!
//! | Value | Source |
//! | --- | --- |
//! | Note windows at 100 %: PERFECT ±21, GREAT ±60, GOOD ±120, BAD ±200 ms, keys and scratch alike | `play/JudgeProperty.java:54-55` |
//! | LN-end edges at 100 %: PERFECT, GREAT and GOOD all 120 ms before the end, keys and scratch alike: a release within 120 ms before the end keeps the head's judgment, an earlier one is BAD | `play/JudgeProperty.java:56`, `:58`; the release, `play/JudgeManager.java:580-594` (the worse of head and release, `:582`; past the GOOD row it is BAD, `:586-590`) |
//! | `#RANK` 0-4: 25, 50, 75, 100, 75 %; no rank or an unknown `#RANK`: 75 % | `play/JudgeProperty.java:169`, `play/BMSPlayerRule.java:62`; jbms-parser's default rank 2 (beatoraja-style) |
//! | `#DEFEXRANK n`: `n × 75 / 100` %, applied as it is (see the known differences) | `play/BMSPlayerRule.java:65` (75 is `#RANK 2`'s rate) |
//! | Windows ([`WindowPolicy::Interpolated`]): PERFECT, GREAT and GOOD, note and LN-end, interpolated on the LR2 table below 100 % and scaled linearly from 100 %, then each clamped on each side to the next wider; BAD not scaled | `play/JudgeProperty.java:185-190` (the table), `:192-222` (the interpolation), `:233-239` (PERFECT, GREAT and GOOD only), `:241-249` (the clamp); the four tables all go through it, `:116-156` |
//! | Empty POOR when no note is taken: a note, judged or not, from 0 to 1000 ms after the press; nothing behind it (the 1000 ms edge: see the known differences) | `play/JudgeProperty.java:54-55` (MS pair {0, 1000000}); a judged note inside it, `play/JudgeManager.java:433-434`, an unjudged one inside it but outside every window, `:438-442`; the scan, `:414-421` |
//! | Empty POOR keeps the combo without adding to it, adds to BP, has its own count | `play/JudgeProperty.java:60` (MS keeps the combo), `play/JudgeManager.java:716-725` (`judge < 5` adds to it); BP and the count as beatoraja's |
//! | BAD and POOR break the combo and add to BP | `play/JudgeProperty.java:60`, `play/JudgeManager.java:722-725` |
//! | Note assignment: combo priority ([`Assignment::ComboPriority`]), the default algorithm | `PlayConfig.java:103`, `play/JudgeAlgorithm.java:15-20`; the candidate loop, `play/JudgeManager.java:414-455` |
//! | A late BAD LN head cannot be the primary ([`HeadEligibility::NotLateBad`]) | `play/JudgeManager.java:435-437` |
//! | Missed once its late BAD edge has passed (timing: see the known differences) | `play/JudgeManager.java:640-660` |
//! | Judgment headers ([`HeaderPolicy::DefExRankFirst`]) | jbms-parser, as beatoraja-style reads it |
//! | `#TOTAL` absent ([`DefaultTotal::NotesLinear`]): `160 + (N + clamp(N − 400, 0, 200)) × 0.16` over the whole chart's notes | `play/BMSPlayerRule.java:73-79`, `:88-89` |
//! | Gauges `{min, max, start, clear, death}` and changes `{PG, GR, GD, BD, PR, MS}` | `play/GaugeProperty.java:117-122`, field order `:161`; the rule set's gauges, `:52-61` |
//! | ASSIST EASY, EASY and NORMAL increases × `TOTAL / notes` | `play/GrooveGauge.java:263` (`TOTAL` modifier), `play/GaugeProperty.java:117-119` |
//! | HARD and EX HARD damage × `max(fix1, fix2)` ([`TotalCorrection::ScaleDamage`]), MS included | `play/GrooveGauge.java:279-309` (`MODIFY_DAMAGE`), `play/GaugeProperty.java:120-121`; once per chart, `play/GrooveGauge.java:207-211` |
//! | HARD damage × 0.6 below 32 % | `play/GaugeProperty.java:120`, `play/GrooveGauge.java:232-243` |
//! | A survival gauge a change leaves strictly below 2 % drops to 0, a mine's change included | `play/GrooveGauge.java:218-225`, `play/GaugeProperty.java:120-122` (death 2); a mine changes the gauge by `setValue`, `play/GrooveGauge.java:63-66`, `play/JudgeManager.java:266-269` |
//! | Mines: 1 % per unit of the chart's value, while the lane is held as it passes | `play/JudgeManager.java:266-269`, `play/GrooveGauge.java:63-66` |
//! | LN mode; a head pressed at or after its end decides the long note with its own judgment | `play/JudgeManager.java:465-486`; no rule of its own there: the end check completes the long note with the head's judgment at the next frame (`:619-620`), and here at the press ([`LateHeadRule::CompletesAtPress`]) |
//!
//! At `#RANK` 0-4, no rank and some `#DEFEXRANK` values, the note windows
//! (PERFECT, GREAT, GOOD; BAD stays 200 ms) and the LN-end edges are:
//!
//! | Rank | Percentage | PERFECT | GREAT | GOOD | LN end |
//! | --- | --- | --- | --- | --- | --- |
//! | `#RANK 0` | 25 % | 8 ms | 24 ms | 40 ms | 40 ms |
//! | `#RANK 1`, `#DEFEXRANK 66.667` | 50 % | 15 ms | 30 ms | 60 ms | 60 ms |
//! | `#RANK 2`, `#RANK 4`, none, `#DEFEXRANK 100` | 75 % | 18 ms | 40 ms | 100 ms | 100 ms |
//! | `#RANK 3` | 100 % | 21 ms | 60 ms | 120 ms | 120 ms |
//! | `#DEFEXRANK 1` | 0.75 % | 240 µs | 720 µs | 1.2 ms | 1.2 ms |
//! | `#DEFEXRANK 50` | 37.5 % | 11.5 ms | 27 ms | 50 ms | 50 ms |
//! | `#DEFEXRANK 80` | 60 % | 16.2 ms | 34 ms | 76 ms | 76 ms |
//! | `#DEFEXRANK 133` | 99.75 % | 20.97 ms | 59.8 ms | 119.8 ms | 119.8 ms |
//! | `#DEFEXRANK 400` | 300 % | 63 ms | 180 ms | 200 ms (360 clamped) | 200 ms |
//!
//! # Known differences
//!
//! Implementation structure is not reproduced (design.md "What a -style
//! Preset Reproduces"); the first six below are instances.
//!
//! - **One press judges one note (decided).** The source's multi-BAD
//!   judges BAD, with the same press, the other unjudged notes in BAD but
//!   not GOOD range, filtered by the primary's timing, judgment and LN
//!   type, leading LN heads trimmed (`play/JudgeManager.java:993-1061`).
//!   That approximates LR2, where the lane pointer advances past a BAD and
//!   the same press judges the next note again, an implementation
//!   artifact. Here the notes a press passes are POOR at the press. At
//!   `#RANK 3`, with taps at 0 and 160 ms and a long note from 50 to
//!   100 ms on one lane, a press at 190 ms takes the 160 ms tap (GREAT);
//!   the source gives BAD on the 0 ms tap and the head, here they are POOR
//!   at the press.
//! - **An ineligible head keeps the earlier candidate (decided).** The
//!   source makes a late BAD LN head ineligible by giving it judge 6
//!   (`play/JudgeManager.java:435-437`), which reuses the existing "no
//!   judgment here" branch, and that branch also resets the candidate the
//!   scan had already taken (`:445-452`). The rule only says a late BAD
//!   head cannot be taken, and LR2 judges the oldest note, so here the
//!   earlier candidate stays the primary. With a tap at 0 ms and a long
//!   note from 30 ms on one lane, a press at 190 ms is BAD on the tap here
//!   and the head is missed at 230 ms; there the press judges nothing, the
//!   tap is missed at 200 ms and the head at 230 ms.
//! - **Notes a press passes (decided).** When a press takes a later note,
//!   the earlier unjudged notes of its lane are POOR at the press here; the
//!   source keeps them hittable until their own late BAD edge, since its
//!   misses are a separate timeout pass (`play/JudgeManager.java:640-660`).
//!   At `#RANK 3`, with notes at 0 and 160 ms on one lane, a press at
//!   130 ms takes the 160 ms note (GREAT); a second press at 190 ms is BAD
//!   on the 0 ms note there, while here that note was POOR at 130 ms.
//! - **When a BAD long-note head is scored (decided).** The source holds it
//!   and scores it at the release or the end (`play/JudgeManager.java:473`,
//!   `play/JudgeProperty.java:62`); here it is scored at the press, since a
//!   BAD head cannot get worse. The counts are the same, but notes of other
//!   lanes judged during the hold extend the combo before the break there,
//!   after it here.
//! - **When a miss is decided.** The source misses a note at the first
//!   frame strictly past its late BAD edge (`play/JudgeManager.java:642`:
//!   `note time < now − 200 ms`); here the POOR is decided at the edge
//!   itself, after any input at that time. At `#RANK 3`, with a tap at
//!   1000 ms left unhit and another lane's tap hit at 1200.001 ms, the
//!   source judges the press before the miss, here the miss comes first,
//!   so combo and gauges can differ.
//! - **The empty POOR range's far edge.** The source's MS pair reaches
//!   1000 ms ahead inclusively, but its note scan stops at a note at least
//!   that far (`play/JudgeManager.java:416`, the farthest early edge of the
//!   note and scratch tables, `:202-210`), a scan bound, not a window. Here
//!   the range is a window like the others, both edges inclusive, so when
//!   a press judges no note, a note exactly 1000 ms after it makes an
//!   empty POOR here and nothing there; 1 µs closer, both.
//! - **`#DEFEXRANK`'s percentage is not truncated.** The source stores
//!   `n × 75 / 100` as an `int` (`play/BMSPlayerRule.java:65`) before
//!   interpolating; here the interpolation takes it as it is. `#DEFEXRANK
//!   1` is 0 % there, zero-width PERFECT, GREAT and GOOD, and 0.75 % here
//!   (240, 720 and 1200 µs); `#DEFEXRANK 133` interpolates at 99 % there
//!   (PERFECT ±20.88 ms) and 99.75 % here (±20.97 ms); `#DEFEXRANK 50` at
//!   37 % there (±11.36 ms) and 37.5 % here (±11.5 ms).
//! - **A release long after the end, inside one poll.** The source grades
//!   a release on both sides of the end (`play/JudgeManager.java:539-541`)
//!   and completes a long note held through its end only at the first
//!   frame past it (`:619-620`); when no frame runs between the end and a
//!   release 121 ms or more after it, the release is BAD there (`:582-590`).
//!   Here the long note is complete at its end with the head's judgment
//!   (design.md "LN Judgment Modes"), and a release after the end changes
//!   nothing.
//! - **`#DEFEXRANK` as a number, over `#RANK`; number syntax, digits and
//!   whitespace of judgment headers.** The headers are read by the same
//!   decoder as beatoraja's, and differ the same way (beatoraja-style's
//!   known differences): `#RANK 3` then `#DEFEXRANK 120.5` plays 90.375 %
//!   here and `#RANK 3` (100 %) there.
//! - **A huge `#DEFEXRANK`.** Every derived edge here is at most its BAD
//!   edge, so no `#DEFEXRANK` makes a window pass 2^32 µs or refuses the
//!   chart; from 28 633 116, `n × 75` wraps in the source's `int`
//!   arithmetic (`play/BMSPlayerRule.java:65`).
//! - **Gauge arithmetic.** The source keeps gauges in `f32` and computes its
//!   modifiers once per chart in `double`, then `f32`
//!   (`play/GrooveGauge.java:207-211`, `:279-309`); here a gauge is
//!   fixed-point in 1/100000 %, each change is computed exactly and rounded
//!   down once, and the default `#TOTAL` is in thousandths rounded down.
//!   From 20 %, 600 increases of 0.1 % reach about 79.9993 % in `f32`,
//!   below NORMAL's 80 % clear, and exactly 80 % here; this is an
//!   arithmetic example, not a run of the source.
//!
//! The empty POOR range fits one range for judged and unjudged notes:
//! the source's unjudged range starts past the early BAD edge, and an
//! unjudged note inside early BAD is always a candidate here, so no press
//! that judges no note has one there.
//!
//! The examples are in `crates/eval/tests/style_lr2.rs`. The CLASS gauges
//! (course play only) and the player's own judge-window rates (an option,
//! 100 % unless set, `play/JudgeManager.java:180-185`, `play/JudgeProperty.java:251-262`)
//! are not included.

use bms_model::TimeUs;

use crate::{
    Assignment, DefaultTotal, Denominator, EmptyPoorRange, EmptyPoorRule, GaugeDelta, GaugeKind,
    GaugeRule, GaugeValue, HeadEligibility, HeaderPolicy, Implementation, JudgeWindows, LaneTable,
    LateHeadRule, LnEndWindows, LnMode, LnModeRule, LowDamage, MineRule, MissRule, PerJudgment,
    PerLaneClass, RulePreset, SameTimeOrder, ScaledRank, ScoreWeights, TotalCorrection, TotalMode,
    Window, WindowPolicy,
};

const MS: i64 = 1_000;

/// `n` hundredths of a percent as a fixed change.
const fn centi(n: i64) -> GaugeDelta {
    GaugeDelta::Fixed(GaugeValue::centi_percent(n))
}

const fn pct(n: i64) -> GaugeValue {
    GaugeValue(n * GaugeValue::PERCENT)
}

/// A gauge decided at the end: start 20 %, min 2 %, the `TOTAL` modifier on
/// its increases, `up` in tenths of `TOTAL / notes` (PERFECT and GREAT,
/// GOOD) and `damage` in hundredths of a percent (BAD, POOR, empty POOR).
fn end_gauge(id: &str, clear: i64, up: [u32; 2], damage: [i64; 3]) -> GaugeRule {
    let share = |tenths| GaugeDelta::TotalShare {
        num: tenths,
        den: 10,
    };
    let [bad, poor, empty] = damage.map(|d| centi(-d));
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::EndThreshold { clear: pct(clear) },
        start: pct(20),
        min: pct(2),
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: share(up[0]),
            great: share(up[0]),
            good: share(up[1]),
            bad,
            poor,
        },
        empty_poor: empty,
        correction: TotalCorrection::None,
        low_damage: Vec::new(),
    }
}

/// A survival gauge from 100 %, failing below 2 %, with fixed changes in
/// hundredths of a percent: PERFECT, GREAT, GOOD, BAD, POOR, empty POOR.
fn survival_gauge(
    id: &str,
    change: [i64; 6],
    correction: TotalCorrection,
    low_damage: Vec<LowDamage>,
) -> GaugeRule {
    let [perfect, great, good, bad, poor, empty] = change.map(centi);
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::Survival {
            fails_below: pct(2),
        },
        start: GaugeValue::FULL,
        min: GaugeValue::EMPTY,
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect,
            great,
            good,
            bad,
            poor,
        },
        empty_poor: empty,
        correction,
        low_damage,
    }
}

/// One lane class's table: the keys' and the scratch's are the same.
const fn table() -> LaneTable {
    LaneTable {
        windows: JudgeWindows {
            perfect: Window::symmetric(21 * MS),
            great: Window::symmetric(60 * MS),
            good: Window::symmetric(120 * MS),
            bad: Window::symmetric(200 * MS),
        },
        ln_end: LnEndWindows {
            perfect: TimeUs(120 * MS),
            great: TimeUs(120 * MS),
            good: TimeUs(120 * MS),
        },
        empty_poor: EmptyPoorRange {
            ahead: TimeUs(1_000 * MS),
            behind: TimeUs(0),
        },
    }
}

/// The LR2-style preset.
#[must_use]
pub fn lr2_style() -> RulePreset {
    // NORMAL comes first: the displayed gauge defaults to index 0.
    let gauges = vec![
        end_gauge("normal", 80, [10, 5], [400, 600, 200]),
        end_gauge("assist_easy", 60, [12, 6], [320, 480, 160]),
        end_gauge("easy", 80, [12, 6], [320, 480, 160]),
        survival_gauge(
            "hard",
            [10, 10, 5, -600, -1_000, -200],
            TotalCorrection::ScaleDamage,
            vec![LowDamage {
                below: pct(32),
                num: 3,
                den: 5,
            }],
        ),
        survival_gauge(
            "ex_hard",
            [10, 10, 5, -1_200, -2_000, -200],
            TotalCorrection::ScaleDamage,
            Vec::new(),
        ),
        survival_gauge(
            "hazard",
            [15, 6, 0, -10_000, -10_000, -1_000],
            TotalCorrection::None,
            Vec::new(),
        ),
    ];
    RulePreset {
        id: "lr2-style".to_owned(),
        version: 1,
        implementation: Implementation::Standard { version: 2 },
        tables: PerLaneClass::both(table()),
        miss: MissRule::PoorAfterLateBadOrOvertaken,
        empty_poor: EmptyPoorRule {
            breaks_combo: false,
            counts_bp: true,
            counted: false,
        },
        headers: HeaderPolicy::DefExRankFirst,
        window_policy: WindowPolicy::Interpolated(ScaledRank {
            rank_percent: [25, 50, 75, 100, 75],
            absent_percent: 75,
            defexrank_percent: 75,
        }),
        default_total: DefaultTotal::NotesLinear,
        denominator: Denominator::TapsAndLnHeads,
        gauges,
        score: ScoreWeights {
            ex: PerJudgment {
                perfect: 2,
                great: 1,
                good: 0,
                bad: 0,
                poor: 0,
            },
            breaks_combo: PerJudgment {
                perfect: false,
                great: false,
                good: false,
                bad: true,
                poor: true,
            },
            counts_bp: PerJudgment {
                perfect: false,
                great: false,
                good: false,
                bad: true,
                poor: true,
            },
        },
        same_time_order: SameTimeOrder::InputsFirst,
        assignment: Assignment::ComboPriority,
        ln_mode: LnModeRule {
            default: LnMode::Ln,
            late_head: LateHeadRule::CompletesAtPress,
            head_eligibility: HeadEligibility::NotLateBad,
        },
        total_mode: TotalMode::ScalesIncrease,
        mine: MineRule::WhileHeld { per_unit: pct(1) },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lr2_style_validates() {
        assert_eq!(super::lr2_style().validate(), Ok(()));
    }
}
