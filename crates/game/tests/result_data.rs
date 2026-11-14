//! What a result skin's graphs read beside the built-in ones (design.md
//! "Play Lifecycle", the result's views): the timing statistics' samples
//! by ms, the key lanes' judge areas and the judged notes per timeline
//! bin, through `ViewSummary::of` and `ResultBasis::of`.

mod common;

use bms_model::{Chart, ChartBuilder, HeaderValue, JudgmentHeader, JudgmentHeaderName, TimeUs};
use common::lane;
use eval::{Evaluation, JudgeEvent, JudgeEventKind};
use game::{Axis, DEVIATION_SLOTS, ResultBasis, Timing, ViewSummary};
use replay::verify::Coverage;
use rules::{EmptyPoorRange, JudgeWindows, Judgment, RankScale, RulePreset, Window, WindowPolicy};

fn judged(deviation: Option<i64>, release: Option<i64>) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(0),
        origin: None,
        kind: JudgeEventKind::Judged {
            lane: lane(1),
            note: 0,
            judgment: Judgment::Good,
            deviation: deviation.map(TimeUs),
            release: release.map(TimeUs),
            combo: 0,
            presses: 0,
        },
    }
}

/// An evaluation holding `events` and nothing else.
fn evaluation(events: Vec<JudgeEvent>) -> Evaluation {
    Evaluation {
        events,
        tally: eval::Tally::default(),
        gauges: Vec::new(),
        completed: true,
        final_deadline: TimeUs(10_000_000),
    }
}

/// The summary of `e` on `chart` under `preset`, the play's own.
fn summary(e: &Evaluation, chart: &Chart, preset: &RulePreset) -> ViewSummary {
    ViewSummary::of(
        e,
        chart,
        preset,
        ResultBasis::of(chart, None),
        Axis {
            origin: TimeUs(0),
            end: e.final_deadline,
        },
        Coverage::Recorded,
        true,
        &[],
        None,
    )
}

fn one_tap() -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000_000))
        .build()
        .unwrap()
}

/// C11: the histogram holds the timing statistics' samples (early
/// positive) in centred 1 ms bins, each from 500 µs below its ms up to
/// 500 µs above, excluded: 150 000 µs early in slot 300, as late in slot
/// 0, 150 001 µs either way in none; 499 µs either way and 500 µs late in
/// slot 150; 500 µs and 999 µs early in slot 151; 999 µs late in slot
/// 149; a long note by the tally's side (its release when farther, or on
/// a tie, else its head); a miss and an empty POOR in none. It counts as
/// many samples as the statistics, and the built-in 2 ms histogram is the
/// presses' as before.
#[test]
fn the_histogram_holds_the_timing_samples_by_centred_ms() {
    let empty_poor = JudgeEvent {
        kind: JudgeEventKind::EmptyPoor {
            lane: lane(1),
            deviation: TimeUs(0),
            presses: 0,
        },
        ..judged(None, None)
    };
    let events = vec![
        judged(Some(-150_000), None),
        judged(Some(150_000), None),
        judged(Some(-150_001), None),
        judged(Some(150_001), None),
        judged(Some(-499), None),
        judged(Some(499), None),
        judged(Some(-500), None),
        judged(Some(500), None),
        judged(Some(-999), None),
        judged(Some(999), None),
        // The release 40 ms early is farther than the head 30 ms late.
        judged(Some(30_000), Some(-40_000)),
        // The head 50 ms late is farther than the release 10 ms early.
        judged(Some(50_000), Some(-10_000)),
        // A tie: the release, 20 ms late.
        judged(Some(-20_000), Some(20_000)),
        judged(None, None),
        empty_poor,
    ];
    let chart = one_tap();
    let e = evaluation(events);
    let s = summary(&e, &chart, &rules::tapestry());
    let mut want = [0_u32; DEVIATION_SLOTS];
    want[300] = 1;
    want[0] = 1;
    want[150] = 3;
    want[151] = 2;
    want[149] = 1;
    want[150 + 40] = 1;
    want[150 - 50] = 1;
    want[150 - 20] = 1;
    assert_eq!(s.graphs.deviations, want);
    let samples: u32 = s.graphs.deviations.iter().sum();
    assert_eq!(Some(samples), s.figures.timing.map(|t| t.samples));
    assert_eq!(s.graphs.timing, Timing::of(&e.events));
    assert_eq!(s.graphs.timing.samples(), 13);
}

/// A preset whose key and scratch tables differ, each edge asymmetric,
/// scaled to half at `#RANK 0` and unchanged without a rank.
fn asymmetric() -> RulePreset {
    let mut p = rules::tapestry();
    let w = |early, late| Window {
        early: TimeUs(early),
        late: TimeUs(late),
    };
    p.tables.keys.windows = JudgeWindows {
        perfect: w(20_000, 31_998),
        great: w(41_999, 60_000),
        good: w(100_000, 80_002),
        bad: w(150_000, 220_000),
    };
    p.tables.keys.empty_poor = EmptyPoorRange {
        ahead: TimeUs(150_999),
        behind: TimeUs(0),
    };
    p.tables.scratch.windows = JudgeWindows {
        perfect: w(1_000, 1_000),
        great: w(2_000, 2_000),
        good: w(3_000, 3_000),
        bad: w(4_000, 4_000),
    };
    p.tables.scratch.empty_poor = EmptyPoorRange {
        ahead: TimeUs(9_000),
        behind: TimeUs(9_000),
    };
    p.window_policy = WindowPolicy::Thousandths(RankScale {
        rank_milli: [50_000, 75_000, 100_000, 100_000],
        absent_milli: 100_000,
    });
    p
}

/// C12: the areas are the key lanes' windows as the chart's rank prepares
/// them, `[−late, early]` in ms truncated toward 0, and the empty POOR
/// range, never scaled, `[−behind, ahead]`.
#[test]
fn the_areas_are_the_key_windows_for_the_charts_rank() {
    let preset = asymmetric();
    let e = evaluation(Vec::new());
    let plain = one_tap();
    let s = summary(&e, &plain, &preset);
    assert_eq!(
        s.figures.areas,
        [[-31, 20], [-60, 41], [-80, 100], [-220, 150], [0, 150]]
    );
    let mut ranked = one_tap();
    ranked.meta.judgment_headers = vec![JudgmentHeader {
        name: JudgmentHeaderName::Rank,
        value: HeaderValue::integer(0),
    }];
    let s = summary(&e, &ranked, &preset);
    // Halved and truncated: 15 999 µs late is 15 ms, 20 999.5 µs early
    // 20 ms, 40 001 µs late 40 ms.
    assert_eq!(
        s.figures.areas,
        [[-15, 10], [-30, 20], [-40, 50], [-110, 75], [0, 150]]
    );
}

/// C13: each judged note counts in its bin on the final axis. Lane 1's
/// notes at 10.5 s and 20 s are scanned before lane 7's at 600 s widens
/// the bins to 2 s: they count in bins 5 and 10 of 301. A practice
/// section from 10 s to 30 s keeps 1 s bins from its start; a mine and a
/// long note ending past the section count in none. A bin holds 65 536
/// notes without saturating.
#[test]
fn notes_count_in_their_final_bins() {
    let s = |ms: i64| TimeUs(ms * 1_000);
    let chart = ChartBuilder::new(150_000)
        .tap(lane(1), s(10_500))
        .tap(lane(1), s(20_000))
        .mine(lane(2), s(10_500), 10)
        .long(lane(3), s(29_000), s(31_000))
        .tap(lane(7), s(600_000))
        .build()
        .unwrap();
    let basis = ResultBasis::of(&chart, None);
    assert_eq!(
        (basis.timeline.width, basis.timeline.count, basis.notes),
        (s(2_000), 301, 4)
    );
    let mut want = [0_u32; 512];
    want[5] = 1;
    want[10] = 1;
    want[14] = 1; // the long note at 29 s
    want[300] = 1;
    assert_eq!(basis.notes_per_bin, want);
    assert_eq!(basis.notes_used(), &want[..301]);
    let section = ResultBasis::of(&chart, Some((s(10_000), s(30_000))));
    assert_eq!(
        (
            section.timeline.width,
            section.timeline.count,
            section.notes
        ),
        (s(1_000), 11, 2)
    );
    let mut want = [0_u32; 512];
    want[0] = 1;
    want[10] = 1;
    assert_eq!(section.notes_per_bin, want);
    // 65 536 taps in one 1 s bin, 8 192 a lane over the 8 lanes: one more
    // than a `u16` holds.
    let mut b = ChartBuilder::new(150_000);
    for l in 0..8 {
        for i in 0..8_192 {
            b = b.tap(lane(l), TimeUs(i * 100));
        }
    }
    let dense = ResultBasis::of(&b.build().unwrap(), None);
    assert_eq!(dense.notes, 65_536);
    assert_eq!(dense.notes_used(), &[65_536]);
}
