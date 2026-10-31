//! The lanes drawn as the source player draws an imported skin
//! (`LaneStyle::Source`), on the simple skin's columns with the source
//! geometry of `lanes_support::source`: the region's lower edge at 880,
//! 600 high, notes 20 high. At 150 BPM with 600 ms visible and no covers
//! the notes scroll 1 px per ms.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, NoteKind, TimeUs};
use eval::JudgeEvent;
use lanes_support::{
    BAR, BODY_OFF, COVER, HEAD, LIFT, LINE, MINE, NOTE, TAIL, in_lane, lanes_element, lanes_skin,
    rects, skin_with, source, source_with,
};
use play_support::{Harness, MS, REGIONS, ev, full, judged, lane, mine};
use rules::Judgment;
use skin_model::{LANE_QUADS, LANE_VISITS, LanesDef, LanesError, LoadError};

/// 150 BPM, in thousandths.
const BPM: i64 = 150_000;

fn harness_of(def: LanesDef, chart: &Chart) -> Harness {
    Harness::new(lanes_skin(def), chart, &full(0))
}

fn harness(chart: &Chart) -> Harness {
    harness_of(source(), chart)
}

fn tap_at(l: u8, ms: i64) -> Chart {
    ChartBuilder::new(BPM)
        .tap(lane(l), TimeUs(ms * MS))
        .build()
        .unwrap()
}

/// Loads BMS `text` as a session does.
fn loaded(text: &str) -> Chart {
    let dir = tempfile::tempdir().unwrap();
    let opts = bms_parser::LoadOptions::default();
    let chart = bms_parser::load_bms(text.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart;
    chart.validate().unwrap();
    chart
}

#[test]
fn a_note_sits_on_the_line_by_its_lower_edge() {
    let chart = tap_at(3, 1_000);
    let mut h = harness(&chart);
    h.frame(1_000, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 860.0, 64.0, 20.0]]);
    h.frame(900, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 760.0, 64.0, 20.0]]);
    // Odd and fractional heights keep the lower edge, unrounded.
    for (n, y) in [(13.0, 867.0), (17.0, 863.0), (19.5, 860.5)] {
        let mut h = harness_of(source_with(|s| s.note_h = n), &chart);
        h.frame(1_000, &[]);
        assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, y, 64.0, n]], "{n}");
    }
    // The y offset lifts it.
    let mut h = harness_of(source_with(|s| s.offset_y = 5.0), &chart);
    h.frame(1_000, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 855.0, 64.0, 20.0]]);
}

#[test]
fn nothing_is_drawn_below_the_line() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .mine(lane(4), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    let shown = |h: &Harness| {
        [
            in_lane(h, NOTE, lane(3)).len(),
            in_lane(h, MINE, lane(4)).len(),
        ]
    };
    let mut h = harness(&chart);
    h.frame_us(1_000 * MS, &[]);
    assert_eq!(shown(&h), [1, 1]);
    // Unjudged, both leave at their time.
    h.frame_us(1_000 * MS + 1, &[]);
    assert_eq!(shown(&h), [0, 0]);
    // A tap judged early and a mine hit early stay until their time.
    let early: Vec<JudgeEvent> = vec![
        ev(950, judged(3, 0, Judgment::Great, Some(-50), None, 1)),
        ev(950, mine(4, 0)),
    ];
    let mut h = harness(&chart);
    h.frame(950, &early);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 810.0, 64.0, 20.0]]);
    assert_eq!(in_lane(&h, MINE, lane(4)), [[448.0, 810.0, 64.0, 20.0]]);
    h.frame(1_000, &[]);
    assert_eq!(shown(&h), [1, 1]);
}

/// A long note on key 2 from 1 s to 1.3 s in mode `mode`.
fn long_note(mode: Option<LnMode>) -> Chart {
    let mut chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    chart.meta.ln_mode = mode;
    chart
}

/// The long note's body, tail and head rects on key 2.
fn ln_parts(h: &Harness) -> [Vec<[f32; 4]>; 3] {
    [BODY_OFF, TAIL, HEAD].map(|p| in_lane(h, p, lane(2)))
}

#[test]
fn a_long_notes_tail_shows_only_when_its_release_is_judged() {
    for (mode, tail) in [
        (None, 0),
        (Some(LnMode::Ln), 0),
        (Some(LnMode::Cn), 1),
        (Some(LnMode::Hcn), 1),
    ] {
        let mut h = harness(&long_note(mode));
        h.frame(1_150, &[]);
        assert_eq!(ln_parts(&h).map(|p| p.len()), [1, tail, 1], "{mode:?}");
    }
}

#[test]
fn a_long_notes_body_runs_between_the_caps_inner_edges() {
    // At 1.15 s the head is held on the line, the end 150 px ahead.
    let mut h = harness(&long_note(Some(LnMode::Cn)));
    h.frame(1_150, &[]);
    assert_eq!(
        ln_parts(&h),
        [
            vec![[320.0, 730.0, 64.0, 130.0]],
            vec![[320.0, 710.0, 64.0, 20.0]],
            vec![[320.0, 860.0, 64.0, 20.0]],
        ]
    );
    // Within a note's height of its end there is no body left (the
    // source draws one of negative height).
    h.frame(1_290, &[]);
    assert_eq!(ln_parts(&h).map(|p| p.len()), [0, 1, 1]);
}

#[test]
fn a_long_note_without_length_is_not_drawn() {
    let mut chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.meta.ln_mode = Some(LnMode::Cn);
    let mut h = harness(&chart);
    h.frame(900, &[]);
    assert_eq!(ln_parts(&h).map(|p| p.len()), [0, 0, 0]);
}

/// The long note's parts drawn at `now` µs.
fn drawn(chart: &Chart, now: i64) -> usize {
    let mut h = harness(chart);
    h.frame_us(now, &[]);
    ln_parts(&h).iter().map(Vec::len).sum()
}

#[test]
fn a_long_note_leaves_at_its_end_time() {
    let end = 1_300 * MS;
    let chart = long_note(Some(LnMode::Cn));
    assert_eq!(drawn(&chart, end - 50 * MS), 3);
    assert!(drawn(&chart, end - 1) > 0);
    assert_eq!(drawn(&chart, end), 0);
    assert_eq!(drawn(&chart, end + 1), 0);
    // A STOP at the end holds its position on the line, but not past its
    // time.
    let mut stopped = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(end))
        .stop(TimeUs(end), TimeUs(200 * MS))
        .build()
        .unwrap();
    stopped.meta.ln_mode = Some(LnMode::Cn);
    assert_eq!(drawn(&stopped, end - 50 * MS), 3);
    assert!(drawn(&stopped, end - 1) > 0);
    assert_eq!(drawn(&stopped, end), 0);
    assert_eq!(drawn(&stopped, end + 100 * MS), 0);
}

/// A chart at 199.999 BPM with `channel` notes at measure 1's start and
/// a third of a beat later (`#LNTYPE 1`: a long note on channel 51).
fn third_of_a_beat(channel: &str) -> Chart {
    loaded(&format!(
        "#BPM 199.999\n#LNTYPE 1\n#WAV01 a.wav\n#001{channel}:{}\n",
        format_args!("0101{}", "00".repeat(10))
    ))
}

#[test]
fn a_long_note_ending_off_the_microsecond_leaves_at_its_floored_end() {
    // The end is 4⅓ beats at 199.999 BPM: 1 300 006.500 03 µs, judged at
    // 1 300 006 while its exact position is still half a µs ahead.
    let mut chart = third_of_a_beat("51");
    chart.meta.ln_mode = Some(LnMode::Cn);
    let Some(NoteKind::Long { end, .. }) = chart.lanes[1].first().map(|n| n.kind) else {
        panic!("no long note");
    };
    assert_eq!(end, TimeUs(1_300_006));
    let parts = |now: i64| {
        let mut h = harness(&chart);
        h.frame_us(now, &[]);
        [BODY_OFF, TAIL, HEAD]
            .map(|p| in_lane(&h, p, lane(1)).len())
            .iter()
            .sum::<usize>()
    };
    assert_eq!(parts(end.0 - 50 * MS), 3);
    assert!(parts(end.0 - 1) > 0);
    assert_eq!(parts(end.0), 0);
    assert_eq!(parts(end.0 + 1), 0);
}

#[test]
fn a_note_is_within_a_microsecond_of_scroll_of_the_line_at_its_judgment_time() {
    let chart = third_of_a_beat("11");
    let t = chart.lanes[1][1].time;
    assert_eq!(t, TimeUs(1_300_006));
    let mut h = harness(&chart);
    h.frame_us(t.0, &[]);
    let [[_, y, _, n]] = in_lane(&h, NOTE, lane(1))[..] else {
        panic!("not drawn");
    };
    // One µs of scroll at the initial rate is 600 px / 600 000 µs.
    let off = (f64::from(y + n) - 880.0).abs();
    assert!(off <= 0.001 + 1e-4, "{off}");
}

#[test]
fn the_lift_and_the_cover_take_their_shares_of_the_region() {
    // Lift 200 ‰ of 600: the line at 760. Cover 500 ‰ of the 480 left:
    // SUDDEN+'s lower edge 240 below its anchor (280), and 240 px of
    // scroll for 600 ms: a long note of 300 ms is 120 px long.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    let mut h = harness(&chart);
    h.view.lift = 200;
    h.view.cover = 500;
    h.frame(1_000, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 740.0, 64.0, 20.0]]);
    assert_eq!(in_lane(&h, HEAD, lane(2)), [[320.0, 740.0, 64.0, 20.0]]);
    // The end's lower edge (120 px up), the body up to the head's top.
    assert_eq!(
        in_lane(&h, BODY_OFF, lane(2)),
        [[320.0, 640.0, 64.0, 100.0]]
    );
    let cover = rects(&h, COVER);
    assert_eq!(
        cover.iter().map(|r| r[1] + r[3]).collect::<Vec<_>>(),
        [520.0]
    );
    let lift = rects(&h, LIFT);
    assert_eq!(lift.iter().map(|r| r[1]).collect::<Vec<_>>(), [760.0]);
}

#[test]
fn with_no_scroll_left_only_taps_and_mines_are_drawn_on_the_line() {
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(5), TimeUs(500 * MS))
        .tap(lane(3), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(2_000 * MS))
        .mine(lane(4), TimeUs(3_000 * MS), 10)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    chart.meta.ln_mode = Some(LnMode::Cn);
    // On the line: at the bottom, or lifted to the region's top, where
    // the lanes' top (270) cuts the note's upper half.
    for (lift, cover, [y, n]) in [
        (1_000, 0, [270.0, 10.0]),
        (0, 1_000, [860.0, 20.0]),
        (1_000, 1_000, [270.0, 10.0]),
    ] {
        let mut h = harness(&chart);
        h.view.lift = lift;
        h.view.cover = cover;
        h.frame(900, &[]);
        let at = |x: f32| [x, y, 64.0, n];
        let case = format!("lift {lift}, cover {cover}");
        assert_eq!(in_lane(&h, NOTE, lane(3)), [at(384.0), at(384.0)], "{case}");
        assert_eq!(in_lane(&h, MINE, lane(4)), [at(448.0)], "{case}");
        assert!(in_lane(&h, NOTE, lane(5)).is_empty(), "{case}");
        assert_eq!(ln_parts(&h).map(|p| p.len()), [0, 0, 0], "{case}");
        assert!(rects(&h, BAR).is_empty(), "{case}");
        assert_eq!(rects(&h, LINE).len(), 1, "{case}");
    }
}

#[test]
fn with_no_scroll_left_both_budgets_bound_the_frame() {
    let mut def = source();
    def.parts.bar = None;
    let run = |chart: &Chart| {
        let mut h = harness_of(def, chart);
        h.view.cover = 1_000;
        h.frame(900, &[]);
        h
    };
    // `LANE_VISITS` long notes ahead, none drawn, then a tap never
    // visited: one drop.
    let per_lane = i64::from(LANE_VISITS) / 8;
    let mut b = ChartBuilder::new(BPM);
    for l in Lane::all() {
        for i in 0..per_lane {
            let t = 1_000 * MS + i * 20;
            b = b.long(l, TimeUs(t), TimeUs(t + 10));
        }
    }
    let h = run(&b.tap(lane(7), TimeUs(9_000 * MS)).build().unwrap());
    assert!(rects(&h, NOTE).is_empty());
    assert!(rects(&h, HEAD).is_empty());
    assert_eq!(h.batch.dropped(), 1);
    // `LANE_QUADS` taps ahead and one more: the budget's taps, one drop.
    let per_lane = i64::from(LANE_QUADS) / 8;
    for (extra, dropped) in [(0, 0), (1, 1)] {
        let mut b = ChartBuilder::new(BPM);
        for l in Lane::all() {
            let n = per_lane + if l == lane(7) { extra } else { 0 };
            for i in 0..n {
                b = b.tap(l, TimeUs(1_000 * MS + i * 1_000 * MS));
            }
        }
        let h = run(&b.build().unwrap());
        assert_eq!(rects(&h, NOTE).len(), LANE_QUADS as usize, "{extra}");
        assert_eq!(h.batch.dropped(), dropped, "{extra}");
    }
}

#[test]
fn the_source_geometry_is_checked_at_load() {
    let err = |def: LanesDef| match skin_with(REGIONS, Some(def), vec![lanes_element()]) {
        Err(LoadError::Lanes(e)) => Some(e),
        Err(e) => panic!("{e:?}"),
        Ok(_) => None,
    };
    for (what, def) in [
        ("negative offset", source_with(|s| s.offset_y = -1.0)),
        ("no height", source_with(|s| s.height = 0.0)),
        ("no note height", source_with(|s| s.note_h = 0.0)),
        ("not finite", source_with(|s| s.bottom = f32::NAN)),
        // 880 − 611 is above the top (270).
        ("above the top", source_with(|s| s.note_h = 611.0)),
        ("offset above the top", source_with(|s| s.offset_y = 600.0)),
    ] {
        assert_eq!(err(def), Some(LanesError::Source), "{what}");
    }
    assert_eq!(err(source_with(|s| s.note_h = 610.0)), None);
    assert_eq!(err(source_with(|s| s.offset_y = 590.0)), None);
}
