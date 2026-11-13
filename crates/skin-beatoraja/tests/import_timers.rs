//! Timers: the built-in namespace, custom aliases, the rank `when`
//! conditions, and untimed critical numbers (D3).

mod common;

use common::play::{judged, tally};
use common::{d3_notes, has, import_body};
use rules::Judgment;
use skin_beatoraja::{CriticalPart, ImportError, NoteKind, TimerWhy};

fn timer(why: TimerWhy) -> NoteKind {
    NoteKind::Timer(why)
}

/// Images on destination `timer`s: each moves from x 0 to 100 over
/// 100 ms, then holds.
fn images(timers: &[&str], custom: &str) -> common::Loaded {
    let dsts: Vec<String> = timers
        .iter()
        .map(|t| {
            format!(
                r#"{{"id": "i", {t} "loop": 100, "dst": [{{"time": 0, "x": 0, "w": 10, "h": 10}},
                                                        {{"time": 100, "x": 100}}]}}"#
            )
        })
        .collect();
    import_body(&format!(
        r#""customTimers": [{custom}],
           "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{}]"#,
        dsts.join(", ")
    ))
}

/// A two-frame image cycled every 100 ms on image timer `t`.
fn cycled(t: &str) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100 {t}}}],
           "destination": [{{"id": "i", "timer": 41, "dst": [{{"w": 10, "h": 10}}]}}]"#
    ))
}

/// `judgetimer` decides which grades start a lane's bomb timer: a
/// judgment at or below it, PERFECT 0 to empty POOR 5
/// (`play/JudgeManager.java:673`); 1 when absent (`JsonSkin.java:20`).
#[test]
fn judgetimer_is_the_bomb_threshold() {
    use bms_model::{Lane, TimeUs};
    use eval::{JudgeEvent, JudgeEventKind};
    let grades = [
        Judgment::Perfect,
        Judgment::Great,
        Judgment::Good,
        Judgment::Bad,
        Judgment::Poor,
    ];
    // Grade `g`'s press at 100 ms on lane 1, 5 for an empty POOR.
    let press = |g: usize| JudgeEvent {
        seq: 0,
        time: TimeUs(100 * common::play::MS),
        origin: None,
        kind: match grades.get(g) {
            Some(&judgment) => JudgeEventKind::Judged {
                lane: Lane::new(1).unwrap(),
                note: 0,
                judgment,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 0,
                presses: 0,
            },
            None => JudgeEventKind::EmptyPoor {
                lane: Lane::new(1).unwrap(),
                deviation: TimeUs(0),
                presses: 0,
            },
        },
    };
    for (field, up_to) in [
        ("", Some(1)),
        (r#""judgetimer": 0,"#, Some(0)),
        (r#""judgetimer": 1,"#, Some(1)),
        (r#""judgetimer": 2,"#, Some(2)),
        (r#""judgetimer": 3,"#, Some(3)),
        (r#""judgetimer": 4,"#, Some(4)),
        (r#""judgetimer": 5,"#, Some(5)),
        (r#""judgetimer": -1,"#, None),
        (r#""judgetimer": 2147483647,"#, Some(5)),
    ] {
        for g in 0..=5 {
            let mut d = import_body(&format!(
                r#"{field}
                   "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
                   "destination": [{{"id": "i", "timer": 51,
                                     "dst": [{{"w": 10, "h": 10}}]}}]"#
            ))
            .drawn();
            let drawn = !d.frame(100, &[press(g)]).is_empty();
            assert_eq!(drawn, up_to.is_some_and(|u| g <= u), "{field} grade {g}");
        }
    }
}

#[test]
fn timer_446_is_the_judgment_timer() {
    let mut d = images(&[r#""timer": 46,"#, r#""timer": 446,"#], "").drawn();
    assert!(d.frame(0, &[]).is_empty());
    let events = [
        (10, judged(10, Judgment::Perfect, 1)),
        (60, judged(60, Judgment::Poor, 0)),
        (90, judged(90, Judgment::Great, 1)),
    ];
    for t in 0..300 {
        let fx: Vec<_> = events
            .iter()
            .filter(|(at, _)| *at == t)
            .map(|(_, e)| *e)
            .collect();
        let q = d.frame(t, &fx);
        if t >= 10 {
            assert_eq!(q.len(), 2, "{t}");
            assert_eq!(q[0].rect, q[1].rect, "{t}");
        }
    }
}

/// Timers 46 and 446 start on every judgment whether or not the skin
/// draws a judge: as destination timers, through an alias and as cycle
/// timers.
#[test]
fn judge_timers_need_no_judge() {
    let alias = r#"{"id": 10000, "timer": 446}"#;
    let perfect = [judged(10, Judgment::Perfect, 1)];
    for t in [r#""timer": 46,"#, r#""timer": 446,"#, r#""timer": 10000,"#] {
        let loaded = images(&[t], alias);
        assert!(
            loaded.report().notes.is_empty(),
            "{t}: {:?}",
            loaded.report()
        );
        let mut d = loaded.drawn();
        assert!(d.frame(0, &[]).is_empty(), "{t}");
        assert_eq!(d.frame(10, &perfect).len(), 1, "{t}");
    }
    // A cycle on 46 runs from the judgment.
    let mut d = cycled(r#", "timer": 46"#).drawn();
    let first = d.frame(10, &perfect)[0].uv;
    assert_ne!(d.frame(160, &[])[0].uv, first);
}

#[test]
fn known_unsupported_and_unset_timers() {
    for n in [58, 2_999] {
        let loaded = images(&[&format!(r#""timer": {n},"#)], "");
        assert!(has(loaded.report(), timer(TimerWhy::Unsupported(n))));
        assert!(loaded.quads(0).is_empty());
        let loaded = cycled(&format!(r#", "timer": {n}"#));
        assert!(has(loaded.report(), timer(TimerWhy::Unsupported(n))));
        assert_eq!(loaded.report().usage.images, 0);
    }
    // Timer 0 is never set: never drawn as a destination timer ...
    let loaded = images(&[r#""timer": 0,"#], "");
    assert!(has(loaded.report(), timer(TimerWhy::Unset(0))));
    assert_eq!(loaded.report().usage.images, 0);
    // ... and frame 0 forever as a cycle timer.
    let loaded = cycled(r#", "timer": 0"#);
    assert!(has(loaded.report(), timer(TimerWhy::Unset(0))));
    let mut d = loaded.drawn();
    let first = d.frame(0, &[])[0].uv;
    assert_eq!(d.frame(150, &[])[0].uv, first);
    // On a running timer the frame changes.
    let mut d = cycled(r#", "timer": 41"#).drawn();
    let first = d.frame(0, &[])[0].uv;
    assert_ne!(d.frame(150, &[])[0].uv, first);
}

/// Timer 140 is the rhythm clock (`play/RhythmTimerProcessor.java:60-71`):
/// on the harness's 150 BPM chart with no bar line, 1 000 ms per beat
/// runs 2.5 ms of timer per ms of play, as a destination timer, an alias
/// and a cycle timer.
#[test]
fn timer_140_is_the_rhythm_clock() {
    let alias = r#"{"id": 10000, "timer": 140}"#;
    for t in [r#""timer": 140,"#, r#""timer": 10000,"#] {
        let loaded = images(&[t], alias);
        assert!(
            loaded.report().notes.is_empty(),
            "{t}: {:?}",
            loaded.report()
        );
        let mut d = loaded.drawn();
        // 20 ms of play at 150 BPM: 50 ms of the 100 ms move.
        assert_eq!(d.rects(20), [[50, 1_070, 10, 10]], "{t}");
        assert_eq!(d.rects(32), [[80, 1_070, 10, 10]], "{t}");
    }
    // A two-frame cycle of 100 ms on 140 shows its second frame from 50
    // ms of its time: 20 ms of play.
    let mut d = cycled(r#", "timer": 140"#).drawn();
    let first = d.frame(0, &[])[0].uv;
    assert_eq!(d.frame(19, &[])[0].uv, first);
    assert_ne!(d.frame(20, &[])[0].uv, first);
}

/// Timer 2, the fade-out, runs on play only from a full play's published
/// fade-out (docs/stage3.md D30): a destination on it is not drawn and a
/// cycle on it stays on its first frame through READY (a 2 s lead-in),
/// Playing, the audio tail's hold and Closing, while the READY, play and
/// failure timers run; from the fade-out's start both run. Neither is
/// noted or dropped. Told apart by width: 10 on timer 2, 11 on READY (40), 12
/// on play (41), 13 on failure (3), 14 untimed; the cycles are 15 on
/// timer 2 and 16 on READY.
#[test]
fn the_fade_out_timer_runs_only_in_the_fade_out() {
    use bms_model::TimeUs;
    use common::play::{Drawn, MS, context};
    use skin_model::{Attempt, PlayContext, timer as slot};
    let dst = |id: &str, t: &str, w: i32| {
        format!(r#"{{"id": "{id}", {t} "dst": [{{"x": 0, "y": 0, "w": {w}, "h": 10}}]}}"#)
    };
    let loaded = import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64}},
                     {{"id": "c2", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100, "timer": 2}},
                     {{"id": "c40", "src": "a", "w": 64, "h": 32, "divx": 2, "cycle": 100, "timer": 40}}],
           "destination": [{}, {}, {}, {}, {}, {}, {}]"#,
        dst("i", r#""timer": 2,"#, 10),
        dst("i", r#""timer": 40,"#, 11),
        dst("i", r#""timer": 41,"#, 12),
        dst("i", r#""timer": 3,"#, 13),
        dst("i", "", 14),
        dst("c2", "", 15),
        dst("c40", "", 16),
    ));
    let skin = loaded.ok().skin.clone();
    let mut d = Drawn::new(skin.clone(), 100);
    let lead_in = PlayContext {
        attempt: Attempt::Full {
            lead_in: TimeUs(2_000 * MS),
        },
        ..context()
    };
    d.play = skin.prepare(&d.chart, &lead_in).unwrap();
    // Each frame's widths, and the uv of each cycled image.
    let shown = |d: &mut Drawn, ms: i64| {
        let q = d.frame(ms, &[]);
        let w = |q: &skin_model::Quad| q.rect[2].round() as i32;
        let mut widths: Vec<i32> = q.iter().map(w).collect();
        widths.sort_unstable();
        let uv = |width| q.iter().find(|q| w(q) == width).map(|q| q.uv);
        (widths, uv(15), uv(16))
    };
    let (ready, c2, c40) = shown(&mut d, -1_500);
    assert_eq!(ready, [11, 14, 15, 16], "READY");
    assert_eq!(d.play.timer(slot::READY), Some(TimeUs(-2_000 * MS)));
    assert_eq!(d.play.timer(slot::PLAY), None, "READY is before the play");
    let (later, c2_ready, c40_ready) = shown(&mut d, -1_350);
    assert_eq!(later, ready);
    assert_eq!(c2_ready, c2, "READY: the cycle on 2 stays");
    assert_ne!(c40_ready, c40, "READY: the cycle on 40 runs");
    let (playing, c2_playing, _) = shown(&mut d, 1_550);
    assert_eq!(playing, [11, 12, 14, 15, 16], "Playing");
    assert_eq!(c2_playing, c2, "Playing");
    d.playing = false;
    d.complete = true;
    let (tail, c2_tail, _) = shown(&mut d, 3_050);
    assert_eq!(tail, [11, 12, 14, 15, 16], "the tail");
    assert_eq!(c2_tail, c2, "the tail");
    d.fading = Some(TimeUs(3_100 * MS));
    let (fading, c2_fading, _) = shown(&mut d, 3_250);
    assert_eq!(fading, [10, 11, 12, 14, 15, 16], "the fade-out");
    assert_ne!(c2_fading, c2, "the fade-out: the cycle on 2 runs");
    assert_eq!(d.play.timer(slot::FADEOUT), d.fading, "the fade-out runs");
    d.fading = None;
    d.complete = false;
    d.closing = Some(TimeUs(3_500 * MS));
    let (closing, c2_closing, _) = shown(&mut d, 4_150);
    assert_eq!(closing, [11, 12, 13, 14, 15, 16], "Closing");
    assert_eq!(c2_closing, c2, "Closing");
    assert_eq!(d.play.timer(slot::FAILED), d.closing, "the failure runs");
    assert_eq!(d.play.timer(slot::FADEOUT), None, "Closing never fades");
    let r = loaded.report();
    assert!(r.notes.is_empty(), "{r:?}");
}

#[test]
fn custom_timers_alias_supported_built_ins() {
    // A declaration in the built-in range has no effect.
    let loaded = images(&[r#""timer": 2999,"#], r#"{"id": 2999, "timer": 41}"#);
    assert!(has(loaded.report(), timer(TimerWhy::CustomBuiltin(2_999))));
    assert!(has(loaded.report(), timer(TimerWhy::Unsupported(2_999))));
    // An alias of `play`; the last of an id wins.
    let custom = r#"{"id": 10000, "timer": 2999}, {"id": 10000, "timer": 41},
                    {"id": 10001, "timer": 10000}, {"id": 10002},
                    {"id": 10003, "timer": "script()"}, {"id": 10004, "timer": 58}"#;
    let loaded = images(&[r#""timer": 10000,"#], custom);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    assert_eq!(loaded.drawn().rects(50), [[50, 1_070, 10, 10]]);
    for id in [10_001, 10_002, 10_003, 10_004, 10_005] {
        let loaded = images(&[&format!(r#""timer": {id},"#)], custom);
        assert!(has(loaded.report(), timer(TimerWhy::Custom(id))), "{id}");
    }
}

/// The round's judgments behind an EX score of `ex`: PERFECTs and at most
/// one GREAT.
fn judged_ex(ex: u32) -> eval::Tally {
    let mut t = tally(ex, 1);
    t.counts.perfect = ex / 2;
    t.counts.great = ex % 2;
    t
}

#[test]
fn rank_timers_open_at_their_thresholds() {
    // ⌈2·i·100/27⌉ for i = 18, 21, 24.
    for (id, ex) in [(348, 134), (349, 156), (350, 178)] {
        let mut d = images(&[&format!(r#""timer": {id},"#)], "").drawn();
        d.tally = judged_ex(ex - 1);
        assert!(d.frame(10, &[]).is_empty(), "{id}");
        d.tally = judged_ex(ex);
        assert_eq!(d.frame(20, &[]).len(), 1, "{id}");
        // The timer starts when the condition first holds.
        assert_eq!(d.rects(70), [[50, 1_070, 10, 10]]);
        // An EX score the round has no judgment for (a play state's carried
        // one) waits (s7-06s-design.md §2.4).
        let mut d = images(&[&format!(r#""timer": {id},"#)], "").drawn();
        d.tally = tally(ex, 1);
        assert!(d.frame(10, &[]).is_empty(), "{id}: unjudged");
    }
    // At 27 notes each threshold is a whole score, 2·i, and opens there.
    for (id, ex) in [(348, 36), (349, 42), (350, 48)] {
        let mut d = images(&[&format!(r#""timer": {id},"#)], "").drawn_over(27);
        d.tally = judged_ex(ex - 1);
        assert!(d.frame(10, &[]).is_empty(), "{id}");
        d.tally = judged_ex(ex);
        assert_eq!(d.frame(20, &[]).len(), 1, "{id}");
    }
    // No notes: never.
    let mut d = images(&[r#""timer": 348,"#], "").drawn_over(0);
    d.tally = judged_ex(1_000);
    assert!(d.frame(10, &[]).is_empty());
    // Each rank compiles once per skin.
    let loaded = images(
        &[r#""timer": 348,"#, r#""timer": 348,"#, r#""timer": 350,"#],
        "",
    );
    assert_eq!(loaded.report().usage.whens, 2);
}

/// A critical EX score with no timer and the given image fields and keys.
fn untimed(image: &str, dst: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 71 {image}}}],
           "destination": [{{"id": "v", {dst}}}]"#
    ))
}

#[test]
fn untimed_critical_numbers_vary_only_their_presentation_phase() {
    // Static from 0 and perpetual: admitted without a note.
    for dst in [
        r#""dst": [{"time": 0, "w": 12, "h": 20}]"#,
        r#""timer": -1, "dst": [{"time": 0, "w": 12, "h": 20}]"#,
        r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100}]"#,
    ] {
        let loaded = untimed("", dst);
        assert!(loaded.report().notes.is_empty(), "{dst}");
        assert_eq!(loaded.quads(0).len(), 1);
    }
    // A cycle on a supported timer keeps it independent of the start.
    let n20 = r#", "w": 120, "divx": 20, "cycle": 100, "timer": 41"#;
    let accepted = untimed(n20, r#""dst": [{"w": 12, "h": 20}]"#);
    assert!(accepted.report().notes.is_empty());
    // Alpha, a delayed first key, an ending window and an untimed cycle
    // run on `screen` from READY, each noted once.
    for (image, dst) in [
        (
            "",
            r#""loop": 100, "dst": [{"time": 0, "w": 12, "h": 20, "a": 0}, {"time": 100, "a": 255}]"#,
        ),
        ("", r#""loop": 10, "dst": [{"time": 10, "w": 12, "h": 20}]"#),
        (
            "",
            r#""loop": -1, "dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100}]"#,
        ),
        (
            r#", "w": 120, "divx": 20, "cycle": 100"#,
            r#""dst": [{"w": 12, "h": 20}]"#,
        ),
    ] {
        let l = untimed(image, dst);
        assert_eq!(d3_notes(&l), 1, "{image} {dst}");
    }
    let alpha = |ms| {
        let l = untimed(
            "",
            r#""loop": 100, "dst": [{"time": 0, "w": 12, "h": 20, "a": 0}, {"time": 100, "a": 255}]"#,
        );
        l.quads(ms).first().map(|q| q.color[3])
    };
    assert_eq!(alpha(0), Some(0.0));
    assert_eq!(alpha(100), Some(1.0));
    let delayed = |ms| {
        untimed("", r#""loop": 10, "dst": [{"time": 10, "w": 12, "h": 20}]"#)
            .quads(ms)
            .len()
    };
    assert_eq!((delayed(5), delayed(10)), (0, 1));
    let ending = |ms| {
        untimed(
            "",
            r#""loop": -1, "dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100}]"#,
        )
        .quads(ms)
        .len()
    };
    assert_eq!((ending(100), ending(101)), (1, 0));
    // The untimed cycle's frame by `screen` time: 50 ms a frame.
    let uv = |ms| {
        untimed(
            r#", "w": 120, "divx": 20, "cycle": 100"#,
            r#""dst": [{"w": 12, "h": 20}]"#,
        )
        .quads(ms)[0]
            .uv
    };
    assert_ne!(uv(0), uv(50));
    assert_eq!(uv(0), uv(100));
    // Moving, resizing, turning or changing a colour channel other than
    // alpha refuses.
    for dst in [
        r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100, "x": 5}]"#,
        r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100, "w": 24}]"#,
        r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100, "angle": 90}]"#,
        r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100, "r": 0}]"#,
    ] {
        let e = untimed("", dst).err();
        assert!(
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::ExScore,
                    why: NoteKind::Timer(TimerWhy::Untimed),
                    ..
                }
            ),
            "{dst}: {e:?}"
        );
        // On a timer of its own it imports, without a note.
        let timed = untimed("", &format!(r#""timer": 41, {dst}"#));
        assert_eq!(d3_notes(&timed), 0, "{dst}");
    }
    // An untimed decorative number animates freely, without a note.
    let l = import_body(
        r#""value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 100, "cycle": 100}],
           "destination": [{"id": "v", "dst": [{"time": 0, "w": 12, "h": 20},
                                               {"time": 100, "x": 5, "r": 0}]}]"#,
    );
    assert_eq!(d3_notes(&l), 0);
}

/// The key-on timers 100–107 and key-off timers 120–127 map through the
/// import to the scratch's and keys 1–7's `press` and `release`, running
/// from the frame's lane input, one excluding the other; 108, 109, 128 and
/// 129, keys this 7-key mode does not have, are never set: never drawn,
/// whatever the input, and never another lane's.
#[test]
fn key_on_and_key_off_timers_follow_the_lane_input() {
    use bms_model::{Lane, TimeUs};
    for n in 0..8_u8 {
        let lane = Lane::new(n).unwrap();
        let (on, off) = (100 + i32::from(n), 120 + i32::from(n));
        let loaded = images(
            &[
                &format!(r#""timer": {on},"#),
                &format!(r#""timer": {off},"#),
            ],
            "",
        );
        assert!(
            loaded.report().notes.is_empty(),
            "{n}: {:?}",
            loaded.report()
        );
        let mut d = loaded.drawn();
        // Never pressed: neither runs.
        assert!(d.frame(10, &[]).is_empty(), "{n}");
        // Held from 10 ms: key-on only, 40 ms along at 50 ms.
        d.input.press(lane, TimeUs(10 * common::play::MS));
        assert_eq!(d.rects(50), [[40, 1_070, 10, 10]], "{n}");
        // Released at 60 ms: key-off only, 20 ms along at 80 ms.
        d.input.release(lane, TimeUs(60 * common::play::MS));
        assert_eq!(d.rects(80), [[20, 1_070, 10, 10]], "{n}");
        // Another lane's input runs neither.
        let mut d = images(&[&format!(r#""timer": {on},"#)], "").drawn();
        let other = Lane::new((n + 1) % 8).unwrap();
        d.input.press(other, TimeUs(0));
        assert!(d.frame(50, &[]).is_empty(), "{n}");
    }
    for n in [108, 109, 128, 129] {
        let loaded = images(&[&format!(r#""timer": {n},"#)], "");
        assert!(has(loaded.report(), timer(TimerWhy::Unset(n))), "{n}");
        assert_eq!(loaded.report().usage.images, 0, "{n}");
    }
}
