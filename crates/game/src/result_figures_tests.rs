//! The figures against the source's arithmetic, run on the same inputs
//! (the 0.8.9 `ScoreDataProperty` rate and next-grade loop, the
//! `TimingDistribution` statistics and numbers 374-377), with each stated
//! difference: exact integers where the source uses floats, µs samples,
//! no miss samples, and nothing shown without a sample.

use bms_model::{Lane, TimeUs};
use eval::{JudgeEvent, JudgeEventKind};
use rules::Judgment;

use super::{Rate, TimelineAxis, TimingStats, next_rank};

/// `(ex, max_ex, 102, 103, grade, 154)` as the source computed them for
/// the largest EX score `notes * 2`; every row agrees with the exact
/// figures, rates truncated, a grade boundary at `27 · ex = k · max_ex`
/// met and one below it not, except 1333 of 2000, whose 66.65 % the
/// source's float rate shows as 66.64 ([`FLOAT_RATES`]), and 29 of 54,
/// whose next grade the source's float product puts 2 away
/// ([`FLOAT_NEXT`]).
const SOURCE_RANKS: [(u32, u32, u32, u32, u8, u32); 26] = [
    (0, 270, 0, 0, 0, 30),
    (59, 270, 21, 85, 0, 1),
    (60, 270, 22, 22, 1, 30),
    (61, 270, 22, 59, 1, 29),
    (89, 270, 32, 96, 1, 1),
    (90, 270, 33, 33, 2, 30),
    (239, 270, 88, 51, 6, 1),
    (240, 270, 88, 88, 7, 30),
    (241, 270, 89, 25, 7, 29),
    (270, 270, 100, 0, 7, 0),
    (1, 2000, 0, 5, 0, 222),
    (1333, 2000, 66, 64, 4, 1),
    (1334, 2000, 66, 70, 5, 222),
    (1777, 2000, 88, 85, 6, 1),
    (1778, 2000, 88, 90, 7, 222),
    (1999, 2000, 99, 95, 7, 1),
    (1, 6, 16, 66, 0, 1),
    (2, 6, 33, 33, 2, 1),
    (3, 6, 50, 0, 3, 1),
    (4, 6, 66, 66, 5, 1),
    (5, 6, 83, 33, 6, 1),
    (6, 6, 100, 0, 7, 0),
    (2009, 2500, 80, 36, 6, 214),
    (1234, 6666, 18, 51, 0, 248),
    (5678, 9998, 56, 79, 4, 988),
    (29, 54, 53, 70, 3, 2),
];

/// The rows where the source's float rate loses its last digit:
/// `(ex, max_ex, the exact 103)`.
const FLOAT_RATES: [(u32, u32, u32); 1] = [(1333, 2000, 65)];

/// The rows where the source's float product `rate · max_ex` falls just
/// below the EX score before its `ceil`: `(ex, max_ex, the exact 154)`.
/// At 29 of 54 the next step, 15/27, is EX 30, one away, where the source
/// shows 2 (`ScoreDataProperty.java:100-104`); tapestry keeps the exact
/// value, a stated deviation.
const FLOAT_NEXT: [(u32, u32, u32); 1] = [(29, 54, 1)];

#[test]
fn rates_grades_and_next_grade_match_the_source() {
    for (ex, max, whole, mut hundredths, grade, mut next) in SOURCE_RANKS {
        if let Some(&(_, _, exact)) = FLOAT_RATES.iter().find(|r| (r.0, r.1) == (ex, max)) {
            hundredths = exact;
        }
        if let Some(&(_, _, exact)) = FLOAT_NEXT.iter().find(|r| (r.0, r.1) == (ex, max)) {
            next = exact;
        }
        let r = Rate::of(ex, max).unwrap();
        assert_eq!(
            (
                r.rate_bp / 100,
                r.rate_bp % 100,
                r.grade,
                next_rank(ex, max)
            ),
            (whole, hundredths, grade, next),
            "ex {ex} of {max}"
        );
    }
    // No largest EX score: no rate (the source shows 100 %).
    assert_eq!(Rate::of(0, 0), None);
    // Each grade's boundary, met and missed by one 27th of a point.
    for (k, low) in [6_u32, 9, 12, 15, 18, 21, 24].into_iter().enumerate() {
        let max = 27 * 1_000;
        let at = Rate::of(low * 1_000, max).unwrap();
        let below = Rate::of(low * 1_000 - 1, max).unwrap();
        assert_eq!(usize::from(at.grade), k + 1, "{low}");
        assert_eq!(usize::from(below.grade), k, "{low}");
    }
    // The rate's boundary: 10 000 · ex / max an integer, and just below.
    assert_eq!(Rate::of(1, 2_000).unwrap().rate_bp, 5);
    assert_eq!(Rate::of(1, 2_001).unwrap().rate_bp, 4);
}

/// A press `d` µs from its note (press − note), judged GREAT.
fn press(d: i64) -> JudgeEvent {
    judged(Some(TimeUs(d)), None)
}

fn judged(deviation: Option<TimeUs>, release: Option<TimeUs>) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(0),
        origin: None,
        kind: JudgeEventKind::Judged {
            lane: Lane::SCRATCH,
            note: 0,
            judgment: Judgment::Great,
            deviation,
            release,
            combo: 0,
            presses: 0,
        },
    }
}

/// Samples in whole ms, early positive, as µs events.
fn ms(samples: &[i64]) -> Vec<JudgeEvent> {
    samples.iter().map(|&s| press(-s * 1_000)).collect()
}

/// Numbers 374, 375, 376 and 377.
type Numbers = (i32, i32, u32, u32);

/// 374, 375, 376 and 377 of `stats`.
fn numbers(stats: TimingStats) -> Numbers {
    let (m, s) = (stats.mean_x100, stats.sd_x100);
    (m / 100, m % 100, s / 100, s % 100)
}

/// (a) The source's table, run: each row's whole-ms samples give its
/// numbers, except the stated float cases, where the exact value differs.
#[test]
fn timing_numbers_match_the_source_baseline() {
    let rep = |n: usize, v: i64| vec![v; n];
    let rows: [(Vec<i64>, Numbers); 9] = [
        (vec![1, 2], (1, 50, 0, 50)),
        (vec![-1, -2], (-1, -50, 0, 50)),
        (vec![1, 1, 0], (0, 66, 0, 47)),
        (vec![-1, 0, 0], (0, -33, 0, 47)),
        ([rep(29, 1), rep(71, 0)].concat(), (0, 29, 0, 45)),
        (vec![151, 0], (0, 0, 0, 0)),
        (vec![150, -150], (0, 0, 150, 0)),
        ([rep(7, 3), rep(3, -4)].concat(), (0, 90, 3, 20)),
        (vec![0, 10], (5, 0, 5, 0)),
    ];
    for (samples, want) in rows {
        let stats = TimingStats::of(&ms(&samples)).unwrap();
        assert_eq!(numbers(stats), want, "{samples:?}");
    }
    // The source's floats: four of −150 ms and one of −147 ms give
    // −149/−39 and 1/19; the exact mean is −149.40 ms and the deviation
    // 1.20 ms.
    let floats = TimingStats::of(&ms(&[-150, -150, -150, -150, -147])).unwrap();
    assert_eq!(numbers(floats), (-149, -40, 1, 20));
    // No sample: nothing, where the source shows 2147483647/47 and −1/0.
    assert_eq!(TimingStats::of(&[]), None);
}

/// (b) µs samples: 1.5 ms and 0, where the source's ms samples (1 and 0)
/// give 0/50 for both; (c) the range's edge; (d) a miss, an empty POOR and
/// a held head's feedback are no samples, and a long note counts once, by
/// its head when its release lies closer.
#[test]
fn timing_samples_are_presses_in_us_within_the_range() {
    let stats = TimingStats::of(&[press(-1_500), press(0)]).unwrap();
    assert_eq!(numbers(stats), (0, 75, 0, 75));
    assert_eq!(stats.samples, 2);
    let edge = TimingStats::of(&[
        press(-150_999),
        press(150_999),
        press(151_000),
        press(-151_000),
    ])
    .unwrap();
    assert_eq!(edge.samples, 2);
    assert_eq!(numbers(edge), (0, 0, 150, 99));
    let mut events = vec![press(-2_000), judged(None, None)];
    events.push(JudgeEvent {
        kind: JudgeEventKind::EmptyPoor {
            lane: Lane::SCRATCH,
            deviation: TimeUs(-3_000),
            presses: 1,
        },
        ..press(0)
    });
    events.push(JudgeEvent {
        kind: JudgeEventKind::LnHeld {
            lane: Lane::SCRATCH,
            note: 0,
            judgment: Judgment::Great,
            deviation: TimeUs(-9_000),
            presses: 1,
        },
        ..press(0)
    });
    // A long note released 1 ms early, closer than its head: its head's.
    events.push(judged(Some(TimeUs(-4_000)), Some(TimeUs(-1_000))));
    let stats = TimingStats::of(&events).unwrap();
    assert_eq!(stats.samples, 2);
    assert_eq!(stats.mean_x100, 300);
}

/// A long note finished by its release gives one sample: the farther of
/// its head's press and its release from its time, the release on a tie,
/// as the tally decides its side; a release closer than the head leaves
/// the head's. The expectations are the source's arithmetic on whole-ms
/// samples (`play/JudgeManager.java:512-519,625`,
/// `result/MusicResult.java:347-354`).
#[test]
fn a_long_note_samples_the_farther_of_its_head_and_release() {
    // A tap 2 ms early; a head 4 ms early, released 50 ms early: the
    // samples are 2 and 50 ms.
    let farther = TimingStats::of(&[
        press(-2_000),
        judged(Some(TimeUs(-4_000)), Some(TimeUs(-50_000))),
    ])
    .unwrap();
    assert_eq!(numbers(farther), (26, 0, 24, 0));
    // A head 10 ms late, released 10 ms early: the tie takes the release,
    // +10 ms; with a tap 2 ms early the samples are 2 and 10 ms.
    let tie = TimingStats::of(&[
        press(-2_000),
        judged(Some(TimeUs(10_000)), Some(TimeUs(-10_000))),
    ])
    .unwrap();
    assert_eq!(numbers(tie), (6, 0, 4, 0));
    // A head 30 ms early, released 5 ms early: the head's, 30 ms.
    let head = TimingStats::of(&[
        press(-2_000),
        judged(Some(TimeUs(-30_000)), Some(TimeUs(-5_000))),
    ])
    .unwrap();
    assert_eq!(numbers(head), (16, 0, 14, 0));
    assert_eq!(head.samples, 2);
}

/// 1 s bins up to 512 of them; past that, every bin one whole second wide
/// enough to keep the count within 512.
#[test]
fn timeline_bins_widen_past_the_cap() {
    let s = |n: i64| TimeUs(n * 1_000_000);
    let axis = TimelineAxis::new(TimeUs(0), s(600));
    assert_eq!((axis.width, axis.count), (s(2), 301));
    let axis = TimelineAxis::new(TimeUs(0), s(511));
    assert_eq!((axis.width, axis.count), (s(1), 512));
    let axis = TimelineAxis::new(TimeUs(0), TimeUs(512_000_000));
    assert_eq!((axis.width, axis.count), (s(2), 257));
    let axis = TimelineAxis::new(s(10), s(10));
    assert_eq!((axis.width, axis.count), (s(1), 1));
    assert_eq!(axis.bin(s(9)), None);
    assert_eq!(axis.bin(s(99)), Some(0));
    // A very long span stays within the cap.
    let axis = TimelineAxis::new(TimeUs(0), TimeUs(i64::MAX / 2));
    assert!(usize::from(axis.count) <= super::MAX_TIMELINE_BINS);
}
