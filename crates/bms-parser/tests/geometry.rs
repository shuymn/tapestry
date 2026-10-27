//! Display geometry against exact arithmetic done here, never against the
//! scroll map under test (design.md "Chart Time", "Testing Strategy").

use bms_model::{BEAT, Chart, DOMAIN, Position, ScrollMap, TimeUs};
use bms_parser::{Limits, LoadError, LoadOptions, Loaded, load_bms};
use num_bigint::BigInt;
use num_rational::Ratio;

type Q = Ratio<BigInt>;

fn q(n: i64, d: i64) -> Q {
    Q::new(BigInt::from(n), BigInt::from(d))
}

/// A decimal written `int.frac`, exactly.
fn dec(text: &str) -> Q {
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    let digits: BigInt = format!("{int}{frac}").parse().unwrap();
    Q::new(
        digits,
        BigInt::from(10).pow(u32::try_from(frac.len()).unwrap()),
    )
}

/// `floor(2^80 × beats)`.
fn units(beats: &Q) -> i128 {
    i128::try_from((beats * BigInt::from(BEAT)).floor().to_integer()).unwrap()
}

fn floor(x: &Q) -> i64 {
    i64::try_from(x.floor().to_integer()).unwrap()
}

/// µs per beat at `bpm`.
fn us_per_beat(bpm: &Q) -> Q {
    Q::from_integer(BigInt::from(60_000_000)) / bpm
}

fn load_with(text: &str, limits: Limits) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        limits,
        ..LoadOptions::default()
    };
    load_bms(text.as_bytes(), dir.path(), &opts)
}

fn load(text: &str) -> Chart {
    load_with(text, Limits::default()).unwrap().chart
}

fn at(map: &ScrollMap, t: i64) -> i128 {
    map.position_at(TimeUs(t)).0
}

/// The playhead at `t` stays at or below the exact curve's `exact` (in
/// beats) by at most a unit per µs since the last segment start.
#[track_caller]
fn brackets(map: &ScrollMap, t: i64, exact: &Q) {
    let start = map
        .segments()
        .iter()
        .rev()
        .find(|s| s.start.0 <= t)
        .unwrap()
        .start
        .0;
    let d = units(exact) - at(map, t);
    assert!(
        (0..=i128::from(t - start)).contains(&d),
        "t = {t}: {d} units off, {} µs since the segment start",
        t - start
    );
}

/// Quarter-beat measures at 9,990,400 BPM (1.5014... µs each), with the
/// same BPM restated at every measure, through measure `last`, which holds
/// a note.
fn quarter_beats(last: usize) -> String {
    let mut text = String::from("#BPM 9990400\n#BPM01 9990400\n#WAV01 a.wav\n");
    for m in 0..=last {
        text.push_str(&format!("#{m:03}02:0.0625\n#{m:03}08:01\n"));
    }
    text.push_str(&format!("#{last:03}11:01\n"));
    text
}

#[test]
fn bar_lines_sit_on_their_exact_beats() {
    let chart = load(&quarter_beats(64));
    let expected: Vec<Position> = (0..=64).map(|k| Position(k * (BEAT / 4))).collect();
    assert_eq!(chart.bar_lines, expected);
    assert_eq!(chart.lanes[1][0].pos, Position(64 * (BEAT / 4)));
}

#[test]
fn the_playhead_brackets_each_bar_line_between_its_microseconds() {
    // Quarter-beat measures alternating 9,990,400 and 7,777,777 BPM, so
    // every change falls inside a µs and the rate differs on either side.
    let bpms = [q(9_990_400, 1), q(7_777_777, 1)];
    let mut text = String::from("#BPM 9990400\n#BPM01 9990400\n#BPM02 7777777\n#WAV01 a.wav\n");
    for m in 0..=64 {
        let id = ["01", "02"][m % 2];
        text.push_str(&format!("#{m:03}02:0.0625\n#{m:03}08:{id}\n"));
    }
    text.push_str("#06411:01\n");
    let chart = load(&text);
    let map = &chart.scroll;
    let mut time = Q::from_integer(BigInt::from(0));
    let mut fractional = 0;
    for k in 0..=64_usize {
        let t = floor(&time);
        let beats = q(i64::try_from(k).unwrap(), 4);
        let line = units(&beats);
        assert!(at(map, t) <= line, "k = {k}");
        assert!(at(map, t + 1) >= line, "k = {k}");
        let per_beat = us_per_beat(&bpms[k % 2]);
        if !time.is_integer() {
            fractional += 1;
            // The segment of bar k's change starts at its first whole µs,
            // anchored on the exact curve there.
            let gap = Q::from_integer(BigInt::from(t + 1)) - &time;
            assert_eq!(
                at(map, t + 1),
                units(&(&beats + gap / &per_beat)),
                "k = {k}"
            );
        }
        time += q(1, 4) * per_beat;
    }
    assert!(fractional > 60);
}

#[test]
fn a_bpm_section_inside_one_microsecond_keeps_its_beats() {
    // Measure 0 at BPM 7 ends at 240e6 / 7 µs (34_285_714.28...); measure 1
    // runs its 4 beats at 999_999_999_999 BPM, 0.00024 µs, so both changes
    // fall in one µs; measure 2 is back at 7.
    let text = "#BPM 7\n#BPM01 999999999999\n#BPM02 7\n#WAV01 a.wav\n\
                #00108:01\n#00208:02\n#00211:01\n#00311:01\n";
    let chart = load(text);
    let t1 = q(4, 1) * us_per_beat(&q(7, 1));
    let t2 = &t1 + q(4, 1) * us_per_beat(&q(999_999_999_999, 1));
    assert_eq!(floor(&t1), floor(&t2));
    assert_eq!(chart.lanes[1][0].pos, Position(8 * BEAT));
    // Later samples carry the section's 4 beats.
    for t in [
        floor(&t2) + 1,
        floor(&t2) + 2,
        floor(&t2) + 1_000,
        50_000_000,
    ] {
        let exact = q(8, 1) + (Q::from_integer(BigInt::from(t)) - &t2) / us_per_beat(&q(7, 1));
        brackets(&chart.scroll, t, &exact);
    }
}

#[test]
fn a_stop_shorter_than_a_microsecond_delays_what_follows() {
    // At 120 BPM a STOP of 0.000001 units lasts 1e-6 / 48 beat × 500_000
    // µs = 0.0104... µs, from 2 s exactly.
    let text = "#BPM 120\n#STOP01 0.000001\n#WAV01 a.wav\n#00109:01\n#00211:01\n";
    let chart = load(text);
    let per_beat = us_per_beat(&q(120, 1));
    let end = Q::from_integer(BigInt::from(2_000_000)) + dec("0.000001") / q(48, 1) * &per_beat;
    // Flat at 4 beats through 2_000_000, then on the delayed curve.
    assert_eq!(at(&chart.scroll, 2_000_000), 4 * BEAT);
    let after = |t: i64| q(4, 1) + (Q::from_integer(BigInt::from(t)) - &end) / &per_beat;
    assert_eq!(at(&chart.scroll, 2_000_001), units(&after(2_000_001)));
    for t in [2_000_002, 2_500_000, 3_999_999] {
        brackets(&chart.scroll, t, &after(t));
    }
    // The delay, 2.08e-8 beat, is far more than the bracket allows: the
    // curve without the STOP lies outside it.
    let undelayed =
        q(4, 1) + (Q::from_integer(BigInt::from(2_500_000)) - q(2_000_000, 1)) / &per_beat;
    assert!(units(&undelayed) - at(&chart.scroll, 2_500_000) > 1_000_000_000);
}

#[test]
fn a_tiny_bpm_keeps_a_tiny_position() {
    // At 0.000001 BPM, measure 0 is 4 × 5e-9 = 2e-8 beat, 1.2 s.
    let text = "#BPM 0.000001\n#WAV01 a.wav\n#00002:0.000000005\n#00111:01\n";
    let chart = load(text);
    let note = chart.lanes[1][0];
    assert_eq!(note.time, TimeUs(1_200_000));
    assert_eq!(note.pos, Position((BEAT * 2) / 100_000_000));
    assert!(at(&chart.scroll, 1_200_000) > 0);
    brackets(&chart.scroll, 1_200_000, &dec("0.00000002"));
}

#[test]
fn the_largest_bpm_and_measure_length_place_exactly() {
    let top = "999999999999.999999999999999999";
    let text = format!("#BPM {top}\n#WAV01 a.wav\n#00002:{top}\n#00102:{top}\n#00111:0001\n");
    let chart = load(&text);
    let span = q(4, 1) * dec(top);
    let half = &span + &span / q(2, 1);
    let note = chart.lanes[1][0];
    assert_eq!(note.pos, Position(units(&half)));
    assert_eq!(chart.bar_lines, vec![Position(0), Position(units(&span))]);
    let time = &half * us_per_beat(&dec(top));
    assert_eq!(note.time, TimeUs(floor(&time)));
    brackets(
        &chart.scroll,
        note.time.0,
        &(Q::from_integer(BigInt::from(note.time.0)) / us_per_beat(&dec(top))),
    );
}

#[test]
fn raised_limits_refuse_times_past_the_scroll_domain() {
    // At 60 BPM a beat is 1 s: 4 × 1100 beats end measure 0 at 4400 s,
    // past 2^32 µs (4294.97 s); 4 × 1000 beats end it at 4000 s, within.
    let limits = Limits {
        max_duration: TimeUs(1 << 40),
        ..Limits::default()
    };
    let chart = |length: u32| format!("#BPM 60\n#WAV01 a.wav\n#00002:{length}\n#00111:01\n");
    assert_eq!(
        load_with(&chart(1100), limits),
        Err(LoadError::PositionOverflow { measure: 1 })
    );
    let loaded = load_with(&chart(1000), limits).unwrap();
    assert_eq!(loaded.chart.lanes[1][0].time, TimeUs(4_000_000_000));
    assert_eq!(loaded.chart.lanes[1][0].pos, Position(4000 * BEAT));
    // Within the default limits the duration cap comes first.
    assert_eq!(
        load_with(&chart(1100), Limits::default()),
        Err(LoadError::DurationExceeded { measure: 1 })
    );
    const { assert!(4_000_000_000 < DOMAIN) };
}

#[test]
fn a_deceleration_and_a_stop_inside_a_microsecond_keep_samples_in_order() {
    // Measure 0 at 120 BPM is 1e-6 beat, 0.5 µs; measure 1 turns to 60 BPM
    // and stops for 1/48 beat (20_833.3... µs at 60) a third of the way in,
    // at a fractional µs.
    let text = "#BPM 120\n#STOP01 1\n#WAV01 a.wav\n#00002:0.00000025\n#00103:3C\n\
                #00109:000100\n#00111:000001\n";
    let chart = load(text);
    let map = &chart.scroll;
    let per_beat_60 = us_per_beat(&q(60, 1));
    let t1 = q(1, 2);
    let stop_at = &t1 + q(4, 3) * &per_beat_60;
    let stop_end = &stop_at + q(1, 48) * &per_beat_60;
    let stop_beats = q(1, 1_000_000) + q(4, 3);
    let exact = |t: i64| {
        let t = Q::from_integer(BigInt::from(t));
        if t <= t1 {
            &t * q(1, 500_000)
        } else if t <= stop_at {
            q(1, 1_000_000) + (&t - &t1) / &per_beat_60
        } else if t <= stop_end {
            stop_beats.clone()
        } else {
            &stop_beats + (&t - &stop_end) / &per_beat_60
        }
    };
    assert!(!stop_at.is_integer() && !stop_end.is_integer());
    let mut ts: Vec<i64> = (0..4_000_000).step_by(9_973).collect();
    for edge in [floor(&stop_at), floor(&stop_end)] {
        ts.extend(edge - 1..=edge + 2);
    }
    ts.extend(0..4);
    ts.sort_unstable();
    let mut prev = i128::MIN;
    for t in ts {
        let p = at(map, t);
        assert!(p >= prev, "t = {t}");
        prev = p;
        brackets(map, t, &exact(t));
    }
}

#[test]
fn a_stop_at_time_zero_loads_and_scrolls_back_at_the_initial_rate() {
    let text = "#BPM 120\n#STOP01 48\n#WAV01 a.wav\n#00009:01\n#00011:01\n";
    let chart = load(text);
    let map = &chart.scroll;
    assert_eq!(map.segments()[0].slope, 0);
    // floor(2^80 / 500_000) units per µs at 120 BPM.
    let rate = BEAT / 500_000;
    assert_eq!(map.initial_rate(), rate);
    assert_eq!(at(map, -1_000), -1_000 * rate);
    // Flat through the 1-beat STOP (500 ms), on at 120 BPM after it.
    assert_eq!(at(map, 499_999), 0);
    assert_eq!(at(map, 500_001), rate);
    assert_eq!(chart.lanes[1][0].time, TimeUs(0));
}

#[test]
fn a_bpm_change_and_a_stop_at_one_position_stop_at_the_new_bpm() {
    // Measure 0 at BPM 7 ends at 34_285_714.28... µs, where the BPM turns
    // to 120 and a 1-beat STOP starts: 500_000 µs at 120 (at 7 it would be
    // 8_571_428.57...). The scroll resumes at 120.
    let text = "#BPM 7\n#STOP01 48\n#WAV01 a.wav\n#00103:78\n#00109:01\n#00211:01\n";
    let chart = load(text);
    let map = &chart.scroll;
    let start = q(4, 1) * us_per_beat(&q(7, 1));
    let end = &start + q(500_000, 1);
    let flat_from = floor(&start) + 1;
    let flat_to = floor(&end);
    assert_eq!(at(map, flat_from), 4 * BEAT);
    assert_eq!(at(map, flat_to), 4 * BEAT);
    assert!(at(map, flat_to + 1) > 4 * BEAT);
    assert!(at(map, flat_from - 1) < 4 * BEAT);
    assert_eq!(map.segments().last().unwrap().slope, BEAT / 500_000);
    let note = chart.lanes[1][0];
    assert_eq!(note.pos, Position(8 * BEAT));
    // Measure 1 runs at 120 after the STOP: 4 beats in 2 s.
    assert_eq!(note.time, TimeUs(floor(&(&end + q(2_000_000, 1)))));
}

#[test]
fn notes_in_one_microsecond_keep_their_own_positions() {
    // Measure 0 is 4e-6 beat, 2 µs: notes at 3e-6 beat (1.5 µs) and 2e-6
    // beat (1 µs) share µs 1 in two lanes, each at its exact beat.
    let text = "#BPM 120\n#WAV01 a.wav\n#00002:0.000001\n#00011:00000001\n#00012:0001\n";
    let chart = load(text);
    assert_eq!(chart.lanes[1][0].time, TimeUs(1));
    assert_eq!(chart.lanes[2][0].time, TimeUs(1));
    assert_eq!(chart.lanes[1][0].pos, Position(units(&q(3, 1_000_000))));
    assert_eq!(chart.lanes[2][0].pos, Position(units(&q(2, 1_000_000))));
}
