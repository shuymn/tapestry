//! The previews' worked values (the fixtures of design.md "Loading and
//! Song Selection", Summaries), each against the reference's `double`
//! arithmetic, computed with IEEE doubles.

use bms_model::{
    ChartBuilder, ChartProfile, ChartSummary, HeaderValue, JudgmentHeader, JudgmentHeaderName,
    Lane, MAIN_HISTOGRAM_LIMIT, TimeUs, main_tempo,
};

use super::{ChartInfoView, border_start, decimal_text, gauge_total};

const SECOND: i64 = 1_000_000;

fn lane(i: u8) -> Lane {
    Lane::new(i).unwrap()
}

/// A chart whose second `s` holds `taps[s]` key taps, spread over the
/// seven keys, at 150 BPM.
fn taps(taps: &[u32]) -> ChartBuilder {
    let mut b = ChartBuilder::new(150_000);
    for (s, &n) in taps.iter().enumerate() {
        let s = i64::try_from(s).unwrap();
        for j in 0..i64::from(n) {
            let key = lane(1 + u8::try_from(j % 7).unwrap());
            b = b.tap(key, TimeUs(s * SECOND + (j / 7) * 100));
        }
    }
    b
}

/// `#TOTAL` written as the exact `units` of `10^-18`.
fn total(units: i128) -> JudgmentHeader {
    JudgmentHeader {
        name: JudgmentHeaderName::Total,
        value: HeaderValue::Decimal(units),
    }
}

fn view(b: ChartBuilder, headers: &[JudgmentHeader]) -> ChartInfoView {
    let mut chart = b.build().unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let main = main_tempo(&chart, |_| true, MAIN_HISTOGRAM_LIMIT).ok();
    ChartInfoView::of(&ChartProfile::of(&chart, &[]), headers, main)
}

/// (end, end hundredths, average, average hundredths).
fn densities(v: &ChartInfoView) -> (i32, i32, i32, i32) {
    (
        v.density_end,
        v.density_end_hundredths,
        v.density_average,
        v.density_average_hundredths,
    )
}

const ONE: i128 = HeaderValue::ONE;

/// Fixture A: 39 notes as 5, 13, 11, 10, `#TOTAL 200`: the border at 19
/// is reached in second 2, the window 21 over 2 seconds is 10.50, and the
/// average 39 over 4 seconds 9.75.
#[test]
fn a_base_chart() {
    let v = view(taps(&[5, 13, 11, 10]), &[total(200 * ONE)]);
    assert_eq!(v.density_peak, 13);
    assert_eq!(densities(&v), (10, 50, 9, 75));
    assert_eq!(v.total, 200);
    assert_eq!(v.main_bpm, Some(150));
}

/// Fixture G: the average's floor is an integer division (19 / 5 / 4 =
/// 0), so every second counts: 19 / 5 = 3.80. A floor in `double` (0.95)
/// would leave out the empty seconds: 19 / 3 = 6.33.
#[test]
fn the_average_floor_is_an_integer_division() {
    let v = view(taps(&[10, 0, 8, 1]), &[]);
    assert_eq!((v.density_average, v.density_average_hundredths), (3, 80));
}

/// Fixture D: 87 notes over 20 seconds is 4.35, whose `double` times 100
/// is 434.99999999999994, so the hundredths are 34, not an exact 35.
#[test]
fn hundredths_are_the_double_ones() {
    let mut seconds = vec![4; 16];
    seconds.extend([6, 6, 6, 5]);
    let v = view(taps(&seconds), &[]);
    assert_eq!((v.density_average, v.density_average_hundredths), (4, 34));
}

/// Fixture E: mines are not notes: 4 then 2 notes over 3 seconds, the most
/// 4 and the average 2.00; counting the three mines would give 7 and 3.
#[test]
fn mines_are_not_notes() {
    let b = taps(&[4, 2])
        .mine(lane(5), TimeUs(500_000), 1)
        .mine(lane(6), TimeUs(500_000), 1)
        .mine(lane(7), TimeUs(500_000), 1);
    let v = view(b, &[]);
    assert_eq!(v.density_peak, 4);
    assert_eq!((v.density_average, v.density_average_hundredths), (2, 0));
}

/// Fixture F: a long note's body counts in each second it spans: 4, 1, 1,
/// 0 average 1.50; without bodies 1.00.
#[test]
fn long_bodies_count() {
    let b = taps(&[3]).long(lane(7), TimeUs(500_000), TimeUs(2_500_000));
    let v = view(b, &[]);
    assert_eq!(v.density_peak, 4);
    assert_eq!((v.density_average, v.density_average_hundredths), (1, 50));
}

/// Fixture W: `#TOTAL 100` starts the border at 0, never reached, so the
/// window runs from second 0, and its best is the five tens from second
/// 7: 10.00. The first window alone would be 1.00, and one taking in the
/// last (empty) second 8.00.
#[test]
fn the_end_window_is_the_best_one() {
    let v = view(
        taps(&[1, 1, 1, 1, 1, 1, 1, 10, 10, 10, 10, 10]),
        &[total(100 * ONE)],
    );
    assert_eq!((v.density_end, v.density_end_hundredths), (10, 0));
}

/// Fixture T345: 345 notes and no `#TOTAL` give 263.69…, whose border
/// start is 214, reached in second 1, so the window is four seconds,
/// 132 / 4 = 33.00. The truncated total 263 would start it at 213 and
/// give 69.00.
#[test]
fn the_border_uses_the_whole_default_total() {
    let v = view(taps(&[213, 1, 50, 50, 31]), &[]);
    assert_eq!(v.total, 263);
    assert_eq!((v.density_end, v.density_end_hundredths), (33, 0));
    assert_eq!(border_start(345, 263.0), 213);
}

/// Fixture T3913: 3 913 notes and no `#TOTAL` give 652.1666…, whose
/// border start is 3 313, reached in second 1: (1 + 600) / 2 = 300.50.
/// Floored to thousandths (652.166) it would be 3 312, reached in second
/// 0: 3 913 / 3 = 1 304.33.
#[test]
fn the_border_uses_the_default_total_in_double() {
    let v = view(taps(&[3_312, 1, 600]), &[]);
    assert_eq!(v.total, 652);
    assert_eq!((v.density_end, v.density_end_hundredths), (300, 50));
    assert_eq!(border_start(3_913, 652.166), 3_312);
}

/// Fixture T1115: `#TOTAL 111.1115` starts the border at 1 for 10 notes,
/// reached in second 2, the window that second alone: 10.00. Evaluation's
/// thousandths (111.111) would start it at 0, never reached, a window of
/// three seconds: 3.33.
#[test]
fn the_border_uses_the_declared_total_exactly() {
    let precise = total(1_111_115 * (ONE / 10_000));
    let v = view(taps(&[0, 0, 10]), &[precise]);
    assert_eq!(v.total, 111);
    assert_eq!((v.density_end, v.density_end_hundredths), (10, 0));
    assert_eq!(border_start(10, 111.111), 0);
}

/// Fixture B: scratch and keys, taps and long notes count apart; 28 notes
/// default to a total of 260.
#[test]
fn counts_by_kind() {
    let mut b = taps(&[20]);
    for k in 0..4 {
        b = b.tap(Lane::SCRATCH, TimeUs(1_000_000 + k * 10_000));
    }
    b = b.long(Lane::SCRATCH, TimeUs(1_100_000), TimeUs(1_200_000));
    for k in 0..3 {
        b = b.long(lane(1 + k), TimeUs(2_000_000), TimeUs(2_100_000));
    }
    b = b
        .mine(lane(5), TimeUs(2_000_000), 1)
        .mine(lane(6), TimeUs(2_000_000), 1);
    let v = view(b, &[]);
    assert_eq!(
        (v.key_taps, v.key_longs, v.scratch_taps, v.scratch_longs),
        (20, 3, 4, 1)
    );
    assert_eq!(v.total, 260);
}

/// Fixture C: MAIN in whole BPM, truncated.
#[test]
fn main_in_whole_bpm() {
    let mut b = ChartBuilder::new(150_000).bpm(TimeUs(10_000_000), 180_500);
    for k in 0..10 {
        b = b.tap(lane(1), TimeUs(100_000 + k * 100_000));
    }
    for k in 0..30 {
        b = b.tap(lane(1), TimeUs(10_100_000 + k * 100_000));
    }
    assert_eq!(view(b, &[]).main_bpm, Some(180));
}

/// The exact decimal reads back as the nearest `double` of its text.
#[test]
fn a_declared_total_is_its_nearest_double() {
    assert_eq!(decimal_text(1_111_115 * (ONE / 10_000)), "111.1115");
    assert_eq!(decimal_text(200 * ONE), "200");
    assert_eq!(
        decimal_text(ONE / 1_000_000_000_000_000_000),
        "0.000000000000000001"
    );
    assert_eq!(decimal_text(-(ONE + ONE / 2)), "-1.5");
    let header = [total(1_111_115 * (ONE / 10_000))];
    assert_eq!(gauge_total(&header, 10).to_bits(), 111.1115_f64.to_bits());
    // Not positive: the default.
    assert_eq!(gauge_total(&[total(0)], 28).to_bits(), 260.0_f64.to_bits());
}
