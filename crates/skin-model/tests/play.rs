//! A play through `Skin`: retries, READY states, practice rounds, the beat
//! timer, `when` timers shared by elements, the targets against the
//! evaluator's, time extremes, and load errors.

mod play_support;

use bms_model::{BEAT, Chart, ChartBuilder, ScrollMap, TimeUs};
use eval::{EvalInput, JudgeEventKind, Section};
use play_support::{
    GAUGES, Harness, MS, bool_program, element, ev, full, judged, lane, load, pct, state, tally,
};
use rules::{GaugeValue, Judgment};
use skin_model::{
    Attempt, Blend, BpmRange, ClipId, CoverSettings, DurationRatio, Ease, ElementDef, End,
    ErrorKind, Export, FrameReadouts, GREEN_MAX, GaugeDesc, KeyDef, LaneView, LoadError,
    MAX_ELEMENTS, MAX_OPS, MAX_SKIN_KEYS, MAX_WHEN_TIMERS, NativeReadouts, PLAY, PlayContext,
    PrepareError, Prop, READOUT_MAX, Schema, Scope, Side, TIMER_OFF, TimerDef, TimerRef, Track, Ty,
    compile, field, timer,
};

const BPM_150: i64 = 150_000;

fn taps(times_ms: &[i64]) -> Chart {
    times_ms
        .iter()
        .fold(ChartBuilder::new(BPM_150), |b, &t| {
            b.tap(lane(1), TimeUs(t * MS))
        })
        .build()
        .unwrap()
}

fn section(start_ms: i64, end_ms: i64, lead_in_ms: i64) -> Section {
    Section {
        start: TimeUs(start_ms * MS),
        end: TimeUs(end_ms * MS),
        lead_in: TimeUs(lead_in_ms * MS),
        initial_gauges: vec![pct(50), GaugeValue::FULL],
    }
}

fn in_section(s: &Section) -> PlayContext<'_> {
    PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &GAUGES,
        attempt: Attempt::Section(s),
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    }
}

#[test]
fn a_retry_starts_clean() {
    let chart = taps(&[1_000, 2_000]);
    let skin = || {
        load(
            vec![bool_program("on(judge)")],
            vec![
                element(TimerRef::When(0), 1),
                element(TimerRef::Builtin(timer::FULL_COMBO), 2),
            ],
        )
        .unwrap()
    };
    let mut h = Harness::new(skin(), &chart, &full(1_000));
    h.tally = tally(2);
    h.frame(
        2_000,
        &[
            ev(1_000, judged(1, 0, Judgment::Perfect, Some(0), None, 1)),
            ev(1_000, play_support::gauge(0, GaugeValue::FULL)),
            ev(2_000, judged(1, 1, Judgment::Perfect, Some(0), None, 2)),
            ev(2_100, JudgeEventKind::SongEnd),
        ],
    );
    assert!(h.start(timer::FULL_COMBO).is_some());
    assert_eq!(h.batch.quads().len(), 2);
    h.play = h.skin.prepare(&chart, &full(1_000)).unwrap();
    h.frame(2_200, &[]);
    let mut fresh = Harness::new(skin(), &chart, &full(1_000));
    fresh.tally = tally(2);
    fresh.frame(2_200, &[]);
    for slot in 0..timer::BUILTIN + 1 {
        assert_eq!(h.start(slot), fresh.start(slot), "slot {slot}");
    }
    assert_eq!(h.play.state().timers, fresh.play.state().timers);
    assert_eq!(h.play.state().fields, fresh.play.state().fields);
    assert_eq!(h.batch.quads(), fresh.batch.quads());
    assert_eq!(h.batch.runs(), fresh.batch.runs());
}

#[test]
fn a_mid_chart_ready_state() {
    let chart = taps(&[1_000, 31_000]);
    let s = section(30_000, 60_000, 1_000);
    let skin = load(Vec::new(), vec![element(TimerRef::Builtin(timer::PLAY), 1)]).unwrap();
    let mut h = Harness::new(skin, &chart, &in_section(&s));
    h.frame_us(28_999_999, &[]);
    assert_eq!(h.elapsed(timer::READY), TIMER_OFF);
    h.frame(29_000, &[]);
    assert_eq!(h.elapsed(timer::READY), 0);
    h.frame_us(29_999_500, &[]);
    assert_eq!(h.elapsed(timer::READY), 999);
    assert_eq!(h.elapsed(timer::PLAY), TIMER_OFF);
    assert!(h.batch.quads().is_empty());
    h.frame(30_000, &[]);
    assert_eq!(h.elapsed(timer::PLAY), 0);
    assert_eq!(h.batch.quads().len(), 1);
    assert_eq!(h.field(field::NOTES), 1);
}

/// In a play the screen timer, which untimed elements run on, starts at
/// `ready`'s start, so an untimed animation starts again at
/// READY; `loading` stays off and `load.percent` reads 100.
#[test]
fn the_screen_timer_starts_with_ready_in_a_play() {
    let chart = taps(&[1_000, 31_000]);
    let s = section(30_000, 60_000, 1_000);
    let skin = load(
        Vec::new(),
        vec![element(TimerRef::Builtin(timer::SCREEN), 1)],
    )
    .unwrap();
    let mut h = Harness::new(skin, &chart, &in_section(&s));
    h.frame_us(28_999_999, &[]);
    assert_eq!(h.elapsed(timer::SCREEN), TIMER_OFF);
    assert!(h.batch.quads().is_empty());
    for (now, ms) in [(29_000, 0), (29_250, 250), (31_000, 2_000)] {
        h.frame(now, &[]);
        assert_eq!(h.start(timer::SCREEN), h.start(timer::READY), "at {now}");
        assert_eq!(h.elapsed(timer::SCREEN), ms, "at {now}");
        assert_eq!(h.batch.quads().len(), 1, "at {now}");
        assert_eq!(h.elapsed(timer::LOADING), TIMER_OFF, "at {now}");
        assert_eq!(h.field(field::LOAD_PERCENT), 100, "at {now}");
    }
}

/// Key-on and key-off from the view's lane input, mutually exclusive, in
/// µs on the frame's clock: a lane held since the opening runs key-on from
/// the READY start with key-off off; a release runs key-off and stops
/// key-on, a press the reverse; a reversal's release and press at one µs
/// leave key-on from that µs; a lane never released has key-off off. The
/// display offset moves neither, and neither stops in READY or once the
/// play is no longer playing.
#[test]
fn key_on_and_key_off_follow_the_lane_input() {
    use skin_model::{LaneTimer, lane_timer};
    let chart = taps(&[1_000, 31_000]);
    let mut h = Harness::empty(&chart, &full(2_000));
    let on = |l: u8| lane_timer(LaneTimer::Press, lane(l));
    let off = |l: u8| lane_timer(LaneTimer::Release, lane(l));
    // Key 3 held at the opening, from the evaluator's own opening facts;
    // the scratch untouched.
    let device = eval::DeviceProfile {
        sources: vec![eval::SourceProfile {
            input_offset: TimeUs(0),
            bindings: vec![eval::Binding {
                control: eval::ControlId(3),
                lane: lane(3),
            }],
            axes: Vec::new(),
        }],
    };
    let rule = rules::tapestry();
    let start = eval::InputStart {
        opened_at: TimeUs(0),
        held: vec![(eval::SourceId(0), eval::ControlId(3), 1)],
    };
    let ev = eval::Evaluator::new(&chart, &rule, &device, &start, None, 4).unwrap();
    h.input = *ev.input();
    h.offset = TimeUs(150 * MS);
    h.playing = false;
    h.frame(-1_500, &[]);
    let ready_at = Some(-2_000 * MS);
    assert_eq!(h.start(timer::READY), ready_at);
    assert_eq!((h.start(on(3)), h.start(off(3))), (ready_at, None));
    assert_eq!(h.elapsed(on(3)), 500);
    assert_eq!((h.start(on(0)), h.start(off(0))), (None, None));
    // Released at −1 s: key-off from there, key-on off.
    h.input.release(lane(3), TimeUs(-1_000 * MS));
    h.frame(-900, &[]);
    assert_eq!((h.start(on(3)), h.start(off(3))), (None, Some(-1_000 * MS)));
    assert_eq!(h.elapsed(off(3)), 100);
    // Pressed again at 0.5 s, playing.
    h.playing = true;
    h.input.press(lane(3), TimeUs(500 * MS));
    h.frame(700, &[]);
    assert_eq!((h.start(on(3)), h.start(off(3))), (Some(500 * MS), None));
    assert_eq!(h.elapsed(on(3)), 200);
    // A reversal: released and pressed again at one µs.
    h.input.release(lane(3), TimeUs(800 * MS + 1));
    h.input.press(lane(3), TimeUs(800 * MS + 1));
    h.frame(900, &[]);
    assert_eq!(
        (h.start(on(3)), h.start(off(3))),
        (Some(800 * MS + 1), None)
    );
    // Stopped: still running.
    h.playing = false;
    h.input.release(lane(3), TimeUs(1_000 * MS));
    h.frame(5_000, &[]);
    assert_eq!((h.start(on(3)), h.start(off(3))), (None, Some(1_000 * MS)));
    assert_eq!(h.elapsed(off(3)), 4_000);
}

/// The smallest µs whose display position reaches `k` beats, from the
/// map's segments: before 0 at the initial rate, then per segment from its
/// anchor at its slope.
fn crossing(map: &ScrollMap, k: i128) -> i64 {
    let target = k * BEAT;
    let ceil_div = |a: i128, b: i128| -((-a).div_euclid(b));
    if target <= 0 {
        return i64::try_from(ceil_div(target, map.initial_rate())).unwrap();
    }
    let segments = map.segments();
    for (i, s) in segments.iter().enumerate() {
        let next = segments.get(i + 1).map(|n| i128::from(n.start.0));
        let at = if s.anchor.0 >= target {
            i128::from(s.start.0)
        } else if s.slope > 0 {
            i128::from(s.start.0) + ceil_div(target - s.anchor.0, s.slope)
        } else {
            continue;
        };
        if next.is_none_or(|n| at < n) {
            return i64::try_from(at).unwrap();
        }
    }
    unreachable!()
}

#[test]
fn beat_follows_the_display_clock() {
    let chart = taps(&[1_000]);
    let map = chart.scroll.clone();
    let mut h = Harness::empty(&chart, &full(1_000));
    // Before `ready`: off.
    h.frame(-1_500, &[]);
    assert_eq!(h.start(timer::BEAT), None);
    // Before chart time 0: −500 ms is beat −1.25, crossed at beat −2.
    h.frame(-500, &[]);
    assert_eq!(h.start(timer::BEAT), Some(crossing(&map, -2)));
    // 150 BPM: a beat every 400 ms; 1 s is beat 2.5.
    h.frame(1_000, &[]);
    let c2 = crossing(&map, 2);
    assert!((800 * MS..=800 * MS + 1).contains(&c2));
    assert_eq!(h.start(timer::BEAT), Some(c2));
    // With a 50 ms display offset, 1.2 s shows 1.25 s, in beat 3: the start
    // is the crossing moved back to the frame's clock.
    h.offset = TimeUs(50 * MS);
    h.frame(1_200, &[]);
    let c3 = crossing(&map, 3);
    assert_eq!(h.start(timer::BEAT), Some(c3 - 50 * MS));
    assert_eq!(i64::from(h.elapsed(timer::BEAT)), (1_250 * MS - c3) / MS);
    // The offset changes within the beat: elapsed is display time minus
    // the crossing.
    h.offset = TimeUs(30 * MS);
    h.frame(1_250, &[]);
    assert_eq!(h.start(timer::BEAT), Some(c3 - 30 * MS));
    assert_eq!(i64::from(h.elapsed(timer::BEAT)), (1_280 * MS - c3) / MS);
}

#[test]
fn beat_inside_a_stop_keeps_the_latest_crossing() {
    // 120 BPM: a beat every 500 ms. A STOP at 250 ms (off the beat) for
    // 300 ms: beat 0 started at 0 and keeps running through the STOP; beat
    // 1 comes 250 ms after it ends.
    let chart = ChartBuilder::new(120_000)
        .stop(TimeUs(250 * MS), TimeUs(300 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut h = Harness::empty(&chart, &full(0));
    h.frame(400, &[]);
    assert_eq!(h.start(timer::BEAT), Some(0));
    assert_eq!(h.elapsed(timer::BEAT), 400);
    h.frame(549, &[]);
    assert_eq!(h.start(timer::BEAT), Some(0));
    h.frame(900, &[]);
    let c1 = crossing(&chart.scroll, 1);
    assert!((800 * MS..=800 * MS + 1).contains(&c1));
    assert_eq!(h.start(timer::BEAT), Some(c1));

    // A STOP on the beat (at 500 ms): the crossing is the STOP's start.
    let aligned = ChartBuilder::new(120_000)
        .stop(TimeUs(500 * MS), TimeUs(300 * MS))
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    let mut h = Harness::empty(&aligned, &full(0));
    h.frame(700, &[]);
    assert_eq!(h.start(timer::BEAT), Some(500 * MS));
    assert_eq!(crossing(&aligned.scroll, 1), 500 * MS);
}

#[test]
fn the_beat_follows_the_exact_playhead_at_a_fractional_boundary() {
    // 9,375,000 BPM: a beat every 6.4 µs. A tap on beat 1 sounds at
    // 6.4 µs, floored to 6: the exact playhead reaches beat 1 at 6 µs, as
    // the tap reaches the line, where the sampled scroll map reaches it
    // only at 7 µs.
    let dir = tempfile::tempdir().unwrap();
    let text = "#BPM 9375000\n#WAV01 a.wav\n#00011:00010000\n";
    let chart = bms_parser::load_bms(text.as_bytes(), dir.path(), &Default::default())
        .unwrap()
        .chart;
    assert_eq!(chart.lanes[1][0].time, TimeUs(6));
    assert_eq!(crossing(&chart.scroll, 1), 7);
    let mut h = Harness::empty(&chart, &full(0));
    h.frame_us(5, &[]);
    assert_eq!(h.start(timer::BEAT), Some(0));
    h.frame_us(6, &[]);
    assert_eq!(h.start(timer::BEAT), Some(6));
    assert_eq!(h.play.display_position(), chart.lanes[1][0].pos);
}

/// A track on a timer that started at the earliest time, sampled at the
/// latest: its age saturates and it shows its last key.
#[test]
fn an_element_ages_without_overflow() {
    let chart = taps(&[1_000]);
    let mut first = [None; 9];
    first[Prop::X as usize] = Some(1);
    let mut last = [None; 9];
    last[Prop::X as usize] = Some(2);
    let track = Track::new(
        &[
            KeyDef {
                at: TimeUs(0),
                props: first,
            },
            KeyDef {
                at: TimeUs(1_000 * MS),
                props: last,
            },
        ],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    let skin = load(
        Vec::new(),
        vec![ElementDef {
            track,
            ..element(TimerRef::Builtin(timer::READY), 0)
        }],
    )
    .unwrap();
    let ctx = PlayContext {
        attempt: Attempt::Full {
            lead_in: TimeUs(i64::MAX),
        },
        ..full(0)
    };
    let mut h = Harness::new(skin, &chart, &ctx);
    h.frame_us(i64::MIN + 1, &[]);
    assert_eq!(h.batch.quads()[0].rect[0], 1.0);
    h.frame_us(i64::MAX, &[]);
    assert_eq!(h.start(timer::READY), Some(i64::MIN + 1));
    assert_eq!(h.batch.quads().len(), 1);
    assert_eq!(h.batch.quads()[0].rect[0], 2.0);
}

#[test]
fn time_extremes_neither_panic_nor_wrap() {
    let chart = taps(&[1_000]);
    // Far outside the scroll map's domain: the beat is the clamped one.
    let mut h = Harness::empty(&chart, &full(1_000));
    h.frame_us(i64::MAX, &[]);
    // The reading saturates; a program clamps it to `MAX_ELAPSED_MS`.
    assert_eq!(h.elapsed(timer::PLAY), i32::MAX);
    let b = h.start(timer::BEAT).unwrap();
    assert!(b <= bms_model::DOMAIN);
    h.frame_us(i64::MAX - 1, &[]);
    assert_eq!(h.start(timer::BEAT), Some(b));
    // The longest lead-in, the earliest frame and an offset whose crossing
    // leaves the time range: `beat` is off, the rest reads.
    let mut h = Harness::empty(
        &chart,
        &PlayContext {
            attempt: Attempt::Full {
                lead_in: TimeUs(i64::MAX),
            },
            ..full(0)
        },
    );
    h.offset = TimeUs(i64::MAX - 1);
    h.frame_us(i64::MIN + 1, &[]);
    assert_eq!(h.start(timer::READY), Some(i64::MIN + 1));
    assert_eq!(h.elapsed(timer::READY), 0);
    assert_eq!(h.start(timer::BEAT), None);
    // A display time before the domain: the crossing is searched inside it.
    h.offset = TimeUs(-1);
    h.frame_us(i64::MIN + 1, &[]);
    let b = h.start(timer::BEAT).unwrap();
    assert!(
        (-bms_model::DOMAIN + 1..=bms_model::DOMAIN + 1).contains(&b),
        "{b}"
    );
    // A READY state that would start before the time range is refused.
    let skin = load(Vec::new(), Vec::new()).unwrap();
    let early = Section {
        start: TimeUs(i64::MIN),
        ..section(0, 1, 1)
    };
    assert_eq!(
        skin.prepare(&chart, &in_section(&early)).err(),
        Some(PrepareError::Overflow)
    );
    let negative = Section {
        lead_in: TimeUs(-1),
        ..section(0, 1, 0)
    };
    assert_eq!(
        skin.prepare(&chart, &in_section(&negative)).err(),
        Some(PrepareError::NegativeLeadIn)
    );
    assert_eq!(
        skin.prepare(
            &chart,
            &PlayContext {
                attempt: Attempt::Full {
                    lead_in: TimeUs(-1)
                },
                ..full(0)
            }
        )
        .err(),
        Some(PrepareError::NegativeLeadIn)
    );
    let one_gauge = Section {
        initial_gauges: vec![pct(50)],
        ..section(0, 1, 0)
    };
    assert_eq!(
        skin.prepare(&chart, &in_section(&one_gauge)).err(),
        Some(PrepareError::InitialGauges {
            expected: 2,
            got: 1
        })
    );
    // An empty section is accepted.
    assert!(skin.prepare(&chart, &in_section(&section(5, 5, 0))).is_ok());
}

/// Taps at 5, 10, 15, 20 and 25 s, long notes 16-19 s and 18-21 s, and a
/// mine at 12 s: the section [10 s, 20 s) holds the taps at 10 and 15 s and
/// the first long note.
fn practice_chart() -> Chart {
    ChartBuilder::new(BPM_150)
        .tap(lane(1), TimeUs(5_000 * MS))
        .tap(lane(1), TimeUs(10_000 * MS))
        .tap(lane(1), TimeUs(15_000 * MS))
        .tap(lane(1), TimeUs(20_000 * MS))
        .tap(lane(1), TimeUs(25_000 * MS))
        .long(lane(2), TimeUs(16_000 * MS), TimeUs(19_000 * MS))
        .long(lane(3), TimeUs(18_000 * MS), TimeUs(21_000 * MS))
        .mine(lane(4), TimeUs(12_000 * MS), 10)
        .build()
        .unwrap()
}

#[test]
fn a_practice_round() {
    let chart = practice_chart();
    let s = section(10_000, 20_000, 2_000);
    for round in 0..2 {
        let mut h = Harness::empty(&chart, &in_section(&s));
        h.frame_us(7_999_999, &[]);
        assert_eq!(h.start(timer::READY), None, "round {round}");
        h.frame(8_000, &[]);
        assert_eq!(h.field(field::NOTES), 3);
        assert_eq!(h.start(timer::READY), Some(8_000 * MS));
        assert_eq!(h.start(timer::PLAY), None);
        h.frame(
            10_000,
            &[ev(
                10_000,
                judged(1, 1, Judgment::Perfect, Some(0), None, 1),
            )],
        );
        assert_eq!(h.start(timer::PLAY), Some(10_000 * MS));
        h.frame(
            16_000,
            &[
                ev(15_000, judged(1, 2, Judgment::Great, Some(0), None, 2)),
                ev(16_000, play_support::ln_held(2, 0, Judgment::Perfect)),
            ],
        );
        h.frame(
            19_000,
            &[
                ev(19_000, play_support::hold_ended(2, 0)),
                ev(19_000, judged(2, 0, Judgment::Perfect, Some(0), None, 3)),
            ],
        );
        assert_eq!(h.start(timer::FULL_COMBO), Some(19_000 * MS));
    }
}

#[test]
fn targets_match_the_evaluators() {
    let chart = practice_chart();
    let preset = rules::tapestry();
    let skin = load(Vec::new(), Vec::new()).unwrap();
    let gauges: Vec<_> = preset
        .gauges
        .iter()
        .map(skin_model::GaugeDesc::from_rule)
        .collect();
    let edge = Section {
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
        ..section(10_000, 20_000, 2_000)
    };
    let full_ctx = PlayContext {
        meta: skin_model::ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: preset.empty_poor.breaks_combo,
        empty_poor_counted: preset.empty_poor.counted,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    };
    let section_ctx = PlayContext {
        attempt: Attempt::Section(&edge),
        ..full_ctx
    };
    for (section, ctx) in [(None, full_ctx), (Some(edge.clone()), section_ctx)] {
        let evaluation = eval::evaluate(
            &chart,
            &EvalInput {
                section,
                ..EvalInput::default()
            },
            &preset,
        )
        .unwrap();
        let judged = evaluation
            .events
            .iter()
            .filter(|e| matches!(e.kind, JudgeEventKind::Judged { .. }))
            .count();
        let mut h = Harness::new(skin.clone(), &chart, &ctx);
        h.gauges = evaluation.gauges.iter().map(|g| state(g.value)).collect();
        h.frame(0, &[]);
        assert_eq!(usize::try_from(h.field(field::NOTES)).unwrap(), judged);
    }
}

#[test]
fn two_elements_share_one_when_timer() {
    let chart = taps(&[1_000]);
    let mut b_props = [None; 9];
    b_props[Prop::X as usize] = Some(2);
    let b_track = Track::new(
        &[
            KeyDef {
                at: TimeUs(100 * MS),
                props: b_props,
            },
            KeyDef {
                at: TimeUs(300 * MS),
                props: [None; 9],
            },
        ],
        Ease::Linear,
        End::Hide,
    )
    .unwrap();
    let skin = load(
        vec![bool_program("combo >= 2")],
        vec![
            element(TimerRef::When(0), 1),
            ElementDef {
                track: b_track,
                ..element(TimerRef::When(0), 0)
            },
        ],
    )
    .unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    let drawn = |h: &Harness| -> Vec<f32> { h.batch.quads().iter().map(|q| q.rect[0]).collect() };
    let w = timer::BUILTIN;
    h.tally = tally(1);
    h.frame(900, &[]);
    assert_eq!(h.start(w), None);
    assert!(drawn(&h).is_empty());
    h.tally = tally(2);
    h.frame(1_000, &[]);
    assert_eq!(h.start(w), Some(1_000 * MS));
    assert_eq!(drawn(&h), [1.0]);
    h.tally = tally(3);
    h.frame(1_100, &[]);
    assert_eq!(h.start(w), Some(1_000 * MS));
    assert_eq!(h.elapsed(w), 100);
    assert_eq!(drawn(&h), [1.0, 2.0]);
    h.tally = tally(0);
    h.frame(1_500, &[]);
    assert_eq!(h.start(w), None);
    assert_eq!(h.elapsed(w), TIMER_OFF);
    assert!(drawn(&h).is_empty());
    h.tally = tally(2);
    h.frame(2_000, &[]);
    assert_eq!(h.start(w), Some(2_000 * MS));
    assert_eq!(drawn(&h), [1.0]);
    h.frame(2_100, &[]);
    assert_eq!(drawn(&h), [1.0, 2.0]);
}

#[test]
fn no_when_reads_a_when() {
    // The play scope names no `when` timer.
    let err = compile("on(w0)", &Scope::new(&PLAY), Ty::Bool).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::UnknownTimer { .. }));
    // A condition compiled against a wider schema, which names `when` 0 at
    // slot 55, still reads it as off: conditions see the built-in slots
    // only. Elements see every slot.
    let mut timers = PLAY.timers.to_vec();
    timers.push(TimerDef {
        name: "w0",
        export: Export::TapestryOnly,
    });
    let extended: &'static Schema = Box::leak(Box::new(Schema {
        timers: timers.leak(),
        ..PLAY
    }));
    let reads_when = compile("on(w0)", &Scope::new(extended), Ty::Bool).unwrap();
    let chart = taps(&[1_000]);
    let skin = load(
        vec![bool_program("on(play)"), reads_when.clone()],
        vec![
            element(TimerRef::When(0), 1),
            element(TimerRef::When(1), 2),
            ElementDef {
                visible: Some(reads_when),
                ..element(TimerRef::Builtin(timer::PLAY), 3)
            },
        ],
    )
    .unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    for now in [0, 16] {
        h.frame(now, &[]);
        assert_eq!(h.start(timer::BUILTIN), Some(0), "at {now}");
        assert_eq!(h.start(timer::BUILTIN + 1), None, "at {now}");
        assert_eq!(h.elapsed(timer::BUILTIN + 1), TIMER_OFF, "at {now}");
        let xs: Vec<f32> = h.batch.quads().iter().map(|q| q.rect[0]).collect();
        assert_eq!(xs, [1.0, 3.0], "at {now}");
    }
}

#[test]
fn elements_draw_in_order_with_their_keys_and_visibility() {
    let chart = taps(&[1_000]);
    let mut hidden = element(TimerRef::Builtin(timer::PLAY), 9);
    hidden.visible = Some(bool_program("combo > 0"));
    let added = ElementDef {
        blend: Blend::Add,
        clip: Some(ClipId(3)),
        ..element(TimerRef::Builtin(timer::PLAY), 2)
    };
    let skin = load(
        Vec::new(),
        vec![element(TimerRef::Builtin(timer::READY), 1), hidden, added],
    )
    .unwrap();
    assert_eq!(skin.quad_capacity(), 3);
    assert_eq!(skin.run_capacity(), 3);
    let mut h = Harness::new(skin, &chart, &full(0));
    h.frame(0, &[]);
    let quads = h.batch.quads();
    assert_eq!(quads.len(), 2);
    assert_eq!(quads[0].rect[0], 1.0);
    assert_eq!(quads[1].rect[0], 2.0);
    // The white texel at (8, 8) of a 256-texel page, on layer 1.
    let texel = [8.0 / 256.0, 8.0 / 256.0, 9.0 / 256.0, 9.0 / 256.0];
    assert_eq!(quads[1].uv, texel);
    assert_eq!(quads[1].layer, 1);
    assert_eq!(h.batch.runs().len(), 2);
    assert_eq!(h.batch.runs()[1].key.clip, Some(ClipId(3)));
    h.tally = tally(1);
    h.frame(10, &[]);
    assert_eq!(h.batch.quads().len(), 3);
    // A play from a skin with other timer slots draws nothing.
    let other = load(vec![bool_program("true")], Vec::new()).unwrap();
    h.play = other.prepare(&chart, &full(0)).unwrap();
    h.frame(20, &[]);
    assert!(h.batch.quads().is_empty());
}

fn int_program(src: &str) -> skin_model::Program {
    compile(src, &Scope::new(&PLAY), Ty::Int).unwrap()
}

/// A bool program of exactly `ops` operations, at least 3.
fn sized_program(ops: usize) -> skin_model::Program {
    // A balanced sum of n loads (n − 1 adds), which does not fold; `>= 0`
    // adds a constant and a compare (2n + 1), and `!` one more (2n + 2).
    fn sum(n: usize) -> String {
        if n == 1 {
            "bp".to_owned()
        } else {
            format!("({}+{})", sum(n / 2), sum(n - n / 2))
        }
    }
    let sum = sum((ops - 1) / 2);
    let src = if ops % 2 == 1 {
        format!("{sum} >= 0")
    } else {
        format!("!({sum} >= 0)")
    };
    let p = compile(&src, &Scope::new(&PLAY), Ty::Bool).unwrap();
    assert_eq!(p.op_count(), ops);
    p
}

#[test]
fn load_errors() {
    let el = || element(TimerRef::Builtin(timer::PLAY), 0);
    assert!(matches!(
        load(Vec::new(), vec![el(); MAX_ELEMENTS + 1]),
        Err(LoadError::TooManyElements { n }) if n == MAX_ELEMENTS + 1
    ));
    assert!(matches!(
        load(vec![bool_program("true"); MAX_WHEN_TIMERS + 1], Vec::new()),
        Err(LoadError::TooManyWhenTimers { n }) if n == MAX_WHEN_TIMERS + 1
    ));
    let keys: Vec<_> = (0..256)
        .map(|i| KeyDef {
            at: TimeUs(i),
            props: [None; 9],
        })
        .collect();
    let long = ElementDef {
        track: Track::new(&keys, Ease::Linear, End::Hold).unwrap(),
        ..el()
    };
    let at_limit = vec![long.clone(); MAX_SKIN_KEYS / 256];
    assert!(load(Vec::new(), at_limit.clone()).is_ok());
    let mut over = at_limit;
    over.push(el());
    assert!(matches!(
        load(Vec::new(), over),
        Err(LoadError::TooManyKeys { n }) if n == MAX_SKIN_KEYS + 1
    ));
    assert!(matches!(
        load(vec![bool_program("true"), int_program("combo")], Vec::new()),
        Err(LoadError::WhenNotBool { when: 1 })
    ));
    let not_bool = ElementDef {
        visible: Some(int_program("combo")),
        ..el()
    };
    assert!(matches!(
        load(Vec::new(), vec![el(), not_bool]),
        Err(LoadError::VisibleNotBool { element: 1 })
    ));
    for bad in [TimerRef::Builtin(timer::BUILTIN), TimerRef::When(1)] {
        assert!(matches!(
            load(vec![bool_program("true")], vec![el(), element(bad, 0)]),
            Err(LoadError::UnknownTimer { element: 1 })
        ));
    }
}

#[test]
fn operations_are_charged_once_per_program() {
    // 256 whens of 256 operations fill the skin's budget exactly.
    let whens = vec![sized_program(MAX_OPS); 256];
    assert_eq!(256 * MAX_OPS, skin_model::MAX_SKIN_OPS);
    // Elements sharing `when` 0 charge it once.
    let shared = load(
        whens.clone(),
        vec![element(TimerRef::When(0), 0), element(TimerRef::When(0), 0)],
    )
    .unwrap();
    assert_eq!(shared.ops().total(), skin_model::MAX_SKIN_OPS);
    // One more operation passes it.
    let one = ElementDef {
        visible: Some(bool_program("true")),
        ..element(TimerRef::When(0), 0)
    };
    assert_eq!(one.visible.as_ref().unwrap().op_count(), 1);
    assert!(matches!(
        load(whens.clone(), vec![one.clone()]),
        Err(LoadError::Ops(_))
    ));
    let mut fewer = whens;
    fewer.truncate(255);
    assert_eq!(
        load(fewer, vec![one]).unwrap().ops().total(),
        255 * MAX_OPS + 1
    );
}

/// A snapshot with the counts `[PERFECT, GREAT, GOOD, BAD, POOR]`, `empty`
/// empty POORs and the EX score `ex`.
fn scored(counts: [u32; 5], empty: u32, ex: u32) -> eval::Tally {
    let [perfect, great, good, bad, poor] = counts;
    eval::Tally {
        counts: rules::PerJudgment {
            perfect,
            great,
            good,
            bad,
            poor,
        },
        ex_score: ex,
        empty_poor: empty,
        ..tally(0)
    }
}

/// The two rates as of the last build, in basis points.
fn rates(h: &Harness) -> (i32, i32) {
    (
        h.field(field::RATE_CURRENT_BP),
        h.field(field::RATE_WHOLE_BP),
    )
}

/// The whole-percent and fraction portions an importer maps the rate
/// `field` to, as of the last build.
fn portions(h: &Harness, field: &str) -> (i32, i32) {
    let int = |src: String| {
        compile(&src, &Scope::new(&PLAY), Ty::Int)
            .unwrap()
            .eval(&h.play.state())
    };
    (int(format!("{field} / 100")), int(format!("{field} % 100")))
}

#[test]
fn rates_divide_the_ex_score_by_twice_the_targets() {
    let chart = taps(&[1_000, 2_000, 3_000, 4_000]);
    let mut h = Harness::empty(&chart, &full(0));
    // Before any judgment: the current rate's denominator is empty.
    h.frame(0, &[]);
    assert_eq!(rates(&h), (10_000, 0));
    assert_eq!(portions(&h, "rate.current_bp"), (100, 0));
    assert_eq!(portions(&h, "rate.whole_bp"), (0, 0));
    // EX 3 over 2 judged of 4 targets.
    h.tally = scored([1, 1, 0, 0, 0], 0, 3);
    h.frame(2_000, &[]);
    assert_eq!(rates(&h), (7_500, 3_750));
    assert_eq!(portions(&h, "rate.current_bp"), (75, 0));
    assert_eq!(portions(&h, "rate.whole_bp"), (37, 50));
    // BP, combo and the empty POORs alone judge no target.
    h.tally = eval::Tally {
        bp: 7,
        combo: 5,
        max_combo: 5,
        ..scored([0; 5], 3, 0)
    };
    h.frame(2_100, &[]);
    assert_eq!(rates(&h), (10_000, 0));
    // No target at all: both denominators are empty.
    let none = ChartBuilder::new(BPM_150)
        .mine(lane(4), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    let mut h = Harness::empty(&none, &full(0));
    h.frame(0, &[]);
    assert_eq!(rates(&h), (10_000, 10_000));
}

#[test]
fn a_rate_floors_one_basis_point_value() {
    // 100 taps.
    let times: Vec<i64> = (1..=100).map(|i| i * 100).collect();
    let mut h = Harness::empty(&taps(&times), &full(0));
    h.tally = scored([39, 0, 0, 0, 61], 0, 39);
    h.frame(0, &[]);
    assert_eq!(rates(&h), (1_950, 1_950));
    assert_eq!(portions(&h, "rate.current_bp"), (19, 50));
    assert_eq!(portions(&h, "rate.whole_bp"), (19, 50));
    // The source rounds each portion from its own float product
    // (`ScoreDataProperty.java:94-99`) and shows 19.49 here; the model's
    // portions come from the one floored value.
    let source = 39.0_f32 / 200.0;
    #[expect(clippy::cast_possible_truncation, reason = "the source's casts")]
    let source = ((source * 100.0) as i32, ((source * 10_000.0) as i32) % 100);
    assert_eq!(source, (19, 49));
}

/// The point as of the last build, after a frame with `PERFECT`, `GREAT`
/// and `GOOD` counts `pg`, `gr`, `gd` and the maximum combo `max` over
/// `h`'s chart.
fn point_after(h: &mut Harness, [pg, gr, gd]: [u32; 3], max: u32) -> i32 {
    h.tally = eval::Tally {
        max_combo: max,
        ..scored([pg, gr, gd, 0, 0], 0, 0)
    };
    h.frame(0, &[]);
    h.field(field::POINT)
}

/// The source's 7-key point with its intermediate widths
/// (`ScoreDataProperty.java:76-79` at `0.8.9`); each expected value was
/// computed by that expression in Java.
#[test]
fn the_point_follows_the_source_s_widths() {
    let chart = |n: i64| taps(&(1..=n).map(|i| i * 10).collect::<Vec<_>>());
    let mut four = Harness::empty(&chart(4), &full(0));
    // Every target PERFECT, the maximum combo all of them.
    assert_eq!(point_after(&mut four, [4, 0, 0], 4), 200_000);
    // A broken current combo with a kept maximum: the maximum counts.
    four.tally = eval::Tally {
        combo: 1,
        max_combo: 3,
        ..scored([4, 0, 0, 0, 0], 0, 0)
    };
    four.frame(0, &[]);
    assert_eq!(four.field(field::POINT), 187_500);
    let mut seven = Harness::empty(&chart(7), &full(0));
    assert_eq!(point_after(&mut seven, [1, 1, 1], 1), 45_713);
    // GREAT's product wraps as an `int`: the r2 witness.
    let mut many = Harness::empty(&chart(22_000), &full(0));
    assert_eq!(point_after(&mut many, [0, 22_000, 0], 22_000), -45_225);
    // GOOD's product wraps as an `int`.
    assert_eq!(point_after(&mut many, [0, 0, 107_375], 0), -97_612);
    // The PERFECT sum is a `long` whose quotient keeps its low 32 bits,
    // and the last addition wraps.
    let mut one = Harness::empty(&chart(1), &full(0));
    assert_eq!(point_after(&mut one, [14_316, 0, 0], 2), -2_147_467_296);
    // The PERFECT quotient past `i32` keeps its low 32 bits.
    assert_eq!(point_after(&mut one, [14_317, 0, 0], 0), -2_147_417_296);
    // The maximum combo's quotient keeps its low 32 bits.
    assert_eq!(point_after(&mut one, [0, 0, 0], 50_000), -1_794_967_296);
    // No target: 0, whatever the counts.
    let none = ChartBuilder::new(BPM_150)
        .mine(lane(4), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    let mut none = Harness::empty(&none, &full(0));
    assert_eq!(point_after(&mut none, [3, 2, 1], 3), 0);
}

/// During section practice the point is over the section's targets, as
/// the source's practice counts only its section's notes
/// (`pattern/PracticeModifier.java:30-40`, `play/JudgeManager.java:135`).
#[test]
fn a_sections_point_is_over_its_targets() {
    // Four taps, two of them in the section.
    let chart = taps(&[1_000, 31_000, 32_000, 61_000]);
    let s = section(30_000, 60_000, 1_000);
    let mut h = Harness::empty(&chart, &in_section(&s));
    assert_eq!(point_after(&mut h, [2, 0, 0], 2), 200_000);
    assert_eq!(h.field(field::NOTES), 2);
}

#[test]
fn a_rate_is_not_capped_and_never_overflows() {
    // A custom EX weighting: 3 for one target.
    let mut h = Harness::empty(&taps(&[1_000]), &full(0));
    h.tally = scored([1, 0, 0, 0, 0], 0, 3);
    h.frame(0, &[]);
    assert_eq!(rates(&h), (15_000, 15_000));
    assert_eq!(portions(&h, "rate.current_bp"), (150, 0));
    // The largest counts: 10 000 · ex / (2 · 5 · (2^32 − 1)).
    h.tally = scored([u32::MAX; 5], 0, u32::MAX);
    h.frame(16, &[]);
    assert_eq!(rates(&h), (1_000, i32::MAX));
    // One judged target and the largest EX: the field's bound.
    h.tally = scored([1, 0, 0, 0, 0], 0, u32::MAX);
    h.frame(32, &[]);
    assert_eq!(rates(&h), (i32::MAX, i32::MAX));
    assert_eq!(portions(&h, "rate.current_bp"), (i32::MAX / 100, 47));
}

#[test]
fn the_empty_poor_policy_leaves_the_judged_targets_alone() {
    // A PERFECT at 1 s, an empty press at 1.6 s and a missed tap at 2 s,
    // evaluated with empty POORs counted and not.
    let chart = taps(&[1_000, 2_000]);
    let device = eval::DeviceProfile {
        sources: vec![eval::SourceProfile {
            input_offset: TimeUs(0),
            bindings: vec![eval::Binding {
                control: eval::ControlId(1),
                lane: lane(1),
            }],
            axes: Vec::new(),
        }],
    };
    let key = |ms: i64, value: i32| eval::PhysicalInputEvent {
        received: TimeUs(ms * MS),
        source: eval::SourceId(0),
        kind: eval::InputKind::Control {
            control: eval::ControlId(1),
            value,
        },
    };
    let mut seen = Vec::new();
    for counted in [false, true] {
        let mut preset = rules::tapestry();
        preset.empty_poor.counted = counted;
        let evaluation = eval::evaluate(
            &chart,
            &EvalInput {
                events: vec![key(1_000, 1), key(1_050, 0), key(1_600, 1), key(1_650, 0)],
                device: device.clone(),
                ..EvalInput::default()
            },
            &preset,
        )
        .unwrap();
        let t = evaluation.tally;
        assert_eq!(t.empty_poor, 1, "counted {counted}");
        assert_eq!(t.counts.poor, 1 + u32::from(counted));
        let ctx = PlayContext {
            empty_poor_counted: counted,
            ..full(0)
        };
        let mut h = Harness::empty(&chart, &ctx);
        h.tally = t;
        h.frame(3_000, &[]);
        seen.push((t.ex_score, rates(&h)));
    }
    assert_eq!(seen[0], seen[1]);
    let (ex, (current, whole)) = seen[0];
    let want = i32::try_from(10_000 * u64::from(ex) / 4).unwrap();
    assert_eq!((current, whole), (want, want));
}

#[test]
fn the_rates_read_the_snapshot_in_the_same_frame() {
    let chart = taps(&[1_000, 2_000]);
    let skin = load(
        vec![bool_program(
            "rate.current_bp >= 5000 && rate.whole_bp < 5000",
        )],
        vec![element(TimerRef::When(0), 1)],
    )
    .unwrap();
    let mut h = Harness::new(skin, &chart, &full(0));
    // The snapshot is ahead of the drain: no effect carries the judgment.
    h.tally = scored([0, 1, 0, 0, 0], 0, 1);
    h.frame(1_000, &[]);
    assert_eq!(rates(&h), (5_000, 2_500));
    assert_eq!(h.start(timer::BUILTIN), Some(1_000 * MS));
    assert_eq!(h.batch.quads().len(), 1);
}

#[test]
fn the_attempt_targets_are_prepared_per_attempt() {
    let chart = practice_chart();
    let mut h = Harness::empty(&chart, &full(0));
    h.tally = scored([1, 0, 0, 0, 0], 0, 2);
    h.frame(0, &[]);
    // Five taps and two long notes; the mine is no target.
    assert_eq!(rates(&h), (10_000, 1_428));
    // A round from 10 s to 20 s: the taps at 10 s and 15 s and the long
    // note from 16 s; the tap at its end and the long note spanning its
    // start are not targets.
    let s = section(10_000, 20_000, 2_000);
    h.play = h.skin.prepare(&chart, &in_section(&s)).unwrap();
    h.frame(0, &[]);
    assert_eq!(h.field(field::NOTES), 3);
    assert_eq!(rates(&h), (10_000, 3_333));
}

#[test]
fn an_inconsistent_snapshot_poisons_the_play() {
    use skin_model::{BuildError, LaneTimer, lane_timer};
    let chart = ChartBuilder::new(BPM_150)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    let skin = || {
        load(
            Vec::new(),
            vec![element(TimerRef::Builtin(timer::READY), 1)],
        )
        .unwrap()
    };
    let ctx = PlayContext {
        empty_poor_counted: true,
        ..full(0)
    };
    let hold = lane_timer(LaneTimer::Hold, lane(2));
    let mut h = Harness::new(skin(), &chart, &ctx);
    h.frame(
        1_000,
        &[ev(1_000, play_support::ln_held(2, 0, Judgment::Great))],
    );
    assert_eq!(h.start(hold), Some(1_000 * MS));
    let state = |h: &Harness| {
        let s = h.play.state();
        (s.fields.to_vec(), s.timers.to_vec())
    };
    let before = state(&h);
    let quads = h.batch.quads().to_vec();
    assert_eq!(quads.len(), 1);
    let frame = |h: &mut Harness, now: i64, effects: &[eval::JudgeEvent]| {
        let tally = h.tally;
        let layout = h.layout;
        let f = skin_model::SkinFrame {
            state_us: 0,
            now: TimeUs(now * MS),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &h.gauges,
            gauge: 0,
            effects,
            notes: &h.notes,
            input: &eval::InputFacts::NONE,
            complete: false,
            playing: true,
            closing: None,
            target: h.target,
            layout: &layout,
            lanes: h.view,
            readouts: play_support::readouts_for(h.view),
        };
        // Not cleared: a refused build appends nothing.
        h.skin.build(&mut h.play, &f, &h.uvs, &mut h.batch)
    };
    // One more empty POOR counted than POORs: refused before the effects.
    h.tally = scored([0, 1, 0, 0, 1], 2, 2);
    let ended = [ev(1_100, play_support::hold_ended(2, 0))];
    assert_eq!(
        frame(&mut h, 1_100, &ended),
        Err(BuildError::InconsistentTally)
    );
    assert_eq!(state(&h), before);
    assert_eq!(h.batch.quads(), &quads[..]);
    // A valid snapshot after it is refused too: the end it carries is not
    // applied, so the hold would otherwise survive it.
    h.tally = scored([0, 1, 0, 0, 2], 2, 2);
    assert_eq!(
        frame(&mut h, 1_150, &ended),
        Err(BuildError::InconsistentTally)
    );
    assert_eq!(state(&h), before);
    assert_eq!(h.start(hold), Some(1_000 * MS));
    assert_eq!(h.batch.quads(), &quads[..]);
    // As many empty POORs as POORs is consistent; a fresh prepare draws.
    h.play = h.skin.prepare(&chart, &ctx).unwrap();
    h.tally = scored([0, 1, 0, 0, 2], 2, 2);
    h.batch.clear();
    assert_eq!(frame(&mut h, 1_200, &[]), Ok(()));
    assert_eq!(h.batch.quads().len(), 1);
    assert_eq!(rates(&h), (10_000, 10_000));
}

// ---- Play readouts (stage3.md R3, R4, R6) ----

/// Every cover off at 0.
const COVERS_OFF: CoverSettings = CoverSettings {
    cover: 0,
    lift: 0,
    hidden: 0,
    cover_enabled: false,
    lift_enabled: false,
    hidden_enabled: false,
    adjusting: false,
};

/// Nothing known, every cover off at 0.
const UNKNOWN: FrameReadouts = FrameReadouts {
    covers: COVERS_OFF,
    ..FrameReadouts::UNAVAILABLE
};

fn ratio(numerator: u128, denominator: u128) -> Option<DurationRatio> {
    Some(DurationRatio {
        numerator,
        denominator,
    })
}

/// The readout fields, `bpm.current_x100` to `duration.valid`.
fn readout_fields(h: &Harness) -> &[i32] {
    let fields = h.play.state().fields;
    &fields[usize::from(field::BPM_CURRENT_X100)..=usize::from(field::DURATION_VALID)]
}

/// `time.remaining_s` and `time.elapsed_s`.
fn clocks(h: &Harness) -> (i32, i32) {
    (
        h.field(field::TIME_REMAINING_S),
        h.field(field::TIME_ELAPSED_S),
    )
}

/// The remaining time counts to the last resolved object: a background
/// sound's start past the last note, a mine's time, a long note's end;
/// completion changes nothing.
#[test]
fn the_remaining_time_counts_to_the_last_object() {
    let chart = ChartBuilder::new(BPM_150)
        .sound("bgm.wav")
        .tap(lane(1), TimeUs(10_000 * MS))
        .bgm(TimeUs(17_000 * MS), bms_model::SoundId(0))
        .build()
        .unwrap();
    let mut h = Harness::empty(&chart, &full(0));
    h.readouts = Some(UNKNOWN);
    h.frame(0, &[]);
    assert_eq!(clocks(&h), (17, 0));
    h.frame(10_500, &[]);
    assert_eq!(clocks(&h), (7, 10));
    h.complete = true;
    h.frame(16_000, &[]);
    assert_eq!(clocks(&h), (1, 16));
    // Past the end: none remains, and the elapsed time goes on.
    h.frame(30_000, &[]);
    assert_eq!(clocks(&h), (0, 30));
    for (chart, end_s) in [
        (
            ChartBuilder::new(BPM_150)
                .tap(lane(1), TimeUs(3_000 * MS))
                .mine(lane(2), TimeUs(9_000 * MS), 10)
                .build()
                .unwrap(),
            9,
        ),
        (
            ChartBuilder::new(BPM_150)
                .long(lane(2), TimeUs(1_000 * MS), TimeUs(12_000 * MS))
                .tap(lane(1), TimeUs(5_000 * MS))
                .build()
                .unwrap(),
            12,
        ),
        (ChartBuilder::new(BPM_150).build().unwrap(), 0),
    ] {
        let mut h = Harness::empty(&chart, &full(0));
        h.frame(0, &[]);
        assert_eq!(clocks(&h), (end_s, 0));
    }
}

/// Whole remaining seconds round up, elapsed ones down; at the end none
/// remains.
#[test]
fn the_clocks_round_at_whole_seconds() {
    let chart = taps(&[17_000]);
    let mut h = Harness::empty(&chart, &full(0));
    let end = 17_000 * MS;
    for (left_us, want) in [(0, 0), (1, 1), (999_999, 1), (1_000_000, 1), (1_000_001, 2)] {
        h.frame_us(end - left_us, &[]);
        assert_eq!(h.field(field::TIME_REMAINING_S), want, "{left_us} µs left");
    }
    for (now_us, want) in [(999_999, 0), (1_000_000, 1), (1_999_999, 1)] {
        h.frame_us(now_us, &[]);
        assert_eq!(h.field(field::TIME_ELAPSED_S), want, "{now_us} µs");
    }
}

/// READY freezes both clocks at the attempt's start, the display
/// offset never moves them, and a section counts its own real-time length
/// from its mapped start to its mapped end, reset per round.
#[test]
fn the_clocks_follow_the_attempt_not_the_display() {
    let chart = practice_chart();
    let mut h = Harness::empty(&chart, &full(3_000));
    h.frame(-2_000, &[]);
    assert_eq!(clocks(&h), (25, 0));
    h.offset = TimeUs(-400 * MS);
    h.frame(2_000, &[]);
    assert_eq!(clocks(&h), (23, 2));
    // [10 s, 20 s) at 50 % is [20 s, 40 s) in real time.
    let s = section(20_000, 40_000, 2_000);
    for _round in 0..2 {
        h.play = h.skin.prepare(&chart, &in_section(&s)).unwrap();
        h.offset = TimeUs(0);
        h.frame(18_500, &[]);
        assert_eq!(clocks(&h), (20, 0));
        h.offset = TimeUs(700 * MS);
        h.frame(20_000, &[]);
        assert_eq!(clocks(&h), (20, 0));
        h.frame(29_500, &[]);
        assert_eq!(clocks(&h), (11, 9));
        h.frame(45_000, &[]);
        assert_eq!(clocks(&h), (0, 25));
    }
    // An empty section, its end at its start, has nothing left.
    let empty = section(5_000, 5_000, 0);
    h.play = h.skin.prepare(&chart, &in_section(&empty)).unwrap();
    h.frame(5_000, &[]);
    assert_eq!(clocks(&h), (0, 0));
}

/// The progress (stage3.md R3, `skin/property/FloatPropertyFactory.java:
/// 128-136`) runs over the remaining time's own span, from the attempt's
/// start to the last resolved object, a background sound past the last
/// note here: 0 in READY and at the start, the share rounded down in
/// between, the whole at and after the end; neither the display offset nor
/// completion moves it.
#[test]
fn the_progress_runs_to_the_last_object() {
    let chart = ChartBuilder::new(BPM_150)
        .sound("bgm.wav")
        .tap(lane(1), TimeUs(10_000 * MS))
        .bgm(TimeUs(16_000 * MS), bms_model::SoundId(0))
        .build()
        .unwrap();
    let mut h = Harness::empty(&chart, &full(2_000));
    let progress = |h: &Harness| h.field(field::TIME_PROGRESS_PPM);
    for (now_us, want) in [
        (-2_000 * MS, 0),
        (-1, 0),
        (0, 0),
        (15, 0),
        (16, 1),
        (4_000 * MS, 250_000),
        (8_000 * MS + 7, 500_000),
        (16_000 * MS - 1, 999_999),
        (16_000 * MS, 1_000_000),
        (30_000 * MS, 1_000_000),
    ] {
        h.frame_us(now_us, &[]);
        assert_eq!(progress(&h), want, "{now_us} µs");
    }
    h.offset = TimeUs(-700 * MS);
    h.complete = true;
    h.frame(12_000, &[]);
    assert_eq!(progress(&h), 750_000);
}

/// A section's progress runs over its real-time span, so a practice speed
/// scales the time it takes, not the share: [10 s, 20 s) at 50 % is
/// [20 s, 40 s), halfway at 30 s. READY reads 0, each round starts over,
/// and an empty section is whole from its start.
#[test]
fn a_sections_progress_runs_over_its_real_time() {
    let chart = practice_chart();
    let s = section(20_000, 40_000, 2_000);
    let mut h = Harness::empty(&chart, &full(0));
    let progress = |h: &Harness| h.field(field::TIME_PROGRESS_PPM);
    for _round in 0..2 {
        h.play = h.skin.prepare(&chart, &in_section(&s)).unwrap();
        for (now_ms, want) in [
            (18_500, 0),
            (20_000, 0),
            (25_000, 250_000),
            (30_000, 500_000),
            (40_000, 1_000_000),
            (45_000, 1_000_000),
        ] {
            h.frame(now_ms, &[]);
            assert_eq!(progress(&h), want, "{now_ms} ms");
        }
    }
    let empty = section(5_000, 5_000, 1_000);
    h.play = h.skin.prepare(&chart, &in_section(&empty)).unwrap();
    h.frame(4_500, &[]);
    assert_eq!(progress(&h), 0);
    h.frame(5_000, &[]);
    assert_eq!(progress(&h), 1_000_000);
}

/// A section ending before its start, or a BPM range out of order or
/// past the readout maximum, is refused after the existing checks.
#[test]
fn prepare_refuses_inverted_ranges() {
    let chart = taps(&[1_000]);
    let skin = load(Vec::new(), Vec::new()).unwrap();
    let inverted = section(5_000, 4_999, 0);
    assert_eq!(
        skin.prepare(&chart, &in_section(&inverted)).err(),
        Some(PrepareError::InvalidSectionRange)
    );
    // The lead-in check comes first.
    let both = section(5_000, 4_999, -1);
    assert_eq!(
        skin.prepare(&chart, &in_section(&both)).err(),
        Some(PrepareError::NegativeLeadIn)
    );
    let top = READOUT_MAX.unsigned_abs();
    let with = |min_x100, max_x100| PlayContext {
        bpm_range: Some(BpmRange { min_x100, max_x100 }),
        ..full(0)
    };
    for (min, max, ok) in [
        (15_000, 15_000, true),
        (0, top, true),
        (15_001, 15_000, false),
        (0, top + 1, false),
        (top + 1, top + 1, false),
    ] {
        let got = skin.prepare(&chart, &with(min, max)).err();
        let want = (!ok).then_some(PrepareError::InvalidBpmRange);
        assert_eq!(got, want, "{min}..{max}");
    }
    // The section's order is checked before the range's.
    let ctx = PlayContext {
        bpm_range: Some(BpmRange {
            min_x100: 2,
            max_x100: 1,
        }),
        ..in_section(&inverted)
    };
    assert_eq!(
        skin.prepare(&chart, &ctx).err(),
        Some(PrepareError::InvalidSectionRange)
    );
}

/// Supplied values show as given from the first frame, missing ones read
/// 0 and invalid, a present 0 stays valid, and they change without any
/// effect, a STOP included.
#[test]
fn readouts_show_what_the_frame_supplies() {
    let chart = ChartBuilder::new(BPM_150)
        .tap(lane(1), TimeUs(1_000 * MS))
        .stop(TimeUs(2_000 * MS), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let ctx = PlayContext {
        bpm_range: Some(BpmRange {
            min_x100: 7_500,
            max_x100: 30_000,
        }),
        ..full(0)
    };
    let mut h = Harness::empty(&chart, &ctx);
    // The 600 ms visible duration at the initial tempo: green 360.
    h.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(15_000),
        hispeed_x100: Some(105),
        duration: ratio(600, 1),
        covers: COVERS_OFF,
        ..UNKNOWN
    });
    h.frame(0, &[]);
    #[rustfmt::skip]
    assert_eq!(
        readout_fields(&h),
        [15_000, 7_500, 30_000, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 105, 1, 600, 360, 1]
    );
    // Within the STOP the tempo and duration stay as supplied; a change
    // within it shows at once.
    h.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(0),
        hispeed_x100: Some(0),
        duration: ratio(0, 1),
        covers: COVERS_OFF,
        ..UNKNOWN
    });
    h.frame(2_500, &[]);
    assert_eq!(h.field(field::BPM_CURRENT_X100), 0);
    assert_eq!(h.field(field::BPM_CURRENT_VALID), 1);
    assert_eq!(h.field(field::HISPEED_VALID), 1);
    assert_eq!(h.field(field::DURATION_VALID), 1);
    // A duration without a hi-speed: the hi-speed is invalid.
    h.readouts = Some(FrameReadouts {
        hispeed_x100: None,
        duration: ratio(6_675, 10),
        ..UNKNOWN
    });
    h.frame(2_600, &[]);
    assert_eq!(h.field(field::HISPEED_X100), 0);
    assert_eq!(h.field(field::HISPEED_VALID), 0);
    assert_eq!(h.field(field::BPM_CURRENT_VALID), 0);
    assert_eq!(
        (
            h.field(field::DURATION_MS),
            h.field(field::DURATION_GREEN),
            h.field(field::DURATION_VALID)
        ),
        (668, 400, 1)
    );
    // Without a range, both bounds read 0 and invalid; the helper's
    // covers are on at 0.
    let mut h = Harness::empty(&chart, &full(0));
    h.frame(0, &[]);
    #[rustfmt::skip]
    assert_eq!(
        readout_fields(&h),
        [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 0]
    );
}

/// The duration rounds half up exactly before the green number takes
/// three fifths of it; the largest rounded duration is accepted.
#[test]
fn the_duration_rounds_half_up_before_the_green_number() {
    let chart = taps(&[1_000]);
    let mut h = Harness::empty(&chart, &full(0));
    let top = READOUT_MAX.unsigned_abs();
    for (n, d, ms, green) in [
        (6_675, 10, 668, 400),
        (6_674_999, 10_000, 667, 400),
        (3, 2, 2, 1),
        (4, 3, 1, 0),
        (5, 3, 2, 1),
        (u128::MAX, u128::MAX, 1, 0),
        (1, u128::MAX, 0, 0),
        (u128::MAX / 2, u128::MAX, 0, 0),
        (u128::MAX / 2 + 1, u128::MAX, 1, 0),
        (u128::from(top) * 2 - 1, 2, READOUT_MAX, GREEN_MAX),
    ] {
        h.readouts = Some(FrameReadouts {
            duration: ratio(n, d),
            ..UNKNOWN
        });
        h.frame(0, &[]);
        assert_eq!(
            (h.field(field::DURATION_MS), h.field(field::DURATION_GREEN)),
            (ms, green),
            "{n}/{d}"
        );
    }
}

/// Configured amounts show as stored whatever their switches, the
/// absolute cover from the stored amounts; a switch on at 0 reads on.
#[test]
fn configured_covers_show_as_stored() {
    let chart = taps(&[1_000]);
    let mut h = Harness::empty(&chart, &full(0));
    let covers = |h: &Harness| {
        [
            field::COVER_PERMILLE,
            field::LIFT_PERMILLE,
            field::HIDDEN_PERMILLE,
            field::COVER_ABSOLUTE_PERMILLE,
            field::COVER_ENABLED,
            field::LIFT_ENABLED,
            field::HIDDEN_ENABLED,
            field::COVER_ADJUSTING,
        ]
        .map(|f| h.field(f))
    };
    // Both off, the lanes drawing none of them.
    h.readouts = Some(FrameReadouts {
        covers: CoverSettings {
            cover: 500,
            lift: 250,
            ..COVERS_OFF
        },
        ..UNKNOWN
    });
    h.frame(0, &[]);
    assert_eq!(covers(&h), [500, 250, 0, 375, 0, 0, 0, 0]);
    // On at 0, and the adjustment mode.
    h.readouts = Some(FrameReadouts {
        covers: CoverSettings {
            hidden_enabled: true,
            adjusting: true,
            ..COVERS_OFF
        },
        ..UNKNOWN
    });
    h.frame(16, &[]);
    assert_eq!(covers(&h), [0, 0, 0, 0, 0, 0, 1, 1]);
    // Everything full: the absolute cover is 0 under a full lift.
    h.view = LaneView {
        cover: 1_000,
        lift: 1_000,
        hidden: 1_000,
        ..h.view
    };
    h.readouts = None;
    h.frame(32, &[]);
    assert_eq!(covers(&h), [1_000, 1_000, 1_000, 0, 1, 1, 1, 0]);
    // A lanes amount past 1 000 draws as 1 000, so it matches.
    h.view = LaneView {
        cover: 1_500,
        lift: 0,
        hidden: 0,
        ..h.view
    };
    h.readouts = Some(FrameReadouts {
        covers: CoverSettings {
            cover: 1_000,
            cover_enabled: true,
            ..COVERS_OFF
        },
        ..UNKNOWN
    });
    h.frame(48, &[]);
    assert_eq!(covers(&h), [1_000, 0, 0, 1_000, 1, 0, 0, 0]);
}

/// Each invalid readout, and each stored setting that is not what the
/// lanes draw.
fn invalid_readouts() -> Vec<(FrameReadouts, LaneView)> {
    let top = READOUT_MAX.unsigned_abs();
    let on = |cover, lift, hidden| CoverSettings {
        cover,
        lift,
        hidden,
        cover_enabled: true,
        lift_enabled: true,
        hidden_enabled: true,
        adjusting: false,
    };
    let stored = |cover, lift, hidden| CoverSettings {
        cover,
        lift,
        hidden,
        ..COVERS_OFF
    };
    let view = |cover, lift, hidden| LaneView {
        cover,
        lift,
        hidden,
        ..play_support::VIEW
    };
    let none = view(0, 0, 0);
    let with = |covers| FrameReadouts { covers, ..UNKNOWN };
    vec![
        (
            FrameReadouts {
                bpm_current_x100: Some(top + 1),
                ..UNKNOWN
            },
            none,
        ),
        (
            FrameReadouts {
                hispeed_x100: Some(top + 1),
                ..UNKNOWN
            },
            none,
        ),
        (
            FrameReadouts {
                duration: ratio(1, 0),
                ..UNKNOWN
            },
            none,
        ),
        (
            FrameReadouts {
                duration: ratio(u128::from(top) * 2 + 1, 2),
                ..UNKNOWN
            },
            none,
        ),
        (
            FrameReadouts {
                duration: ratio(u128::MAX, 1),
                ..UNKNOWN
            },
            none,
        ),
        (with(stored(1_001, 0, 0)), none),
        (with(stored(0, 1_001, 0)), none),
        (with(stored(0, 0, 1_001)), none),
        (with(on(300, 0, 0)), view(301, 0, 0)),
        (with(on(0, 300, 0)), view(0, 0, 0)),
        (with(on(0, 0, 999)), view(0, 0, 1_000)),
        (with(stored(0, 0, 300)), view(0, 0, 300)),
        (with(stored(300, 0, 0)), view(300, 0, 0)),
        (with(stored(0, 300, 0)), view(0, 300, 0)),
        // A variant: a zero denominator, then ms past the maximum
        // (`2·top + 1` halves round to `top + 1`) and far past it.
        (variant(7, ratio(1, 0)), none),
        (variant(0, ratio(u128::from(top) * 2 + 1, 2)), none),
        (variant(3, ratio(u128::MAX, 1)), none),
        // The native block: a zero denominator, its rounding past the
        // maximum, a target outside 1 to 6 000 or outside Floating.
        (native(ratio(1, 0), None, false), none),
        (native(ratio(u128::from(top) * 2 + 1, 2), None, false), none),
        (native(None, Some(0), true), none),
        (native(None, Some(6_001), true), none),
        (native(None, Some(300), false), none),
    ]
}

/// [`UNKNOWN`] with variant `i` set to `d`.
fn variant(i: usize, d: Option<DurationRatio>) -> FrameReadouts {
    let mut r = UNKNOWN;
    r.duration_variants[i] = d;
    r
}

/// [`UNKNOWN`] with the native block's inputs.
fn native(
    duration: Option<DurationRatio>,
    target_green: Option<u32>,
    floating: bool,
) -> FrameReadouts {
    FrameReadouts {
        native: NativeReadouts {
            duration,
            target_green,
            floating,
        },
        ..UNKNOWN
    }
}

/// The fields of the eight variants, slots 51 to 74, and the native
/// block, 75 to 85.
fn appended_fields(h: &Harness) -> (&[i32], &[i32]) {
    let fields = h.play.state().fields;
    let first = usize::from(field::DURATION_VARIANTS);
    let native = usize::from(field::NATIVE_HISPEED_X100);
    let end = usize::from(field::NATIVE_FLOATING) + 1;
    (&fields[first..native], &fields[native..end])
}

/// Each variant rounds its ms half up and its green number half up from
/// the exact ratio × 3/5, beside its own validity; the native block
/// rounds `H` from the gain, `T` and `3T/5` the same way, and gives the
/// whites from the stored covers over the effective lift. 502.5 ms is 503
/// in every order, its green 301.5 → 302 here and ⌊3 · 503 / 5⌋ = 301 in
/// slot 35 (design.md "Play readouts").
#[test]
fn variants_round_half_up_from_the_exact_ratio() {
    let chart = taps(&[1_000]);
    let mut h = Harness::new(load(Vec::new(), Vec::new()).unwrap(), &chart, &full(0));
    let half = ratio(1_005, 2);
    let mut variants = [None; 8];
    // 502.5 ms (green 301.5), 937.5 ms (562.5), 1/3 ms (1/5), 0, two
    // unknown, 2 000 000 ms exactly (1 200 000), 5/6 ms (1/2).
    for (v, d) in variants.iter_mut().zip([
        half,
        ratio(1_875, 2),
        ratio(1, 3),
        ratio(0, 1),
        None,
        None,
        ratio(2_000_000, 1),
        ratio(5, 6),
    ]) {
        *v = d;
    }
    h.view = LaneView {
        gain: skin_model::Gain::new(16, 5),
        cover: 0,
        lift: 300,
        hidden: 0,
    };
    h.readouts = Some(FrameReadouts {
        duration: half,
        covers: CoverSettings {
            cover: 200,
            lift: 300,
            hidden: 500,
            cover_enabled: false,
            lift_enabled: true,
            hidden_enabled: false,
            adjusting: false,
        },
        duration_variants: variants,
        native: NativeReadouts {
            duration: half,
            target_green: Some(300),
            floating: true,
        },
        ..UNKNOWN
    });
    h.frame(0, &[]);
    let (variants, native) = appended_fields(&h);
    #[rustfmt::skip]
    assert_eq!(
        variants,
        [
            503, 302, 1,
            938, 563, 1,
            0, 0, 1,
            0, 0, 1,
            0, 0, 0,
            0, 0, 0,
            2_000_000, 1_200_000, 1,
            1, 1, 1,
        ]
    );
    // H 3.20; 503 and 302; target 300; whites ⌊700 · 200 / 1 000⌋ = 140,
    // the configured 300, ⌊700 · 500 / 1 000⌋ = 350; Floating.
    assert_eq!(native, [320, 1, 503, 302, 1, 300, 1, 140, 300, 350, 1]);
    // Slots 34 and 35 keep the source's order: 503, then ⌊1509 / 5⌋.
    assert_eq!(
        (h.field(field::DURATION_MS), h.field(field::DURATION_GREEN)),
        (503, 301)
    );
    // No gain, no native duration, Classic: those read 0 and invalid,
    // the whites stay.
    h.view.gain = None;
    h.readouts = Some(FrameReadouts {
        covers: CoverSettings {
            cover: 200,
            lift: 300,
            hidden: 500,
            cover_enabled: false,
            lift_enabled: true,
            hidden_enabled: false,
            adjusting: false,
        },
        ..UNKNOWN
    });
    h.frame(10, &[]);
    let (variants, native) = appended_fields(&h);
    assert!(variants.iter().all(|&v| v == 0));
    assert_eq!(native, [0, 0, 0, 0, 0, 0, 0, 140, 300, 350, 0]);
}

/// An invalid readout refuses the frame before its effects, after the
/// tally's check, and poisons the play with that error until a prepare.
#[test]
fn invalid_readouts_poison_the_play() {
    use skin_model::{BuildError, LaneTimer, lane_timer};
    let chart = ChartBuilder::new(BPM_150)
        .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
        .build()
        .unwrap();
    let hold = lane_timer(LaneTimer::Hold, lane(2));
    let skin = load(
        Vec::new(),
        vec![element(TimerRef::Builtin(timer::READY), 1)],
    )
    .unwrap();
    let ctx = PlayContext {
        empty_poor_counted: true,
        ..full(0)
    };
    let state = |h: &Harness| {
        let s = h.play.state();
        (s.fields.to_vec(), s.timers.to_vec())
    };
    for (i, (bad, bad_view)) in invalid_readouts().into_iter().enumerate() {
        // An inconsistent tally is reported first.
        let mut first = Harness::new(skin.clone(), &chart, &ctx);
        first.tally = scored([0, 0, 0, 0, 0], 1, 0);
        first.readouts = Some(bad);
        first.view = bad_view;
        assert_eq!(
            first.try_frame_us(0, &[]),
            Err(BuildError::InconsistentTally),
            "case {i}"
        );
        let mut h = Harness::new(skin.clone(), &chart, &ctx);
        h.frame(
            1_000,
            &[ev(1_000, play_support::ln_held(2, 0, Judgment::Great))],
        );
        assert_eq!(h.start(hold), Some(1_000 * MS));
        let before = state(&h);
        let ended = [ev(1_100, play_support::hold_ended(2, 0))];
        h.readouts = Some(bad);
        h.view = bad_view;
        assert_eq!(
            h.try_frame_us(1_100 * MS, &ended),
            Err(BuildError::InvalidReadouts),
            "case {i}"
        );
        assert_eq!(state(&h), before, "case {i}");
        assert_eq!(h.start(hold), Some(1_000 * MS));
        // Valid readouts after it are refused with the same error, and a
        // fresh prepare builds.
        h.readouts = None;
        h.view = play_support::VIEW;
        assert_eq!(
            h.try_frame_us(1_200 * MS, &ended),
            Err(BuildError::InvalidReadouts)
        );
        assert_eq!(state(&h), before, "case {i}");
        h.play = h.skin.prepare(&chart, &ctx).unwrap();
        assert_eq!(h.try_frame_us(1_200 * MS, &[]), Ok(()));
    }
}

/// The clips a skin draws under: one past the highest clip an element
/// names, and none without one.
#[test]
fn a_skin_counts_the_clips_its_elements_name() {
    let clipped = |id: Option<u16>| ElementDef {
        clip: id.map(ClipId),
        ..element(TimerRef::Builtin(timer::READY), 0)
    };
    let skin = load(Vec::new(), vec![clipped(None)]).unwrap();
    assert_eq!(skin.clip_count(), 0);
    let skin = load(
        Vec::new(),
        vec![clipped(Some(2)), clipped(None), clipped(Some(0))],
    )
    .unwrap();
    assert_eq!(skin.clip_count(), 3);
}

/// An attempt's state counts what it copies of the chart: the notes per
/// lane and the timers, past the bare state of an empty chart.
#[test]
fn an_attempts_state_counts_its_buffers() {
    let empty = ChartBuilder::new(BPM_150).build().unwrap();
    let mut b = ChartBuilder::new(BPM_150);
    for k in 0..100 {
        b = b.tap(lane(1), TimeUs(1_000 * MS + k * 10 * MS));
    }
    let full_chart = b.build().unwrap();
    let base = Harness::empty(&empty, &full(1_000)).play.resident_bytes();
    let notes = Harness::empty(&full_chart, &full(1_000))
        .play
        .resident_bytes();
    assert!(base >= size_of::<skin_model::Play>() as u64, "{base}");
    // 100 notes of at least 16 bytes each, copied for the attempt.
    assert!(notes >= base + 100 * 16, "{base} {notes}");
}

/// Each gauge adds its descriptor and the reducer's record of its maximum,
/// when it reached it and when it did as the attempt started (what a
/// READY rearm restores), no more and no less.
#[test]
fn an_attempts_state_counts_each_gauge() {
    let chart = ChartBuilder::new(BPM_150).build().unwrap();
    let bytes = |gauges: &[GaugeDesc]| {
        let ctx = PlayContext {
            gauges,
            ..full(1_000)
        };
        Harness::empty(&chart, &ctx).play.resident_bytes()
    };
    let one = bytes(&GAUGES[..1]);
    let three = bytes(&[GAUGES[0], GAUGES[1], GAUGES[0]]);
    let each = size_of::<GaugeDesc>() + 2 * size_of::<Option<TimeUs>>() + size_of::<GaugeValue>();
    assert_eq!(three - one, 2 * each as u64);
}
