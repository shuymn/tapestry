//! The chart profile the loader builds (stage 3, S10-10): the notes of
//! each 1× second by category, their condensation to at most 512 bins and
//! the tempo rows, all from the 1× times whatever the speed the chart is
//! loaded at.

use bms_model::{
    ChartProfile, KEY_LONG_BODY, KEY_LONG_HEAD, KEY_TAP, MINE, NOTE_BINS, NOTE_CATEGORIES,
    SCRATCH_LONG_BODY, SCRATCH_LONG_HEAD, SCRATCH_TAP, Speed, Tempo,
};
use bms_parser::{LoadOptions, Loaded, load_bms};

fn load_at(text: &str, twentieths: u32) -> Loaded {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        speed: Speed::from_twentieths(twentieths).unwrap(),
        ..LoadOptions::default()
    };
    load_bms(text.as_bytes(), dir.path(), &opts).unwrap()
}

fn load(text: &str) -> ChartProfile {
    load_at(text, 20).profile
}

fn bpm(n: i64) -> Option<Tempo> {
    Tempo::from_milli(n * 1_000)
}

/// The speed rows as `(ms, tempo)`, `None` for a STOP.
fn rows(profile: &ChartProfile) -> Vec<(u32, Option<Tempo>)> {
    profile.speeds().iter().map(|r| (r.ms, r.tempo)).collect()
}

/// One second's counts by category.
fn second(cells: &[(usize, u32)]) -> [u32; NOTE_CATEGORIES] {
    let mut s = [0; NOTE_CATEGORIES];
    for &(c, n) in cells {
        s[c] = n;
    }
    s
}

/// 120 BPM, a measure 2 s, its four slots 0.5 s apart. Key 1 taps at
/// 1.0 s; a key 2 long note from 1.5 s to 4.2 s; the scratch taps at
/// 2.0 s and holds from 2.5 s to 3.5 s; a key 3 long note from 2.0 s to
/// 4.0 s; a key 4 mine at 3.0 s. Every endpoint but 4.2 s and 1.5 s lies on
/// a whole second.
const NOTES: &str = "\
#BPM 120
#LNTYPE 1
#00011:00000100
#00052:00000001
#00116:01000000
#00156:00010001
#00153:01
#001D4:00000100
#00252:00010000000000000000
#00253:01
";

/// Each note counts in its 1× second's category; a long note's body
/// fills every second from its head's to its end's, the head taking its
/// own second's place; its end counts nothing. `⌊4.2 s / 1 s⌋ + 2` = 6
/// seconds. The same at 1×, ×1.5 and ×0.5, though the chart's own times
/// moved.
#[test]
fn notes_count_in_their_1x_seconds_whatever_the_speed() {
    let expected = [
        second(&[]),
        second(&[(KEY_TAP, 1), (KEY_LONG_HEAD, 1)]),
        second(&[
            (SCRATCH_TAP, 1),
            (SCRATCH_LONG_HEAD, 1),
            (KEY_LONG_HEAD, 1),
            (KEY_LONG_BODY, 1),
        ]),
        second(&[(SCRATCH_LONG_BODY, 1), (KEY_LONG_BODY, 2), (MINE, 1)]),
        second(&[(KEY_LONG_BODY, 2)]),
        second(&[]),
    ];
    let normal = load_at(NOTES, 20);
    assert_eq!(normal.chart.meta.length.0, 4_200_000);
    assert_eq!(normal.profile.seconds(), expected);
    assert_eq!(normal.profile.bin_seconds(), 1);
    let bins: Vec<[u16; NOTE_CATEGORIES]> = expected
        .iter()
        .map(|s| s.map(|c| u16::try_from(c).unwrap()))
        .collect();
    assert_eq!(normal.profile.bins(), bins);
    for (twentieths, key1) in [(30, 666_666), (10, 2_000_000)] {
        let at = load_at(NOTES, twentieths);
        // The chart is at its real times, the profile at 1×.
        assert_eq!(at.chart.lanes[1][0].time.0, key1);
        assert_eq!(at.profile, normal.profile, "at {twentieths}/20");
    }
}

/// `⌊length / 1 s⌋ + 2` seconds: up to 512 a bin is one second; a chart
/// whose last note is at 510 s has 512, one past it (511 s) 513, in bins
/// of 2 s, 257 of them, each its fullest second's counts over every
/// category (the earliest on a tie), not their sum: two mines outweigh a
/// tap, and two long-note bodies a tap, though the tap's second holds more
/// taps.
#[test]
fn past_512_seconds_a_bin_takes_its_fullest_second() {
    let at_510 = load("#BPM 120\n#25511:01\n");
    assert_eq!(at_510.seconds().len(), 512);
    assert_eq!(at_510.bins().len(), NOTE_BINS);
    assert_eq!(at_510.bin_seconds(), 1);
    assert_eq!(at_510.bins()[510], [0, 0, 0, 0, 0, 1, 0]);

    // Second 0: a key tap; second 1: a scratch tap (a tie, the earliest
    // wins). Second 2: one key tap; second 3: two (the fullest). Second 4:
    // a key tap; second 5: two mines. Seconds 9 and 10: a scratch and a
    // key long note from 9.5 s to 10.5 s, their heads in second 9, their
    // bodies alone in second 10; second 11: a tap.
    let at_511 = load(
        "#BPM 120\n#LNTYPE 1\n#00011:01000000\n#00016:00000100\n\
         #00112:01000000\n#00113:00000100\n#00114:00000001\n\
         #00215:01000000\n#002D1:00000100\n#002D2:00000100\n\
         #00456:00000001\n#00453:00000001\n#00556:00010000\n#00553:00010000\n\
         #00514:00000100\n#25511:0001\n",
    );
    assert_eq!(at_511.seconds().len(), 513);
    assert_eq!(at_511.bin_seconds(), 2);
    assert_eq!(at_511.bins().len(), 257);
    let bins = at_511.bins();
    assert_eq!(bins[0], [0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(bins[1], [0, 0, 0, 0, 0, 2, 0]);
    assert_eq!(bins[2], [0, 0, 0, 0, 0, 0, 2]);
    assert_eq!(bins[4], [1, 0, 0, 1, 0, 0, 0]);
    assert_eq!(bins[5], [0, 1, 0, 0, 1, 0, 0]);
    assert_eq!(bins[255], [0, 0, 0, 0, 0, 1, 0]);
    let drawn = bins.iter().filter(|b| b.iter().any(|&c| c > 0)).count();
    assert_eq!(drawn, 6);
}

/// A STOP of 2 ms and a change to 240 BPM at one point give only the
/// stopped row; the speed comes back at the next point. A STOP's length
/// takes the tempo after a change at its point (0.384 units at 240 BPM).
#[test]
fn a_stop_with_a_tempo_change_gives_only_the_stopped_row() {
    let p = load("#BPM 120\n#STOP01 0.384\n#00011:01000001\n#00003:0000F000\n#00009:00000100\n");
    assert_eq!(rows(&p), [(0, bpm(120)), (1_000, None), (1_252, bpm(240))]);
}

/// A STOP below 1 ms is no stop: with a change at its point, the tempo's
/// row (500 µs); alone, no row at all (999 µs), even from a fractional
/// start whose floored end lies 1 000 µs after its floored start (999.5 µs
/// from 666 666⅔ µs at 180 BPM).
#[test]
fn a_sub_ms_stop_stops_nothing() {
    let with_change =
        load("#BPM 120\n#STOP01 0.096\n#00011:01000001\n#00003:0000F000\n#00009:00000100\n");
    assert_eq!(
        rows(&with_change),
        [(0, bpm(120)), (1_000, bpm(240)), (1_250, bpm(240))]
    );
    let alone = load("#BPM 120\n#STOP01 0.095904\n#00011:01000001\n#00009:00000100\n");
    assert_eq!(rows(&alone), [(0, bpm(120)), (1_500, bpm(120))]);
    let fractional = load_at(
        "#BPM 180\n#STOP01 0.143928\n#00011:01000001\n#00009:00000100\n",
        20,
    );
    let points: Vec<i64> = fractional
        .chart
        .timeline
        .points()
        .iter()
        .map(|p| p.at.0)
        .collect();
    assert_eq!(points, [0, 666_666, 667_666, 1_000_999]);
    assert_eq!(
        rows(&fractional.profile),
        [(0, bpm(180)), (1_000, bpm(180))]
    );
}

/// A STOP of 500 ms from 1.5 s: its end (2.0 s) is no point, so the speed
/// comes back at measure 1's bar line (2.5 s), a point with nothing else,
/// before the tap at 3.5 s. The rows are the same, at 1× ms, loaded at
/// ×1.5 and ×0.5.
#[test]
fn a_stop_resumes_at_the_next_point_a_bar_line_included() {
    let text = "#BPM 120\n#STOP01 48\n#00011:01\n#00009:00000001\n#00111:00000100\n";
    let expected = [
        (0, bpm(120)),
        (1_500, None),
        (2_500, bpm(120)),
        (3_500, bpm(120)),
    ];
    assert_eq!(rows(&load(text)), expected);
    for twentieths in [30, 10] {
        assert_eq!(rows(&load_at(text, twentieths).profile), expected);
    }
}

/// The last row is at the last point, measure 2's bar line at 4.0 s after
/// the last note (its BGM object names no sound, so the chart drops it),
/// with the current tempo.
#[test]
fn a_trailing_bar_line_is_the_last_point() {
    let loaded = load_at("#BPM 120\n#00011:0001\n#00201:01\n", 20);
    assert!(loaded.chart.bgm.is_empty());
    assert_eq!(rows(&loaded.profile), [(0, bpm(120)), (4_000, bpm(120))]);
}

/// A change at time 0 adds a row there after the initial tempo's, so
/// three points that each change or end it give four rows; a change to the
/// current tempo adds none.
#[test]
fn a_change_at_zero_adds_a_row_and_an_equal_tempo_none() {
    // 60 BPM from 0 (a beat 1 s), 90 BPM at beat 2 (2.0 s), a tap a beat
    // of 90 BPM later (2.666 666 s).
    let changes = load("#BPM 120\n#00003:3C005A00\n#00011:00000001\n");
    assert_eq!(
        rows(&changes),
        [
            (0, bpm(120)),
            (0, bpm(60)),
            (2_000, bpm(90)),
            (2_666, bpm(90))
        ]
    );
    let equal = load("#BPM 120\n#00003:78007800\n#00011:00000001\n");
    assert_eq!(rows(&equal), [(0, bpm(120)), (1_500, bpm(120))]);
}
