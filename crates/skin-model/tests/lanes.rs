//! The lanes widget on the simple skin's geometry at 1920×1080 with the
//! author's layout: the simple skin's tests ported with their literal
//! values (crates/render/src/simple.rs), then cropping, the covers, the
//! scratch side, the lane width, section targets, the ring-out, both
//! budgets and the load errors. Parts are told apart by their uv.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{JudgeEvent, JudgeEventKind, Section};
use lanes_support::{
    BAR, BODY_OFF, BODY_ON, COVER, HEAD, LIFT, LINE, MINE, NOTE, TAIL, WHITE, bound, image,
    in_column, in_lane, in_region, lanes_element, lanes_skin, quads_of, rects, simple, skin_with,
    still,
};
use play_support::{GAUGES, Harness, MS, REGIONS, ev, full, hold_ended, judged, lane, ln_held};
use rules::Judgment;
use skin_model::{
    Attempt, Bind, Canvas, Fill, Fit, LANE_QUADS, LANE_VISITS, LanePart, LanesDef, LanesError,
    Layout, LoadError, MAX_COORD, Place, PlayContext, QuadBatch, Region, Scale, Side, Skin,
    SkinDef,
};

/// 150 BPM, in thousandths.
const BPM: i64 = 150_000;

fn harness(chart: &Chart) -> Harness {
    Harness::new(lanes_skin(simple()), chart, &full(0))
}

#[test]
fn note_on_the_line_and_at_the_top() {
    // 150 BPM over 600 ms: 1.5 beats on 800 px. A note at the line is
    // centered on y = 880; one exactly 600 ms ahead is centered on the
    // lane top (80) and shows its lower half, 80..88.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut h = harness(&chart);
    h.frame(1_000, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 872.0, 64.0, 16.0]]);
    h.frame(400, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 80.0, 64.0, 8.0]]);
    // 6 ms earlier the note sits 8 px higher (800 px / 600 ms), wholly
    // above the lane.
    h.frame(394, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(3)), Vec::<[f32; 4]>::new());
}

/// A long note's parts in `l`: active body, inactive body, tail and head.
fn long_parts(h: &Harness, l: Lane) -> [Vec<[f32; 4]>; 3] {
    let mut caps = in_lane(h, TAIL, l);
    caps.extend(in_lane(h, HEAD, l));
    [in_lane(h, BODY_ON, l), in_lane(h, BODY_OFF, l), caps]
}

fn event(at: i64, kind: JudgeEventKind) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(at),
        origin: None,
        kind,
    }
}

/// A long note on key 2 from 1 s to 1.3 s.
fn long_note() -> Chart {
    ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap()
}

#[test]
fn a_long_note_stays_drawn_whatever_its_judgment_in_its_state() {
    // Displayed at 1.15 s, 800 px per 600 ms: the head 150 ms past the
    // line is pinned to it (y 880), the tail 150 ms ahead at y 680. The
    // body from 680 to 880 is active only while its head was pressed and
    // the lane stays held, whatever the judgment.
    let chart = long_note();
    let caps = vec![[320.0, 672.0, 64.0, 16.0], [320.0, 872.0, 64.0, 16.0]];
    let body = vec![[320.0, 680.0, 64.0, 200.0]];
    let none = Vec::<[f32; 4]>::new();
    let ln = |at: i64, judgment, deviation, release| {
        ev(at, judged(2, 0, judgment, deviation, release, 1))
    };
    let cases: [(&str, Vec<JudgeEvent>, bool); 6] = [
        ("never pressed", vec![], false),
        (
            "head missed",
            vec![ln(1_180, Judgment::Poor, None, None)],
            false,
        ),
        (
            "held",
            vec![ev(1_000, ln_held(2, 0, Judgment::Great))],
            true,
        ),
        (
            "released early",
            vec![
                ev(1_000, ln_held(2, 0, Judgment::Great)),
                ev(1_100, hold_ended(2, 0)),
                ln(1_100, Judgment::Bad, Some(0), Some(-200)),
            ],
            false,
        ),
        (
            "a BAD head decided at its press",
            vec![ln(850, Judgment::Bad, Some(-150), None)],
            true,
        ),
        (
            "a BAD head released",
            vec![
                ln(850, Judgment::Bad, Some(-150), None),
                ev(1_100, hold_ended(2, 0)),
            ],
            false,
        ),
    ];
    for (name, effects, active) in cases {
        let mut h = harness(&chart);
        h.frame(1_150, &effects);
        let expected = if active {
            [body.clone(), none.clone(), caps.clone()]
        } else {
            [none.clone(), body.clone(), caps.clone()]
        };
        assert_eq!(long_parts(&h, lane(2)), expected, "{name}");
    }
    // Before its head reaches the line, the head is drawn where it is:
    // at 0.85 s the head is 150 ms ahead (y 680) and the tail 450 ms
    // (y 280).
    let mut h = harness(&chart);
    h.frame(850, &[]);
    assert_eq!(
        long_parts(&h, lane(2)),
        [
            none.clone(),
            vec![[320.0, 280.0, 64.0, 400.0]],
            vec![[320.0, 272.0, 64.0, 16.0], [320.0, 672.0, 64.0, 16.0]]
        ]
    );
    // A tap still disappears when judged.
    let tap = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut h = harness(&tap);
    h.frame(
        1_000,
        &[ev(1_000, judged(3, 0, Judgment::Great, Some(0), None, 1))],
    );
    assert!(in_lane(&h, NOTE, lane(3)).is_empty());
}

#[test]
fn a_held_long_notes_judgment_leaves_it_drawn_inactive() {
    // The lane parts of the simple skin's test of the same name (its
    // feedback is the reducer's, tests/reducer.rs).
    let chart = long_note();
    let mut h = harness(&chart);
    h.frame(1_000, &[ev(1_000, ln_held(2, 0, Judgment::Great))]);
    let [active, _, _] = long_parts(&h, lane(2));
    assert_eq!(active.len(), 1);
    let released = ev(1_250, hold_ended(2, 0));
    let judged = ev(1_250, judged(2, 0, Judgment::Good, Some(0), Some(-50), 1));
    h.frame(1_250, &[released, judged]);
    let [active, inactive, caps] = long_parts(&h, lane(2));
    assert!(active.is_empty());
    assert_eq!(inactive.len(), 1);
    assert_eq!(caps.len(), 2);
}

/// The long note's parts drawn at `now` (µs) with `offset`, after
/// `effects`.
fn drawn(chart: &Chart, now: i64, offset: i64, effects: &[JudgeEvent]) -> usize {
    let mut h = harness(chart);
    h.offset = TimeUs(offset);
    h.frame_us(now, effects);
    long_parts(&h, lane(2)).iter().map(Vec::len).sum()
}

/// The events the evaluator emits up to logic time `to` for key 2
/// pressed at `press` and released at `release` (µs), with the custom
/// preset and one keyboard source.
fn evaluated(chart: &Chart, press: i64, release: i64, to: i64) -> Vec<JudgeEvent> {
    let device = eval::DeviceProfile {
        sources: vec![eval::SourceProfile {
            input_offset: TimeUs(0),
            bindings: vec![eval::Binding {
                control: eval::ControlId(2),
                lane: lane(2),
            }],
            axes: Vec::new(),
        }],
    };
    let preset = rules::custom();
    let key = |at: i64, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(at),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(2),
            value,
        },
    };
    let mut ev = eval::Evaluator::new(
        chart,
        &preset,
        &device,
        &eval::InputStart::default(),
        None,
        4,
    )
    .unwrap();
    ev.push_input(key(press, 1)).unwrap();
    ev.push_input(key(release, 0)).unwrap();
    let mut out = Vec::new();
    ev.advance_to(TimeUs(to), &mut out).unwrap();
    out
}

#[test]
fn a_long_note_turns_inactive_at_its_logic_end_while_its_tail_still_shows() {
    // The long note from 1 s to 1.3 s, drawn with a display offset of
    // −100 ms, so its tail is still 100 ms ahead of the line when its
    // logic end passes. A GOOD-or-better head (on time) and a BAD head
    // (150 ms early), each released exactly at the end or held through
    // it (released at 1.5 s, judged by the end deadline): the body is
    // active at logic 1.29 s, and inactive from the logic end on, with
    // the tail drawn (at 1.31 s, 90 ms ahead: y 760).
    let chart = long_note();
    let end = 1_300 * MS;
    let tail = [320.0, 752.0, 64.0, 16.0];
    for press in [1_000 * MS, 850 * MS] {
        for release in [end, 1_500 * MS] {
            for (now, active) in [(end - 10 * MS, true), (end, false), (end + 10 * MS, false)] {
                let effects = evaluated(&chart, press, release, now);
                let mut h = harness(&chart);
                h.offset = TimeUs(-100 * MS);
                h.frame_us(now, &effects);
                let [on, off, caps] = long_parts(&h, lane(2));
                let case = format!("press {press}, release {release}, at {now}");
                assert_eq!(
                    (on.len(), off.len()),
                    (usize::from(active), usize::from(!active)),
                    "{case}"
                );
                if now == end + 10 * MS {
                    assert_eq!(caps[0], tail, "{case}");
                }
            }
        }
    }
}

#[test]
fn a_long_note_is_culled_once_its_tail_passes_below_the_line() {
    let chart = long_note();
    let end = 1_300 * MS;
    // Before the crossing: body and caps; at it, the tail on the line and
    // no body left: both caps; 1 µs after: nothing, judged or not.
    assert_eq!(drawn(&chart, end - 1, 0, &[]), 3);
    assert_eq!(drawn(&chart, end, 0, &[]), 2);
    assert_eq!(drawn(&chart, end + 1, 0, &[]), 0);
    let held = [ev(1_000, ln_held(2, 0, Judgment::Great))];
    assert_eq!(drawn(&chart, end, 0, &held), 2);
    assert_eq!(drawn(&chart, end + 1, 0, &held), 0);
    // The display offset moves the crossing: 150 ms later on screen, so
    // 150 ms earlier on the frame's clock.
    let offset = 150 * MS;
    assert_eq!(drawn(&chart, end - offset, offset, &[]), 2);
    assert_eq!(drawn(&chart, end - offset + 1, offset, &[]), 0);
    // A STOP of 200 ms at the end holds the tail on the line for it.
    let stopped = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(end))
        .stop(TimeUs(end), TimeUs(200 * MS))
        .build()
        .unwrap();
    assert_eq!(drawn(&stopped, end - 1, 0, &[]), 3);
    assert_eq!(drawn(&stopped, end + 100 * MS, 0, &[]), 2);
    assert_eq!(drawn(&stopped, end + 200 * MS, 0, &[]), 2);
    assert_eq!(drawn(&stopped, end + 200 * MS + 1, 0, &[]), 0);
}

#[test]
fn dense_notes_below_a_pixel_stay_bounded_by_the_batch() {
    // 3 000 taps 100 µs apart in each of two lanes: at 800 px per 600 ms
    // they are 0.1333 px apart, and 6 000 are visible.
    let mut b = ChartBuilder::new(BPM);
    for i in 0..3_000 {
        b = b.tap(lane(1), TimeUs(1_000 * MS + i * 100));
        b = b.tap(lane(7), TimeUs(1_000 * MS + i * 100));
    }
    let chart = b.build().unwrap();
    let mut h = harness(&chart);
    h.batch = QuadBatch::with_capacity(4_000, 4_000);
    h.frame(1_000, &[]);
    assert!(h.batch.is_full());
    assert!(h.batch.dropped() >= 1);
    let lane1 = in_lane(&h, NOTE, lane(1));
    assert_eq!(lane1.len(), 3_000);
    let gap = lane1[0][1] - lane1[1][1];
    assert!((gap - 800.0 / 6_000.0).abs() < 1e-3, "gap {gap}");
    // The skin's own capacity holds the lane budget.
    h.batch = QuadBatch::with_capacity(h.skin.quad_capacity(), h.skin.run_capacity());
    h.frame(1_000, &[]);
    assert_eq!(h.batch.dropped(), 0);
    assert_eq!(in_lane(&h, NOTE, lane(7)).len(), 3_000);
}

/// Loads BMS `text` as a session does: the loader, then the validation of
/// its output (the session's transform at 1× is the identity).
fn loaded(text: &str) -> Chart {
    let dir = tempfile::tempdir().unwrap();
    let opts = bms_parser::LoadOptions::default();
    let chart = bms_parser::load_bms(text.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart;
    chart.validate().unwrap();
    chart
}

/// Whole bar lines (cut ones excluded).
fn bars(h: &Harness) -> Vec<[f32; 4]> {
    rects(h, BAR).into_iter().filter(|r| r[3] == 2.0).collect()
}

#[test]
fn loaded_notes_and_bar_lines_sit_at_their_exact_beats() {
    // Quarter-beat measures at 9,990,400 BPM: measure k starts at beat
    // k/4, k × 1.5014416... µs, so its notes sound on floored µs (1, 3,
    // 4, ...). Taps on key 1 at measures 1-6, a long note on key 2 from
    // measure 2 to 5, a last tap on key 3 at measure 9.
    let mut text = String::from("#BPM 9990400\n#LNTYPE 1\n#WAV01 a.wav\n");
    for m in 0..=9 {
        text.push_str(&format!("#{m:03}02:0.0625\n"));
    }
    for m in 1..=6 {
        text.push_str(&format!("#{m:03}11:01\n"));
    }
    text.push_str("#00252:01\n#00552:01\n#00913:01\n");
    let chart = loaded(&text);
    let mut h = harness(&chart);
    // The playhead at 1 µs, 10 µs visible: 800 px over 10 µs of the start
    // BPM, so 80 px per µs of scroll at 9,990,400 BPM.
    h.view.visible_time = TimeUs(10);
    h.frame_us(1, &[]);
    // By hand: beat k/4 is k × 15e6 / 9_990_400 µs of scroll, drawn 80 px
    // per µs above the line once past the playhead's 1 µs: measure 1 at
    // 880 - 80 × 0.5014416... = 839.8847 px, where its floored time (1 µs)
    // would put it on the line.
    let y = |k: i32| 880.0 - 80.0 * (f64::from(k) * 15e6 / 9_990_400.0 - 1.0);
    assert!((y(1) - 839.884_7).abs() < 1e-3);
    let centers = |rects: Vec<[f32; 4]>| -> Vec<f64> {
        rects.iter().map(|r| f64::from(r[1] + r[3] / 2.0)).collect()
    };
    let close = |got: Vec<f64>, want: Vec<f64>| {
        assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
        for (g, w) in got.iter().zip(&want) {
            assert!((g - w).abs() < 1e-3, "{got:?} vs {want:?}");
        }
    };
    close(
        centers(in_lane(&h, NOTE, lane(1))),
        (1..=6).map(y).collect(),
    );
    // The long note's end cap (measure 5), then its head (measure 2).
    let mut caps = in_lane(&h, TAIL, lane(2));
    caps.extend(in_lane(&h, HEAD, lane(2)));
    close(centers(caps), vec![y(5), y(2)]);
    // Bar lines 1-7 lie wholly inside the lanes (y from 80 to 960).
    close(centers(bars(&h)), (1..=7).map(y).collect());
}

#[test]
fn bar_lines_of_equal_measures_are_drawn_evenly() {
    // Quarter-beat measures at 9,990,400 BPM last 1.5014... µs each, so
    // their floored times step by 1 or 2 µs; their positions are exactly
    // a quarter beat apart. Over a 10 µs visible time a quarter beat is
    // 120.1... px.
    let mut text = String::from("#BPM 9990400\n#WAV01 a.wav\n");
    for m in 0..=64 {
        text.push_str(&format!("#{m:03}02:0.0625\n"));
    }
    text.push_str("#06411:01\n");
    let chart = loaded(&text);
    let mut h = harness(&chart);
    h.view.visible_time = TimeUs(10);
    h.frame_us(40, &[]);
    let ys: Vec<f32> = bars(&h).iter().map(|r| r[1]).collect();
    assert!(ys.len() >= 6, "{ys:?}");
    let quarter = 800.0 / (10.0 * 9_990_400.0 / 60_000_000.0) / 4.0;
    for w in ys.windows(2) {
        let gap = w[0] - w[1];
        assert!((gap - quarter).abs() < 1e-2, "{ys:?}");
    }
}

#[test]
fn drawn_notes_are_those_whose_span_meets_the_visible_range() {
    // 150 BPM: a beat is 400 ms, 1.5 beats over 800 px at 600 ms. Taps
    // every sixteenth beat on key 1 for 8 measures; a long note on key 2
    // from measure 1 to measure 6; a tap on key 3 in measure 7.
    let mut text = String::from("#BPM 150\n#LNTYPE 1\n#WAV01 a.wav\n");
    let sixteenths = "01".repeat(64);
    for m in 0..8 {
        text.push_str(&format!("#{m:03}11:{sixteenths}\n"));
    }
    text.push_str("#00152:01\n#00652:01\n#00713:01\n");
    let chart = loaded(&text);
    let mut h = harness(&chart);
    // Display at 3.31 s, beat 8.275: the judgment line is past measure
    // 2's start and the long note covers the lanes.
    h.frame(3_310, &[]);
    // Independently: tap k sits at k/16 beat; its center is drawn 800 px
    // per 1.5 beats above the line for each beat ahead, and it shows
    // while its center is within half a note of the lanes.
    let playhead = 8.275;
    let expected = (0..8 * 64)
        .filter(|&k| {
            let ahead = f64::from(k) / 16.0 - playhead;
            let y = 880.0 - ahead * 800.0 / 1.5;
            y > 80.0 - 8.0 && y < 960.0 + 8.0
        })
        .count();
    assert!(expected > 20);
    assert_eq!(in_lane(&h, NOTE, lane(1)).len(), expected);
    // The long note's head has passed the line, so it is pinned there:
    // its body runs from the lane top to the line, under its head cap.
    assert_eq!(in_lane(&h, BODY_OFF, lane(2)), [[320.0, 80.0, 64.0, 800.0]]);
    assert_eq!(
        in_lane(&h, HEAD, lane(2)),
        [[320.0, 880.0 - 8.0, 64.0, 16.0]]
    );
    assert!(in_lane(&h, NOTE, lane(3)).is_empty());
}

#[test]
fn visible_time_below_a_microsecond_counts_as_one() {
    let chart = ChartBuilder::new(BPM).build().unwrap();
    let one = Scale::new(&chart.scroll, TimeUs(0), TimeUs(1), 800.0, 880.0);
    // 1 µs of 150 BPM (2.5 millionths of a beat) on 800 px.
    let beat = (1_u128 << 80) as f64;
    assert!((one.px_per_pos * beat * 2.5e-6 - 800.0).abs() < 1e-6);
    for v in [0, -5, i64::MIN] {
        assert_eq!(
            Scale::new(&chart.scroll, TimeUs(0), TimeUs(v), 800.0, 880.0),
            one
        );
    }
}

#[test]
fn effects_for_unknown_notes_are_ignored() {
    let chart = ChartBuilder::new(BPM)
        .tap(lane(5), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut h = harness(&chart);
    let stray = event(
        500 * MS,
        JudgeEventKind::MineHit {
            lane: lane(5),
            note: 4_000,
        },
    );
    h.frame(700, &[stray]);
    assert_eq!(in_lane(&h, NOTE, lane(5)).len(), 1);
}

#[test]
fn display_offset_moves_notes() {
    // The lane 3 note is 200 ms ahead of 1 s but 50 ms ahead of the
    // display time: 66.67 px above the line.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_200 * MS))
        .tap(lane(6), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut h = harness(&chart);
    h.offset = TimeUs(150 * MS);
    h.frame(1_000, &[]);
    let note = in_lane(&h, NOTE, lane(3));
    assert_eq!(note.len(), 1);
    assert!((note[0][1] + 8.0 - (880.0 - 200.0 / 3.0)).abs() < 1e-3);
}

#[test]
fn the_culling_range_follows_the_tallest_part() {
    // A tap, a mine and a bar line 615 ms ahead: at 800 px per 600 ms
    // their centres are 20 px above the top (80). Made 64 tall, each
    // reaches 12 px into the lanes, so it is drawn cut to [80, 92], though
    // a padding of half the default note (8) would have culled it.
    let at = TimeUs(1_600 * MS);
    let mut chart = ChartBuilder::new(BPM)
        .tap(lane(3), at)
        .mine(lane(5), at, 10)
        .build()
        .unwrap();
    chart.bar_lines = vec![chart.scroll.position_at(at)];
    let tall = |set: fn(&mut LanesDef)| {
        let mut def = simple();
        set(&mut def);
        let mut h = Harness::new(lanes_skin(def), &chart, &full(0));
        h.frame(985, &[]);
        h
    };
    let h = tall(|d| d.note_h = 64);
    assert_eq!(in_lane(&h, NOTE, lane(3)), [[384.0, 80.0, 64.0, 12.0]]);
    let h = tall(|d| d.mine_h = 64);
    assert_eq!(in_lane(&h, MINE, lane(5)), [[512.0, 80.0, 64.0, 12.0]]);
    let h = tall(|d| d.bar_h = 64);
    assert_eq!(rects(&h, BAR), [[160.0, 80.0, 544.0, 12.0]]);
    // At the default heights all three are culled there.
    let h = tall(|_| {});
    assert!(in_lane(&h, NOTE, lane(3)).is_empty());
    assert!(in_lane(&h, MINE, lane(5)).is_empty());
    assert!(rects(&h, BAR).is_empty());
}

#[test]
fn caps_are_cropped_with_their_images_not_squashed() {
    // A note 600 ms ahead is centred on the top (80): its lower half shows,
    // and the uv keeps exactly the lower half of its 16-texel image.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let mut h = harness(&chart);
    h.frame(400, &[]);
    let [q] = quads_of(&h, NOTE)[..] else {
        panic!("one note");
    };
    assert_eq!(q.rect, [384.0, 80.0, 64.0, 8.0]);
    let whole = h.uvs.get(NOTE).unwrap().uv;
    let half = (whole[1] + whole[3]) / 2.0;
    assert_eq!(q.uv, [whole[0], half, whole[2], whole[3]]);
    // Whole caps keep the whole uv, bit for bit.
    h.frame(1_000, &[]);
    assert_eq!(quads_of(&h, NOTE)[0].uv, whole);
}

/// The note centres in lane 3 at `now` with `cover` and `lift`.
fn centres(chart: &Chart, now: i64, cover: u16, lift: u16) -> Vec<f32> {
    let mut h = harness(chart);
    h.view.cover = cover;
    h.view.lift = lift;
    h.frame(now, &[]);
    in_lane(&h, NOTE, lane(3))
        .iter()
        .map(|r| r[1] + r[3] / 2.0)
        .collect()
}

#[test]
fn the_cover_and_the_lift_shorten_the_visible_span() {
    // L = 800. Cover 250 ‰: the span is 600 px from the cover's edge
    // (280) to the line (880), and 600 ms of 150 BPM take all of it.
    // Lift 100 ‰ as well: the line is at 800 and the span 520 px, again
    // from 280. A note 300 ms ahead is halfway: 580 without the lift, 540
    // with it.
    let chart = ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    assert_eq!(centres(&chart, 700, 250, 0), [580.0, 280.0]);
    assert_eq!(centres(&chart, 700, 250, 100), [540.0, 280.0]);
    let mut h = harness(&chart);
    h.view.cover = 250;
    h.view.lift = 100;
    h.frame(700, &[]);
    // The judgment line follows the lift; the lift cover runs from it to
    // the bottom.
    assert_eq!(rects(&h, LINE), [[160.0, 797.0, 544.0, 6.0]]);
    assert_eq!(rects(&h, LIFT), [[160.0, 800.0, 544.0, 160.0]]);
    // The cover is 800 tall with its bottom edge at 280, cut at the top:
    // its lowest 200 texel rows of 800, the last quarter of its image.
    let [cover] = quads_of(&h, COVER)[..] else {
        panic!("one cover");
    };
    assert_eq!(cover.rect, [160.0, 80.0, 544.0, 200.0]);
    let whole = h.uvs.get(COVER).unwrap().uv;
    let quarter = whole[1] + (whole[3] - whole[1]) * 0.75;
    assert_eq!(cover.uv, [whole[0], quarter, whole[2], whole[3]]);
    // At most 900 ‰ is covered: a cover of 950 is 900 (its edge at 800),
    // and with the lift 100 it is 800 (its edge at 720).
    for (cover, lift, edge) in [(950, 0, 800.0), (900, 0, 800.0), (950, 100, 720.0)] {
        let mut h = harness(&chart);
        h.view.cover = cover;
        h.view.lift = lift;
        h.frame(700, &[]);
        let r = rects(&h, COVER)[0];
        assert_eq!(r[1] + r[3], edge, "cover {cover}, lift {lift}");
    }
    // Without cover and lift, neither is drawn.
    let mut h = harness(&chart);
    h.frame(700, &[]);
    assert!(rects(&h, COVER).is_empty() && rects(&h, LIFT).is_empty());
    assert_eq!(rects(&h, LINE), [[160.0, 877.0, 544.0, 6.0]]);
}

/// A tap on every lane at 1 s.
fn chord() -> Chart {
    Lane::all()
        .fold(ChartBuilder::new(BPM), |b, l| b.tap(l, TimeUs(1_000 * MS)))
        .build()
        .unwrap()
}

/// The x of each lane's note, by lane index.
fn note_xs(h: &Harness) -> Vec<f32> {
    let mut xs: Vec<f32> = rects(h, NOTE).iter().map(|r| r[0]).collect();
    xs.sort_by(f32::total_cmp);
    xs
}

#[test]
fn the_scratch_side_moves_the_scratch_and_the_key_block() {
    // A bomb bound to the scratch lane, authored 8 px left of it.
    let bomb = image(
        WHITE,
        [152, 824, 112, 112],
        bound(Bind::Lane(Lane::SCRATCH)),
    );
    let skin = skin_with(REGIONS, Some(simple()), vec![lanes_element(), bomb]).unwrap();
    let right = PlayContext {
        scratch: Side::Right,
        ..full(0)
    };
    let mut h = Harness::new(skin, &chord(), &right);
    h.frame(1_000, &[]);
    // Keys at 160, 224, ..., 544, the scratch at 160 + 704 − 160 − 96.
    let scratch = in_column(&h, NOTE, 608.0);
    assert_eq!(scratch, [[608.0, 872.0, 96.0, 16.0]]);
    let keys: Vec<f32> = (0..7).map(|k| 160.0 + 64.0 * k as f32).collect();
    let mut want = keys.clone();
    want.push(608.0);
    assert_eq!(note_xs(&h), want);
    // The bomb keeps its offset from its lane: 600 to 712, past the span.
    let bombs = rects(&h, WHITE);
    assert_eq!(bombs, [[600.0, 824.0, 112.0, 112.0]]);
    // The playfield's box moves with it: [152, 704] as authored, [160, 712]
    // with the scratch on the right. Pushed right as far as it goes, the
    // box keeps 64 px on the canvas from its current left edge: a
    // translation of 1920 − 64 − 160 = 1696 (the authored box would allow
    // 1704), so the judgment line starts at 160 + 1696 = 1856.
    h.layout
        .set(
            Region::Playfield,
            skin_model::Adjust {
                x: 4_320,
                ..skin_model::Adjust::IDENTITY
            },
        )
        .unwrap();
    h.frame(1_000, &[]);
    let p = h.skin.placement(
        &Canvas::new([1920, 1080], Fit::Adapt),
        &h.layout,
        Side::Right,
    );
    assert_eq!(p.region(Region::Playfield).affine.offset, [1_696.0, 0.0]);
    assert_eq!(rects(&h, LINE)[0][0], 1_856.0);
    assert_eq!(rects(&h, WHITE)[0][0], 600.0 + 1_696.0);
    // On the author's side nothing moves.
    let skin = skin_with(
        REGIONS,
        Some(simple()),
        vec![
            lanes_element(),
            image(
                WHITE,
                [152, 824, 112, 112],
                bound(Bind::Lane(Lane::SCRATCH)),
            ),
        ],
    )
    .unwrap();
    let mut h = Harness::new(skin, &chord(), &full(0));
    h.frame(1_000, &[]);
    assert_eq!(rects(&h, WHITE), [[152.0, 824.0, 112.0, 112.0]]);
    assert_eq!(note_xs(&h)[0], 160.0);

    // Lanes with gaps: the scratch [100, 150), keys 40 wide every 50 from
    // 200 to 540. On the right the scratch is at 100 + 540 − 100 − 50 =
    // 490 and the key block [200, 540) moves to [100, 440), each key at
    // its own offset.
    let mut gaps = simple();
    gaps.lanes[0] = [100, 50];
    for k in 1..8 {
        gaps.lanes[k] = [200 + 50 * (k as i32 - 1), 40];
    }
    let skin = skin_with(REGIONS, Some(gaps), vec![lanes_element()]).unwrap();
    let canvas = Canvas::new([1920, 1080], Fit::Adapt);
    let p = skin.placement(&canvas, &Layout::AUTHOR, Side::Right);
    assert_eq!(p.lane(Lane::SCRATCH), [490.0, 50.0]);
    for k in 1..8_u8 {
        let x = 100.0 + 50.0 * f32::from(k - 1);
        assert_eq!(p.lane(lane(k)), [x, 40.0], "key {k}");
    }
    let p = skin.placement(&canvas, &Layout::AUTHOR, Side::Left);
    assert_eq!(p.lane(lane(7)), [500.0, 40.0]);
}

#[test]
fn the_lane_width_scales_the_lanes_about_the_anchor() {
    // A key beam bound to key 1, 8 px into it and 48 wide.
    let beam = image(WHITE, [264, 100, 48, 780], bound(Bind::Lane(lane(1))));
    let skin = skin_with(REGIONS, Some(simple()), vec![lanes_element(), beam]).unwrap();
    let mut h = Harness::new(skin, &chord(), &full(0));
    h.layout.set_lane_width(150).unwrap();
    h.frame(1_000, &[]);
    // Anchored left: the span keeps its left edge at 160; each lane's x
    // is 160 + (x − 160)·1.5 and its width 1.5 times.
    let mut want = vec![160.0];
    want.extend((0..7).map(|k| 304.0 + 96.0 * k as f32));
    assert_eq!(note_xs(&h), want);
    assert_eq!(in_column(&h, NOTE, 160.0), [[160.0, 872.0, 144.0, 16.0]]);
    assert_eq!(in_column(&h, NOTE, 304.0), [[304.0, 872.0, 96.0, 16.0]]);
    // The beam: 304 + 8·1.5, 48·1.5 wide.
    assert_eq!(rects(&h, WHITE), [[316.0, 100.0, 72.0, 780.0]]);
    // The line spans the scaled lanes: 160 to 160 + 544·1.5.
    assert_eq!(rects(&h, LINE), [[160.0, 877.0, 816.0, 6.0]]);
    // Anchored at the centre, the span keeps its centre, 432.
    let skin = skin_with(
        lanes_support::anchored(skin_model::Anchor::Center),
        Some(simple()),
        vec![lanes_element()],
    )
    .unwrap();
    let mut layout = Layout::AUTHOR;
    layout.set_lane_width(150).unwrap();
    let p = skin.placement(&Canvas::new([1920, 1080], Fit::Adapt), &layout, Side::Left);
    assert_eq!(p.lane(Lane::SCRATCH), [24.0, 144.0]);
    assert_eq!(p.lane(lane(7)), [744.0, 96.0]);
    assert_eq!((24.0 + 744.0 + 96.0) / 2.0, 432.0);
}

fn section(start_ms: i64, end_ms: i64) -> Section {
    Section {
        start: TimeUs(start_ms * MS),
        end: TimeUs(end_ms * MS),
        lead_in: TimeUs(2_000 * MS),
        initial_gauges: vec![GAUGES[0].start, GAUGES[1].start],
    }
}

/// Whether `chart`'s only note, on key 1, is drawn in a practice round of
/// `[10 s, 20 s)` when it is 300 ms ahead.
fn drawn_in_round(chart: &Chart) -> bool {
    let round = section(10_000, 20_000);
    let ctx = PlayContext {
        attempt: Attempt::Section(&round),
        ..full(0)
    };
    let mut h = Harness::new(lanes_skin(simple()), chart, &ctx);
    let at = chart.lanes[1][0].time.0;
    h.frame_us(at - 300 * MS, &[]);
    let parts = [NOTE, HEAD, TAIL, BODY_ON, BODY_OFF, MINE];
    parts.iter().any(|&p| !in_lane(&h, p, lane(1)).is_empty())
}

#[test]
fn a_practice_round_draws_only_its_targets() {
    let ms = |t: i64| TimeUs(t * MS);
    let tap = |t| ChartBuilder::new(BPM).tap(lane(1), ms(t));
    let long = |t, e| ChartBuilder::new(BPM).long(lane(1), ms(t), ms(e));
    let mine = |t| ChartBuilder::new(BPM).mine(lane(1), ms(t), 10);
    let cases = [
        ("a tap at the start", tap(10_000), true),
        ("a tap inside", tap(15_000), true),
        ("a tap 1 ms before the end", tap(19_999), true),
        ("a tap 1 ms before the start", tap(9_999), false),
        ("a tap at the end", tap(20_000), false),
        ("a long note inside", long(12_000, 14_000), true),
        ("a long note ending at the end", long(19_000, 20_000), false),
        ("a long note crossing the end", long(19_500, 20_500), false),
        ("a long note crossing the start", long(9_500, 10_500), false),
        ("a mine at the start", mine(10_000), true),
        ("a mine 1 ms before the end", mine(19_999), true),
        ("a mine at the end", mine(20_000), false),
        ("a mine before the start", mine(9_999), false),
    ];
    for (name, chart, want) in cases {
        let chart = chart.build().unwrap();
        assert_eq!(drawn_in_round(&chart), want, "{name}");
    }
    // A whole-chart attempt draws all of them.
    let chart = tap(9_999).build().unwrap();
    let mut h = harness(&chart);
    h.frame(9_699, &[]);
    assert_eq!(in_lane(&h, NOTE, lane(1)).len(), 1);
}

#[test]
fn a_finished_round_rings_out_without_notes() {
    // The round's last long note, drawn 200 ms behind the clock: its tail
    // is still ahead of the line when evaluation completes. No `SongEnd`
    // is delivered.
    let chart = ChartBuilder::new(BPM)
        .long(lane(2), TimeUs(18_000 * MS), TimeUs(19_500 * MS))
        .build()
        .unwrap();
    let round = section(10_000, 20_000);
    let ctx = PlayContext {
        attempt: Attempt::Section(&round),
        ..full(0)
    };
    let mut h = Harness::new(lanes_skin(simple()), &chart, &ctx);
    h.offset = TimeUs(-200 * MS);
    h.frame(19_600, &[]);
    assert_eq!(long_parts(&h, lane(2))[2].len(), 2);
    h.complete = true;
    h.frame(19_600, &[]);
    assert_eq!(
        long_parts(&h, lane(2)).iter().map(Vec::len).sum::<usize>(),
        0
    );
    // The line still draws.
    assert_eq!(rects(&h, LINE).len(), 1);
    // A whole-chart attempt keeps drawing until its notes pass.
    let mut h = Harness::new(lanes_skin(simple()), &chart, &full(0));
    h.offset = TimeUs(-200 * MS);
    h.complete = true;
    h.frame(19_600, &[]);
    assert_eq!(long_parts(&h, lane(2))[2].len(), 2);
}

/// Taps `per_lane` on every lane every `step` µs from 1 s, and `extra`
/// more on key 7 after them.
fn taps(per_lane: i64, step: i64, extra: i64) -> Chart {
    let mut b = ChartBuilder::new(BPM);
    for l in Lane::all() {
        let n = per_lane + if l == lane(7) { extra } else { 0 };
        for i in 0..n {
            b = b.tap(l, TimeUs(1_000 * MS + i * step));
        }
    }
    b.build().unwrap()
}

/// The simple lanes without bar lines, two image elements beside them.
fn budget_skin() -> Skin {
    let mut def = simple();
    def.parts.bar = None;
    let screen = Place::SCREEN;
    skin_with(
        REGIONS,
        Some(def),
        vec![
            image(WHITE, [0, 0, 10, 10], screen),
            lanes_element(),
            image(WHITE, [20, 0, 10, 10], screen),
        ],
    )
    .unwrap()
}

#[test]
fn the_quad_budget_fills_exactly_and_keeps_the_reserved_quads() {
    // 1 024 taps per lane, 0.5 ms apart: 8 192 in 512 ms, all visible.
    let per_lane = i64::from(LANE_QUADS) / 8;
    let run = |extra: i64| {
        let mut h = Harness::new(budget_skin(), &taps(per_lane, 500, extra), &full(0));
        h.view.cover = 100;
        h.view.lift = 100;
        h.frame(1_000, &[]);
        h
    };
    let h = run(0);
    assert_eq!(rects(&h, NOTE).len(), LANE_QUADS as usize);
    assert_eq!(h.batch.dropped(), 0);
    for part in [LINE, COVER, LIFT] {
        assert_eq!(rects(&h, part).len(), 1, "part {part}");
    }
    // Every image element drew too: the batch is exactly full.
    assert_eq!(rects(&h, WHITE).len(), 2);
    assert_eq!(h.skin.quad_capacity(), 2 + LANE_QUADS + 3);
    assert_eq!(h.batch.quads().len(), h.skin.quad_capacity() as usize);
    assert!(h.batch.is_full());
    // One more: still the budget's notes, one drop, the covers kept.
    let h = run(1);
    assert_eq!(rects(&h, NOTE).len(), LANE_QUADS as usize);
    assert_eq!(h.batch.dropped(), 1);
    for part in [LINE, COVER, LIFT] {
        assert_eq!(rects(&h, part).len(), 1, "part {part}");
    }
    assert_eq!(rects(&h, WHITE).len(), 2);
}

#[test]
fn the_budget_can_end_inside_a_long_note() {
    // `LANE_QUADS − left` taps, then a long note on key 7 drawn body, tail,
    // head: with 1 quad left only its body fits, with 2 its body and tail,
    // and the rest is dropped as one; with 3 it is whole.
    let per_lane = i64::from(LANE_QUADS) / 8;
    for (left, parts, dropped) in [(1, [1, 0, 0], 1), (2, [1, 1, 0], 1), (3, [1, 1, 1], 0)] {
        let mut b = ChartBuilder::new(BPM);
        let mut taps = 0;
        for l in Lane::all() {
            let n = if l == lane(7) {
                per_lane - left
            } else {
                per_lane
            };
            for i in 0..n {
                b = b.tap(l, TimeUs(1_000 * MS + i * 500));
                taps += 1;
            }
        }
        let at = 1_000 * MS + per_lane * 500;
        let chart = b
            .long(lane(7), TimeUs(at), TimeUs(at + 20 * MS))
            .build()
            .unwrap();
        let mut h = Harness::new(budget_skin(), &chart, &full(0));
        h.frame(1_000, &[]);
        assert_eq!(rects(&h, NOTE).len(), taps, "{left} left");
        let got = [BODY_OFF, TAIL, HEAD].map(|p| rects(&h, p).len());
        assert_eq!(got, parts, "{left} left");
        assert_eq!(h.batch.dropped(), dropped, "{left} left");
    }
}

#[test]
fn skipped_candidates_spend_the_visit_budget() {
    // 2 048 judged taps per lane, 0.25 ms apart (512 ms, all visible):
    // `LANE_VISITS` candidates, none drawn. Key 7's next tap is never
    // visited.
    let per_lane = i64::from(LANE_VISITS) / 8;
    let chart = taps(per_lane, 250, 1);
    let mut effects = Vec::new();
    for l in Lane::all() {
        for n in 0..per_lane {
            let n = usize::try_from(n).unwrap();
            let l = u8::try_from(l.index()).unwrap();
            effects.push(ev(0, judged(l, n, Judgment::Great, Some(0), None, 1)));
        }
    }
    let mut h = Harness::new(budget_skin(), &chart, &full(0));
    h.frame(1_000, &effects);
    assert!(rects(&h, NOTE).is_empty());
    assert_eq!(h.batch.dropped(), 1);
    assert_eq!(rects(&h, LINE).len(), 1);
    // Without the extra tap the visits end exactly with the candidates:
    // nothing is dropped.
    let mut h = Harness::new(budget_skin(), &taps(per_lane, 250, 0), &full(0));
    h.frame(1_000, &effects);
    assert!(rects(&h, NOTE).is_empty());
    assert_eq!(h.batch.dropped(), 0);

    // Culled candidates: long notes of 10 µs every 20 µs whose ends have
    // passed the line but lie within the culling range below it.
    let mut b = ChartBuilder::new(BPM);
    for l in Lane::all() {
        for i in 0..per_lane {
            let t = 1_000 * MS - 50 * MS + i * 20;
            b = b.long(l, TimeUs(t), TimeUs(t + 10));
        }
    }
    let chart = b.tap(lane(7), TimeUs(1_200 * MS)).build().unwrap();
    let mut h = Harness::new(budget_skin(), &chart, &full(0));
    h.frame(1_000, &[]);
    assert!(rects(&h, NOTE).is_empty());
    assert!(rects(&h, HEAD).is_empty());
    assert_eq!(h.batch.dropped(), 1);
}

fn def_with(elements: Vec<skin_model::ElementDef>, lanes: Option<LanesDef>) -> SkinDef {
    SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: lanes_support::sheet(),
        lanes,
        elements,
    }
}

#[test]
fn places_and_lanes_are_checked_at_load() {
    let err = |def: SkinDef| Skin::new(def).unwrap_err();
    let screen = |r| image(WHITE, r, Place::SCREEN);
    // A fixed canvas holds `screen` only.
    let mut fixed = def_with(
        vec![screen([0; 4]), image(WHITE, [0; 4], in_region(Region::Top))],
        None,
    );
    fixed.fit = Fit::Fixed;
    assert!(matches!(
        err(fixed),
        LoadError::RegionInFixed { element: 1 }
    ));
    let cover = Place {
        region: Region::Top,
        fill: Fill::Cover,
        ..Place::SCREEN
    };
    assert!(matches!(
        err(def_with(vec![image(WHITE, [0; 4], cover)], None)),
        LoadError::FillOutsideScreen { element: 0 }
    ));
    // Keys within ±2^20.
    let far = MAX_COORD;
    assert!(Skin::new(def_with(vec![screen([-far, far, far, -far])], None)).is_ok());
    assert!(matches!(
        err(def_with(
            vec![screen([0; 4]), screen([0, far + 1, 0, 0])],
            None
        )),
        LoadError::Coordinates { element: 1 }
    ));
    // A widget track gives color and alpha only.
    for prop in [0, 1, 2, 3, 8] {
        let mut props = [None; 9];
        props[prop] = Some(1);
        let moved = skin_model::ElementDef {
            track: still(props),
            ..lanes_element()
        };
        assert!(
            matches!(
                err(def_with(vec![moved], Some(simple()))),
                LoadError::WidgetGeometry { element: 0 }
            ),
            "prop {prop}"
        );
    }
    let mut alpha = [None; 9];
    alpha[4] = Some(10);
    let faded = skin_model::ElementDef {
        track: still(alpha),
        ..lanes_element()
    };
    assert!(Skin::new(def_with(vec![faded], Some(simple()))).is_ok());
    assert!(matches!(
        err(def_with(
            vec![lanes_element(), lanes_element()],
            Some(simple())
        )),
        LoadError::LanesTwice
    ));
    let elsewhere = skin_model::ElementDef {
        place: in_region(Region::Screen),
        ..lanes_element()
    };
    assert!(matches!(
        err(def_with(vec![elsewhere.clone()], Some(simple()))),
        LoadError::LanesRegion
    ));
    // Under a fixed canvas the lanes sit in `screen`, and only there.
    let mut fixed = def_with(vec![elsewhere], Some(simple()));
    fixed.fit = Fit::Fixed;
    assert!(Skin::new(fixed).is_ok());
    let beam = |region| {
        image(
            WHITE,
            [0; 4],
            Place {
                region,
                bind: Bind::Lanes,
                ..Place::SCREEN
            },
        )
    };
    assert!(matches!(
        err(def_with(vec![beam(Region::Playfield)], None)),
        LoadError::Unbound { element: 0 }
    ));
    assert!(matches!(
        err(def_with(
            vec![lanes_element(), beam(Region::Top)],
            Some(simple())
        )),
        LoadError::Unbound { element: 1 }
    ));
    assert!(
        Skin::new(def_with(
            vec![lanes_element(), beam(Region::Playfield)],
            Some(simple())
        ))
        .is_ok()
    );

    let lanes_err = |lanes: LanesDef| match err(def_with(vec![lanes_element()], Some(lanes))) {
        LoadError::Lanes(e) => e,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        err(def_with(Vec::new(), Some(simple()))),
        LoadError::Lanes(LanesError::MissingElement)
    ));
    assert!(matches!(
        err(def_with(vec![lanes_element()], None)),
        LoadError::Lanes(LanesError::MissingDef)
    ));
    let mut bad = simple();
    bad.lanes[3][1] = 0;
    assert_eq!(lanes_err(bad), LanesError::LaneWidth { lane: 3 });
    let mut bad = simple();
    bad.lanes[7] = [MAX_COORD - 63, 64];
    assert_eq!(lanes_err(bad), LanesError::Span);
    let mut ok = simple();
    ok.lanes[7] = [MAX_COORD - 64, 64];
    assert!(Skin::new(def_with(vec![lanes_element()], Some(ok))).is_ok());
    for (top, line, bottom) in [
        (880, 880, 960),
        (80, 961, 960),
        (-MAX_COORD - 1, 880, 960),
        (80, 880, MAX_COORD + 1),
    ] {
        let mut bad = simple();
        (bad.top, bad.line, bad.bottom) = (top, line, bottom);
        assert_eq!(lanes_err(bad), LanesError::Heights, "{top} {line} {bottom}");
    }
    let mut edge = simple();
    (edge.top, edge.line, edge.bottom) = (-MAX_COORD, 880, 880);
    assert!(Skin::new(def_with(vec![lanes_element()], Some(edge))).is_ok());
    let mut bad = simple();
    bad.mine_h = 0;
    assert_eq!(lanes_err(bad), LanesError::Heights);
    // Authored on the right: the scratch is left of a key.
    let mut bad = simple();
    bad.scratch = Side::Right;
    assert_eq!(lanes_err(bad), LanesError::ScratchNotAtSide);
    let mut bad = simple();
    bad.lanes[4] = [100, 64];
    assert_eq!(lanes_err(bad), LanesError::ScratchNotAtSide);
    let mut bad = simple();
    bad.parts.head[5] = 11;
    assert_eq!(
        lanes_err(bad),
        LanesError::UnknownPart {
            part: LanePart::Head
        }
    );
    let mut bad = simple();
    bad.parts.lift = Some(99);
    assert_eq!(
        lanes_err(bad),
        LanesError::UnknownPart {
            part: LanePart::Lift
        }
    );
}

#[test]
fn the_lanes_draw_in_one_run_with_the_widgets_color() {
    let chart = long_note();
    let mut alpha = [None; 9];
    alpha[4] = Some(51);
    let faded = skin_model::ElementDef {
        track: still(alpha),
        ..lanes_element()
    };
    let skin = skin_with(REGIONS, Some(simple()), vec![faded]).unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    h.frame(1_150, &[]);
    assert_eq!(h.batch.runs().len(), 1);
    assert!(h.batch.quads().len() > 3);
    for q in h.batch.quads() {
        assert_eq!(q.color, [1.0, 1.0, 1.0, 0.2]);
        assert_eq!(q.angle, 0.0);
    }
}
