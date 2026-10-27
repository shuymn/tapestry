//! Exact time over many BPMs, decimal forms, and the time size cap (design.md
//! "Chart Time").

use std::time::Instant;

use bms_model::{BEAT, Chart, JudgeRank, Note, NoteKind, Position, TimeUs};
use bms_parser::{Limits, LoadError, LoadOptions, Loaded, Warning, WarningKind, load_bms};
use sha2::{Digest, Sha256};

/// Loads `text` from an empty folder, dropping the missing-sound warnings
/// that follow.
fn load(text: &str) -> Result<Loaded, LoadError> {
    load_with(text, Limits::default())
}

/// [`load`] with other limits.
fn load_with(text: &str, limits: Limits) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        limits,
        ..LoadOptions::default()
    };
    let mut loaded = load_bms(text.as_bytes(), dir.path(), &opts)?;
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

/// Every time the chart holds that a clock is compared with, in one list:
/// per lane each note's time (and a long note's end), then the BGM.
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
    let sevenths = "01".repeat(7);
    let ninths = "0100".repeat(9);
    let elevenths = "000001".repeat(11);
    for m in 0..60 {
        let bpm = ["01", "02", "03"][m % 3];
        text.push_str(&format!("#{m:03}08:00{bpm}00\n"));
        text.push_str(&format!("#{m:03}11:{sevenths}\n"));
        text.push_str(&format!("#{m:03}14:{ninths}\n"));
        text.push_str(&format!("#{m:03}19:{elevenths}\n"));
        if m % 4 == 1 {
            text.push_str(&format!("#{m:03}09:0001000200\n"));
        }
        if m % 5 == 2 {
            text.push_str(&format!("#{m:03}02:0.{:06}\n", 123_457 + m * 1_013));
        }
    }
    text
}

/// What the evaluator and the audio see of a load, as text: the outcome,
/// then per lane each note's time, kind, mine damage, sound and long-note
/// end and end sound, the BGM, the sound names, rank, TOTAL and LN mode, and
/// the warnings in order. Display geometry is left out.
fn projection(result: &Result<Loaded, LoadError>) -> String {
    use std::fmt::Write;
    let loaded = match result {
        Ok(loaded) => loaded,
        Err(e) => return format!("error {e:?}\n"),
    };
    let chart = &loaded.chart;
    let mut out = String::from("ok\n");
    for (l, lane) in chart.lanes.iter().enumerate() {
        write!(out, "lane {l}:").unwrap();
        for note in lane {
            write!(out, " {} {:?}", note.time.0, note.sound).unwrap();
            match note.kind {
                NoteKind::Tap => write!(out, " tap"),
                NoteKind::Long { end, end_sound, .. } => {
                    write!(out, " long {} {end_sound:?}", end.0)
                }
                NoteKind::Mine { damage } => write!(out, " mine {damage}"),
            }
            .unwrap();
        }
        out.push('\n');
    }
    for e in &chart.bgm {
        writeln!(out, "bgm {} {}", e.time.0, e.sound.0).unwrap();
    }
    for s in &chart.sounds {
        writeln!(out, "sound {:?}", s.name).unwrap();
    }
    let meta = &chart.meta;
    writeln!(
        out,
        "meta {:?} {:?} {:?}",
        meta.rank, meta.total_milli, meta.ln_mode
    )
    .unwrap();
    for w in &loaded.warnings {
        writeln!(out, "warning {w:?}").unwrap();
    }
    out
}

/// Every chart this file and `tests/charts` hold, by name.
fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/charts");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    let mut out: Vec<(String, Vec<u8>)> = files
        .into_iter()
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(p).unwrap())
        })
        .collect();
    for (name, text) in [
        ("mixed", MIXED.to_owned()),
        ("cycling", cycling()),
        ("primes", primes()),
        ("long decimals", LONG_DECIMALS.to_owned()),
        ("growing", growing(40, 3)),
    ] {
        out.push((name.to_owned(), text.into_bytes()));
    }
    out
}

/// The evaluator-visible projection of every fixture is what loader
/// version 4 produced before display positions entered the chart model.
#[test]
fn every_fixture_keeps_its_judgment_projection() {
    let mut h = Sha256::new();
    let fixtures = fixtures();
    for (name, bytes) in &fixtures {
        let dir = tempfile::tempdir().unwrap();
        let result = load_bms(bytes, dir.path(), &LoadOptions::default());
        h.update(name.as_bytes());
        h.update(projection(&result).as_bytes());
    }
    let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(fixtures.len(), 25);
    assert_eq!(
        digest,
        "37922a6eb2fabb55672e19099ed30c7b6eac8b374f5d189fe42d71089c084e94"
    );
}

#[test]
fn few_bpm_charts_keep_their_old_times() {
    // Both expectations were produced by loader version 1 (checked `i128`
    // rationals) on these exact charts before the time representation
    // changed, and kept since.
    let mixed = fingerprint(&load(MIXED).unwrap().chart);
    assert_eq!(
        mixed,
        vec![
            5706286, 6628400, 7362754, 8097107, 8831460, -1, 1745454, 2072727, 2400000, 3600000,
            4727272, 5172211, 9565813, 10417884, -1, -1, 10701908, -1, -1, -1, -1, -1, 10474689,
            -1
        ]
    );
    let cycling = fingerprint(&load(&cycling()).unwrap().chart);
    assert_eq!(cycling.len(), 1629);
    assert_eq!(
        digest(&cycling),
        "39aef5203823109c6c050ff3e9c43062a512e9c7f79b6e9798435abfcff44100"
    );
}

/// Twenty distinct odd prime BPMs, one per measure. The time's denominator
/// is their product, 144 bits, past what `i128` holds.
const PRIMES: [u32; 20] = [
    101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181, 191, 193,
    197,
];

/// One measure at each of [`PRIMES`], a note at the half of each, and one
/// at measure 20.
fn primes() -> String {
    let mut text = String::from("#BPM 199\n#WAV01 a.wav\n");
    for (m, bpm) in PRIMES.iter().enumerate() {
        let id = id(m + 1);
        text.push_str(&format!("#BPM{id} {bpm}\n#{m:03}08:{id}\n#{m:03}11:0001\n"));
    }
    text.push_str("#02011:01\n");
    text
}

#[test]
fn many_distinct_bpms_keep_exact_time() {
    let loaded = load(&primes()).unwrap();
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

/// Decimals with exponents and many digits, a sub-measure STOP, and a
/// measure 4 beats in 24 µs.
const LONG_DECIMALS: &str = "\
#BPM 1.5E+02
#BPM02 98.7654321
#BPM03 1.00001E+07
#STOP01 4.16E-02
#WAV01 a.wav
#00011:0101
#00102:0.0078125
#00111:0101
#00208:02
#00209:01
#00211:010100
#00308:03
#00311:0101
#00411:01
";

#[test]
fn long_decimals_and_exponents_time_exactly() {
    let loaded = load(LONG_DECIMALS).unwrap();
    let chart = &loaded.chart;
    // Exact fractions in an independent script. Measure 1 is 0.03125 beats
    // at 150 (12_500 µs); measure 2 starts at 98.7654321 with a STOP of
    // 0.0416 units (526.5... µs); measure 3 runs at 10_000_100 BPM, 4
    // beats in 24 µs.
    assert_eq!(
        times(&chart.lanes[1]),
        vec![
            0, 800_000, 1_600_000, 1_606_250, 1_612_500, 2_423_026, 4_043_026, 4_043_038, 4_043_050
        ]
    );
    // Bar lines at 0, 4, 4.03125, 8.03125 and 12.03125 beats.
    assert_eq!(
        chart.bar_lines,
        [0, 128, 129, 257, 385].map(|n| Position(BEAT * n / 32))
    );
    // floor(2^80 × 150 / 60e6) units per µs.
    assert_eq!(chart.scroll.initial_rate(), BEAT / 400_000);
    // Flat from the STOP's start through the last µs before its exact end.
    let pos = |t| chart.scroll.position_at(TimeUs(t));
    assert_eq!(pos(1_612_500), Position(BEAT * 129 / 32));
    assert_eq!(pos(1_612_500), pos(1_613_026));
    assert!(pos(1_613_027) > pos(1_613_026));
    assert_eq!(loaded.warnings, vec![]);
}

#[test]
fn digits_past_eighteen_are_cut_off_and_a_length_cut_to_zero_is_refused() {
    // 40 fractional digits; the time uses the first 18: floor(4 beats ×
    // 0.765432109876543210 × 60e6 / 150) = 1_224_691 (an independent
    // script).
    let text = "#BPM 150\n#00002:0.7654321098765432109876543210987654321098\n#00111:01\n";
    let loaded = load(text).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![1_224_691]);
    for (text, line) in [("#00102:1E-100\n", 1), ("#BPM 120\n#00302:0\n", 2)] {
        assert_eq!(
            load(text),
            Err(LoadError::InvalidValue {
                line,
                what: "measure length",
                reason: "the measure has no length, so its timing is undefined"
            }),
            "{text}"
        );
    }
}

#[test]
fn total_and_defexrank_take_the_decimal_grammar() {
    let meta = |header: &str| {
        let loaded = load(&format!("#BPM 120\n{header}\n#WAV01 a.wav\n#00111:01\n")).unwrap();
        (loaded.chart.meta, loaded.warnings)
    };
    let (m, w) = meta("#TOTAL 312.3456789");
    assert_eq!((m.total_milli, w), (Some(312_345), vec![]));
    let (m, w) = meta("#TOTAL 2.5E+02");
    assert_eq!((m.total_milli, w), (Some(250_000), vec![]));
    let (m, w) = meta("#DEFEXRANK 1.0000005E+02");
    assert_eq!(
        (m.rank, w),
        (Some(JudgeRank::DefExRankMilli(100_000)), vec![])
    );
    // An invalid value stays a warning: it does not change timing.
    for (header, what) in [("#TOTAL 1e19", "#TOTAL"), ("#DEFEXRANK 7e13", "#DEFEXRANK")] {
        let (m, w) = meta(header);
        assert_eq!((m.total_milli, m.rank), (None, None), "{header}");
        assert_eq!(
            w,
            vec![Warning {
                line: Some(2),
                kind: WarningKind::IgnoredValue(what)
            }],
            "{header}"
        );
    }
}

#[test]
fn bpm_below_one_millionth_is_refused() {
    for (text, what) in [
        ("#BPM 0.0000009\n", "#BPM"),
        ("#BPM 120\n#BPM01 9E-07\n#00108:01\n", "#BPMxx"),
    ] {
        match load(text) {
            Err(LoadError::InvalidValue {
                what: w, reason, ..
            }) => assert_eq!((w, reason), (what, "below 0.000001"), "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
    let loaded = load("#BPM 0.000001\n").unwrap();
    assert_eq!(
        loaded.chart.scroll.initial_rate(),
        BEAT / 60_000_000_000_000
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
        let line = "01".repeat(notes);
        for m in last + 1..=999 {
            text.push_str(&format!("#{m:03}11:{line}\n"));
        }
    }
    text
}

#[test]
fn time_size_cap_is_reached_at_the_computed_measure() {
    // An independent script sums the exact fractions: entering measure 808
    // the time's denominator has 16371 bits, within the 16384-bit cap, and
    // entering 809 it has 16386.
    let loaded = load(&growing(808, 0)).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![1_939]);
    assert_eq!(
        load(&growing(809, 0)),
        Err(LoadError::TimeOverflow { measure: 809 })
    );
}

#[test]
fn a_stop_that_overflows_the_time_cap_is_refused() {
    // Measure 0 starts with a 1-unit STOP at BPM 3: 1_250_000 / 3 µs, a
    // 2-bit denominator. Measure 1 adds 80_000_000 µs (4 beats at 3), then
    // turns to BPM 7 and stops 1_250_000 / 7 µs: the denominator becomes
    // 21, 5 bits, on the STOP's addition.
    let text = "#BPM 3\n#BPM01 7\n#STOP01 1\n#00009:01\n#00108:01\n#00109:01\n#00111:01\n";
    let with_bits = |max_time_bits| {
        load_with(
            text,
            Limits {
                max_time_bits,
                ..Limits::default()
            },
        )
    };
    assert!(with_bits(5).is_ok());
    assert_eq!(with_bits(4), Err(LoadError::TimeOverflow { measure: 1 }));
}

/// A heavy load at the cap, timed: the time's denominator reaches 16371
/// bits first, then each of 480,000 notes (2500 to a measure through
/// measure 999) adds a segment at a BPM already in it, the path that takes
/// both greatest common divisors. The notes sit far below 1 µs apart, so
/// most are dropped as conflicts, each with a warning; that work counts in
/// the load time too. The time measured here describes this fixture, not
/// every chart the limits allow.
#[test]
#[ignore = "timing run of about 480,000 objects: cargo test --release -p bms-parser --test time -- --ignored --nocapture"]
fn time_size_cap_worst_case_load_time() {
    // The BPM changes stop at measure 807, so measure 808 enters at 16371
    // bits; a note step of 4/2500 beat adds 96_000 / BPM µs, whose
    // denominator divides the BPM.
    let text = growing(807, 2500);
    let start = Instant::now();
    let loaded = load(&text).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(loaded.chart.bar_lines.len(), 1000);
    // Every note was timed: kept in its lane or dropped with a warning.
    let dropped = loaded
        .warnings
        .iter()
        .filter(|w| w.kind == WarningKind::SameTimeNoteDropped)
        .count();
    assert_eq!(loaded.chart.lanes[1].len() + dropped, 1 + 192 * 2500);
    assert!(dropped > 0);
    println!(
        "heavy load: {elapsed:?}, {} notes kept, {dropped} dropped",
        loaded.chart.lanes[1].len()
    );
}
