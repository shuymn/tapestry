//! Loading at a practice speed (design.md "Practice Speed Changes"): each
//! real time is the exact time scaled by `20/k` and floored once, the scroll
//! map follows the real timeline, and the identity is the 1× chart's.

use bms_model::{BEAT, Chart, ChartIdentity, NoteKind, Speed, TimeUs};
use bms_parser::{Limits, LoadError, LoadOptions, Loaded, WarningKind, load_bms};
use proptest::prelude::*;

fn load_at(text: &str, speed: Speed) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        speed,
        ..LoadOptions::default()
    };
    let mut loaded = load_bms(text.as_bytes(), dir.path(), &opts)?;
    loaded
        .warnings
        .retain(|w| w.kind != WarningKind::MissingSoundFile);
    Ok(loaded)
}

fn at(k: u32) -> Speed {
    Speed::from_twentieths(k).unwrap()
}

/// Every time the chart holds, in one list: per lane each note's time and a
/// long note's end, then the BGM.
fn times(chart: &Chart) -> Vec<i64> {
    let mut out = Vec::new();
    for lane in &chart.lanes {
        for note in lane {
            out.push(note.time.0);
            if let NoteKind::Long { end, .. } = note.kind {
                out.push(end.0);
            }
        }
    }
    out.extend(chart.bgm.iter().map(|e| e.time.0));
    out
}

/// 140 BPM for two measures, then 157.5 BPM, with positions whose times
/// fall between whole µs: key 1 taps at sevenths, thirds, thirteenths and
/// sixteenths of measures 0 to 3, a key 2 long note over measure 1's first
/// half, and a BGM object at two thirds of measure 3.
const FRACTIONAL: &str = "\
#BPM 140
#BPM01 157.5
#WAV01 a.wav
#00011:01010101010101
#00111:010101
#00152:0101
#00208:01
#00211:01010101010101010101010101
#00301:000001
#00311:01010101010101010101010101010101
";

/// A fraction `(numerator, denominator)`.
type Frac = (i128, i128);

/// The exact time of beat `p / q` in [`FRACTIONAL`], as `(num, den)` µs: 3e6/7
/// µs per beat up to beat 8, 8e6/21 after it.
fn exact(p: i128, q: i128) -> Frac {
    if p <= 8 * q {
        (3_000_000 * p, 7 * q)
    } else {
        (72_000_000 * q + 8_000_000 * (p - 8 * q), 21 * q)
    }
}

/// [`FRACTIONAL`]'s objects as beats `(p, q)`: key 1's taps, key 2's long
/// note (head, end), and the BGM object.
fn fractional_beats() -> (Vec<Frac>, [Frac; 2], Frac) {
    let mut taps = Vec::new();
    for (m, count) in [(0, 7), (1, 3), (2, 13), (3, 16)] {
        for i in 0..count {
            taps.push((4 * (m * count + i), count));
        }
    }
    (taps, [(4, 1), (6, 1)], (12 * 3 + 8, 3))
}

#[test]
fn mapped_times_floor_the_exact_scaled_time() {
    let (taps, long, bgm) = fractional_beats();
    let floor = |(n, d): Frac| i64::try_from(n.div_euclid(d)).unwrap();
    // At 1× the times are the exact ones floored.
    let one = load_at(FRACTIONAL, Speed::NORMAL).unwrap().chart;
    let one_taps: Vec<i64> = one.lanes[1].iter().map(|n| n.time.0).collect();
    assert_eq!(
        one_taps,
        taps.iter()
            .map(|&(p, q)| floor(exact(p, q)))
            .collect::<Vec<_>>()
    );
    for k in [10, 13, 29] {
        let real = |(p, q): Frac| {
            let (n, d) = exact(p, q);
            floor((n * 20, d * i128::from(k)))
        };
        let chart = load_at(FRACTIONAL, at(k)).unwrap().chart;
        chart.validate().unwrap();
        let expected: Vec<i64> = taps.iter().map(|&b| real(b)).collect();
        let got: Vec<i64> = chart.lanes[1].iter().map(|n| n.time.0).collect();
        assert_eq!(got, expected, "{k}/20");
        // Scaling the floored 1× time again would round twice, and this
        // chart tells the two apart.
        let refloored: Vec<i64> = one_taps
            .iter()
            .map(|&t| i64::try_from(i128::from(t) * 20 / i128::from(k)).unwrap())
            .collect();
        assert_ne!(refloored, expected, "{k}/20");
        let [head, end] = long.map(real);
        assert!(matches!(
            chart.lanes[2][..],
            [n] if n.time.0 == head && n.end().0 == end
        ));
        assert_eq!(chart.bgm[0].time.0, real(bgm), "{k}/20");
        // The scroll map is the real timeline's: the playhead reaches each
        // note's position within the µs of its real time.
        for note in &chart.lanes[1] {
            assert!(chart.scroll.position_at(note.time) <= note.pos, "{k}/20");
            let next = TimeUs(note.time.0 + 1);
            assert!(chart.scroll.position_at(next) > note.pos, "{k}/20");
        }
        // Hi-speed starts from the mapped start BPM: 140 × k/20.
        let rate = BEAT * 7 * i128::from(k) / 60_000_000;
        assert_eq!(chart.scroll.initial_rate(), rate, "{k}/20");
        // Positions do not move.
        let pos = |c: &Chart| c.lanes[1].iter().map(|n| n.pos).collect::<Vec<_>>();
        assert_eq!(pos(&chart), pos(&one));
        assert_eq!(chart.bar_lines, one.bar_lines);
    }
}

/// Measure lengths, BPM changes, STOPs, a long note and BGM.
const MIXED: &str = "\
#BPM 137.5
#BPM01 211.25
#STOP01 13.5
#WAV01 a.wav
#WAV02 b.wav
#00111:01020102
#00102:0.75
#00108:0001
#00109:0001
#00212:0102
#00253:0102
#00301:01
#00316:01000001
";

/// The identity shared by every speed: all of it but the played hash.
fn shared(identity: &ChartIdentity) -> ChartIdentity {
    ChartIdentity {
        played_hash: [0; 32],
        ..identity.clone()
    }
}

#[test]
fn identity_is_the_same_at_every_speed() {
    let one = load_at(MIXED, Speed::NORMAL).unwrap().chart;
    assert_eq!(one.identity.played_hash, one.identity.resolved_hash);
    let mut played = std::collections::BTreeSet::new();
    for speed in Speed::ladder() {
        let chart = load_at(MIXED, speed).unwrap().chart;
        assert_eq!(shared(&chart.identity), shared(&one.identity), "{speed:?}");
        // The played hash pins each speed's real timeline.
        assert!(played.insert(chart.identity.played_hash), "{speed:?}");
        if speed != Speed::NORMAL {
            assert_ne!(times(&chart), times(&one), "{speed:?}");
            assert_ne!(chart.scroll, one.scroll, "{speed:?}");
        }
    }
}

#[test]
fn a_chart_too_long_at_a_slow_speed_is_refused() {
    // 60 BPM: measure 600 starts at 40 min, inside the duration cap at 1×,
    // and past 2^32 µs (about 71.6 min) at 50 %. At 60 % it is 66.7 min.
    let text = "#BPM 60\n#WAV01 a.wav\n#60011:01\n";
    let one = load_at(text, Speed::NORMAL).unwrap().chart;
    assert_eq!(one.lanes[1][0].time, TimeUs(2_400_000_000));
    // The first kept position past the domain is measure 537's bar line:
    // 537 × 8 s = 4296 s.
    assert_eq!(
        load_at(text, Speed::SLOWEST),
        Err(LoadError::TooLongAtSpeed { measure: 537 })
    );
    let sixty = load_at(text, at(12)).unwrap().chart;
    assert_eq!(sixty.lanes[1][0].time, TimeUs(4_000_000_000));
    assert_eq!(shared(&sixty.identity), shared(&one.identity));
    // With a raised duration cap, 66.6 minutes load at 1× and 150 %, and
    // at 90 % (74 minutes) the domain still refuses them, from measure
    // 967 (4,297.8 s) on.
    let long = "#BPM 60\n#WAV01 a.wav\n#99911:01\n";
    let dir = tempfile::tempdir().unwrap();
    let opts = |speed| LoadOptions {
        speed,
        limits: Limits {
            max_duration: TimeUs(4_000_000_000),
            ..Limits::default()
        },
        ..LoadOptions::default()
    };
    assert!(load_bms(long.as_bytes(), dir.path(), &opts(Speed::NORMAL)).is_ok());
    assert!(load_bms(long.as_bytes(), dir.path(), &opts(Speed::FASTEST)).is_ok());
    assert_eq!(
        load_bms(long.as_bytes(), dir.path(), &opts(at(18))),
        Err(LoadError::TooLongAtSpeed { measure: 967 })
    );
    // The duration cap applies at 1×: 60.07 minutes are refused at 150 %,
    // where they would play in 40.
    let past_cap = "#BPM 60\n#WAV01 a.wav\n#90111:01\n";
    assert_eq!(
        load_at(past_cap, Speed::FASTEST),
        Err(LoadError::DurationExceeded { measure: 901 })
    );
}

/// A small random chart: a header BPM, BPM changes by reference and in hex,
/// STOPs, taps on every lane, long notes and BGM over four measures.
fn chart_text() -> impl Strategy<Value = String> {
    let bpm = prop::sample::select(vec!["60", "137.5", "140", "200.25", "333", "7.5"]);
    let stop = prop::sample::select(vec!["1", "13.5", "96", "0.001"]);
    let channel = prop::sample::select(vec![
        "11", "12", "13", "14", "15", "18", "19", "16", "51", "52", "56", "01", "08", "09", "03",
        "02",
    ]);
    let line = (0_u32..4, channel, prop::collection::vec(0_u8..4, 1..=16));
    (bpm.clone(), bpm, stop, prop::collection::vec(line, 1..24)).prop_map(
        |(header, change, stop, lines)| {
            let mut text = format!(
                "#BPM {header}\n#BPM01 {change}\n#STOP01 {stop}\n#WAV01 a.wav\n#WAV02 b.wav\n"
            );
            for (m, ch, slots) in lines {
                if ch == "02" {
                    // A measure length.
                    text.push_str(&format!("#{m:03}02:0.{}\n", slots.len() + 1));
                    continue;
                }
                let data: String = slots
                    .iter()
                    .map(|&s| match (ch, s) {
                        (_, 0) => "00",
                        ("03", 1) => "3C",
                        ("03", _) => "B4",
                        (_, 1) => "01",
                        (_, _) => "02",
                    })
                    .collect();
                text.push_str(&format!("#{m:03}{ch}:{data}\n"));
            }
            text
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// At any speed the chart has the 1× chart's objects, positions and
    /// identity; every real time lies within `20/k` of the scaled 1× time,
    /// so time order is kept; and the chart validates.
    #[test]
    fn mapped_chart_keeps_order_and_validates(text in chart_text(), k in 10_u32..=30) {
        let one = load_at(&text, Speed::NORMAL).unwrap();
        let mapped = load_at(&text, at(k)).unwrap();
        let (one, chart) = (one.chart, mapped.chart);
        prop_assert_eq!(chart.validate(), Ok(()));
        prop_assert_eq!(shared(&chart.identity), shared(&one.identity));
        prop_assert_eq!(&chart.bar_lines, &one.bar_lines);
        prop_assert_eq!(chart.bgm.len(), one.bgm.len());
        let k = i64::from(k);
        // T is in [t, t + 1), so floor(T × 20/k) is in
        // (t × 20/k - 1, (t + 1) × 20/k).
        let within = |t: i64, real: i64| t * 20 - k < real * k && real * k < (t + 1) * 20;
        for (lane, ones) in chart.lanes.iter().zip(&one.lanes) {
            prop_assert_eq!(lane.len(), ones.len());
            for (n, o) in lane.iter().zip(ones) {
                prop_assert_eq!((n.pos, n.end_pos(), n.sound), (o.pos, o.end_pos(), o.sound));
                prop_assert!(within(o.time.0, n.time.0), "{:?} {:?}", o, n);
                prop_assert!(within(o.end().0, n.end().0), "{:?} {:?}", o, n);
                prop_assert!(chart.scroll.position_at(n.time) <= n.pos);
            }
        }
        for (e, o) in chart.bgm.iter().zip(&one.bgm) {
            prop_assert!(within(o.time.0, e.time.0));
        }
        let (real, ones) = (times(&chart), times(&one));
        for (i, j) in (0..ones.len()).flat_map(|i| (0..ones.len()).map(move |j| (i, j))) {
            if ones[i] < ones[j] {
                prop_assert!(real[i] <= real[j]);
            }
        }
    }
}
