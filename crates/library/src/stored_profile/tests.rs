//! The stored profile's round trip and bounds.

use bms_model::{NOTE_CATEGORIES, SpeedRow, Tempo};

use super::{MAX_PROFILE_BYTES, MAX_ROWS, StoredProfile, decode, encode_parts};

type Bin = [u16; NOTE_CATEGORIES];

fn tempo(units: u128) -> Option<Tempo> {
    Some(Tempo::from_units(units).unwrap())
}

/// `n` rows 1 ms apart, alternating 150 and 151 BPM with a STOP every
/// tenth.
fn rows(n: u32) -> Vec<SpeedRow> {
    (0..n)
        .map(|ms| SpeedRow {
            ms,
            tempo: match ms % 10 {
                9 => None,
                k => tempo((150 + u128::from(k % 2)) * Tempo::UNITS_PER_BPM),
            },
        })
        .collect()
}

fn round_trip(bins: &[Bin], rows: &[SpeedRow]) -> (usize, StoredProfile) {
    let bytes = encode_parts(1, bins, rows).unwrap();
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
}

/// R4097: one row more leaves the rows out and keeps the bins.
#[test]
fn a_row_past_the_most_leaves_the_rows_out() {
    let bins = [[1, 2, 3, 4, 5, 6, 7]; 3];
    let (_, stored) = round_trip(&bins, &rows(u32::try_from(MAX_ROWS).unwrap() + 1));
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows, None);
}

/// K512: the most bins, each count at its largest, are kept (21 bytes a
/// bin, 10 752 in all), with the rows.
#[test]
fn the_fullest_bins_round_trip() {
    let bins = vec![[u16::MAX; NOTE_CATEGORIES]; bms_model::NOTE_BINS];
    let rows = rows(2);
    let (len, stored) = round_trip(&bins, &rows);
    assert!(len < 10_800, "{len}");
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows.as_deref(), Some(rows.as_slice()));
}

/// K24: 3 000 rows whose tempos have 27 significant digits pass the byte
/// bound, so they are left out, the bins kept.
#[test]
fn rows_past_the_byte_bound_are_left_out() {
    let rows: Vec<_> = (0..3_000_u32)
        .map(|ms| SpeedRow {
            ms,
            tempo: tempo(999_999_999_999_999_999_999_999_999 - u128::from(ms) * 2),
        })
        .collect();
    let bins = [[9; NOTE_CATEGORIES]];
    let (len, stored) = round_trip(&bins, &rows);
    assert!(len < 32, "{len}");
    assert_eq!(stored.bins, bins);
    assert_eq!(stored.rows, None);
}

/// The byte bound is inclusive: a profile of exactly
/// [`MAX_PROFILE_BYTES`] keeps its rows, one byte more leaves them out.
#[test]
fn the_byte_bound_is_inclusive() {
    // 4 096 rows of 5 bytes (a delta of 0, the tag, a 3-byte mantissa).
    let rows: Vec<_> = (0..MAX_ROWS)
        .map(|_| SpeedRow {
            ms: 0,
            tempo: tempo(100_001 * 10_u128.pow(12)),
        })
        .collect();
    let mut bins = vec![[0_u16; NOTE_CATEGORIES]; bms_model::NOTE_BINS];
    let len = |bins: &[Bin]| encode_parts(1, bins, &rows).unwrap().len();
    // Widen counts from 1 byte to 2 until the profile is exactly the bound.
    let mut cells = 0;
    let mut kept = len(&bins);
    while kept < MAX_PROFILE_BYTES {
        bins[cells / NOTE_CATEGORIES][cells % NOTE_CATEGORIES] = 128;
        cells += 1;
        kept = len(&bins);
    }
    assert_eq!(kept, MAX_PROFILE_BYTES);
    let at_bound = decode(&encode_parts(1, &bins, &rows).unwrap()).unwrap();
    assert_eq!(at_bound.rows.as_deref(), Some(rows.as_slice()));
    bins[cells / NOTE_CATEGORIES][cells % NOTE_CATEGORIES] = 128;
    let past = decode(&encode_parts(1, &bins, &rows).unwrap()).unwrap();
    assert_eq!(past.rows, None);
    assert_eq!(past.bins, bins);
}

/// What this build did not write within the bounds is refused.
#[test]
fn damaged_blobs_are_refused() {
    let good = encode_parts(1, &[[1; NOTE_CATEGORIES]], &rows(3)).unwrap();
    assert!(decode(&good).is_some());
    // Truncated, or with a byte more.
    assert_eq!(decode(&good[..good.len() - 1]), None);
    let mut longer = good.clone();
    longer.push(0);
    assert_eq!(decode(&longer), None);
    // Another version, an unknown flag.
    assert_eq!(decode(&[2, 0, 1, 0, 0]), None);
    assert_eq!(decode(&[1, 2, 1, 0, 0]), None);
    // An empty profile reads back.
    assert!(decode(&[1, 0, 1, 0, 0]).is_some());
    // 513 bins (LEB 0x81 0x04).
    assert_eq!(decode(&[1, 0, 1, 0x81, 0x04]), None);
    // 4 097 rows (LEB 0x81 0x20), refused before they are read.
    assert_eq!(decode(&[1, 0, 1, 0, 0x81, 0x20]), None);
    // A count past u16 (65 536, LEB 0x80 0x80 0x04).
    assert_eq!(
        decode(&[1, 0, 1, 1, 0x80, 0x80, 0x04, 0, 0, 0, 0, 0, 0, 0]),
        None
    );
    // Rows left out yet counted.
    assert_eq!(decode(&[1, 1, 1, 0, 1, 0, 0]), None);
    // A tempo outside the tempo range (1 unit).
    assert_eq!(decode(&[1, 0, 1, 0, 1, 0, 1, 1]), None);
    // A number past u128.
    let mut huge = vec![1, 0];
    huge.extend([0xff; 19]);
    huge.push(0x01);
    assert_eq!(decode(&huge), None);
    // Past the byte bound.
    assert_eq!(decode(&vec![0; MAX_PROFILE_BYTES + 1]), None);
}
