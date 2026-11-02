//! The chart's timeline (design.md "Chart Model", "Timeline"): one row per
//! retained position and positive STOP end, every note endpoint and bar
//! line linked to its row, the exact original tempos, and presentation
//! validity at the loaded speed computed from the chart, never refused by
//! the loader.

use bms_model::{
    BEAT, EndpointLocation, EndpointPart, Lane, NoteKind, Position, PresentationError, Speed,
    Tempo, TimeUs, TimelineId,
};
use bms_parser::{Limits, LoadError, LoadOptions, Loaded, WarningKind, load_bms};

fn load_with(text: &str, speed: Speed, limits: Limits) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        speed,
        limits,
        ..LoadOptions::default()
    };
    let mut loaded = load_bms(text.as_bytes(), dir.path(), &opts)?;
    loaded
        .warnings
        .retain(|w| w.kind != WarningKind::MissingSoundFile);
    Ok(loaded)
}

fn load_at(text: &str, speed: Speed) -> Loaded {
    load_with(text, speed, Limits::default()).unwrap()
}

fn at(k: u32) -> Speed {
    Speed::from_twentieths(k).unwrap()
}

fn bpm(milli: i64) -> Tempo {
    Tempo::from_milli(milli).unwrap()
}

/// `slots` object pairs with `01` at `index` and `00` elsewhere.
fn line(slots: usize, index: usize) -> String {
    (0..slots)
        .map(|i| if i == index { "01" } else { "00" })
        .collect()
}

/// The rows as `(µs, position, tempo)`.
fn rows(loaded: &Loaded) -> Vec<(i64, Position, Tempo)> {
    loaded
        .chart
        .timeline
        .points()
        .iter()
        .map(|p| (p.at.0, p.pos, p.tempo))
        .collect()
}

fn beats(n: i128) -> Position {
    Position(BEAT * n)
}

#[test]
fn every_retained_position_and_stop_end_has_a_row() {
    // 120 BPM, 500 ms a beat. Measure 0: a BGM object at beat 1, a STOP of
    // one beat (48 units) with a change to 240 BPM at beat 2, a tap at beat
    // 3. Measure 1: a tap at its start, with its bar line.
    let text = "#BPM 120\n#WAV01 a.wav\n#STOP01 48\n#BPM01 240\n\
                #00001:00010000\n#00009:00000100\n#00008:00000100\n#00011:00000001\n#00111:01\n";
    let loaded = load_at(text, Speed::NORMAL);
    let chart = &loaded.chart;
    assert_eq!(chart.validate_presentation(), Ok(()));
    let (b120, b240) = (bpm(120_000), bpm(240_000));
    assert_eq!(
        rows(&loaded),
        [
            // Measure 0's bar line.
            (0, beats(0), b120),
            // The note-free BGM position.
            (500_000, beats(1), b120),
            // The change and the STOP's start: the STOP is one beat at the
            // new 240 BPM, 250 ms, holding beat 2.
            (1_000_000, beats(2), b240),
            (1_250_000, beats(2), b240),
            // The tap at beat 3.
            (1_500_000, beats(3), b240),
            // Measure 1's bar line and tap.
            (1_750_000, beats(4), b240),
        ]
    );
    assert_eq!(chart.timeline.bar_ids(), [TimelineId(0), TimelineId(5)]);
    let heads: Vec<u32> = chart.lanes[1].iter().map(|n| n.anchors.head.0).collect();
    assert_eq!(heads, [4, 5]);
    let meta = chart.timeline.meta();
    assert_eq!(
        (meta.initial, meta.min, meta.max, meta.speed),
        (b120, b120, b240, Speed::NORMAL)
    );
}

#[test]
fn notes_at_a_stop_name_its_start_and_the_initial_tempo_survives_time_zero() {
    // 150 BPM header, 300 BPM and a STOP at time 0 with a tap there.
    let text = "#BPM 150\n#WAV01 a.wav\n#BPM01 300\n#STOP01 48\n\
                #00008:01\n#00009:01\n#00011:01\n#00111:01\n";
    let loaded = load_at(text, Speed::NORMAL);
    let t = &loaded.chart.timeline;
    let b300 = bpm(300_000);
    // The STOP is a beat at 300 BPM: 200 ms at position 0.
    assert_eq!(
        rows(&loaded)[..3],
        [
            (0, beats(0), b300),
            (200_000, beats(0), b300),
            (1_000_000, beats(4), b300)
        ]
    );
    assert_eq!(loaded.chart.lanes[1][0].anchors.head, TimelineId(0));
    assert_eq!(t.meta().initial, bpm(150_000));
    assert_eq!(t.tempo_at(TimeUs(-1)), bpm(150_000));
    // During the STOP the tempo stays positive.
    assert_eq!(t.tempo_at(TimeUs(100_000)), b300);
    let events: Vec<(i64, Tempo)> = t.tempo_events().map(|e| (e.at.0, e.tempo)).collect();
    assert_eq!(events, [(0, b300)]);
}

#[test]
fn tempos_are_exact_original_bpm() {
    let fine = Tempo::from_units(150 * Tempo::UNITS_PER_BPM + 1).unwrap();
    let text = "#BPM 150.000000000000000001\n#WAV01 a.wav\n#00011:01\n";
    for k in [10, 20, 30] {
        let loaded = load_at(text, at(k));
        let meta = loaded.chart.timeline.meta();
        // The practice speed is carried apart, never applied to tempos.
        assert_eq!((meta.initial, meta.max, meta.speed), (fine, fine, at(k)));
        assert_eq!(fine.ratio(), None);
    }
    let hex = load_at(
        "#BPM 1\n#WAV01 a.wav\n#00003:FF\n#00011:01\n",
        Speed::NORMAL,
    );
    assert_eq!(hex.chart.timeline.meta().max, bpm(255_000));
    assert_eq!(hex.chart.timeline.meta().min, bpm(1_000));
}

#[test]
fn a_speed_maps_row_times_once_and_keeps_the_rest() {
    // 120 BPM with measure 0 0.0010005 long (2 001 µs): its half is
    // 1 000.5 µs, 2 001 µs at 50 % (mapped before flooring), not 2 × 1 000.
    let text = "#BPM 120\n#WAV01 a.wav\n#00002:0.0010005\n#00011:0001\n#00111:01\n";
    let one = load_at(text, Speed::NORMAL);
    let half = load_at(text, Speed::SLOWEST);
    assert_eq!(rows(&one)[1].0, 1_000);
    assert_eq!(rows(&half)[1].0, 2_001);
    assert_eq!(half.chart.lanes[1][0].time, TimeUs(2_001));
    assert_eq!(half.chart.validate_presentation(), Ok(()));
    let (t1, th) = (&one.chart.timeline, &half.chart.timeline);
    assert_eq!(th.meta().speed, Speed::SLOWEST);
    assert_eq!(
        (th.meta().initial, th.meta().min, th.meta().max),
        (t1.meta().initial, t1.meta().min, t1.meta().max)
    );
    assert_eq!(th.bar_ids(), t1.bar_ids());
    for (a, b) in t1.points().iter().zip(th.points()) {
        assert_eq!((a.pos, a.tempo), (b.pos, b.tempo));
    }
    assert_eq!(th.points().len(), t1.points().len());
    for (a, b) in one
        .chart
        .lanes
        .iter()
        .flatten()
        .zip(half.chart.lanes.iter().flatten())
    {
        assert_eq!(a.anchors, b.anchors);
    }
    // The identity covers the rows at 1×; the played hash the real ones.
    assert_eq!(
        half.chart.identity.resolved_hash,
        one.chart.identity.resolved_hash
    );
    assert_ne!(
        half.chart.identity.played_hash,
        one.chart.identity.resolved_hash
    );
}

#[test]
fn dropped_notes_keep_their_rows_and_renumber_nothing() {
    // Two taps at one position (the later in the file wins) and a mine
    // inside a long note: rows stay, and later notes keep theirs.
    let text = "#BPM 120\n#WAV01 a.wav\n#WAV02 a.wav\n\
                #00011:01000000\n#00011:02000000\n\
                #00052:01000100\n#000D2:00010000\n#00012:0000000001\n";
    let loaded = load_at(text, Speed::NORMAL);
    let lanes = &loaded.chart.lanes;
    assert_eq!(lanes[1].len(), 1);
    assert_eq!(lanes[1][0].sound.map(|s| s.0), Some(1));
    assert_eq!(lanes[1][0].anchors.head, TimelineId(0));
    // Rows: 0 (start), 1/4 (the mine), 1/2 (the end), 4/5 (key 2's tap).
    assert_eq!(rows(&loaded).len(), 4);
    assert!(matches!(lanes[2][0].kind, NoteKind::Long { .. }));
    assert_eq!(
        (lanes[2][0].anchors.head, lanes[2][0].anchors.end),
        (TimelineId(0), TimelineId(2))
    );
    assert_eq!(lanes[2][1].anchors.head, TimelineId(3));
    assert_eq!(loaded.chart.validate_presentation(), Ok(()));
}

#[test]
fn the_row_limit_is_charged_before_rows_are_built() {
    // Rows: measure 0's bar line, the STOP's position and its end, the tap
    // in measure 1 with its bar line: four.
    let text = "#BPM 120\n#WAV01 a.wav\n#STOP01 48\n#00009:0001\n#00111:01\n";
    let limits = |n| Limits {
        max_timeline_points: n,
        ..Limits::default()
    };
    for speed in [Speed::NORMAL, Speed::SLOWEST] {
        let loaded = load_with(text, speed, limits(4)).unwrap();
        assert_eq!(loaded.chart.timeline.points().len(), 4);
        assert_eq!(
            load_with(text, speed, limits(3)),
            Err(LoadError::TimelineLimit { measure: 1 })
        );
        assert_eq!(
            load_with(text, speed, limits(2)),
            Err(LoadError::TimelineLimit { measure: 0 })
        );
    }
    assert_eq!(
        Limits::default().max_timeline_points,
        bms_model::MAX_TIMELINE_POINTS
    );
}

/// Two taps in one four-beat measure at 150 BPM (1.6 s), each on its own
/// line of `slots` with its `index`: key 1 the first, key 2 the second.
fn pair(first: (usize, usize), second: (usize, usize)) -> String {
    format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{}\n#00012:{}\n",
        line(first.1, first.0),
        line(second.1, second.0)
    )
}

fn head(lane: u8, row: u32) -> EndpointLocation {
    EndpointLocation {
        lane: Lane::new(lane).unwrap(),
        index: 0,
        part: EndpointPart::Head,
        timeline_id: TimelineId(row),
    }
}

/// The conflict of [`pair`] at `at` µs: rows 1 and 2 hold the earlier and
/// the later position.
fn conflict(
    at: i64,
    keys: (u8, u8),
    first: (i128, i128),
    second: (i128, i128),
) -> PresentationError {
    PresentationError::EndpointConflict {
        at: TimeUs(at),
        first: head(keys.0, 1),
        second: head(keys.1, 2),
        first_pos: Position(BEAT * 4 * first.0 / first.1),
        second_pos: Position(BEAT * 4 * second.0 / second.1),
    }
}

#[test]
fn speed_mapping_can_make_a_valid_chart_conflict() {
    // 1003/2003 and 1002/2001 are 801 198 and 801 199 µs at 100 %, both
    // 534 132 at 150 %; 8/10009 and 8/10007 are 1 278 and 1 279 µs, both
    // 1 598 at 80 %. The loader keeps each chart; validity is the chart's
    // at its speed. Key 1 holds the earlier position, row 1.
    for (first, second, speed, at_speed) in [
        ((1003, 2003), (1002, 2001), at(30), 534_132),
        ((8, 10009), (8, 10007), at(16), 1_598),
    ] {
        let text = pair(first, second);
        let normal = load_at(&text, Speed::NORMAL);
        assert_eq!(normal.chart.validate_presentation(), Ok(()));
        let mapped = load_at(&text, speed);
        assert_eq!(mapped.chart.validate(), Ok(()));
        assert_eq!(mapped.chart.validate_presentation_links(), Ok(()));
        let frac = |(i, n): (usize, usize)| (i as i128, n as i128);
        assert_eq!(
            mapped.chart.validate_presentation(),
            Err(conflict(at_speed, (1, 2), frac(first), frac(second)))
        );
    }
}

#[test]
fn a_chart_conflicting_at_normal_speed_loads_and_is_valid_at_half() {
    // 1000/2001 and 1001/2003 both floor to 799 600 µs at 100 %, and to
    // 1 599 200 and 1 599 201 at 50 %.
    let text = pair((1000, 2001), (1001, 2003));
    let normal = load_at(&text, Speed::NORMAL);
    assert_eq!(normal.chart.validate(), Ok(()));
    assert_eq!(
        normal.chart.validate_presentation(),
        Err(conflict(799_600, (1, 2), (1000, 2001), (1001, 2003)))
    );
    let half = load_at(&text, Speed::SLOWEST);
    assert_eq!(half.chart.validate_presentation(), Ok(()));
    let times: Vec<i64> = rows(&half).iter().map(|r| r.0).collect();
    assert_eq!(times, [0, 1_599_200, 1_599_201]);
}

#[test]
fn equal_positions_at_one_us_are_no_conflict() {
    // The same position on two keys, and BGM at a neighbouring position in
    // the same µs: neither conflicts.
    let text = format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{a}\n#00013:{a}\n#00001:{b}\n",
        a = line(2001, 1000),
        b = line(2003, 1001)
    );
    let loaded = load_at(&text, Speed::NORMAL);
    assert_eq!(rows(&loaded)[1].0, rows(&loaded)[2].0);
    assert_eq!(loaded.chart.validate_presentation(), Ok(()));
}
