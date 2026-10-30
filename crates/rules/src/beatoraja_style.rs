//! The beatoraja-style preset: similar behavior to beatoraja's 7-key rule,
//! from its public source code. It is "-style", not "compatible"
//! (design.md "Standard Presets").
//!
//! # Sources
//!
//! Every value cites a file and line at these commits:
//!
//! - beatoraja, <https://github.com/exch-bms2/beatoraja> at
//!   `83574048ba489082eb46df7fe9806b464bf1f526`, under
//!   `src/bms/player/beatoraja/`. The 7-key rule is `BMSPlayerRule.Beatoraja_7`
//!   (`play/BMSPlayerRule.java:14`): `GaugeProperty.SEVENKEYS` and
//!   `JudgeProperty.SEVENKEYS`.
//! - jbms-parser, <https://github.com/exch-bms2/jbms-parser> at
//!   `8108cd54a19275810e1f5d2c9586f008eb6db95e`, under `src/bms/model/`.
//!
//! | Value | Source |
//! | --- | --- |
//! | Note windows PERFECT ±20, GREAT ±60, GOOD ±150 ms, BAD 220 ms early and 280 ms late | `play/JudgeProperty.java:23`; pairs are {late edge, early edge} (`:58-59`), deviation is note − press (`play/JudgeManager.java:386`, `:403`) |
//! | LN-end edges PERFECT 120, GREAT 160, GOOD 200 ms before the end | `play/JudgeProperty.java:25` |
//! | Release earlier than the GOOD edge is BAD, never POOR | `play/JudgeManager.java:533-541` (`lnendJudge = 3`; the release margin is 0, `play/JudgeProperty.java:26`) |
//! | Release: the worse of head and release; held through the end keeps the head | `play/JudgeManager.java:534`, `:572-573` |
//! | Scratch notes: PERFECT ±30, GREAT ±70, GOOD ±160 ms, BAD 230 ms early and 290 ms late; scratch LN-end edges 130, 170, 210 ms; scratch empty POOR 160 ms behind | `play/JudgeProperty.java:24` (scratch notes and MS pair), `:27` (scratch LN ends), `:191-197` (the table per note type); a scratch press, release and miss read it (`play/JudgeManager.java:380`, `:494`, `:595`) |
//! | Empty POOR: a note at most 500 ms after the press or 150 ms before it (160 ms on the scratch), inclusive (the scan's far end: see the known differences) | `play/JudgeProperty.java:23-24` (MS pairs), `play/JudgeManager.java:401` |
//! | Empty POOR keeps the combo, adds to BP, has its own count | `play/JudgeProperty.java:29` (combo), `play/JudgeManager.java:660-668`, `BMSPlayer.java:915` (BP), `ScoreData.java:278` (MS count) |
//! | BAD and POOR break the combo and add to BP | `play/JudgeProperty.java:29`, `BMSPlayer.java:915` |
//! | Missed once its late BAD edge has passed (timing: see H below) | `play/JudgeManager.java:594-598` |
//! | `#RANK` 0-4: 25, 50, 75, 100, 125 %; no rank or an unknown `#RANK`: 75 % | `play/JudgeProperty.java:210`, `play/BMSPlayerRule.java:62`; jbms-parser `BMSModel.java:67,71` (default rank 2), `BMSDecoder.java:702-713` |
//! | `#DEFEXRANK n`: `n × 75 / 100` % in integer arithmetic, so `#DEFEXRANK 1` is 0 % and plays zero-width windows | `play/BMSPlayerRule.java:63` (75 is `#RANK 2`'s rate) |
//! | Judgment headers ([`HeaderPolicy::IntegerRanks`]): `#RANK` an integer token 0-4, `#DEFEXRANK` an integer token of at least 1, `#TOTAL` any number above 0; a later accepted one replaces an earlier one, and a refused one keeps it | jbms-parser `BMSDecoder.java:702-713`, `:716-727`, `:730-743` |
//! | Each window and LN-end edge scaled by `edge × rate / 100`, in µs; derived edges may tie | `play/JudgeProperty.java:234` |
//! | Empty POOR distances not scaled | `play/JudgeProperty.java:210` (`fixjudge`) |
//! | `#TOTAL` absent: 260 (see E below) | `play/BMSPlayerRule.java:75-78`, `:85`; jbms-parser `BMSModel.java:75,79` (a chart without `#TOTAL` keeps the bmson total type) |
//! | Gauge denominator: taps and LN heads, mines excluded | jbms-parser `TimeLine.java:139-155` |
//! | Gauges, `{min, max, start, clear}` and changes `{PG, GR, GD, BD, PR, MS}` | `play/GaugeProperty.java:87-92`, field order `:157` |
//! | Increases of ASSIST EASY, EASY and NORMAL are `value × TOTAL / notes` | `play/GrooveGauge.java:260` (`TOTAL` modifier) |
//! | A change is clamped to `min..=max`; a gauge at 0 never changes again | `play/GrooveGauge.java:218-222`, `:229-240` |
//! | Cleared when the value at the end is at or above the border; survival gauges have border 0 | `play/GrooveGauge.java:246-248` |
//! | Mines: 1 % per unit of the chart's value, while the lane is held as it passes, not reduced by the gauge | `play/JudgeManager.java:244-246`, `play/GrooveGauge.java:63-66`; jbms-parser `Section.java:513`, `MineNote.java` |
//! | EX score: PERFECT 2, GREAT 1 | `ScoreData.java:421-422` |
//! | LN mode | `play/JudgeManager.java:425-441` |
//!
//! # Not expressed
//!
//! These behaviors need an axis the preset model does not have yet
//! (design.md "Handling Differences Not Expressible as Numbers"). This
//! preset plays the stated substitute; each line names the example that
//! would differ.
//!
//! - **B. Note assignment and misses.** beatoraja's default assignment is
//!   combo priority (`play/JudgeAlgorithm.java:17-20`, `PlayConfig.java:103`):
//!   it takes the later note only when the earlier one cannot be hit GOOD or
//!   better. A press does not pass earlier notes: they stay hittable until
//!   their late BAD edge (`play/JudgeManager.java:594-598`). With notes at 0
//!   and 200 ms on one lane, a press at 120 ms is GOOD on the first note
//!   there, but GOOD on the second here (nearest, ties earlier).
//! - **C. HARD gauge damage while low.** Damage is multiplied by 0.4 below
//!   10 %, 0.5 below 20 %, 0.6 below 30 %, 0.7 below 40 % and 0.8 below 50 %
//!   (`play/GaugeProperty.java:90`, `play/GrooveGauge.java:229-238`); the
//!   model holds one step. Here HARD takes full damage: a BAD at 15 % leaves
//!   10 %, not 12.5 %.
//! - **D. HARD and EX HARD increases limited by `#TOTAL`.** An increase is
//!   multiplied by `min(0.15, (2 × TOTAL − 320) / notes) / 0.15`, at least 0
//!   (`play/GrooveGauge.java:264-270`). Here the increases are fixed, which
//!   matches whenever `2 × TOTAL − 320 ≥ 0.15 × notes`: a 1000-note chart
//!   with `#TOTAL 200` gains 0.08 % per PERFECT there, 0.15 % here.
//! - **E. `#TOTAL` when absent.** It is `max(260, 7.605 × notes / (0.01 ×
//!   notes + 6.5))` (`play/BMSPlayerRule.java:85`). Here it is 260, which
//!   matches up to 337 notes; a 1000-note chart without `#TOTAL` has about
//!   460.9 there.
//! - **G. A BAD long-note head.** It is held and scored at the release or the
//!   end (`play/JudgeManager.java:428`, `play/JudgeProperty.java:31`); here it
//!   is scored at the press. The counts are the same, but notes of other lanes
//!   judged during the hold extend the combo before the break there, after it
//!   here: combo 10, a BAD head, then 5 taps during the hold gives max combo
//!   15 there, 10 here.
//! - **H. When a miss is decided.** A note is missed only strictly after
//!   its late BAD edge, at the first frame past it
//!   (`play/JudgeManager.java:592-598`: `note time < now − 280 ms`); here
//!   the POOR is decided at the edge itself, after any input at that time.
//!   A press exactly on the edge is BAD in both, but the order of
//!   judgments can differ: at `#RANK 3`, with a tap at 1000 ms left unhit
//!   and another lane's tap hit at 1280.001 ms, beatoraja judges the press
//!   before the miss (NORMAL ends at 94 %, combo 0), while here the miss
//!   comes first (NORMAL 100 %, combo 1). The time of the POOR, and of a
//!   survival gauge's failure it causes, differs too.
//!
//! # Known differences
//!
//! - **Number syntax of judgment headers.** The header policy reads the
//!   loader's decimal grammar (design.md "Chart Time"), not Java's:
//!   `Double.parseDouble` also reads `200f`, `0x1p8`, `Infinity` and values
//!   beyond 12 integer digits, and `Integer.parseInt` non-ASCII digits. Such
//!   a declaration is not a number here, so `#TOTAL 200` then `#TOTAL 300f`
//!   keeps 200 here and takes 300 there. `#TOTAL` is also used in
//!   thousandths rounded down, not as a `double`.
//! - **Digits past the 18th fractional one.** They are cut off here, so a
//!   positive value below 10^-18 is 0 and not above 0: `#TOTAL 200` then
//!   `#TOTAL 1e-19` keeps 200 here, while `Double.parseDouble` keeps the
//!   second positive and jbms-parser takes it (`BMSDecoder.java:730-743`).
//! - **Whitespace around a value.** Non-ASCII whitespace (U+00A0, U+3000)
//!   is trimmed here, so `#RANK 3` then `#RANK` with U+00A0 around `2`
//!   reads rank 2 here; Java's `trim` removes only characters up to U+0020,
//!   so `Integer.parseInt` throws and rank 3 stays there
//!   (`BMSDecoder.java:367-368`, `:702-713`).
//! - **A `#DEFEXRANK` whose windows pass 2^32 µs.** Every edge here stays
//!   within the chart time domain, so from `#DEFEXRANK 1974699` (1 481 024 %,
//!   which takes the scratch's 290 ms late edge past 2^32 µs) the chart is
//!   refused, where the source plays windows over an hour wide; from
//!   28 633 116, `n × 75` also wraps in the source's `int` arithmetic
//!   (`play/BMSPlayerRule.java:63`).
//! - **The note scan's far end.** Implementation structure is not
//!   reproduced (design.md "What a -style Preset Reproduces"). The source
//!   scans a lane's notes up to the farthest early edge of the scaled note
//!   and scratch tables, their MS pairs' 500 ms included, and stops at a
//!   note exactly that far (`play/JudgeManager.java:189-196` for the bound,
//!   `:387-389` for the stop). Here a press looks at the notes inside its
//!   lane class's own BAD window and empty POOR range, each with its own
//!   edges. So a note exactly 500 ms after a press makes an empty POOR here
//!   at every scale, and there only once a scaled early BAD edge passes
//!   500 ms; and from about 218 %, where the scratch's early BAD edge is
//!   that far end, a scratch press exactly on it (501.4 ms early at
//!   `#DEFEXRANK 291`) is BAD here and judges nothing there.
//!
//! Each of the second and third is an example in
//! `crates/bms-parser/tests/judgment_headers.rs`, and the last two in
//! `crates/eval/tests/style_beatoraja.rs`.
//!
//! The CLASS gauges (course play only) are not included.

use bms_model::TimeUs;

use crate::{
    Assignment, Denominator, EmptyPoorRange, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule,
    GaugeValue, HeaderPolicy, Implementation, JudgeWindows, LaneTable, LateHeadRule, LnEndWindows,
    LnMode, LnModeRule, MineRule, MissRule, PerJudgment, PerLaneClass, RulePreset, SameTimeOrder,
    ScaledRank, ScoreWeights, TotalMode, Window, WindowPolicy,
};

const MS: i64 = 1_000;

/// `n` hundredths of a percent as a fixed change.
const fn centi(n: i64) -> GaugeDelta {
    GaugeDelta::Fixed(GaugeValue::centi_percent(n))
}

const fn pct(n: i64) -> GaugeValue {
    GaugeValue(n * GaugeValue::PERCENT)
}

/// A gauge decided at the end, with the `TOTAL` modifier on its increases:
/// PERFECT and GREAT `1.0 × TOTAL / notes`, GOOD `0.5 ×`.
fn end_gauge(id: &str, clear: i64, damage: [i64; 3]) -> GaugeRule {
    let full = GaugeDelta::TotalShare { num: 1, den: 1 };
    let [bad, poor, empty] = damage;
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::EndThreshold { clear: pct(clear) },
        start: pct(20),
        min: pct(2),
        max: GaugeValue::FULL,
        change: PerJudgment {
            perfect: full,
            great: full,
            good: GaugeDelta::TotalShare { num: 1, den: 2 },
            bad: centi(-bad),
            poor: centi(-poor),
        },
        empty_poor: centi(-empty),
        low_damage: None,
    }
}

/// A survival gauge from 100 %, with fixed changes in hundredths of a
/// percent: PERFECT, GREAT, GOOD, BAD, POOR, empty POOR.
fn survival_gauge(id: &str, change: [i64; 6]) -> GaugeRule {
    let [perfect, great, good, bad, poor, empty] = change.map(centi);
    GaugeRule {
        id: id.to_owned(),
        kind: GaugeKind::Survival,
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
        low_damage: None,
    }
}

/// The beatoraja-style preset, the default play preset.
#[must_use]
pub fn beatoraja_style() -> RulePreset {
    // NORMAL comes first: the displayed gauge defaults to index 0.
    let gauges = vec![
        end_gauge("normal", 80, [300, 600, 200]),
        end_gauge("assist_easy", 60, [150, 300, 50]),
        end_gauge("easy", 80, [150, 450, 100]),
        survival_gauge("hard", [15, 12, 3, -500, -1_000, -500]),
        survival_gauge("ex_hard", [15, 6, 0, -800, -1_600, -800]),
        survival_gauge("hazard", [15, 6, 0, -10_000, -10_000, -1_000]),
    ];
    RulePreset {
        id: "beatoraja-style".to_owned(),
        version: 1,
        implementation: Implementation::Standard { version: 2 },
        tables: PerLaneClass {
            keys: LaneTable {
                windows: JudgeWindows {
                    perfect: Window::symmetric(20 * MS),
                    great: Window::symmetric(60 * MS),
                    good: Window::symmetric(150 * MS),
                    bad: Window {
                        early: TimeUs(220 * MS),
                        late: TimeUs(280 * MS),
                    },
                },
                ln_end: LnEndWindows {
                    perfect: TimeUs(120 * MS),
                    great: TimeUs(160 * MS),
                    good: TimeUs(200 * MS),
                },
                empty_poor: EmptyPoorRange {
                    ahead: TimeUs(500 * MS),
                    behind: TimeUs(150 * MS),
                },
            },
            scratch: LaneTable {
                windows: JudgeWindows {
                    perfect: Window::symmetric(30 * MS),
                    great: Window::symmetric(70 * MS),
                    good: Window::symmetric(160 * MS),
                    bad: Window {
                        early: TimeUs(230 * MS),
                        late: TimeUs(290 * MS),
                    },
                },
                ln_end: LnEndWindows {
                    perfect: TimeUs(130 * MS),
                    great: TimeUs(170 * MS),
                    good: TimeUs(210 * MS),
                },
                empty_poor: EmptyPoorRange {
                    ahead: TimeUs(500 * MS),
                    behind: TimeUs(160 * MS),
                },
            },
        },
        miss: MissRule::PoorAfterLateBadOrOvertaken,
        empty_poor: EmptyPoorRule {
            breaks_combo: false,
            counts_bp: true,
            counted: false,
        },
        headers: HeaderPolicy::IntegerRanks,
        window_policy: WindowPolicy::Scaled(ScaledRank {
            rank_percent: [25, 50, 75, 100, 125],
            absent_percent: 75,
            defexrank_percent: 75,
        }),
        default_total_milli: 260_000,
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
        assignment: Assignment::NearestTiesEarlier,
        ln_mode: LnModeRule {
            default: LnMode::Ln,
            late_head: LateHeadRule::CompletesAtPress,
        },
        total_mode: TotalMode::ScalesIncrease,
        mine: MineRule::WhileHeld { per_unit: pct(1) },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn beatoraja_style_validates() {
        assert_eq!(super::beatoraja_style().validate(), Ok(()));
    }
}
