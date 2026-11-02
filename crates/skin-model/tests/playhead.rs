//! The exact presentation playhead (`ExactPlayhead`): every eligible note
//! endpoint is met at its judgment µs, every stored µs of the timeline
//! takes part, rows of other kinds yield to an endpoint at their µs, a
//! section changes precedence but never admits a refused chart, and the
//! curve never decreases. Charts are loaded as a session loads them.

mod play_support;

use bms_model::{
    BEAT, Chart, ChartBuilder, DOMAIN, NoteKind, Position, PresentationError, TimeUs, TimelineId,
};
use play_support::{Harness, full, lane, load};
use skin_model::{ExactPlayhead, PrepareError};

fn loaded(text: &str) -> Chart {
    let dir = tempfile::tempdir().unwrap();
    bms_parser::load_bms(text.as_bytes(), dir.path(), &Default::default())
        .unwrap()
        .chart
}

/// `slots` object pairs with `01` at `index`.
fn line(slots: usize, index: usize) -> String {
    (0..slots)
        .map(|i| if i == index { "01" } else { "00" })
        .collect()
}

/// Every note endpoint of `chart` with its judgment µs and position.
fn endpoints(chart: &Chart) -> Vec<(TimeUs, Position)> {
    let mut out = Vec::new();
    for n in chart.lanes.iter().flatten() {
        out.push((n.time, n.pos));
        if let NoteKind::Long { end, end_pos, .. } = n.kind {
            out.push((end, end_pos));
        }
    }
    out
}

/// Positions at every µs of `[from, to]` never decrease.
fn assert_monotone(p: &ExactPlayhead, from: i64, to: i64) {
    let mut prev = p.position_at(TimeUs(from));
    for t in from + 1..=to {
        let here = p.position_at(TimeUs(t));
        assert!(here >= prev, "{t}: {here:?} < {prev:?}");
        prev = here;
    }
}

#[test]
fn every_endpoint_meets_the_line_at_its_judgment_us() {
    // Quarter-beat measures at 9,990,400 BPM: measure k starts k ×
    // 1.5014416 µs in, so its notes floor to µs between their exact
    // times; a BPM change to 7,777,777 at measure 4 and a STOP at measure
    // 6, with a long note across both and a tap on the STOP.
    let mut text = String::from(
        "#BPM 9990400\n#BPM01 7777777\n#STOP01 7\n#LNTYPE 1\n#WAV01 a.wav\n#00408:01\n#00609:01\n",
    );
    for m in 0..=12 {
        text.push_str(&format!("#{m:03}02:0.0625\n"));
    }
    for m in 1..=11 {
        text.push_str(&format!("#{m:03}11:01\n"));
    }
    text.push_str("#00352:01\n#00852:01\n#00613:01\n#01214:01\n");
    let chart = loaded(&text);
    let p = ExactPlayhead::prepare(&chart, None).unwrap();
    let ends = endpoints(&chart);
    assert!(ends.len() > 12);
    for (t, pos) in ends {
        // The sampled scroll map misses most of them by up to a µs of
        // scroll; the playhead meets each exactly.
        assert_eq!(p.position_at(t), pos, "{t:?}");
    }
    assert_monotone(&p, -20, 40);
}

#[test]
fn every_stored_us_takes_part_without_endpoints() {
    // No note: BGM at beat 1, a change to 300 BPM at beat 2, a STOP of a
    // beat at beat 3 (200 ms at 300 BPM), BGM at beat 4. Every row's µs
    // has a representative, the last row of its µs, and the curve meets
    // each; it holds still through the STOP.
    let text = "#BPM 150\n#BPM01 300\n#STOP01 48\n#WAV01 a.wav\n\
                #00001:00010001\n#00008:00000100\n#00009:00000001\n#00101:01\n";
    let chart = loaded(text);
    assert!(chart.lanes.iter().all(Vec::is_empty));
    let p = ExactPlayhead::prepare(&chart, None).unwrap();
    let rows = chart.timeline.points();
    let mut times: Vec<TimeUs> = rows.iter().map(|r| r.at).collect();
    times.dedup();
    assert_eq!(p.representatives().len(), times.len());
    for r in rows {
        assert_eq!(p.position_at(r.at), r.pos, "{:?}", r.at);
    }
    // Beat 1 at 400 ms; beat 2 at 800 ms; the STOP from beat 3 (1 000 ms)
    // to 1 200 ms; the rate between rows is the tempo's.
    let beat = |b: i128| Position(BEAT * b);
    assert_eq!(p.position_at(TimeUs(400_000)), beat(1));
    assert_eq!(p.position_at(TimeUs(600_000)), Position(BEAT * 3 / 2));
    assert_eq!(p.position_at(TimeUs(900_000)), Position(BEAT * 5 / 2));
    for t in [1_000_000, 1_100_000, 1_200_000] {
        assert_eq!(p.position_at(TimeUs(t)), beat(3), "{t}");
    }
    assert_eq!(p.position_at(TimeUs(1_300_000)), Position(BEAT * 7 / 2));
    assert_monotone(&p, -1_000, 1_500_000);
}

/// 150 BPM: a tap on key 1 at `note` of measure 0 and a BGM object at
/// `bgm`, each `(index, slots)`.
fn note_and_bgm(note: (usize, usize), bgm: (usize, usize)) -> Chart {
    loaded(&format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{}\n#00001:{}\n",
        line(note.1, note.0),
        line(bgm.1, bgm.0)
    ))
}

#[test]
fn rows_of_other_kinds_yield_to_an_endpoint_at_their_us() {
    // 1000/2001 and 1001/2003 both floor to 799 600 µs: whichever the note
    // holds, the playhead takes the note's position there.
    for (note, bgm) in [((1000, 2001), (1001, 2003)), ((1001, 2003), (1000, 2001))] {
        let chart = note_and_bgm(note, bgm);
        let n = chart.lanes[1][0];
        assert_eq!(n.time, TimeUs(799_600));
        let p = ExactPlayhead::prepare(&chart, None).unwrap();
        assert_eq!(p.position_at(n.time), n.pos, "{note:?}");
        assert_monotone(&p, 799_000, 800_200);
    }
}

#[test]
fn a_section_sets_precedence_and_never_admits_a_refused_chart() {
    // The note outside the section is no endpoint of the attempt: its µs
    // takes its last row, the later BGM's.
    let chart = note_and_bgm((1000, 2001), (1001, 2003));
    let rows = chart.timeline.points();
    let after = Some((TimeUs(900_000), TimeUs(1_600_000)));
    let p = ExactPlayhead::prepare(&chart, after).unwrap();
    assert_eq!(p.position_at(TimeUs(799_600)), rows[2].pos);
    assert_ne!(rows[2].pos, chart.lanes[1][0].pos);
    // Two notes at one µs at two positions: refused, whatever the
    // section leaves out.
    let conflict = loaded(&format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{}\n#00012:{}\n",
        line(2001, 1000),
        line(2003, 1001)
    ));
    for section in [None, after, Some((TimeUs(0), TimeUs(799_600)))] {
        assert!(
            matches!(
                ExactPlayhead::prepare(&conflict, section),
                Err(PrepareError::Presentation(
                    PresentationError::EndpointConflict { .. }
                ))
            ),
            "{section:?}"
        );
    }
}

#[test]
fn a_stop_aliasing_an_endpoint_follows_the_endpoint() {
    // A STOP at 1001/2003 and a tap at 1000/2001 share 799 600 µs: the µs
    // takes the tap's position, and the curve rises from it to the STOP's
    // position by the STOP's end instead of holding still.
    let chart = loaded(&format!(
        "#BPM 150\n#STOP01 48\n#WAV01 a.wav\n#00011:{}\n#00009:{}\n#00111:01\n",
        line(2001, 1000),
        line(2003, 1001)
    ));
    let rows = chart.timeline.points();
    let (tap, stop, end) = (rows[1], rows[2], rows[3]);
    assert_eq!((tap.at, stop.at), (TimeUs(799_600), TimeUs(799_600)));
    assert_eq!(end.pos, stop.pos);
    let p = ExactPlayhead::prepare(&chart, None).unwrap();
    assert_eq!(p.position_at(tap.at), tap.pos);
    assert_eq!(p.position_at(end.at), end.pos);
    assert!(p.position_at(TimeUs(end.at.0 - 1)) < end.pos);
    assert_monotone(&p, 799_000, end.at.0 + 1_000);
}

#[test]
fn interpolation_is_exact_and_extrapolation_takes_the_map_rates() {
    // 150 BPM, a tap at 1 s (2.5 beats) and a change to 300 BPM there: in
    // between, floor(P · t / 1 s); before 0 and after the last row, the
    // scroll map's initial and final rates.
    let chart = loaded(
        "#BPM 150\n#BPM01 300\n#WAV01 a.wav\n#00011:0000000000010000\n#00008:0000000000010000\n",
    );
    let p = ExactPlayhead::prepare(&chart, None).unwrap();
    let n = chart.lanes[1][0];
    assert_eq!(n.time, TimeUs(1_000_000));
    let span = u128::try_from(n.pos.0).unwrap();
    for t in [1_u128, 333_333, 500_000, 999_999] {
        let want = i128::try_from(span * t / 1_000_000).unwrap();
        assert_eq!(p.position_at(TimeUs(t as i64)), Position(want), "{t}");
    }
    let initial = chart.scroll.initial_rate();
    assert_eq!(p.initial_rate(), initial);
    assert_eq!(p.position_at(TimeUs(-1_000)), Position(-1_000 * initial));
    let last = chart.scroll.segments().last().unwrap().slope;
    assert_eq!(
        p.position_at(TimeUs(2_000_000)),
        Position(n.pos.0 + 1_000_000 * last)
    );
    // Clamped to the domain, saturating rather than wrapping.
    assert!(p.position_at(TimeUs(i64::MAX)) >= p.position_at(TimeUs(2_000_000)));
    assert!(p.position_at(TimeUs(i64::MIN)) <= p.position_at(TimeUs(-1_000)));
}

#[test]
fn rows_past_the_domain_are_refused_before_preparing() {
    // A chart built in code may hold a time past the scroll map's domain,
    // where a query is clamped: the playhead could not meet it there, so
    // preparation refuses it, through the skin too. At the domain's end
    // the note is met.
    let past = ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000))
        .tap(lane(2), TimeUs(DOMAIN + 1))
        .build()
        .unwrap();
    let refused = PrepareError::TimeOutOfDomain {
        row: TimelineId(2),
        at: TimeUs(DOMAIN + 1),
    };
    assert_eq!(ExactPlayhead::prepare(&past, None), Err(refused));
    let skin = load(Vec::new(), Vec::new()).unwrap();
    assert_eq!(skin.prepare(&past, &full(0)).err(), Some(refused));
    let edge = ChartBuilder::new(150_000)
        .tap(lane(2), TimeUs(DOMAIN))
        .build()
        .unwrap();
    let p = ExactPlayhead::prepare(&edge, None).unwrap();
    let n = edge.lanes[2][0];
    assert_eq!(p.position_at(n.time), n.pos);
}

#[test]
fn a_rearm_keeps_the_prepared_playhead() {
    // Preparation happens once per attempt: a READY rearm resets the
    // runtime state and keeps the representatives' very buffer.
    let chart = loaded("#BPM 150\n#WAV01 a.wav\n#00011:01010101\n");
    let mut h = Harness::empty(&chart, &full(0));
    let before = h.play.playhead().clone();
    let buffer = h.play.playhead().representatives().as_ptr();
    h.frame(500, &[]);
    h.play.rearm();
    assert_eq!(h.play.playhead().representatives().as_ptr(), buffer);
    assert_eq!(h.play.playhead(), &before);
}
