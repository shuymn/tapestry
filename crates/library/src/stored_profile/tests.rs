//! The stored profile's round trip and bounds.

use bms_model::{NOTE_BINS, NOTE_CATEGORIES, SCROLL_ONE, SpeedRow, Tempo};

use super::{MAX_PROFILE_BYTES, MAX_ROWS, StoredProfile, decode, encode_parts};

type Bin = [u16; NOTE_CATEGORIES];

fn tempo(units: u128) -> Option<Tempo> {
    Some(Tempo::from_units(units).unwrap())
}

const BPM: u128 = Tempo::UNITS_PER_BPM;
const MAIN: Option<Option<Tempo>> = Some(Some(Tempo::MIN));

/// `n` rows 1 ms apart, alternating 150 and 151 BPM with a STOP every
/// tenth, the scroll factor 1, then 0.5 every third.
fn rows(n: u32) -> Vec<SpeedRow> {
    (0..n)
        .map(|ms| SpeedRow {
            ms,
            tempo: match ms % 10 {
                9 => None,
                k => tempo((150 + u128::from(k % 2)) * BPM),
            },
            scroll: if ms % 3 == 2 {
                SCROLL_ONE / 2
            } else {
                SCROLL_ONE
            },
        })
        .collect()
}

fn round_trip(bins: &[Bin], rows: &[SpeedRow]) -> (usize, StoredProfile) {
    let bytes = encode_parts(MAIN, 1, bins, rows).unwrap();
    (bytes.len(), decode(&bytes).unwrap())
}

/// R4096: the most rows are kept, and read back as written.
#[test]
fn the_most_rows_round_trip() {
    let bins = [[1, 2, 3, 4, 5, 6, 7]; 3];
    let rows = rows(u32::try_from(MAX_ROWS).unwrap());
    let (_, stored) = round_trip(&bins, &rows);
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows.as_deref(), Some(rows.as_slice()));
    assert_eq!(stored.main, MAIN);
}

/// R4097: one row more leaves the rows out and keeps the bins and MAIN.
#[test]
fn a_row_past_the_most_leaves_the_rows_out() {
    let bins = [[1, 2, 3, 4, 5, 6, 7]; 3];
    let (_, stored) = round_trip(&bins, &rows(u32::try_from(MAX_ROWS).unwrap() + 1));
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows, None);
    assert_eq!(stored.main, MAIN);
}

/// K512: the most bins, each count at its largest, are kept (21 bytes a
/// bin, 10 752 in all), with the rows.
#[test]
fn the_fullest_bins_round_trip() {
    let bins = vec![[u16::MAX; NOTE_CATEGORIES]; NOTE_BINS];
    let rows = rows(2);
    let (len, stored) = round_trip(&bins, &rows);
    assert!((10_752..10_800).contains(&len), "{len}");
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows.as_deref(), Some(rows.as_slice()));
}

/// K24-over: 3 000 rows whose tempos have 27 significant digits pass the
/// byte bound, so they are left out, the bins kept.
#[test]
fn rows_past_the_byte_bound_are_left_out() {
    let rows: Vec<_> = (0..3_000_u32)
        .map(|ms| SpeedRow {
            ms,
            tempo: tempo(999_999_999_999_999_999_999_999_999 - u128::from(ms) * 2),
            scroll: SCROLL_ONE,
        })
        .collect();
    let bins = [[9; NOTE_CATEGORIES]];
    let (len, stored) = round_trip(&bins, &rows);
    assert!(len < 32, "{len}");
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows, None);
}

/// K24-accept: the byte bound is inclusive: a profile of exactly 24 576
/// bytes keeps its rows, one byte more leaves them out.
#[test]
fn the_byte_bound_is_inclusive() {
    // 4 096 rows of 5 bytes: a delta of 0, the tag, a 1-byte mantissa and
    // a factor of 1 in 2 bytes.
    let rows: Vec<_> = (0..MAX_ROWS)
        .map(|_| SpeedRow {
            ms: 0,
            tempo: tempo(99 * 10_u128.pow(15)),
            scroll: SCROLL_ONE,
        })
        .collect();
    let mut bins = vec![[0_u16; NOTE_CATEGORIES]; NOTE_BINS];
    let len = |bins: &[Bin]| encode_parts(MAIN, 1, bins, &rows).unwrap().len();
    // Widen counts from 1 byte to 2 until the profile is exactly the bound.
    let mut cells = 0;
    let mut kept = len(&bins);
    while kept < MAX_PROFILE_BYTES {
        bins[cells / NOTE_CATEGORIES][cells % NOTE_CATEGORIES] = 128;
        cells += 1;
        kept = len(&bins);
    }
    assert_eq!(kept, MAX_PROFILE_BYTES);
    assert_eq!(MAX_PROFILE_BYTES, 24_576);
    let at_bound = decode(&encode_parts(MAIN, 1, &bins, &rows).unwrap()).unwrap();
    assert_eq!(at_bound.rows.as_deref(), Some(rows.as_slice()));
    bins[cells / NOTE_CATEGORIES][cells % NOTE_CATEGORIES] = 128;
    let past = decode(&encode_parts(MAIN, 1, &bins, &rows).unwrap()).unwrap();
    assert_eq!(past.rows, None);
    assert_eq!(past.bins, bins);
}

/// The MAIN tempo reads back exactly in each of its three states, a
/// fractional one included (fixture C's 180.5 BPM).
#[test]
fn the_main_tempo_round_trips_exactly() {
    for main in [None, Some(None), Some(tempo(1_805 * BPM / 10))] {
        let bytes = encode_parts(main, 1, &[], &rows(2)).unwrap();
        assert_eq!(decode(&bytes).unwrap().main, main);
    }
}

/// A scroll factor reads back exactly: 0, negative and fractional.
#[test]
fn scroll_factors_round_trip() {
    let rows: Vec<_> = [0, -SCROLL_ONE / 4, 3 * SCROLL_ONE, 123_456_789]
        .into_iter()
        .zip(0..)
        .map(|(scroll, ms)| SpeedRow {
            ms,
            tempo: tempo(150 * BPM),
            scroll,
        })
        .collect();
    let (_, stored) = round_trip(&[], &rows);
    assert_eq!(stored.rows, Some(rows));
}

/// What this build did not write within the bounds is refused.
#[test]
fn damaged_blobs_are_refused() {
    let good = encode_parts(MAIN, 1, &[[1; NOTE_CATEGORIES]], &rows(3)).unwrap();
    assert!(decode(&good).is_some());
    // Truncated, or with a byte more.
    assert_eq!(decode(&good[..good.len() - 1]), None);
    let mut longer = good.clone();
    longer.push(0);
    assert_eq!(decode(&longer), None);
    // An empty profile reads back; another version, an unknown flag or an
    // unknown MAIN tag does not.
    assert!(decode(&[1, 0, 0, 1, 0, 0]).is_some());
    assert_eq!(decode(&[2, 0, 0, 1, 0, 0]), None);
    assert_eq!(decode(&[1, 2, 0, 1, 0, 0]), None);
    assert_eq!(decode(&[1, 0, 3, 1, 0, 0]), None);
    // 513 bins, written in full (LEB 0x81 0x04), so the count alone
    // refuses them.
    let mut bins_513 = vec![1, 0, 0, 1, 0x81, 0x04];
    bins_513.extend(vec![0; 513 * NOTE_CATEGORIES]);
    bins_513.push(0);
    assert_eq!(decode(&bins_513), None);
    let mut bins_512 = vec![1, 0, 0, 1, 0x80, 0x04];
    bins_512.extend(vec![0; 512 * NOTE_CATEGORIES]);
    bins_512.push(0);
    assert!(decode(&bins_512).is_some(), "the same blob at 512 reads");
    // 4 097 rows (LEB 0x81 0x20), refused before they are read.
    assert_eq!(decode(&[1, 0, 0, 1, 0, 0x81, 0x20]), None);
    // A count past u16 (65 536, LEB 0x80 0x80 0x04).
    assert_eq!(
        decode(&[1, 0, 0, 1, 1, 0x80, 0x80, 0x04, 0, 0, 0, 0, 0, 0, 0]),
        None
    );
    // Rows left out yet counted.
    assert_eq!(decode(&[1, 1, 0, 1, 0, 1]), None);
    // A tempo below the tempo range (1 unit).
    assert_eq!(decode(&[1, 0, 0, 1, 0, 1, 0, 1, 1, 36, 1]), None);
    // A scroll factor past i128 (2 · 10^38).
    assert_eq!(decode(&[1, 0, 0, 1, 0, 1, 0, 0, 76, 2]), None);
    // A number past u128.
    let mut huge = vec![1, 0, 0];
    huge.extend([0xff; 19]);
    huge.push(0x01);
    assert_eq!(decode(&huge), None);
    // Past the byte bound.
    assert_eq!(decode(&vec![0; MAX_PROFILE_BYTES + 1]), None);
}
