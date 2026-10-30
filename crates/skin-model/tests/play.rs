//! A play through `Skin`: retries, countdowns, practice rounds, the beat
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
    Attempt, Blend, ClipId, Ease, ElementDef, End, ErrorKind, Export, KeyDef, LoadError,
    MAX_ELEMENTS, MAX_OPS, MAX_SKIN_KEYS, MAX_WHEN_TIMERS, PLAY, PlayContext, PrepareError, Prop,
    Schema, Scope, TIMER_OFF, TimerDef, TimerRef, Track, Ty, compile, field, timer,
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
        gauges: &GAUGES,
        attempt: Attempt::Section(s),
        empty_poor_breaks_combo: false,
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
fn a_mid_chart_countdown() {
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
    // A countdown that would start before the time range is refused.
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
        assert_eq!(h.start(timer::LAST_NOTE), None);
        h.frame(
            19_000,
            &[
                ev(19_000, play_support::hold_ended(2, 0)),
                ev(19_000, judged(2, 0, Judgment::Perfect, Some(0), None, 3)),
            ],
        );
        assert_eq!(h.start(timer::LAST_NOTE), Some(19_000 * MS));
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
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: preset.empty_poor.breaks_combo,
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
    let mut props = [None; 9];
    props[Prop::Angle as usize] = Some(1);
    let rotated = ElementDef {
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        ..el()
    };
    assert!(matches!(
        load(Vec::new(), vec![rotated]),
        Err(LoadError::Rotation { element: 0 })
    ));
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
