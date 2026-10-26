//! Exact time over many BPMs, and the time size cap (D3).

use std::time::Instant;

use bms_model::{Chart, Note, NoteKind, TimeUs};
use bms_parser::{LoadError, LoadOptions, Loaded, WarningKind, load_bms};
use sha2::{Digest, Sha256};

/// Loads `text` from an empty folder, dropping the missing-sound warnings
/// that follow.
fn load(text: &str) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = load_bms(text.as_bytes(), dir.path(), &LoadOptions::default())?;
    loaded
        .warnings
        .retain(|w| w.kind != WarningKind::MissingSoundFile);
    Ok(loaded)
}

fn times(notes: &[Note]) -> Vec<i64> {
    notes.iter().map(|n| n.time.0).collect()
}

/// A two-character base-36 object id.
fn id(n: usize) -> String {
    const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    assert!((1..36 * 36).contains(&n));
    String::from_utf8(vec![DIGITS[n / 36], DIGITS[n % 36]]).unwrap()
}

/// Every time the chart holds, and the scroll position at each, in one list:
/// per lane each note's time (and a long note's end), then BGM, bar lines,
/// the initial BPM, and the scroll position at every listed time.
fn fingerprint(chart: &Chart) -> Vec<i64> {
    let mut times = Vec::new();
    for lane in &chart.lanes {
        for note in lane {
            times.push(note.time.0);
            if let NoteKind::Long { end, .. } = note.kind {
                times.push(end.0);
            }
        }
        times.push(-1);
    }
    times.extend(chart.bgm.iter().map(|e| e.time.0));
    times.push(-1);
    times.extend(chart.bar_lines.iter().map(|t| t.0));
    times.push(-1);
    let positions: Vec<i64> = times
        .iter()
        .filter(|&&t| t >= 0)
        .map(|&t| chart.scroll.position_at(TimeUs(t)).0)
        .collect();
    times.push(chart.scroll.initial_bpm_micro());
    times.extend(positions);
    times
}

fn digest(values: &[i64]) -> String {
    let mut h = Sha256::new();
    for v in values {
        h.update(v.to_le_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Few BPMs, STOPs at changes, measure lengths, long notes, BGM: every
/// decimal within what loader version 1 accepted.
const MIXED: &str = "\
#BPM 137.5
#BPM01 211.25
#BPM02 89.875
#STOP01 96
#STOP02 13.5
#WAV01 a.wav
#WAV02 b.wav
#00111:01020102
#00102:0.75
#00109:0001
#00203:B4
#00211:0000000201
#00208:000002
#00302:1.375
#00316:0102010201
#00309:02
#00408:01
#00451:01000002
#00401:0000000001
#00513:01
";

/// Sixty measures cycling three 6-digit BPMs, with notes at sevenths,
/// ninths and elevenths, STOPs and short measures.
fn cycling() -> String {
    let mut text = String::from(
        "#BPM 147.123456\n#BPM01 333.333333\n#BPM02 98.765432\n#BPM03 222.222\n\
         #STOP01 7.654321\n#STOP02 0.000003\n#WAV01 a.wav\n",
    );
    for m in 0..60 {
        let bpm = ["01", "02", "03"][m % 3];
        text.push_str(&format!("#{m:03}08:00{bpm}00\n"));
        text.push_str(&format!("#{m:03}11:{}\n", "01".repeat(7)));
        text.push_str(&format!("#{m:03}14:{}\n", "0100".repeat(9)));
        text.push_str(&format!("#{m:03}19:{}\n", "000001".repeat(11)));
        if m % 4 == 1 {
            text.push_str(&format!("#{m:03}09:0001000200\n"));
        }
        if m % 5 == 2 {
            text.push_str(&format!("#{m:03}02:0.{:06}\n", 123_457 + m * 1_013));
        }
    }
    text
}

#[test]
fn few_bpm_charts_keep_their_old_times() {
    // Both expectations were produced by loader version 1 (checked `i128`
    // rationals) on these exact charts before the time representation
    // changed.
    let mixed = fingerprint(&load(MIXED).unwrap().chart);
    assert_eq!(
        mixed,
        vec![
            5706286, 6628400, 7362754, 8097107, 8831460, -1, 1745454, 2072727, 2400000, 3600000,
            4727272, 5172211, 9565813, 10417884, -1, -1, 10701908, -1, -1, -1, -1, -1, 10474689,
            -1, 0, 1745454, 3927272, 5706286, 9565813, 10701908, -1, 137500000, 10999999, 12099998,
            13199999, 14299999, 15399998, 3999998, 4749999, 5500000, 6250000, 9399998, 10199999,
            16499998, 19499998, 20499999, 19699999, 0, 3999998, 6999998, 10999999, 16499998,
            20499999
        ]
    );
    let cycling = fingerprint(&load(&cycling()).unwrap().chart);
    assert_eq!(cycling.len(), 3371);
    assert_eq!(
        digest(&cycling),
        "b3db3cebf6af0a3a005800a04223d80fb1246d37df65211e0de7b1f2de2a477d"
    );
}

/// Twenty distinct odd prime BPMs, one per measure. The time's denominator
/// is their product, 144 bits, past what `i128` holds.
const PRIMES: [u32; 20] = [
    101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181, 191, 193,
    197,
];

#[test]
fn many_distinct_bpms_keep_exact_time() {
    let mut text = String::from("#BPM 199\n#WAV01 a.wav\n");
    for (m, bpm) in PRIMES.iter().enumerate() {
        let id = id(m + 1);
        text.push_str(&format!("#BPM{id} {bpm}\n#{m:03}08:{id}\n#{m:03}11:0001\n"));
    }
    text.push_str("#02011:01\n");
    let loaded = load(&text).unwrap();
    // Exact fractions in an independent script: a note at the half of each
    // measure (2 beats at its BPM after the 4-beat measures before), and
    // one at measure 20.
    assert_eq!(
        times(&loaded.chart.lanes[1]),
        vec![
            1_188_118, 3_541_286, 5_827_830, 8_050_242, 10_213_107, 12_219_935, 14_080_848,
            15_872_791, 17_612_013, 19_280_691, 20_880_762, 22_439_795, 23_940_323, 25_395_082,
            26_807_287, 28_171_319, 29_504_694, 30_795_949, 32_045_983, 33_276_882, 33_886_019
        ]
    );
}

/// A chart whose BPM changes at the start of each measure `m` in
/// `0..=last` to the distinct odd value `99_999_999 - 2m`, with a note at
/// the start of measure `last` and `notes` more in each measure after it.
fn growing(last: usize, notes: usize) -> String {
    let mut text = String::from("#BPM 77777777\n#WAV01 a.wav\n");
    for m in 0..=last {
        let id = id(m + 1);
        text.push_str(&format!(
            "#BPM{id} {}\n#{m:03}08:{id}\n",
            99_999_999 - 2 * m
        ));
    }
    text.push_str(&format!("#{last:03}11:01\n"));
    if notes > 0 {
        for m in last + 1..=999 {
            text.push_str(&format!("#{m:03}11:{}\n", "01".repeat(notes)));
        }
    }
    text
}

#[test]
fn time_size_cap_is_reached_at_the_computed_measure() {
    // An independent script sums the exact fractions: entering measure 386
    // the time's denominator has exactly 8192 bits, the cap, and entering
    // 387 it has 8212.
    let loaded = load(&growing(386, 0)).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![926]);
    assert_eq!(
        load(&growing(387, 0)),
        Err(LoadError::TimeOverflow { measure: 387 })
    );
}

/// The worst case the cap allows, timed: the time's denominator reaches
/// 8192 bits first, then each of 491,200 notes (800 to a measure through
/// measure 999) adds a segment at a BPM already in it, the path that takes
/// both greatest common divisors. The notes sit about 0.003 µs apart, so
/// most are dropped as conflicts, each with a warning; that work counts in
/// the load time too.
#[test]
#[ignore = "timing run of about 490,000 objects: cargo test --release -p bms-parser --test time -- --ignored --nocapture"]
fn time_size_cap_worst_case_load_time() {
    // The BPM changes stop at measure 385, so measure 386 enters at 8192
    // bits; a note step of 4/800 beat adds 300_000 / BPM µs, whose
    // denominator divides the BPM.
    let text = growing(385, 800);
    let start = Instant::now();
    let loaded = load(&text).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(loaded.chart.bar_lines.len(), 1000);
    println!("worst-case load: {elapsed:?}");
}
