//! The previews' worked values (the fixtures of design.md "Loading and
//! Song Selection", Summaries), each computed exactly (Python's
//! `fractions`), where the reference's `double` arithmetic is noted when it
//! differs.

use bms_model::{
    ChartBuilder, ChartProfile, ChartSummary, HeaderValue, JudgmentHeader, JudgmentHeaderName,
    Lane, MAIN_HISTOGRAM_LIMIT, Tempo, TimeUs, main_tempo,
};

use super::{ChartInfoView, GaugeTotal};

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

/// Fixture D: 87 notes over 20 seconds is 4.35 exactly, hundredths 35;
/// the reference's `double` times 100 is 434.99999999999994, which shows
/// 34.
#[test]
fn hundredths_are_exact() {
    let mut seconds = vec![4; 16];
    seconds.extend([6, 6, 6, 5]);
    let v = view(taps(&seconds), &[]);
    assert_eq!((v.density_average, v.density_average_hundredths), (4, 35));
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
    assert_eq!(GaugeTotal::of(&[], 345).border_start(345), 214);
    assert_eq!(
        GaugeTotal::of(&[total(263 * ONE)], 345).border_start(345),
        213
    );
}

/// Fixture T3913: 3 913 notes and no `#TOTAL` give 652.1666…, whose
/// border start is 3 313, reached in second 1: (1 + 600) / 2 = 300.50.
/// Floored to thousandths (652.166) it would be 3 312, reached in second
/// 0: 3 913 / 3 = 1 304.33.
#[test]
fn the_border_uses_the_exact_default_total() {
    let v = view(taps(&[3_312, 1, 600]), &[]);
    assert_eq!(v.total, 652);
    assert_eq!((v.density_end, v.density_end_hundredths), (300, 50));
    let thousandths = total(652_166 * (ONE / 1_000));
    assert_eq!(
        GaugeTotal::of(&[thousandths], 3_913).border_start(3_913),
        3_312
    );
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
    let thousandths = total(111_111 * (ONE / 1_000));
    assert_eq!(GaugeTotal::of(&[thousandths], 10).border_start(10), 0);
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

/// A declared total is its exact value; one not above 0 is the default.
/// The border's start truncates toward 0 on both sides of it: `#TOTAL 50`
/// gives `10 · (1 − 2) = −10`, and `#TOTAL 30` for 10 notes `−23.3…`,
/// −23.
#[test]
fn a_declared_total_is_exact() {
    let header = [total(1_111_115 * (ONE / 10_000))];
    assert_eq!(GaugeTotal::of(&header, 10).whole(), 111);
    assert_eq!(GaugeTotal::of(&[total(0)], 28), GaugeTotal::of(&[], 28));
    assert_eq!(GaugeTotal::of(&[total(0)], 28).whole(), 260);
    assert_eq!(GaugeTotal::of(&[total(50 * ONE)], 10).border_start(10), -10);
    assert_eq!(GaugeTotal::of(&[total(30 * ONE)], 10).border_start(10), -23);
    assert_eq!(GaugeTotal::of(&[total(100 * ONE)], 10).border_start(10), 0);
}

/// Fixture E600: `#TOTAL 600` starts the border at 5 for 6 notes; counting
/// the three mines of second 0, it is reached there, so the window is two
/// seconds, 6 / 2 = 3.00. Without the mines it would be second 1: 2.00.
#[test]
fn mines_count_down_the_border() {
    let b = taps(&[4, 2])
        .mine(lane(5), TimeUs(500_000), 1)
        .mine(lane(6), TimeUs(500_000), 1)
        .mine(lane(7), TimeUs(500_000), 1);
    let v = view(b, &[total(600 * ONE)]);
    assert_eq!((v.density_end, v.density_end_hundredths), (3, 0));
}

/// Fixture T31291: 31 291 notes and no `#TOTAL` give 745.0238…, whose
/// exact border start is 27 091, reached in second 1, so the window is two
/// seconds: 4 201 / 2 = 2 100.50. The reference's `double` starts it at
/// 27 090, reached in second 0: 31 291 / 3 = 10 430.33.
#[test]
fn the_default_total_is_exact() {
    let v = view(taps(&[27_090, 1, 4_200]), &[]);
    assert_eq!(v.total, 745);
    assert_eq!((v.density_end, v.density_end_hundredths), (2_100, 50));
}

/// Fixture T4420: 4 420 notes and no `#TOTAL` give exactly 663, which the
/// reference's `double` shows as 662.
#[test]
fn the_default_total_shows_its_exact_whole() {
    assert_eq!(GaugeTotal::of(&[], 4_420).whole(), 663);
    assert_eq!(view(taps(&[4_420]), &[]).total, 663);
    assert_eq!(GaugeTotal::of(&[], 4_419).whole(), 662);
}

/// Fixture T200: `#TOTAL 199.99999999999999999` shows 199, as play does;
/// the reference's `double` is 200.0.
#[test]
fn a_declared_total_shows_its_exact_whole() {
    let declared = total(200 * ONE - 10);
    assert_eq!(view(taps(&[10]), &[declared]).total, 199);
}

/// Fixture B200: a MAIN of 199.99999999999999999 BPM shows 199; the
/// reference's `double` is 200.0.
#[test]
fn main_shows_its_exact_whole() {
    let mut chart = taps(&[3]).build().unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let main = Tempo::from_units(200 * Tempo::UNITS_PER_BPM - 10).unwrap();
    let profile = ChartProfile::of(&chart, &[]);
    let v = ChartInfoView::of(&profile, &[], Some(Some(main)));
    assert_eq!(v.main_bpm, Some(199));
}

/// Fixture M: a chart of mines alone weighs no tempo, so its MAIN shows 0,
/// as the reference's song information leaves it; an unavailable MAIN
/// shows nothing.
#[test]
fn no_weighted_note_shows_main_zero() {
    let mut chart = ChartBuilder::new(150_000)
        .mine(lane(1), TimeUs(100_000), 1)
        .mine(lane(1), TimeUs(200_000), 1)
        .mine(lane(1), TimeUs(300_000), 1)
        .build()
        .unwrap();
    chart.meta.length = ChartSummary::of(&chart).length;
    let main = main_tempo(&chart, |_| true, MAIN_HISTOGRAM_LIMIT).ok();
    assert_eq!(main, Some(None));
    let profile = ChartProfile::of(&chart, &[]);
    assert_eq!(ChartInfoView::of(&profile, &[], main).main_bpm, Some(0));
    assert_eq!(ChartInfoView::of(&profile, &[], None).main_bpm, None);
}
