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
//! | Note assignment ([`Assignment::ComboPriority`]): the unjudged notes inside BAD in time order, the first taken, and a later one taking over only when the press is past the taken note's late GOOD edge and the later note is not before its early GOOD edge; an LN head is a candidate like a tap; mines and LN ends are never candidates | `play/JudgeAlgorithm.java:17-20` (`Combo`), the default `PlayConfig.java:103`; the candidate loop `play/JudgeManager.java:385-417` |
//! | Empty POOR when no note is taken, from the same scan: a judged note inside the MS pair, or an unjudged one inside it but outside every window (the same range for both) | `play/JudgeManager.java:400-404`, `:419` |
//! | Empty POOR: a note at most 500 ms after the press or 150 ms before it (160 ms on the scratch), inclusive (the scan's far end: see the known differences) | `play/JudgeProperty.java:23-24` (MS pairs), `play/JudgeManager.java:401` |
//! | Empty POOR keeps the combo, adds to BP, has its own count | `play/JudgeProperty.java:29` (combo), `play/JudgeManager.java:660-668`, `BMSPlayer.java:915` (BP), `ScoreData.java:278` (MS count) |
//! | BAD and POOR break the combo and add to BP | `play/JudgeProperty.java:29`, `BMSPlayer.java:915` |
//! | Missed once its late BAD edge has passed (timing: see the known differences) | `play/JudgeManager.java:594-598` |
//! | `#RANK` 0-4: 25, 50, 75, 100, 125 %; no rank or an unknown `#RANK`: 75 % | `play/JudgeProperty.java:210`, `play/BMSPlayerRule.java:62`; jbms-parser `BMSModel.java:67,71` (default rank 2), `BMSDecoder.java:702-713` |
//! | `#DEFEXRANK n`: `n × 75 / 100` %, applied to each edge as it is (see the known differences) | `play/BMSPlayerRule.java:63` (75 is `#RANK 2`'s rate) |
//! | Judgment headers ([`HeaderPolicy::DefExRankFirst`]): `#RANK` an integer token 0-4, `#DEFEXRANK` any number above 0, `#TOTAL` any number above 0; a later accepted one replaces an earlier one, a refused one keeps it, and a `#DEFEXRANK` counts over any `#RANK` | jbms-parser `BMSDecoder.java:702-713`, `:716-727`, `:730-743`; the precedence, jbms-parser a7a0729 and beatoraja's chart documentation |
//! | Each window and LN-end edge scaled by `edge × rate / 100`, in µs; derived edges may tie | `play/JudgeProperty.java:234` |
//! | Empty POOR distances not scaled | `play/JudgeProperty.java:210` (`fixjudge`) |
//! | `#TOTAL` absent ([`DefaultTotal::NotesCurve`]): `max(260, 7.605 N / (0.01 N + 6.5))` over the whole chart's notes, 260 up to 337 notes | `play/BMSPlayerRule.java:68-78` (a `#TOTAL` not above 0 counts as absent), `:85`; jbms-parser `BMSModel.java:75,79` (a chart without `#TOTAL` keeps the bmson total type, whose 100 % is that value) |
//! | A `#TOTAL` not above 0 is refused and an earlier one stays | jbms-parser `BMSDecoder.java:730-743` |
//! | Gauge denominator: taps and LN heads, mines excluded | jbms-parser `TimeLine.java:139-155` |
//! | Gauges, `{min, max, start, clear}` and changes `{PG, GR, GD, BD, PR, MS}` | `play/GaugeProperty.java:87-92`, field order `:157` |
//! | Increases of ASSIST EASY, EASY and NORMAL are `value × TOTAL / notes` | `play/GrooveGauge.java:260` (`TOTAL` modifier) |
//! | Increases of HARD and EX HARD × `max(0, min(0.15, (2 × TOTAL − 320) / notes)) / 0.15` ([`TotalCorrection::LimitIncrease`]), computed once per chart from the `#TOTAL` in effect | `play/GaugeProperty.java:90-91` (`LIMIT_INCREMENT`), `play/GrooveGauge.java:264-270`; applied to the per-judgment values when the gauge is made, `:207-211` (`play/BMSPlayer.java:386`), from the `#TOTAL` the chart's load set, the default included (`PlayerResource.java:215`) |
//! | HARD damage while low, chosen from the value before the change: × 0.4 below 10 %, 0.5 below 20 %, 0.6 below 30 %, 0.7 below 40 %, 0.8 below 50 %, the first band that applies | `play/GaugeProperty.java:90`, `play/GrooveGauge.java:229-238` |
//! | The modifiers and damage while low apply to the MS change too, never to a mine | `play/GrooveGauge.java:207-211` (every value of the six), `:229-238`; a mine adds its value directly, `:63-66` |
//! | A change is clamped to `min..=max`; a gauge at 0 never changes again | `play/GrooveGauge.java:218-222`, `:229-240` |
//! | Cleared when the value at the end is at or above the border; survival gauges have border 0 | `play/GrooveGauge.java:246-248` |
//! | Mines: 1 % per unit of the chart's value, while the lane is held as it passes, not reduced by the gauge | `play/JudgeManager.java:244-246`, `play/GrooveGauge.java:63-66`; jbms-parser `Section.java:513`, `MineNote.java` |
//! | EX score: PERFECT 2, GREAT 1 | `ScoreData.java:421-422` |
//! | LN mode | `play/JudgeManager.java:425-441` |
//!
//! # Known differences
//!
//! Implementation structure is not reproduced (design.md "What a -style
//! Preset Reproduces"); the first six below are instances.
//!
//! - **Notes a press passes (decided).** When a press takes a later note,
//!   the earlier unjudged notes of its lane are POOR at the press here. The
//!   source keeps them hittable until their own late BAD edge: its judge
//!   algorithms only choose which note a press takes (added in caef58b5,
//!   changed in d9b70361 and d19e05b5), and misses are a separate timeout
//!   pass (`play/JudgeManager.java:594-598`), with no stated intent to keep
//!   them. At `#RANK 3`, with notes at 0 and 200 ms on one lane, a press at
//!   170 ms takes the 200 ms note (GREAT); a second press at 250 ms is BAD
//!   on the 0 ms note there, while here that note was POOR at 170 ms.
//! - **When a BAD long-note head is scored (decided).** The source holds it
//!   and scores it at the release or the end (`play/JudgeManager.java:428`,
//!   `play/JudgeProperty.java:31`); here it is scored at the press, when its
//!   judgment is decided, since a BAD head cannot get worse. The counts are
//!   the same, but notes of other lanes judged during the hold extend the
//!   combo before the break there, after it here: combo 10, a BAD head,
//!   then 5 taps during the hold gives max combo 15 there, 10 here.
//! - **When a miss is decided.** The source misses a note at the first
//!   frame strictly past its late BAD edge
//!   (`play/JudgeManager.java:592-598`: `note time < now − 280 ms`), which
//!   its frame loop gives; here the POOR is decided at the edge itself,
//!   after any input at that time. A press exactly on the edge is BAD in
//!   both, but the order of judgments can differ: at `#RANK 3`, with a tap
//!   at 1000 ms left unhit and another lane's tap hit at 1280.001 ms,
//!   beatoraja judges the press before the miss (NORMAL ends at 94 %,
//!   combo 0), while here the miss comes first (NORMAL 100 %, combo 1). The
//!   time of the POOR, and of a survival gauge's failure it causes, differs
//!   too.
//! - **`#DEFEXRANK` as a number, over `#RANK`.** The source parses it with
//!   `Integer.parseInt` (jbms-parser `BMSDecoder.java:716-727`) and, since
//!   jbms-parser d0fba6d, lets a later `#RANK` replace it; here it is any
//!   number above 0 and counts over any `#RANK`, as the decoder that
//!   introduced it did (a7a0729) and beatoraja's chart documentation says.
//!   `#RANK 3` then `#DEFEXRANK 120.5` plays 90.375 % here and #RANK 3
//!   there; `#DEFEXRANK 90` then `#RANK 3` plays 67.5 % here and #RANK 3
//!   there.
//! - **`#DEFEXRANK`'s percentage is not truncated.** The source stores
//!   `n × 75 / 100` as an `int` (`play/BMSPlayerRule.java:59-65`, under a
//!   TODO); here each edge takes it as it is. `#DEFEXRANK 1` is 0 % there,
//!   zero-width windows, and 0.75 % here (PERFECT ±150 µs); `#DEFEXRANK 101`
//!   gives PERFECT ±15 ms there, ±15.15 ms here.
//! - **The note scan's far end.** The source scans a lane's notes up to the
//!   farthest early edge of the scaled note and scratch tables, their MS
//!   pairs' 500 ms included, and stops at a note exactly that far
//!   (`play/JudgeManager.java:189-196` for the bound, `:387-389` for the
//!   stop). Here a press looks at the notes inside its lane class's own BAD
//!   window and empty POOR range, each with its own edges. So when the press
//!   judges no note, a note exactly 500 ms after it makes an empty POOR here
//!   at every scale, and there only once a scaled early BAD edge passes
//!   500 ms; and from about 218 %, where the scratch's early BAD edge is
//!   that far end, a scratch press exactly on it (503.7 ms early at
//!   `#DEFEXRANK 292`) is BAD here and judges nothing there.
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
//!   within the chart time domain, so from `#DEFEXRANK 1974698`
//!   (1 481 023.5 %, which takes the scratch's 290 ms late edge past 2^32 µs)
//!   the chart is refused, where the source plays windows over an hour wide;
//!   from 28 633 116, `n × 75` also wraps in the source's `int` arithmetic
//!   (`play/BMSPlayerRule.java:63`).
//! - **Gauge arithmetic.** The source keeps gauges in `f32` and computes
//!   its modifiers once per chart in `double`, then `f32`
//!   (`play/GrooveGauge.java:188`, `:207-211`, `:260-270`); here a gauge is
//!   fixed-point in 1/100000 %, each change is computed exactly from its
//!   base, correction and reduction and rounded down once, and the default
//!   `#TOTAL` is in thousandths rounded down like a declared one. From
//!   20 %, 600 increases of 0.1 % (600 PERFECTs at `#TOTAL 60`) reach about
//!   79.9993 % in `f32`, below NORMAL's 80 % clear, and exactly 80 % here,
//!   a clear; a 1000-note chart without `#TOTAL` has 460.909 here and
//!   460.909 09… there. These are arithmetic examples, not runs of the
//!   source.
//!
//! The digits and whitespace differences are examples in
//! `crates/bms-parser/tests/judgment_headers.rs`; the `#DEFEXRANK`, scan
//! and 2^32 µs differences are examples in
//! `crates/eval/tests/style_beatoraja.rs`.
//!
//! The CLASS gauges (course play only) are not included.

use bms_model::TimeUs;

use crate::gauge_rules::{end_gauge, pct, survival_gauge};

use crate::{
    Assignment, DefaultTotal, Denominator, EmptyPoorRange, EmptyPoorRule, GaugeValue, HeaderPolicy,
    Implementation, JudgeWindows, LaneTable, LateHeadRule, LnEndWindows, LnMode, LnModeRule,
    LowDamage, MineRule, MissRule, PerJudgment, PerLaneClass, RulePreset, SameTimeOrder,
    ScaledRank, ScoreWeights, TotalCorrection, TotalMode, Window, WindowPolicy,
};

const MS: i64 = 1_000;

/// The increases of the gauges decided at the end: PERFECT and GREAT
/// `1.0 × TOTAL / notes`, GOOD `0.5 ×`.
const BEST_GOOD: [(u32, u32); 2] = [(1, 1), (1, 2)];

/// HARD's damage while low: × `tenths / 10` below `below` %, the first band
/// that applies.
fn hard_bands() -> Vec<LowDamage> {
    [(10, 4), (20, 5), (30, 6), (40, 7), (50, 8)]
        .into_iter()
        .map(|(below, tenths)| LowDamage {
            below: pct(below),
            num: tenths,
            den: 10,
        })
        .collect()
}

/// The beatoraja-style preset, the default play preset.
#[must_use]
pub fn beatoraja_style() -> RulePreset {
    // NORMAL comes first: the displayed gauge defaults to index 0. Lamp
    // ranks, easiest first: ASSIST EASY, EASY, NORMAL, HARD, EX HARD,
    // HAZARD (design.md "Standard Presets").
    let gauges = vec![
        end_gauge("normal", 2, 80, BEST_GOOD, [300, 600, 200]),
        end_gauge("assist_easy", 0, 60, BEST_GOOD, [150, 300, 50]),
        end_gauge("easy", 1, 80, BEST_GOOD, [150, 450, 100]),
        survival_gauge(
            "hard",
            3,
            GaugeValue::EMPTY,
            [15, 12, 3, -500, -1_000, -500],
            TotalCorrection::LimitIncrease,
            hard_bands(),
        ),
        survival_gauge(
            "ex_hard",
            4,
            GaugeValue::EMPTY,
            [15, 6, 0, -800, -1_600, -800],
            TotalCorrection::LimitIncrease,
            Vec::new(),
        ),
        survival_gauge(
            "hazard",
            5,
            GaugeValue::EMPTY,
            [15, 6, 0, -10_000, -10_000, -1_000],
            TotalCorrection::None,
            Vec::new(),
        ),
    ];
    RulePreset {
        id: "beatoraja-style".to_owned(),
        version: 1,
        implementation: Implementation::Standard { version: 3 },
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
        headers: HeaderPolicy::DefExRankFirst,
        window_policy: WindowPolicy::Scaled(ScaledRank {
            rank_percent: [25, 50, 75, 100, 125],
            absent_percent: 75,
            defexrank_percent: 75,
        }),
        default_total: DefaultTotal::NotesCurve,
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
