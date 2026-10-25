//! QPC conversion and the wake interval distribution. Expected values were
//! computed by hand and checked with Python's floor division.

use bms_model::TimeUs;
use input::{Percentile, QpcFrequency, WAKE_HISTOGRAM_LIMIT, WakeHistogram, qpc_to_us};

fn hz(f: i64) -> QpcFrequency {
    QpcFrequency::new(f).unwrap()
}

#[test]
fn qpc_to_us_floors_negative() {
    // (ticks, origin, frequency, µs)
    let cases = [
        // 10 MHz: 100 ns ticks.
        (9, 0, 10_000_000, 0),
        (-1, 0, 10_000_000, -1),
        (-10, 0, 10_000_000, -1),
        (-11, 0, 10_000_000, -2),
        // 3.579545 MHz does not divide 10 MHz or 1 MHz.
        (1, 0, 3_579_545, 0),
        (-1, 0, 3_579_545, -1),
        (7, 0, 3_579_545, 1),
        (-7, 0, 3_579_545, -2),
        (3_579_545, 0, 3_579_545, 1_000_000),
        (-3_579_546, 0, 3_579_545, -1_000_001),
        // 24 MHz, before the origin.
        (0, 1, 24_000_000, -1),
        (-25, 0, 24_000_000, -2),
        // Overflows i64 if multiplied first in i64.
        (i64::MAX, 0, 3_579_545, 2_576_688_388_288_113_658),
        (i64::MIN, 0, 3_579_545, -2_576_688_388_288_113_660),
    ];
    for (i, &(ticks, origin, f, us)) in cases.iter().enumerate() {
        assert_eq!(qpc_to_us(ticks, origin, hz(f)), TimeUs(us), "case {i}");
    }
}

#[test]
fn qpc_to_us_saturates_outside_i64() {
    assert_eq!(qpc_to_us(i64::MAX, i64::MIN, hz(1)), TimeUs(i64::MAX));
    assert_eq!(qpc_to_us(i64::MIN, i64::MAX, hz(1)), TimeUs(i64::MIN));
    // -1.8e16 ticks at 3 Hz are -6e21 µs.
    let nine = 9_000_000_000_000_000;
    assert_eq!(qpc_to_us(-nine, nine, hz(3)), TimeUs(i64::MIN));
    // The widest span at 1 MHz still fits.
    assert_eq!(qpc_to_us(i64::MAX, 0, hz(1_000_000)), TimeUs(i64::MAX));
}

#[test]
fn qpc_frequency_must_be_positive() {
    assert_eq!(QpcFrequency::new(0), None);
    assert_eq!(QpcFrequency::new(-10_000_000), None);
    assert_eq!(QpcFrequency::new(1).map(QpcFrequency::hz), Some(1));
}

#[test]
fn wake_histogram_reports_p99_and_max() {
    let mut h = WakeHistogram::new();
    assert_eq!(h.p99(), Percentile::Empty);
    for _ in 0..99 {
        h.record(1_000);
    }
    h.record(3_000);
    assert_eq!(h.count(), 100);
    // Rank 99 of 100 is the last 1000.
    assert_eq!(h.p99(), Percentile::At(1_000));
    assert_eq!(h.max(), 3_000);
    // 101 intervals: rank ceil(99.99) = 100, which is the 1200; with a
    // second 3000, rank ceil(100.98) = 101 of 102 is a 3000.
    h.record(1_200);
    assert_eq!(h.p99(), Percentile::At(1_200));
    h.record(3_000);
    assert_eq!(h.p99(), Percentile::At(3_000));
}

#[test]
fn wake_histogram_counts_intervals_above_the_limit() {
    let mut h = WakeHistogram::new();
    h.record(WAKE_HISTOGRAM_LIMIT);
    assert_eq!(h.p99(), Percentile::At(20_000));
    let mut above = WakeHistogram::new();
    above.record(WAKE_HISTOGRAM_LIMIT + 1);
    assert_eq!(above.p99(), Percentile::Above);
    assert_eq!(above.max(), 20_001);
    assert_eq!(above.count(), 1);
}
