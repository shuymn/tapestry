//! Timers: the built-in namespace, custom aliases, the rank `when`
//! conditions, and untimed critical numbers (D3).

mod common;

use common::play::{judged, tally};
use common::{has, import_body};
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
            },
            None => JudgeEventKind::EmptyPoor {
                lane: Lane::new(1).unwrap(),
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

#[test]
fn known_unsupported_and_unset_timers() {
    for n in [140, 2] {
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

#[test]
fn custom_timers_alias_supported_built_ins() {
    // A declaration in the built-in range has no effect.
    let loaded = images(&[r#""timer": 2,"#], r#"{"id": 2, "timer": 41}"#);
    assert!(has(loaded.report(), timer(TimerWhy::CustomBuiltin(2))));
    assert!(has(loaded.report(), timer(TimerWhy::Unsupported(2))));
    // An alias of `play`; the last of an id wins.
    let custom = r#"{"id": 10000, "timer": 2}, {"id": 10000, "timer": 41},
                    {"id": 10001, "timer": 10000}, {"id": 10002},
                    {"id": 10003, "timer": "script()"}, {"id": 10004, "timer": 140}"#;
    let loaded = images(&[r#""timer": 10000,"#], custom);
    assert!(loaded.report().notes.is_empty(), "{:?}", loaded.report());
    assert_eq!(loaded.drawn().rects(50), [[50, 1_070, 10, 10]]);
    for id in [10_001, 10_002, 10_003, 10_004, 10_005] {
        let loaded = images(&[&format!(r#""timer": {id},"#)], custom);
        assert!(has(loaded.report(), timer(TimerWhy::Custom(id))), "{id}");
    }
}

#[test]
fn rank_timers_open_at_their_thresholds() {
    // ⌈2·i·100/27⌉ for i = 18, 21, 24.
    for (id, ex) in [(348, 134), (349, 156), (350, 178)] {
        let mut d = images(&[&format!(r#""timer": {id},"#)], "").drawn();
        d.tally = tally(ex - 1, 1);
        assert!(d.frame(10, &[]).is_empty(), "{id}");
        d.tally = tally(ex, 1);
        assert_eq!(d.frame(20, &[]).len(), 1, "{id}");
        // The timer starts when the condition first holds.
        assert_eq!(d.rects(70), [[50, 1_070, 10, 10]]);
    }
    // At 27 notes each threshold is a whole score, 2·i, and opens there.
    for (id, ex) in [(348, 36), (349, 42), (350, 48)] {
        let mut d = images(&[&format!(r#""timer": {id},"#)], "").drawn_over(27);
        d.tally = tally(ex - 1, 1);
        assert!(d.frame(10, &[]).is_empty(), "{id}");
        d.tally = tally(ex, 1);
        assert_eq!(d.frame(20, &[]).len(), 1, "{id}");
    }
    // No notes: never.
    let mut d = images(&[r#""timer": 348,"#], "").drawn_over(0);
    d.tally = tally(1_000, 1);
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
fn untimed_critical_numbers_must_not_depend_on_the_start() {
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
    for (image, dst) in [
        (
            "",
            r#""dst": [{"time": 0, "w": 12, "h": 20}, {"time": 100, "x": 5}]"#,
        ),
        ("", r#""loop": 10, "dst": [{"time": 10, "w": 12, "h": 20}]"#),
        ("", r#""loop": -1, "dst": [{"time": 0, "w": 12, "h": 20}]"#),
        (
            r#", "divx": 20, "cycle": 100"#,
            r#""dst": [{"w": 12, "h": 20}]"#,
        ),
    ] {
        let e = untimed(image, dst).err();
        assert!(
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::ExScore,
                    why: NoteKind::Timer(TimerWhy::Untimed),
                    ..
                }
            ),
            "{image} {dst}: {e:?}"
        );
    }
}
