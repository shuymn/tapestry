//! The pure measurement of D11: frame statistics, the press → on screen
//! table, lost refreshes and the counting rules of design.md "Frame Pacing
//! in Refreshes". Every expected value is worked out by hand in the
//! comments.

use render::measure::{
    FrameStatistics, OnScreen, OnScreenLatency, Presentation, RELATIVE_ERROR, RefreshLedger,
    Samples, StatsFilter, count_span, display_qpc,
};

const MS_NS: i64 = 1_000_000;

fn stats(present: u32, present_refresh: u32, sync_refresh: u32, sync_qpc: i64) -> FrameStatistics {
    FrameStatistics {
        present_count: present,
        present_refresh_count: present_refresh,
        sync_refresh_count: sync_refresh,
        sync_qpc_time: sync_qpc,
    }
}

#[test]
fn display_time_corrects_refresh_count_gap() {
    // 240.085 Hz at a 10 MHz QPC: 41 652 ticks a refresh.
    let period = 41_652;
    // Synced two refreshes after the present: 50 000 000 − 2 × 41 652.
    let s = stats(7, 1_000, 1_002, 50_000_000);
    assert_eq!(display_qpc(&s, period), 49_916_696);
    // No gap: the sync time itself.
    assert_eq!(
        display_qpc(&stats(8, 1_003, 1_003, 50_100_000), period),
        50_100_000
    );
    // A gap of 4 is accepted, 5 is not.
    let mut f = StatsFilter::default();
    assert_eq!(
        f.accept(Some(stats(9, 1_000, 1_004, 1))),
        Some(stats(9, 1_000, 1_004, 1))
    );
    assert_eq!(f.accept(Some(stats(10, 1_000, 1_005, 2))), None);
}

#[test]
fn missing_statistics_give_na_not_a_value() {
    let mut f = StatsFilter::default();
    let first = stats(20, 500, 501, 9_000);
    assert_eq!(f.accept(Some(first)), Some(first));
    // PresentCount did not advance: rejected, whatever else changed.
    assert_eq!(f.accept(Some(stats(20, 502, 503, 9_500))), None);
    // A failed call is no sample, nor is one without a sync, whose zero
    // refresh counts would otherwise count every refresh since boot.
    assert_eq!(f.accept(None), None);
    assert_eq!(f.accept(Some(stats(30, 0, 0, 0))), None);
    // The next present count is accepted again.
    let next = stats(21, 502, 503, 9_900);
    assert_eq!(f.accept(Some(next)), Some(next));

    // Judgment 3 is drawn on the frame presented as 20, but the next
    // accepted sample is for 21: frame 20 has none, so N/A, not a value.
    let mut t = OnScreen::with_capacity(16);
    t.drawn(3, 1_000);
    t.presented(20);
    t.drawn(4, 1_200);
    t.presented(21);
    t.sample(21, 40_000);
    assert_eq!(t.latency(3), OnScreenLatency::Unavailable);
    assert_eq!(t.latency(4), OnScreenLatency::Shown(38_800));
    assert_eq!(t.unavailable(), 1);
    assert_eq!(t.latencies().summary().unwrap().count, 1);
    // A judgment never resolved before the measurement ends is N/A too,
    // and an empty table has no summary at all.
    t.drawn(5, 2_000);
    t.presented(22);
    t.finish();
    assert_eq!(t.latency(5), OnScreenLatency::Unavailable);
    assert_eq!(t.unavailable(), 2);
    assert_eq!(OnScreen::with_capacity(4).latencies().summary(), None);
}

#[test]
fn chord_judgments_drawn_on_different_frames_get_their_own_latency() {
    // Two judgments of one chord share origin 5 000 µs; seq 7 is first
    // drawn on the frame presented as 10, seq 8 on the one presented as 11.
    let mut t = OnScreen::with_capacity(16);
    t.drawn(7, 5_000);
    t.presented(10);
    t.drawn(8, 5_000);
    t.presented(11);
    // Frame 10 reached the screen at 30 000 µs: 30 000 − 5 000.
    t.sample(10, 30_000);
    assert_eq!(t.latency(7), OnScreenLatency::Shown(25_000));
    assert_eq!(t.latency(8), OnScreenLatency::Pending);
    // Frame 11 one refresh later: 34 167 − 5 000.
    t.sample(11, 34_167);
    assert_eq!(t.latency(8), OnScreenLatency::Shown(29_167));
    let s = t.latencies().summary().unwrap();
    assert_eq!((s.count, s.max), (2, 29_167));
    assert_eq!(t.unavailable(), 0);
}

#[test]
fn a_frame_not_presented_passes_its_judgments_to_the_next_presented_one() {
    let mut t = OnScreen::with_capacity(16);
    // Drawn on a frame that was not presented, then nothing new drawn on
    // the frame presented as 31: that frame is its first presented one.
    t.drawn(2, 100);
    t.presented(31);
    // A sample older than its frame leaves it waiting.
    t.sample(30, 9_000);
    assert_eq!(t.latency(2), OnScreenLatency::Pending);
    t.sample(31, 9_500);
    assert_eq!(t.latency(2), OnScreenLatency::Shown(9_400));
}

/// A frame that failed, or was presented without a present count, makes
/// what it drew N/A and leaves the earlier frame's pairing alone.
#[test]
fn a_frame_without_a_present_count_is_na_alone() {
    let mut t = OnScreen::with_capacity(16);
    t.drawn(1, 1_000);
    t.presented(40);
    t.drawn(2, 2_000);
    t.presented_unknown();
    t.sample(40, 50_000);
    assert_eq!(t.latency(1), OnScreenLatency::Shown(49_000));
    assert_eq!(t.latency(2), OnScreenLatency::Unavailable);
    assert_eq!(t.unavailable(), 1);
}

#[test]
fn judgments_beyond_the_table_are_na() {
    // Two slots: seq 3 takes the slot of seq 1 while 1 is unresolved, and
    // the drawing order holds two, so seq 0 leaves it unresolved.
    let mut t = OnScreen::with_capacity(2);
    t.drawn(0, 10);
    t.drawn(1, 20);
    t.drawn(3, 30);
    assert_eq!(t.latency(0), OnScreenLatency::Unavailable);
    assert_eq!(t.latency(1), OnScreenLatency::Unknown);
    assert_eq!(t.latency(3), OnScreenLatency::Pending);
    assert_eq!(t.unavailable(), 2);
}

/// A sample whose sync lies on a fixed grid of 1 000 QPC ticks a refresh.
fn synced(present: u32, present_refresh: u32) -> FrameStatistics {
    stats(
        present,
        present_refresh,
        present_refresh,
        i64::from(present_refresh) * 1_000,
    )
}

const GRID: Option<i64> = Some(1_000);

#[test]
fn refresh_count_deltas_to_lost_refreshes() {
    // Consecutive present counts 100 and 101 three refreshes apart: 2 lost.
    let mut l = RefreshLedger::default();
    l.accepted(&synced(100, 1_000), GRID);
    l.accepted(&synced(101, 1_003), GRID);
    let c = l.count();
    assert_eq!(
        (c.lost(), c.counted, c.unavailable, c.longest),
        (Some(2), 1, 0, 2)
    );
    // One refresh apart: none lost.
    l.accepted(&synced(102, 1_004), GRID);
    assert_eq!((l.count().lost(), l.count().counted), (Some(2), 2));

    // Samples only for 100 and 102, displayed on consecutive refreshes:
    // the intervals 100-101 and 101-102 have no evidence, so N/A, not a
    // lost refresh and not 0.
    let mut l = RefreshLedger::default();
    l.accepted(&synced(100, 1_000), GRID);
    l.accepted(&synced(102, 1_001), GRID);
    let c = l.count();
    assert_eq!(
        (c.lost(), c.lost, c.counted, c.unavailable),
        (None, 0, 0, 2)
    );
    // A present count that goes back (a new swap chain) is one interval
    // without evidence.
    l.accepted(&synced(1, 1_005), GRID);
    assert_eq!(l.count().unavailable, 3);
    l.accepted(&synced(2, 1_006), GRID);
    assert_eq!(l.count().lost(), Some(0));
}

/// Lost refreshes count only on a verified fixed refresh grid: syncs 3
/// refreshes apart but 3.2 refreshes of time apart (more than a tenth
/// off, as under VRR), or no refresh period at all, make the interval
/// N/A; and only vsync on every refresh without tearing is countable.
#[test]
fn refreshes_count_only_where_applicable() {
    let mut l = RefreshLedger::default();
    l.accepted(&synced(10, 500), GRID);
    l.accepted(&stats(11, 503, 503, 503_200), GRID);
    assert_eq!((l.count().lost(), l.count().unavailable), (None, 1));
    // 3.09 refreshes of time: within a tenth, counted.
    l.accepted(&stats(12, 506, 506, 506_290), GRID);
    assert_eq!(l.count().lost(), Some(2));
    l.accepted(&synced(13, 507), None);
    assert_eq!(l.count().unavailable, 2);

    let p = |sync_interval, tearing| Presentation {
        sync_interval,
        tearing,
    };
    assert!(p(1, false).countable());
    assert!(!p(1, true).countable());
    assert!(!p(0, false).countable());
    assert!(!p(2, false).countable());
}

#[test]
fn lost_refresh_counting_matches_rules() {
    let t = 4 * MS_NS;
    // Displayed at 0 (the predecessor of the span [1 ms, 30 ms)), 4, 16
    // (3 refreshes after 4: loses 2, at 8 and 12), 20, 24.6 (n = 1, but
    // 0.6 ms off the grid: one more), 28.6, and 32.6 (the successor).
    let shown = [0, 4, 16, 20, 24, 28, 32].map(|ms| ms * MS_NS);
    let mut shown = shown.to_vec();
    shown[4] += 600_000;
    shown[5] += 600_000;
    shown[6] += 600_000;
    let c = count_span(&shown, (MS_NS, 30 * MS_NS), t, 1, true).unwrap();
    // Grid 0 + 4j ms inside [1, 30): 4, 8, ..., 28.
    assert_eq!((c.lost, c.off_grid, c.longest, c.expected), (3, 1, 2, 7));

    // Without a predecessor or a successor, or a present missing from the
    // capture, the span is unavailable, never 0.
    assert_eq!(count_span(&shown, (0, 30 * MS_NS), t, 1, true), None);
    assert_eq!(count_span(&shown, (MS_NS, 40 * MS_NS), t, 1, true), None);
    assert_eq!(count_span(&shown, (MS_NS, 30 * MS_NS), t, 1, false), None);

    // k = 2: 3 refreshes lose one opportunity (at 16 ms); the grid 8j ms
    // inside [1, 25) has 8, 16 and 24.
    let shown = [0, 8, 20, 28].map(|ms| ms * MS_NS);
    let c = count_span(&shown, (MS_NS, 25 * MS_NS), t, 2, true).unwrap();
    assert_eq!((c.lost, c.off_grid, c.expected), (1, 0, 3));

    // 1.5 refreshes round up to 2: one lost at 4 ms, and 2 ms off the grid
    // adds one; rounded down it would be the off-grid one alone.
    let shown = [0, 6, 10].map(|ms| ms * MS_NS);
    let c = count_span(&shown, (MS_NS, 8 * MS_NS), t, 1, true).unwrap();
    assert_eq!((c.lost, c.off_grid, c.longest), (2, 1, 2));

    // A loss that crosses the span's end is clipped: 4 → 16 loses 8 and 12,
    // and only 8 lies in [1, 10).
    let shown = [0, 4, 16].map(|ms| ms * MS_NS);
    let c = count_span(&shown, (MS_NS, 10 * MS_NS), t, 1, true).unwrap();
    assert_eq!(c.lost, 1);
}

/// Percentiles cover the whole play, far beyond any ring: 20 000 samples
/// 1 to 20 000 µs, pushed backwards, give p50 10 000 and p99 19 800 by
/// nearest rank, within the stated error; the count and the maximum are
/// exact. Below 64 µs every value is exact.
#[test]
fn percentiles_cover_the_whole_play() {
    let mut s = Samples::new();
    assert_eq!(s.summary(), None);
    for v in (1..=20_000).rev() {
        s.push(v);
    }
    let x = s.summary().unwrap();
    assert_eq!((x.count, x.max), (20_000, 20_000));
    let near = |got: i64, exact: f64| (got as f64 - exact).abs() <= exact * RELATIVE_ERROR;
    assert!(near(x.p50, 10_000.0), "{}", x.p50);
    assert!(near(x.p99, 19_800.0), "{}", x.p99);
    // The first half alone would read about 5 000: the old half counts.
    assert!(x.p50 > 9_000);

    let mut s = Samples::new();
    for v in [9, 1, 3, 4, 5, 6, 60, 63] {
        s.push(v);
    }
    let x = s.summary().unwrap();
    // Sorted 1 3 4 5 6 9 60 63: rank 4 and rank 8.
    assert_eq!((x.count, x.p50, x.p99, x.max), (8, 5, 63, 63));
}

/// Negative samples (a finalization delay can be one) keep their sign and
/// magnitude, and samples up to 134 s stay within the error: [-9 000,
/// -1 000] µs has p50 -9 000 by nearest rank, and [20 s, 100 s] has p50
/// 20 s. Beyond 134 s a sample reads as the exact maximum.
#[test]
fn percentiles_keep_negative_and_long_samples() {
    let near = |got: i64, exact: f64| (got as f64 - exact).abs() <= exact.abs() * RELATIVE_ERROR;
    let mut s = Samples::new();
    s.push(-1_000);
    s.push(-9_000);
    let x = s.summary().unwrap();
    assert!(near(x.p50, -9_000.0), "{}", x.p50);
    assert!(near(x.p99, -1_000.0), "{}", x.p99);
    assert_eq!(x.max, -1_000);

    let mut s = Samples::new();
    s.push(100_000_000);
    s.push(20_000_000);
    let x = s.summary().unwrap();
    assert!(near(x.p50, 20_000_000.0), "{}", x.p50);
    assert!(near(x.p99, 100_000_000.0), "{}", x.p99);

    // Small negatives are exact; beyond 134 s, the exact maximum.
    let mut s = Samples::new();
    s.push(-5);
    s.push(200_000_000);
    let x = s.summary().unwrap();
    assert_eq!((x.p50, x.p99, x.max), (-5, 200_000_000, 200_000_000));
}
